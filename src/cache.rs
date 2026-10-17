//! Scan results persisted between runs in `~/Library/Caches/<app>/`.
//!
//! File layout (all integers little-endian):
//! `MAGIC`, a fixed-size [`Header`], the root path, then a zstd stream holding the node array,
//! the names buffer and, for a scan that is not finished, the folders still to read (paths
//! relative to the root, separated by NUL) and the hard links counted so far. The header can
//! be read without decompressing the tree, so the sidebar can show sizes and scan dates of
//! every cached root instantly.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytemuck::{Pod, Zeroable};

use crate::scan::CountedLinks;
use crate::tree::{Node, NodeId, ROOT, Tree};

const MAGIC: &[u8; 8] = b"WHATDSK2";

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Header {
    scanned_at: u64,
    total: u64,
    node_count: u64,
    names_len: u64,
    root_path_len: u64,
    /// Bytes of pending folder paths after the names.
    pending_len: u64,
    /// Counted hard links after the pending folders, as (device, inode) pairs.
    links_count: u64,
    /// 1 when the scan finished.
    complete: u64,
}

/// What the sidebar needs to know about a cached scan.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub root: PathBuf,
    pub total: u64,
    pub scanned_at: SystemTime,
    /// False for a scan that was paused or interrupted and can continue.
    pub complete: bool,
}

pub fn dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Caches")
        .join(crate::BUNDLE_ID)
}

/// Cache file name for `root`. Uses FNV-1a because it must stay stable across Rust releases.
fn file_for(dir: &Path, root: &Path) -> PathBuf {
    let hash = root.as_os_str().as_bytes().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    dir.join(format!("{hash:016x}.scan"))
}

/// Writes `tree` atomically: a crash while saving leaves the previous cache intact. For an
/// unfinished scan, `pending` are the folders still to read and `counted` the hard links
/// counted so far.
pub fn save(dir: &Path, tree: &Tree, scanned_at: SystemTime, pending: &[NodeId], counted: &CountedLinks) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Node ids change when the tree is compacted, so pending folders are stored as paths.
    // Folders removed since they were found are left out.
    let pending: Vec<PathBuf> = pending
        .iter()
        .filter(|&&id| tree.is_attached(id))
        .map(|&id| tree.path(id).strip_prefix(tree.root_path()).map(Path::to_path_buf).unwrap_or_default())
        .collect();
    let complete = pending.is_empty();
    let pending_paths = pending.iter().map(|p| p.as_os_str().as_bytes()).collect::<Vec<_>>().join(&0u8);
    let links: Vec<u64> = if complete { Vec::new() } else { counted.iter().flat_map(|&(dev, ino)| [dev, ino]).collect() };
    let tree = tree.compacted();
    let (nodes, names) = tree.parts();
    let root = tree.root_path().as_os_str().as_bytes();
    let header = Header {
        scanned_at: scanned_at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        total: tree.size(ROOT),
        node_count: nodes.len() as u64,
        names_len: names.len() as u64,
        root_path_len: root.len() as u64,
        pending_len: pending_paths.len() as u64,
        links_count: links.len() as u64 / 2,
        complete: u64::from(complete),
    };
    let path = file_for(dir, tree.root_path());
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(bytemuck::bytes_of(&header))?;
        w.write_all(root)?;
        let mut z = zstd::Encoder::new(w, 1)?;
        z.include_checksum(true)?;
        z.write_all(bytemuck::cast_slice(nodes))?;
        z.write_all(names)?;
        z.write_all(&pending_paths)?;
        z.write_all(bytemuck::cast_slice(&links))?;
        z.finish()?.flush()?;
    }
    std::fs::rename(tmp, path)
}

fn read_header(r: &mut impl Read) -> io::Result<(Header, PathBuf)> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a scan cache"));
    }
    let mut header = Header::zeroed();
    r.read_exact(bytemuck::bytes_of_mut(&mut header))?;
    let mut root = vec![0u8; header.root_path_len as usize];
    r.read_exact(&mut root)?;
    Ok((header, PathBuf::from(std::ffi::OsStr::from_bytes(&root))))
}

fn summary(header: &Header, root: PathBuf) -> Summary {
    Summary {
        root,
        total: header.total,
        scanned_at: UNIX_EPOCH + Duration::from_secs(header.scanned_at),
        complete: header.complete != 0,
    }
}

/// Summaries of all cached scans; unreadable files are skipped.
pub fn list(dir: &Path) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "scan"))
        .filter_map(|e| {
            let mut r = BufReader::new(File::open(e.path()).ok()?);
            let (h, root) = read_header(&mut r).ok()?;
            Some(summary(&h, root))
        })
        .collect()
}

/// The cached scan of `root`, and if it is not finished, the folders still to read and the
/// hard links counted so far.
pub fn load(dir: &Path, root: &Path) -> io::Result<(Tree, Summary, Vec<NodeId>, CountedLinks)> {
    let mut r = BufReader::new(File::open(file_for(dir, root))?);
    let (h, cached_root) = read_header(&mut r)?;
    if cached_root != root {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "cache belongs to another root"));
    }
    let mut z = zstd::Decoder::new(r)?;
    let mut nodes = vec![Node::zeroed(); h.node_count as usize];
    z.read_exact(bytemuck::cast_slice_mut(&mut nodes))?;
    let mut names = vec![0u8; h.names_len as usize];
    z.read_exact(&mut names)?;
    let mut pending_paths = vec![0u8; h.pending_len as usize];
    z.read_exact(&mut pending_paths)?;
    let mut links = vec![0u64; h.links_count as usize * 2];
    z.read_exact(bytemuck::cast_slice_mut(&mut links))?;
    let tree = Tree::from_parts(cached_root.clone(), nodes, names);
    let pending = if h.complete != 0 { Vec::new() } else { find_folders(&tree, &pending_paths) };
    let counted = links.chunks_exact(2).map(|pair| (pair[0], pair[1])).collect();
    Ok((tree, summary(&h, cached_root), pending, counted))
}

/// The folders at `rel_paths`: paths relative to the root, separated by NUL; an empty path is
/// the root. Each folder's children are indexed by name once: thousands of pending folders
/// often sit in a folder with even more items, and searching its list for each of them took
/// minutes.
fn find_folders(tree: &Tree, rel_paths: &[u8]) -> Vec<NodeId> {
    let mut by_name: HashMap<NodeId, HashMap<&OsStr, NodeId>> = HashMap::new();
    rel_paths
        .split(|&b| b == 0)
        .filter_map(|rel| {
            let mut cur = ROOT;
            for part in Path::new(OsStr::from_bytes(rel)).components() {
                let children = by_name
                    .entry(cur)
                    .or_insert_with(|| tree.children(cur).map(|c| (tree.name(c), c)).collect());
                cur = *children.get(part.as_os_str())?;
            }
            Some(cur)
        })
        .collect()
}

pub fn remove(dir: &Path, root: &Path) -> io::Result<()> {
    std::fs::remove_file(file_for(dir, root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Kind, flags};

    #[test]
    fn a_saved_scan_loads_back_identically_without_removed_items() {
        // Reopening the app must show exactly the state the user left, including
        // deletions made after the scan.
        let dir = tempfile::tempdir().unwrap();
        let mut t = Tree::new("/data".into(), "Data", 5);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 1);
        t.add(a, "caf\u{e9}.mov".as_bytes(), Kind::File, 0, 4096, 2);
        let gone = t.add(a, b"gone.bin", Kind::File, 0, 8192, 3);
        t.add(ROOT, b"Foo.app", Kind::Dir, flags::PACKAGE, 512, 4);
        t.remove(gone);
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        save(dir.path(), &t, at, &[], &CountedLinks::new()).unwrap();

        let (loaded, s, pending, _) = load(dir.path(), Path::new("/data")).unwrap();
        assert_eq!(s, Summary { root: "/data".into(), total: 4096 + 512, scanned_at: at, complete: true });
        assert!(pending.is_empty());
        assert_eq!(loaded.size(ROOT), t.size(ROOT));
        assert_eq!(loaded.display_name(ROOT), "Data");
        let mov = loaded.find(Path::new("/data/a/caf\u{e9}.mov")).unwrap();
        assert_eq!((loaded.size(mov), loaded.node(mov).mtime), (4096, 2));
        assert!(loaded.find(Path::new("/data/a/gone.bin")).is_none());
        let app = loaded.find(Path::new("/data/Foo.app")).unwrap();
        assert!(loaded.node(app).has(flags::PACKAGE));

        assert_eq!(list(dir.path()), vec![s]);
        remove(dir.path(), Path::new("/data")).unwrap();
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn an_unfinished_scan_keeps_what_it_needs_to_continue() {
        // Quitting during a scan must not lose it: it reloads as unfinished, knowing which
        // folders to read next (even though saving renumbers the nodes) and which hard links
        // it already counted, so continuing does not count them twice.
        let dir = tempfile::tempdir().unwrap();
        let mut t = Tree::new("/data".into(), "Data", 0);
        let gone = t.add(ROOT, b"gone", Kind::File, 0, 1, 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        let b = t.add(a, b"b", Kind::Dir, 0, 0, 0);
        let c = t.add(ROOT, b"c d", Kind::Dir, 0, 0, 0);
        let removed = t.add(ROOT, b"removed", Kind::Dir, 0, 0, 0);
        t.remove(gone);
        t.remove(removed);
        let counted: CountedLinks = [(1, 2), (1, 3)].into();
        save(dir.path(), &t, UNIX_EPOCH, &[b, c, removed], &counted).unwrap();

        let (loaded, s, pending, links) = load(dir.path(), Path::new("/data")).unwrap();
        assert!(!s.complete);
        let mut paths: Vec<PathBuf> = pending.iter().map(|&id| loaded.path(id)).collect();
        paths.sort();
        assert_eq!(paths, vec![PathBuf::from("/data/a/b"), PathBuf::from("/data/c d")]);
        assert_eq!(links, counted);
        assert!(!list(dir.path())[0].complete);
    }

    #[test]
    fn many_unfinished_folders_in_one_large_folder_load_quickly() {
        // A paused scan of a whole disk had 63,000 pending folders, many of them in folders
        // with tens of thousands of items. Looking each up from the root made loading it take
        // almost two minutes; the bound here is far above the expected milliseconds and far
        // below what that took.
        let dir = tempfile::tempdir().unwrap();
        let mut t = Tree::new("/data".into(), "Data", 0);
        let big = t.add(ROOT, b"big", Kind::Dir, 0, 0, 0);
        let folders: Vec<NodeId> =
            (0..40_000).map(|i| t.add(big, format!("f{i}").as_bytes(), Kind::Dir, 0, 0, 0)).collect();
        save(dir.path(), &t, UNIX_EPOCH, &folders, &CountedLinks::new()).unwrap();

        let started = std::time::Instant::now();
        let (loaded, _, pending, _) = load(dir.path(), Path::new("/data")).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        assert_eq!(pending.len(), folders.len());
        assert_eq!(loaded.path(pending[123]), PathBuf::from(format!("/data/big/f{}", 123)));
    }

    #[test]
    fn a_scan_stopped_before_reading_anything_continues_from_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let t = Tree::new("/data".into(), "Data", 0);
        save(dir.path(), &t, UNIX_EPOCH, &[ROOT], &CountedLinks::new()).unwrap();
        let (_, s, pending, _) = load(dir.path(), Path::new("/data")).unwrap();
        assert!(!s.complete);
        assert_eq!(pending, vec![ROOT]);
    }

    #[test]
    fn a_scan_whose_remaining_folders_were_all_removed_is_complete() {
        // Nothing is left to read, so offering to continue would do nothing.
        let dir = tempfile::tempdir().unwrap();
        let mut t = Tree::new("/data".into(), "Data", 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        t.remove(a);
        save(dir.path(), &t, UNIX_EPOCH, &[a], &[(1, 2)].into()).unwrap();
        let (_, s, pending, links) = load(dir.path(), Path::new("/data")).unwrap();
        assert!(s.complete && pending.is_empty() && links.is_empty());
    }

    #[test]
    fn a_corrupt_cache_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(file_for(dir.path(), Path::new("/x")), b"garbage").unwrap();
        assert!(load(dir.path(), Path::new("/x")).is_err());
        assert!(list(dir.path()).is_empty());
    }
}
