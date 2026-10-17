//! Parallel, resumable scan of a directory into a [`Store`].
//!
//! Worker threads read one folder at a time with `dua-core`'s native reader, which uses
//! `getattrlistbulk` to return names and sizes for a whole folder per system call. One thread
//! records each folder's listing in the [`Store`], so the UI can show live totals, and queues
//! the subfolders it finds. Because a folder counts as read only once its listing is
//! recorded, a scan can stop at any time and later continue with the store's pending
//! folders. Stopping abandons folders being read, so a pause does not wait for a folder
//! with hundreds of thousands of items; they are read again when the scan continues.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use dua_core::Options;
use parking_lot::{Condvar, Mutex};

use crate::store::{DirId, Kind, NewEntry, Store, flags};
use crate::volumes::Boundary;

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    /// Directories or entries that could not be read, usually for lack of permission.
    pub errors: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    /// Stopped early; the store's pending folders are still to be read.
    Paused,
}

/// How long the applying thread waits for listings before checking whether to stop.
const POLL: Duration = Duration::from_millis(50);

/// Reads the folders `folders` of `store` and everything below them that `boundary` allows.
/// Pass the root folder (0) for a new scan, or [`Store::pending`] to continue one.
/// Stops soon after `stop` is set, or after `deadline` once at least one folder was read and
/// the folders being read are finished: a folder abandoned at every deadline would never be
/// read completely.
pub fn scan(
    store: &Store,
    boundary: &Boundary,
    progress: &Progress,
    stop: &AtomicBool,
    folders: Vec<DirId>,
    deadline: Option<Instant>,
) -> Outcome {
    let jobs: Vec<(DirId, PathBuf)> = folders.into_iter().map(|d| (d, store.dir_path(d))).collect();
    let queue = Mutex::new(Queue { jobs, busy: 0, closed: false });
    let ready = Condvar::new();
    let (tx, rx) = mpsc::channel::<Listing>();
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let applier = Applier { store, boundary, progress };

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            let (queue, ready) = (&queue, &ready);
            scope.spawn(move || work(queue, ready, stop, tx));
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
            for listing in &listings {
                if applier.apply(listing, &mut found).is_err() {
                    // The scan file cannot grow any further.
                    stop.store(true, Ordering::Relaxed);
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
    // Every worker has finished: record what they read last, so no read folder is lost.
    let mut found = Vec::new();
    for listing in rx.try_iter() {
        let _ = applier.apply(&listing, &mut found);
    }
    if queue.into_inner().jobs.is_empty() && found.is_empty() { Outcome::Complete } else { Outcome::Paused }
}

struct Queue {
    /// Folders found but not read yet.
    jobs: Vec<(DirId, PathBuf)>,
    /// Folders taken by a worker whose listing is not applied yet.
    busy: usize,
    closed: bool,
}

struct Listing {
    dir: DirId,
    path: PathBuf,
    /// `None` if reading was abandoned because the scan stopped.
    items: Option<io::Result<Vec<Item>>>,
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

/// Modification time of `path` in seconds since the epoch, 0 if unknown.
pub fn mtime_of_path(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).ok().map_or(0, |m| mtime_of(&m))
}

fn mtime_of(m: &std::fs::Metadata) -> u32 {
    u32::try_from(m.mtime().max(0)).unwrap_or(u32::MAX)
}

fn work(queue: &Mutex<Queue>, ready: &Condvar, stop: &AtomicBool, tx: mpsc::Sender<Listing>) {
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
        let items = read_folder(&path, stop);
        if tx.send(Listing { dir, path, items }).is_err() {
            return;
        }
    }
}

/// One folder's entries with their metadata, from a single bulk read where possible. `None`
/// if `stop` was set before the whole folder was read.
fn read_folder(path: &Path, stop: &AtomicBool) -> Option<io::Result<Vec<Item>>> {
    if stop.load(Ordering::Relaxed) {
        return None;
    }
    let entries = match dua_core::read_dir(path, Options::default()) {
        Ok(entries) => entries,
        Err(e) => return Some(Err(e)),
    };
    let mut items = Vec::new();
    for entry in entries {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
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
    Some(Ok(items))
}

struct Applier<'a> {
    store: &'a Store,
    boundary: &'a Boundary,
    progress: &'a Progress,
}

impl Applier<'_> {
    /// Records a folder's entries and returns the subfolders to read in `found`. A folder that
    /// could not be read counts as read and empty, so it is not retried forever.
    fn apply(&self, listing: &Listing, found: &mut Vec<(DirId, PathBuf)>) -> io::Result<()> {
        let Some(items) = &listing.items else {
            // Abandoned: the folder still has to be read.
            found.push((listing.dir, listing.path.clone()));
            return Ok(());
        };
        let items: &[Item] = match items {
            Ok(items) => items,
            Err(_) => {
                self.progress.errors.fetch_add(1, Ordering::Relaxed);
                &[]
            }
        };
        let mut entries = Vec::with_capacity(items.len());
        let mut descended = Vec::new();
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
                entries.push(NewEntry { name: item.name.clone(), kind: item.kind, flags: node_flags, size: 0, mtime: 0, link: None, descend: false });
                continue;
            };
            let mut size = meta.size;
            let descend = item.kind == Kind::Dir && self.boundary.allows_device(meta.dev);
            if item.kind == Kind::Dir && !descend {
                // A mount point of another volume; its contents belong to that volume.
                size = 0;
                node_flags |= flags::PRUNED;
            }
            let link = (item.kind == Kind::File && meta.nlink > 1).then_some((meta.dev, meta.ino));
            if descend {
                descended.push(listing.path.join(&item.name));
            }
            entries.push(NewEntry { name: item.name.clone(), kind: item.kind, flags: node_flags, size, mtime: meta.mtime, link, descend });
            let counter = if item.kind == Kind::Dir { &self.progress.dirs } else { &self.progress.files };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        let dirs = self.store.add_listing(listing.dir, entries)?;
        found.extend(dirs.into_iter().zip(descended));
        Ok(())
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

    use crate::store::ROOT;

    /// A store whose file is deleted at once; the store keeps reading it.
    fn new_store(root: &Path) -> Store {
        let files = tempfile::tempdir().unwrap();
        Store::create(&files.path().join("t.scan"), root, "root", 0).unwrap()
    }

    fn scan_dir(dir: &Path) -> Store {
        let store = new_store(dir);
        let outcome = scan(&store, &Boundary::new(dir), &Progress::default(), &AtomicBool::new(false), vec![0], None);
        assert_eq!(outcome, Outcome::Complete);
        store
    }

    /// Every path below the root with its size, for comparing two scans.
    fn listing(s: &Store) -> Vec<(PathBuf, u64)> {
        let mut out = vec![(s.path(ROOT), s.size(ROOT))];
        s.walk(ROOT, |id, node, _| {
            out.push((s.path(id), node.size));
            true
        });
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
        // Stopped before anything was read, nothing is applied and the root is still to read.
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        let store = new_store(&root);
        let stop = AtomicBool::new(true);
        let outcome = scan(&store, &Boundary::new(&root), &Progress::default(), &stop, vec![0], None);
        assert_eq!(outcome, Outcome::Paused);
        assert_eq!(store.pending(), vec![0]);
        assert_eq!(store.children(ROOT).count(), 0);
    }

    #[test]
    fn an_unreadable_folder_counts_as_read_and_empty() {
        // Otherwise a folder without permission would stay pending, and every resumed scan
        // would try it again and never be complete.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root.join("locked/secret"), 10);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = new_store(&root);
        let progress = Progress::default();
        let outcome = scan(&store, &Boundary::new(&root), &progress, &AtomicBool::new(false), vec![0], None);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(outcome, Outcome::Complete);
        assert!(store.pending().is_empty());
        assert_eq!(store.children(store.find(&root.join("locked")).unwrap()).count(), 0);
        assert_eq!(progress.errors.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stopping_abandons_a_folder_being_read() {
        // Pausing must not wait until a folder with hundreds of thousands of items is read;
        // the folder is reported unread instead of half-read.
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        assert!(read_folder(&root, &AtomicBool::new(true)).is_none());
        assert!(matches!(read_folder(&root, &AtomicBool::new(false)), Some(Ok(ref items)) if !items.is_empty()));
    }

    #[test]
    fn a_scan_paused_many_times_ends_exactly_like_one_uninterrupted_scan() {
        // Quitting or pausing must never lose or double-count anything: every folder is
        // either read and applied, or handed back to be read later.
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        let whole = scan_dir(&root);

        let store = new_store(&root);
        let boundary = Boundary::new(&root);
        let stop = AtomicBool::new(false);
        let mut folders = vec![0];
        let mut rounds = 0;
        loop {
            rounds += 1;
            // A deadline in the past pauses after the first listings are recorded in each round.
            match scan(&store, &boundary, &Progress::default(), &stop, folders, Some(Instant::now())) {
                Outcome::Complete => break,
                Outcome::Paused => folders = store.pending(),
            }
            assert!(rounds < 1000, "no progress");
        }
        // Workers read ahead until the queue closes, but a round still gets at most two
        // levels deeper, and the fixture is five levels deep.
        assert!(rounds >= 3, "the scan should have been paused several times, took {rounds}");
        assert!(store.pending().is_empty());
        assert_eq!(listing(&store), listing(&whole));
        assert_eq!(store.size(ROOT), whole.size(ROOT));
    }

    #[test]
    fn folders_removed_while_paused_are_skipped() {
        let tmp = fixture();
        let root = tmp.path().canonicalize().unwrap();
        let store = new_store(&root);
        let boundary = Boundary::new(&root);
        let stop = AtomicBool::new(false);
        let outcome = scan(&store, &boundary, &Progress::default(), &stop, vec![0], Some(Instant::now()));
        assert_eq!(outcome, Outcome::Paused);
        let gone = store.pending()[0];
        let gone_path = store.dir_path(gone);
        store.remove(store.item_of(gone)).unwrap();
        while scan(&store, &boundary, &Progress::default(), &stop, store.pending(), None) == Outcome::Paused {}
        assert!(!store.is_attached(store.item_of(gone)));
        assert!(store.find(&gone_path).is_none());
        assert_eq!(store.children(store.item_of(gone)).count(), 0, "a removed folder is never read");
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
