//! Parallel, resumable scan of a directory into a [`Tree`].
//!
//! Worker threads read one folder at a time with `dua-core`'s native reader, which uses
//! `getattrlistbulk` to return names and sizes for a whole folder per system call. One thread
//! applies each folder's listing to the shared tree, so the UI can show live totals, and
//! queues the subfolders it finds. Because a folder counts as read only once its listing is
//! applied, a scan can stop at any time and later continue with exactly the folders that are
//! still queued.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use dua_core::Options;
use parking_lot::{Condvar, Mutex, RwLock};

use crate::tree::{Kind, NodeId, ROOT, Tree, flags};
use crate::volumes::Boundary;

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    /// Directories or entries that could not be read, usually for lack of permission.
    pub errors: AtomicU64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    /// Stopped early. These folders were found but not read yet; scanning them continues
    /// the scan.
    Paused(Vec<NodeId>),
}

/// Hard links already counted, kept across pauses of one scan: (device, inode).
pub type CountedLinks = HashSet<(u64, u64)>;

/// How long the applying thread waits for listings before checking whether to stop.
const POLL: Duration = Duration::from_millis(50);

/// Reads the folders `folders` of `tree` and everything below them that `boundary` allows.
/// Pass `vec![ROOT]` for a new scan, or a previous [`Outcome::Paused`] list to continue one.
/// Stops when `stop` is set, or after `deadline` once at least one folder was read.
pub fn scan(
    tree: &RwLock<Tree>,
    boundary: &Boundary,
    progress: &Progress,
    stop: &AtomicBool,
    folders: Vec<NodeId>,
    deadline: Option<Instant>,
    counted: &mut CountedLinks,
) -> Outcome {
    let jobs: Vec<(NodeId, PathBuf)> = {
        let mut t = tree.write();
        if folders.contains(&ROOT) && t.node(ROOT).first_child == crate::tree::NONE {
            let mtime = std::fs::symlink_metadata(t.root_path()).ok().map_or(0, |m| mtime_of(&m));
            t.set_mtime(ROOT, mtime);
        }
        // Folders removed while the scan was paused are not read.
        folders.into_iter().filter(|&id| t.is_attached(id)).map(|id| (id, t.path(id))).collect()
    };
    let queue = Mutex::new(Queue { jobs, busy: 0, closed: false });
    let ready = Condvar::new();
    let (tx, rx) = mpsc::channel::<Listing>();
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let mut applier = Applier { boundary, progress, counted };

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            let (queue, ready) = (&queue, &ready);
            scope.spawn(move || work(queue, ready, tx));
        }
        drop(tx);

        let mut applied_any = false;
        loop {
            {
                let q = queue.lock();
                if q.jobs.is_empty() && q.busy == 0 {
                    break;
                }
            }
            let past_deadline = deadline.is_some_and(|d| Instant::now() >= d) && applied_any;
            if stop.load(Ordering::Relaxed) || past_deadline {
                break;
            }
            let Ok(first) = rx.recv_timeout(POLL) else {
                continue;
            };
            let listings: Vec<Listing> = std::iter::once(first).chain(rx.try_iter()).collect();
            let mut found = Vec::new();
            {
                let mut t = tree.write();
                for listing in &listings {
                    applier.apply(&mut t, listing, &mut found);
                }
            }
            applied_any = true;
            let mut q = queue.lock();
            q.busy -= listings.len();
            q.jobs.extend(found);
            ready.notify_all();
        }
        // Stop handing out folders. Workers finish the folder they are reading.
        queue.lock().closed = true;
        ready.notify_all();
    });
    // Every worker has finished: apply what they read last, so no read folder is lost.
    let mut found = Vec::new();
    {
        let mut t = tree.write();
        for listing in rx.try_iter() {
            applier.apply(&mut t, &listing, &mut found);
        }
    }
    let mut pending: Vec<NodeId> = queue.into_inner().jobs.into_iter().map(|(id, _)| id).collect();
    pending.extend(found.into_iter().map(|(id, _)| id));
    if pending.is_empty() { Outcome::Complete } else { Outcome::Paused(pending) }
}

struct Queue {
    /// Folders found but not read yet.
    jobs: Vec<(NodeId, PathBuf)>,
    /// Folders taken by a worker whose listing is not applied yet.
    busy: usize,
    closed: bool,
}

struct Listing {
    dir: NodeId,
    path: PathBuf,
    items: io::Result<Vec<Item>>,
}

struct Item {
    name: OsString,
    kind: Kind,
    meta: Option<Meta>,
}

struct Meta {
    /// Allocated bytes on disk.
    size: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
    mtime: u32,
}

fn mtime_of(m: &std::fs::Metadata) -> u32 {
    u32::try_from(m.mtime().max(0)).unwrap_or(u32::MAX)
}

fn work(queue: &Mutex<Queue>, ready: &Condvar, tx: mpsc::Sender<Listing>) {
    loop {
        let (dir, path) = {
            let mut q = queue.lock();
            loop {
                if q.closed {
                    return;
                }
                if let Some(job) = q.jobs.pop() {
                    q.busy += 1;
                    break job;
                }
                ready.wait(&mut q);
            }
        };
        let items = read_folder(&path);
        if tx.send(Listing { dir, path, items }).is_err() {
            return;
        }
    }
}

/// One folder's entries with their metadata, from a single bulk read where possible.
fn read_folder(path: &Path) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
    for entry in dua_core::read_dir(path, Options::default())? {
        let Ok(entry) = entry else {
            continue;
        };
        let ft = entry.file_type;
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        let meta = match &entry.metadata {
            Some(Ok(m)) => Some(Meta {
                size: m.allocated_size(),
                dev: m.dev(),
                ino: m.ino(),
                // Only files need it; for folders it would cost another system call.
                nlink: if kind == Kind::File { m.nlink() } else { 1 },
                mtime: m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as u32),
            }),
            // The bulk read did not include metadata for this entry; ask for it directly.
            _ => std::fs::symlink_metadata(path.join(&entry.file_name)).ok().map(|m| Meta {
                size: m.blocks() * 512,
                dev: m.dev(),
                ino: m.ino(),
                nlink: m.nlink(),
                mtime: mtime_of(&m),
            }),
        };
        items.push(Item { name: entry.file_name, kind, meta });
    }
    Ok(items)
}

struct Applier<'a> {
    boundary: &'a Boundary,
    progress: &'a Progress,
    counted: &'a mut CountedLinks,
}

impl Applier<'_> {
    /// Adds a folder's entries to the tree and returns the subfolders to read in `found`.
    fn apply(&mut self, tree: &mut Tree, listing: &Listing, found: &mut Vec<(NodeId, PathBuf)>) {
        let Ok(items) = &listing.items else {
            self.progress.errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for item in items {
            if self.boundary.excludes(&listing.path, &item.name) {
                continue;
            }
            let name = item.name.as_bytes();
            let mut node_flags = 0;
            if name.first() == Some(&b'.') {
                node_flags |= flags::HIDDEN;
            }
            if item.kind == Kind::Dir && is_package(name) {
                node_flags |= flags::PACKAGE;
            }
            let Some(meta) = &item.meta else {
                self.progress.errors.fetch_add(1, Ordering::Relaxed);
                tree.add(listing.dir, name, item.kind, node_flags, 0, 0);
                continue;
            };
            let mut size = meta.size;
            if item.kind == Kind::File && meta.nlink > 1 && !self.counted.insert((meta.dev, meta.ino)) {
                size = 0;
                node_flags |= flags::HARDLINK;
            }
            let descend = item.kind == Kind::Dir && self.boundary.allows_device(meta.dev);
            if item.kind == Kind::Dir && !descend {
                // A mount point of another volume; its contents belong to that volume.
                size = 0;
                node_flags |= flags::PRUNED;
            }
            let id = tree.add(listing.dir, name, item.kind, node_flags, size, meta.mtime);
            if descend {
                found.push((id, listing.path.join(&item.name)));
            }
            let counter = if item.kind == Kind::Dir { &self.progress.dirs } else { &self.progress.files };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Directory extensions that Finder shows as a single item.
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app", "appex", "bundle", "framework", "plugin", "kext", "xpc", "prefpane", "qlgenerator",
    "mdimporter", "saver", "dsym", "xcarchive", "xcodeproj", "xcworkspace", "playground",
    "photoslibrary", "musiclibrary", "tvlibrary", "imovielibrary", "fcpbundle", "logicx",
    "band", "rtfd", "pages", "numbers", "key", "sparsebundle", "pvm", "utm", "vmwarevm",
    "scptd", "workflow",
];

fn is_package(name: &[u8]) -> bool {
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else {
        return false;
    };
    let ext = &name[dot + 1..];
    dot > 0
        && PACKAGE_EXTENSIONS
            .iter()
            .any(|p| p.as_bytes().eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_dir(dir: &Path) -> Tree {
        let tree = RwLock::new(Tree::new(dir.to_path_buf(), "root", 0));
        let boundary = Boundary::new(dir);
        let stop = AtomicBool::new(false);
        let outcome = scan(&tree, &boundary, &Progress::default(), &stop, vec![ROOT], None, &mut CountedLinks::new());
        assert_eq!(outcome, Outcome::Complete);
        tree.into_inner()
    }

    /// Every path below the root with its size, for comparing two scans.
    fn listing(t: &Tree) -> Vec<(PathBuf, u64)> {
        let mut out = Vec::new();
        let mut stack = vec![ROOT];
        while let Some(id) = stack.pop() {
            out.push((t.path(id), t.size(id)));
            stack.extend(t.children(id));
        }
        out.sort();
        out
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for a in 0..6 {
            for b in 0..5 {
                write(&root.join(format!("d{a}/e{b}/file.bin")), 1000 * (a + 1) * (b + 1));
                write(&root.join(format!("d{a}/e{b}/sub/deeper/x.txt")), 10);
            }
        }
        std::fs::hard_link(root.join("d0/e0/file.bin"), root.join("d5/link.bin")).unwrap();
        tmp
    }

    fn allocated(p: &Path) -> u64 {
        std::fs::symlink_metadata(p).unwrap().blocks() * 512
    }

    fn write(p: &Path, len: usize) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; len]).unwrap();
        p.to_path_buf()
    }

    #[test]
    fn totals_match_the_space_the_files_occupy_on_disk() {
        // The whole point of the tool: a folder's size must equal what deleting it frees,
        // which is the allocated size, not the logical length.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = write(&root.join("a/one.bin"), 100_000);
        let b = write(&root.join("a/deep/two.bin"), 5);
        let c = write(&root.join("three.bin"), 300_000);
        let t = scan_dir(&root);

        let a_dir = t.find(&root.join("a")).expect("a");
        let deep = t.find(&root.join("a/deep")).expect("deep");
        let dirs: u64 = [root.join("a"), root.join("a/deep")].iter().map(|p| allocated(p)).sum();
        assert_eq!(t.size(t.find(&b).unwrap()), allocated(&b));
        assert_eq!(t.size(deep), allocated(&b) + allocated(&root.join("a/deep")));
        assert_eq!(t.size(a_dir), allocated(&a) + allocated(&b) + dirs);
        assert_eq!(t.size(ROOT), allocated(&a) + allocated(&b) + allocated(&c) + dirs);
        assert!(t.node(t.find(&c).unwrap()).mtime > 0);
    }

    #[test]
    fn hard_links_are_counted_once() {
        // Hard links share their blocks; counting each link would overstate usage and
        // promise space that deleting one link does not free.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let original = write(&root.join("x/original.bin"), 200_000);
        std::fs::create_dir_all(root.join("y")).unwrap();
        std::fs::hard_link(&original, root.join("y/link.bin")).unwrap();
        let t = scan_dir(&root);

        let x = t.find(&root.join("x/original.bin")).unwrap();
        let y = t.find(&root.join("y/link.bin")).unwrap();
        let sizes = [t.size(x), t.size(y)];
        assert!(sizes.contains(&allocated(&original)) && sizes.contains(&0));
        let dup = if t.size(x) == 0 { x } else { y };
        assert!(t.node(dup).has(flags::HARDLINK));
    }

    #[test]
    fn symlinks_are_listed_but_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let target = write(&root.join("real/big.bin"), 500_000);
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let t = scan_dir(&root);

        let alias = t.find(&root.join("alias")).expect("symlink is listed");
        assert_eq!(t.node(alias).kind(), Kind::Symlink);
        assert!(t.size(alias) < allocated(&target));
        assert_eq!(t.children(alias).count(), 0);
    }

    #[test]
    fn packages_and_hidden_items_are_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root.join("Foo.app/Contents/MacOS/foo"), 10);
        write(&root.join(".cache/blob"), 10);
        write(&root.join("not.app.txt"), 10);
        let t = scan_dir(&root);

        assert!(t.node(t.find(&root.join("Foo.app")).unwrap()).has(flags::PACKAGE));
        assert!(t.node(t.find(&root.join(".cache")).unwrap()).has(flags::HIDDEN));
        assert!(!t.node(t.find(&root.join("not.app.txt")).unwrap()).has(flags::PACKAGE));
    }

    #[test]
    fn a_stopped_scan_reports_the_folders_still_to_read() {
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        let tree = RwLock::new(Tree::new(root.clone(), "root", 0));
        let stop = AtomicBool::new(true);
        let outcome = scan(&tree, &Boundary::new(&root), &Progress::default(), &stop, vec![ROOT], None, &mut CountedLinks::new());
        assert!(matches!(outcome, Outcome::Paused(ref p) if !p.is_empty()), "{outcome:?}");
    }

    #[test]
    fn a_scan_paused_many_times_ends_exactly_like_one_uninterrupted_scan() {
        // Quitting or pausing must never lose or double-count anything: every folder is
        // either read and applied, or handed back to be read later.
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        let whole = scan_dir(&root);

        let tree = RwLock::new(Tree::new(root.clone(), "root", 0));
        let boundary = Boundary::new(&root);
        let stop = AtomicBool::new(false);
        let mut counted = CountedLinks::new();
        let mut folders = vec![ROOT];
        let mut rounds = 0;
        loop {
            rounds += 1;
            // A deadline in the past pauses after the first listings are applied in each round.
            match scan(&tree, &boundary, &Progress::default(), &stop, folders, Some(Instant::now()), &mut counted) {
                Outcome::Complete => break,
                Outcome::Paused(rest) => folders = rest,
            }
            assert!(rounds < 1000, "no progress");
        }
        // Workers read ahead until the queue closes, but a round still gets at most two
        // levels deeper, and the fixture is five levels deep.
        assert!(rounds >= 3, "the scan should have been paused several times, took {rounds}");
        let resumed = tree.into_inner();
        assert_eq!(listing(&resumed), listing(&whole));
        assert_eq!(resumed.size(ROOT), whole.size(ROOT));
    }

    #[test]
    fn folders_removed_while_paused_are_skipped() {
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        let tree = RwLock::new(Tree::new(root.clone(), "root", 0));
        let boundary = Boundary::new(&root);
        let stop = AtomicBool::new(false);
        let mut counted = CountedLinks::new();
        let Outcome::Paused(pending) =
            scan(&tree, &boundary, &Progress::default(), &stop, vec![ROOT], Some(Instant::now()), &mut counted)
        else {
            panic!("expected a pause");
        };
        let gone = pending[0];
        tree.write().remove(gone);
        let mut folders = pending;
        while let Outcome::Paused(rest) = scan(&tree, &boundary, &Progress::default(), &stop, folders, None, &mut counted) {
            folders = rest;
        }
        assert!(!tree.read().is_attached(gone));
    }

    #[test]
    fn package_detection_needs_a_real_extension() {
        assert!(is_package(b"Xcode.app"));
        assert!(is_package(b"Photos Library.photoslibrary"));
        assert!(is_package(b"x.APP"));
        assert!(!is_package(b".app"));
        assert!(!is_package(b"app"));
        assert!(!is_package(b"notes.txt"));
    }
}
