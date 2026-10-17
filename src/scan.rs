//! Parallel scan of a directory into a [`Tree`].
//!
//! `dua-core` walks the file system on many threads using `getattrlistbulk`, which returns
//! names and sizes for a whole directory per system call. This module consumes its entries on
//! one thread and adds them to the shared tree in batches, so the UI can show live totals.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use dua_core::{Entry, Options, Order};
use parking_lot::RwLock;

use crate::tree::{Kind, NONE, NodeId, ROOT, Tree, flags};
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
    Cancelled,
}

/// Entries applied per write lock, and the longest time entries wait before being applied.
const BATCH: usize = 8192;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Scans `tree.root_path()` into `tree`, which must contain only its root. `boundary` comes
/// from the root of the whole scan, so a folder rescan follows the same volume rules.
pub fn scan(
    tree: &RwLock<Tree>,
    boundary: Arc<Boundary>,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Outcome {
    let root = tree.read().root_path().to_path_buf();
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let walk_boundary = Arc::clone(&boundary);
    let mut walk = dua_core::walk(&root, threads, Order::ParentFirst, Options::default(), move |e| {
        let dev_ok = matches!(&e.metadata, Some(Ok(m)) if walk_boundary.allows_device(m.dev()));
        dev_ok && (e.depth == 0 || !walk_boundary.excludes(&e.parent_path, &e.file_name))
    });

    let mut builder = Builder {
        boundary: &boundary,
        progress,
        dir_ids: Vec::new(),
        inodes: HashSet::new(),
    };
    let mut pending: Vec<Entry> = Vec::with_capacity(BATCH);
    loop {
        let started = Instant::now();
        let mut finished = false;
        while pending.len() < BATCH && started.elapsed() < FLUSH_INTERVAL {
            match walk.next_cancellable(cancel) {
                Some(Ok(e)) => pending.push(e),
                Some(Err(_)) => {
                    progress.errors.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    finished = true;
                    break;
                }
            }
        }
        if !pending.is_empty() {
            let mut t = tree.write();
            for e in pending.drain(..) {
                builder.apply(&mut t, e);
            }
        }
        if finished {
            break;
        }
    }
    if cancel.load(Ordering::Relaxed) {
        Outcome::Cancelled
    } else {
        Outcome::Complete
    }
}

struct Builder<'a> {
    boundary: &'a Boundary,
    progress: &'a Progress,
    /// `dua_core::DirectoryId` index -> node id.
    dir_ids: Vec<NodeId>,
    /// (device, inode) of files with several hard links that were already counted.
    inodes: HashSet<(u64, u64)>,
}

impl Builder<'_> {
    fn apply(&mut self, tree: &mut Tree, e: Entry) {
        let meta = match e.metadata {
            Some(Ok(m)) => Some(m),
            Some(Err(_)) => {
                self.progress.errors.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => None,
        };
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as u32);

        let Some(parent_dir) = e.parent_directory_id else {
            // The walk root.
            self.map_dir(e.directory_id, ROOT);
            tree.set_mtime(ROOT, mtime);
            return;
        };
        if self.boundary.excludes(&e.parent_path, &e.file_name) {
            return;
        }
        // Order::ParentFirst guarantees the parent was applied before its children.
        let parent = self.dir_ids[parent_dir.index()];
        debug_assert_ne!(parent, NONE);

        let ft = e.file_type;
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink
        } else {
            Kind::Other
        };
        let name = e.file_name.as_bytes();
        let mut node_flags = 0;
        if name.first() == Some(&b'.') {
            node_flags |= flags::HIDDEN;
        }
        if kind == Kind::Dir && is_package(name) {
            node_flags |= flags::PACKAGE;
        }
        let mut size = meta.as_ref().map_or(0, |m| m.allocated_size());
        if let Some(m) = &meta {
            if kind == Kind::File && m.nlink() > 1 && !self.inodes.insert((m.dev(), m.ino())) {
                size = 0;
                node_flags |= flags::HARDLINK;
            }
            if kind == Kind::Dir && !self.boundary.allows_device(m.dev()) {
                // A mount point of another volume; its contents belong to that volume.
                size = 0;
                node_flags |= flags::PRUNED;
            }
        }

        let id = tree.add(parent, name, kind, node_flags, size, mtime);
        self.map_dir(e.directory_id, id);
        let counter = if kind == Kind::Dir {
            &self.progress.dirs
        } else {
            &self.progress.files
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn map_dir(&mut self, dir: Option<dua_core::DirectoryId>, id: NodeId) {
        if let Some(d) = dir {
            let i = d.index();
            if i >= self.dir_ids.len() {
                self.dir_ids.resize(i + 1, NONE);
            }
            self.dir_ids[i] = id;
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
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    fn scan_dir(dir: &Path) -> Tree {
        let tree = RwLock::new(Tree::new(dir.to_path_buf(), "root", 0));
        let boundary = Arc::new(Boundary::new(dir));
        let outcome = scan(&tree, boundary, &Progress::default(), &AtomicBool::new(false));
        assert_eq!(outcome, Outcome::Complete);
        tree.into_inner()
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
    fn a_cancelled_scan_reports_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root.join("a/b"), 10);
        let tree = RwLock::new(Tree::new(root.clone(), "root", 0));
        let boundary = Arc::new(Boundary::new(&root));
        let outcome = scan(&tree, boundary, &Progress::default(), &AtomicBool::new(true));
        assert_eq!(outcome, Outcome::Cancelled);
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
