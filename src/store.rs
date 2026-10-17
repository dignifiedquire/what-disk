//! A scan kept on disk, so memory holds only folders.
//!
//! Each scanned location has one file: a fixed-size header with the summary the sidebar
//! shows, then an append-only log of records in zstd-compressed blocks. The main record is a
//! folder's listing: the name, kind, size and modification time of each entry, written the
//! moment the folder is read. Memory keeps one small entry per folder (its parent, subtree
//! total and where its listing is); files exist only in the log and are read back through
//! small caches when a view needs them.
//!
//! Items are numbered. A listing covers a consecutive range of numbers, so an item's folder
//! is found by binary search, and folders are numbered in the order they are found. The root
//! is item 0 and folder 0.
//!
//! A folder that was found but not listed yet is pending: a stopped scan continues with
//! exactly those, and nothing else has to be saved. A block torn by a crash while it was
//! written is dropped when the file is opened. Rescanning a folder writes a shadow copy and
//! grafts it in when complete, so the old contents stay visible until then.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::cmp::Reverse;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};

pub type ItemId = u32;
pub type DirId = u32;
pub const ROOT: ItemId = 0;
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Kind {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Kind::File,
            1 => Kind::Dir,
            2 => Kind::Symlink,
            _ => Kind::Other,
        }
    }
}

pub mod flags {
    /// Name starts with a dot.
    pub const HIDDEN: u8 = 1;
    /// Folder that Finder presents as a single item (`.app`, `.photoslibrary`, ...).
    pub const PACKAGE: u8 = 2;
    /// Folder that is not read (another volume).
    pub const PRUNED: u8 = 4;
    /// Another hard link to a file that is already counted; its size counts as 0.
    pub const HARDLINK: u8 = 8;
    /// The entry carries its device and inode, because the file has several links.
    pub(super) const LINKED: u8 = 16;
    /// A folder being rescanned, not yet part of the tree.
    pub(super) const SHADOW: u8 = 64;
}

/// What a view needs to know about one item.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Node {
    /// Allocated bytes on disk; for folders the total of everything inside.
    pub size: u64,
    /// Modification time in seconds since the Unix epoch, 0 if unknown.
    pub mtime: u32,
    pub kind: Kind,
    pub flags: u8,
}

impl Node {
    const MISSING: Node = Node { size: 0, mtime: 0, kind: Kind::Other, flags: 0 };

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// One entry of a folder as the scanner found it.
#[derive(Clone, Debug)]
pub struct NewEntry {
    pub name: OsString,
    pub kind: Kind,
    /// [`flags::HIDDEN`], [`flags::PACKAGE`], [`flags::PRUNED`].
    pub flags: u8,
    pub size: u64,
    pub mtime: u32,
    /// Device and inode of a file with more than one hard link.
    pub link: Option<(u64, u64)>,
    /// A folder whose contents are read later.
    pub descend: bool,
}

/// What the sidebar shows about a saved scan, read from the header alone.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub root: PathBuf,
    pub total: u64,
    pub scanned_at: SystemTime,
    /// False for a scan that was paused or interrupted and can continue.
    pub complete: bool,
}

const MAGIC: &[u8; 8] = b"WDSTORE1";
const HEADER_LEN: u64 = 4096;
/// Raw bytes collected before a block is compressed and written.
const BLOCK: usize = 64 * 1024;
/// A listing's location: the block's file offset in the high bits, the record's offset in
/// the uncompressed block in the low bits.
const IN_BLOCK_BITS: u32 = 24;
const UNLISTED: u64 = u64::MAX;
/// Bytes of uncompressed blocks kept in memory.
const BLOCK_CACHE: usize = 16 << 20;
/// Entries of decoded listings kept in memory.
const LISTING_CACHE: usize = 200_000;

const TAG_LISTING: u8 = 1;
const TAG_REMOVE: u8 = 2;
const TAG_SHADOW: u8 = 3;
const TAG_GRAFT: u8 = 4;

#[derive(Clone, Copy)]
struct Dir {
    /// Subtree total, including the folder's own size.
    size: u64,
    /// Where its current listing is; `UNLISTED` while it is pending.
    loc: u64,
    /// `NONE` for the root and for folders replaced by a graft.
    parent: DirId,
    /// Its number. Increases with the folder number, so folders are found by item.
    item: ItemId,
    first_item: ItemId,
    count: u32,
    /// The folder's own allocated size.
    own: u32,
    name_off: u32,
    name_len: u16,
    flags: u8,
}

struct State {
    dirs: Vec<Dir>,
    names: Vec<u8>,
    /// (first item, folder) of every listing in the order written, so sorted by item.
    listings: Vec<(ItemId, DirId)>,
    next_item: ItemId,
    /// Items the user deleted.
    removed: HashSet<ItemId>,
    /// Files with several hard links that are already counted: (device, inode).
    counted: HashSet<(u64, u64)>,
    /// Rescans in progress: shadow folder -> the folder it replaces.
    shadows: HashMap<DirId, DirId>,
}

impl State {
    fn new(root_name: &str) -> State {
        let mut st = State {
            dirs: Vec::new(),
            names: Vec::new(),
            listings: Vec::new(),
            next_item: 1,
            removed: HashSet::new(),
            counted: HashSet::new(),
            shadows: HashMap::new(),
        };
        push_dir(&mut st, NONE, ROOT, root_name.as_bytes(), 0, 0);
        st
    }
}

struct Writer {
    file: File,
    /// File offset where the next block goes.
    end: u64,
    buf: Vec<u8>,
    header: Header,
}

#[derive(Clone)]
struct Header {
    root: PathBuf,
    root_name: String,
    root_mtime: u32,
    total: u64,
    scanned_at: u64,
    complete: bool,
}

/// An entry of a listing as read back from disk.
#[derive(Clone, Copy, Debug)]
struct Entry {
    size: u64,
    mtime: u32,
    child: DirId,
    name_off: u32,
    name_len: u16,
    kind: u8,
    flags: u8,
}

/// A folder's entries, decoded.
pub struct Listing {
    first_item: ItemId,
    entries: Vec<Entry>,
    names: Vec<u8>,
}

impl Listing {
    fn name(&self, i: usize) -> &OsStr {
        let e = &self.entries[i];
        OsStr::from_bytes(&self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize])
    }
}

/// A cache that evicts the oldest entries once their total weight exceeds its budget. First
/// in, first out: walking a whole disk passes through it, so eviction must stay O(1).
struct Cache<V> {
    map: HashMap<u64, (V, usize)>,
    order: VecDeque<u64>,
    weight: usize,
    budget: usize,
}

impl<V: Clone> Cache<V> {
    fn new(budget: usize) -> Self {
        Cache { map: HashMap::new(), order: VecDeque::new(), weight: 0, budget }
    }

    fn get(&self, key: u64) -> Option<V> {
        self.map.get(&key).map(|(v, _)| v.clone())
    }

    fn insert(&mut self, key: u64, value: V, weight: usize) {
        if self.map.insert(key, (value, weight)).is_none() {
            self.order.push_back(key);
            self.weight += weight;
        }
        while self.weight > self.budget && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, w)) = self.map.remove(&oldest) {
                self.weight -= w;
            }
        }
    }
}

/// One scanned location on disk.
pub struct Store {
    path: PathBuf,
    root: PathBuf,
    root_name: String,
    reader: File,
    state: RwLock<State>,
    writer: Mutex<Writer>,
    blocks: Mutex<Cache<Arc<Vec<u8>>>>,
    listings: Mutex<Cache<Arc<Listing>>>,
}

impl Store {
    /// Starts a new scan of `root` in the file at `path`, replacing anything there.
    pub fn create(path: &Path, root: &Path, root_name: &str, root_mtime: u32) -> io::Result<Store> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        let header = Header {
            root: root.to_path_buf(),
            root_name: root_name.to_string(),
            root_mtime,
            total: 0,
            scanned_at: now(),
            complete: false,
        };
        write_header(&file, &header)?;
        let state = State::new(root_name);
        Store::assemble(path, file, header, state, HEADER_LEN)
    }

    /// Opens a saved scan, replaying its log. A torn block at the end is cut off.
    pub fn open(path: &Path) -> io::Result<Store> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let header = read_header(&file)?;
        let mut state = State::new(&header.root_name);
        let len = file.metadata()?.len();
        let mut off = HEADER_LEN;
        while off < len {
            let Some((raw, next)) = read_block(&file, off, len) else {
                break;
            };
            let mut at = 0;
            while at < raw.len() {
                let Some((tag, payload, next_at)) = split_record(&raw, at) else {
                    break;
                };
                let loc = (off << IN_BLOCK_BITS) | at as u64;
                replay(&mut state, tag, payload, loc);
                at = next_at;
            }
            off = next;
        }
        // Whatever follows the last complete block was torn by a crash.
        if off < len {
            file.set_len(off)?;
        }
        Store::assemble(path, file, header, state, off)
    }

    fn assemble(path: &Path, file: File, header: Header, state: State, end: u64) -> io::Result<Store> {
        Ok(Store {
            path: path.to_path_buf(),
            root: header.root.clone(),
            root_name: header.root_name.clone(),
            reader: file.try_clone()?,
            state: RwLock::new(state),
            writer: Mutex::new(Writer { file, end, buf: Vec::new(), header }),
            blocks: Mutex::new(Cache::new(BLOCK_CACHE)),
            listings: Mutex::new(Cache::new(LISTING_CACHE)),
        })
    }

    pub fn file(&self) -> &Path {
        &self.path
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    // ----- Writing -----

    /// Records the entries of folder `dir` and returns the folders among them to read next,
    /// in entry order. Totals of the folder and its ancestors grow at once. A file with
    /// several hard links counts only the first time one of its links is seen.
    pub fn add_listing(&self, dir: DirId, entries: Vec<NewEntry>) -> io::Result<Vec<DirId>> {
        let mut w = self.writer.lock();
        let mut st = self.state.write();
        if st.dirs.get(dir as usize).is_none_or(|d| d.loc != UNLISTED) {
            return Ok(Vec::new());
        }
        let first_item = st.next_item;
        let mut next_dir = st.dirs.len() as DirId;
        let mut found = Vec::new();
        let mut payload = Vec::with_capacity(12 + entries.len() * 40);
        payload.extend_from_slice(&dir.to_le_bytes());
        payload.extend_from_slice(&first_item.to_le_bytes());
        payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in &entries {
            let mut flags = e.flags & (flags::HIDDEN | flags::PACKAGE | flags::PRUNED);
            let mut size = e.size;
            if let Some(link) = e.link {
                flags |= flags::LINKED;
                if !st.counted.insert(link) {
                    flags |= flags::HARDLINK;
                    size = 0;
                }
            }
            let child = if e.descend && e.kind == Kind::Dir {
                found.push(next_dir);
                next_dir += 1;
                next_dir - 1
            } else {
                NONE
            };
            encode_entry(&mut payload, e.kind, flags, e.name.as_bytes(), e.mtime, size, child, e.link);
        }
        let loc = w.append(TAG_LISTING, &payload)?;
        replay(&mut st, TAG_LISTING, &payload, loc);
        Ok(found)
    }

    /// Takes `item` out, as when it was deleted on disk. Its folder and their ancestors
    /// shrink at once.
    pub fn remove(&self, item: ItemId) -> io::Result<()> {
        if item == ROOT || !self.is_attached(item) {
            return Ok(());
        }
        let size = self.size(item);
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&item.to_le_bytes());
        payload.extend_from_slice(&size.to_le_bytes());
        let mut w = self.writer.lock();
        let mut st = self.state.write();
        w.append(TAG_REMOVE, &payload)?;
        replay(&mut st, TAG_REMOVE, &payload, 0);
        Ok(())
    }

    /// Starts a rescan of folder `item`: returns a shadow folder to read into, which
    /// replaces the folder's contents once [`Store::graft`] is called.
    pub fn shadow(&self, item: ItemId) -> io::Result<Option<DirId>> {
        let mut w = self.writer.lock();
        let mut st = self.state.write();
        let Some(of) = dir_of(&st, item) else {
            return Ok(None);
        };
        let shadow = st.dirs.len() as DirId;
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&shadow.to_le_bytes());
        payload.extend_from_slice(&of.to_le_bytes());
        payload.extend_from_slice(&st.next_item.to_le_bytes());
        w.append(TAG_SHADOW, &payload)?;
        replay(&mut st, TAG_SHADOW, &payload, 0);
        Ok(Some(shadow))
    }

    /// Replaces the contents of the folder that `shadow` was made for with the shadow's.
    pub fn graft(&self, shadow: DirId) -> io::Result<()> {
        let mut w = self.writer.lock();
        let mut st = self.state.write();
        let Some(&target) = st.shadows.get(&shadow) else {
            return Ok(());
        };
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&target.to_le_bytes());
        payload.extend_from_slice(&shadow.to_le_bytes());
        w.append(TAG_GRAFT, &payload)?;
        replay(&mut st, TAG_GRAFT, &payload, 0);
        Ok(())
    }

    pub fn set_root_mtime(&self, mtime: u32) {
        self.writer.lock().header.root_mtime = mtime;
    }

    /// Writes everything recorded so far and the summary, so a crash loses nothing older.
    /// `complete` marks a scan with nothing left to read; `scanned_at` is kept otherwise.
    pub fn checkpoint(&self, complete: bool, scanned_at: Option<SystemTime>) -> io::Result<()> {
        let mut w = self.writer.lock();
        w.flush_block()?;
        w.header.total = self.state.read().dirs[0].size;
        w.header.complete = complete;
        if let Some(at) = scanned_at {
            w.header.scanned_at = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        }
        write_header(&w.file, &w.header)?;
        w.file.sync_data()
    }

    // ----- Reading -----

    pub fn summary(&self) -> Summary {
        let total = self.size(ROOT);
        let w = self.writer.lock();
        Summary {
            root: self.root.clone(),
            total,
            scanned_at: UNIX_EPOCH + Duration::from_secs(w.header.scanned_at),
            complete: w.header.complete,
        }
    }

    /// Folders found but not read yet, in the tree (not inside an unfinished rescan).
    pub fn pending(&self) -> Vec<DirId> {
        let st = self.state.read();
        (0..st.dirs.len() as DirId)
            .filter(|&d| st.dirs[d as usize].loc == UNLISTED && dir_attached(&st, d))
            .collect()
    }

    /// Folders found but not read yet inside the shadow `shadow`.
    pub fn pending_in(&self, shadow: DirId) -> Vec<DirId> {
        let st = self.state.read();
        (0..st.dirs.len() as DirId)
            .filter(|&d| st.dirs[d as usize].loc == UNLISTED && within(&st, d, shadow))
            .collect()
    }

    pub fn items(&self) -> u64 {
        u64::from(self.state.read().next_item)
    }

    /// The folder number of folder item `item`.
    pub fn dir_of(&self, item: ItemId) -> Option<DirId> {
        dir_of(&self.state.read(), item)
    }

    /// The item of folder `dir`.
    pub fn item_of(&self, dir: DirId) -> ItemId {
        self.state.read().dirs.get(dir as usize).map_or(NONE, |d| d.item)
    }

    pub fn dir_path(&self, dir: DirId) -> PathBuf {
        let st = self.state.read();
        let mut names = Vec::new();
        let mut d = dir;
        // A shadow has the parent and name of the folder it replaces, so this works for it too.
        while d != 0 && d != NONE {
            names.push(dir_name(&st, d).to_os_string());
            d = st.dirs[d as usize].parent;
        }
        let mut path = self.root.clone();
        for n in names.iter().rev() {
            path.push(n);
        }
        path
    }

    pub fn node(&self, item: ItemId) -> Node {
        if item == ROOT {
            let size = self.state.read().dirs[0].size;
            return Node { size, mtime: self.writer.lock().header.root_mtime, kind: Kind::Dir, flags: 0 };
        }
        let Some((dir, i)) = self.locate(item) else {
            return Node::MISSING;
        };
        let Some(listing) = self.listing(dir) else {
            return Node::MISSING;
        };
        let e = listing.entries[i];
        let size = if e.child == NONE { e.size } else { self.state.read().dirs.get(e.child as usize).map_or(0, |d| d.size) };
        Node { size, mtime: e.mtime, kind: Kind::from_u8(e.kind), flags: e.flags & !flags::LINKED }
    }

    pub fn size(&self, item: ItemId) -> u64 {
        if let Some(d) = self.dir_of(item) {
            return self.state.read().dirs[d as usize].size;
        }
        self.node(item).size
    }

    pub fn name(&self, item: ItemId) -> OsString {
        if item == ROOT {
            return OsString::from(&self.root_name);
        }
        match self.locate(item).and_then(|(dir, i)| Some((self.listing(dir)?, i))) {
            Some((listing, i)) => listing.name(i).to_os_string(),
            None => OsString::new(),
        }
    }

    pub fn display_name(&self, item: ItemId) -> String {
        self.name(item).to_string_lossy().into_owned()
    }

    /// The folder holding `item`, and the item's position in its listing.
    fn locate(&self, item: ItemId) -> Option<(DirId, usize)> {
        locate(&self.state.read(), item)
    }

    fn listing(&self, dir: DirId) -> Option<Arc<Listing>> {
        let loc = self.state.read().dirs.get(dir as usize)?.loc;
        if loc == UNLISTED {
            return None;
        }
        if let Some(l) = self.listings.lock().get(loc) {
            return Some(l);
        }
        let block_off = loc >> IN_BLOCK_BITS;
        let at = (loc & ((1 << IN_BLOCK_BITS) - 1)) as usize;
        let raw = {
            let w = self.writer.lock();
            if block_off == w.end {
                // Still in the block being collected.
                Arc::new(w.buf.clone())
            } else {
                drop(w);
                self.block(block_off)?
            }
        };
        let (tag, payload, _) = split_record(&raw, at)?;
        if tag != TAG_LISTING {
            return None;
        }
        let listing = Arc::new(decode_listing(payload)?);
        self.listings.lock().insert(loc, listing.clone(), listing.entries.len().max(1));
        Some(listing)
    }

    fn block(&self, off: u64) -> Option<Arc<Vec<u8>>> {
        if let Some(b) = self.blocks.lock().get(off) {
            return Some(b);
        }
        let len = self.reader.metadata().ok()?.len();
        let (raw, _) = read_block(&self.reader, off, len)?;
        let raw = Arc::new(raw);
        self.blocks.lock().insert(off, raw.clone(), raw.len());
        Some(raw)
    }

    /// The items in folder `item`, in the order they were found.
    pub fn children(&self, item: ItemId) -> Vec<ItemId> {
        let st = self.state.read();
        let Some(dir) = dir_of(&st, item) else {
            return Vec::new();
        };
        let d = &st.dirs[dir as usize];
        if d.loc == UNLISTED {
            return Vec::new();
        }
        (d.first_item..d.first_item + d.count).filter(|i| !st.removed.contains(i)).collect()
    }

    /// Children with their sizes and names, read once.
    fn sized_children(&self, item: ItemId) -> Vec<(ItemId, u64, OsString)> {
        let Some(dir) = self.dir_of(item) else {
            return Vec::new();
        };
        let Some(listing) = self.listing(dir) else {
            return Vec::new();
        };
        let st = self.state.read();
        listing
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (listing.first_item + i as u32, i, e))
            .filter(|(id, _, _)| !st.removed.contains(id))
            .map(|(id, i, e)| {
                let size = if e.child == NONE { e.size } else { st.dirs[e.child as usize].size };
                (id, size, listing.name(i).to_os_string())
            })
            .collect()
    }

    /// Children ordered largest first, ties broken by name.
    pub fn sorted_children(&self, item: ItemId) -> Vec<ItemId> {
        let mut v = self.sized_children(item);
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        v.into_iter().map(|(id, _, _)| id).collect()
    }

    /// The `limit` largest children, largest first, plus how many others there are and their
    /// total size. Selects before sorting, so a folder with a million entries costs one pass.
    pub fn largest_children(&self, item: ItemId, limit: usize) -> (Vec<ItemId>, usize, u64) {
        let mut kids = self.sized_children(item);
        let order = |a: &(ItemId, u64, OsString), b: &(ItemId, u64, OsString)| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2));
        let mut rest = Vec::new();
        if kids.len() > limit {
            kids.select_nth_unstable_by(limit, order);
            rest = kids.split_off(limit);
        }
        kids.sort_by(order);
        (kids.into_iter().map(|k| k.0).collect(), rest.len(), rest.iter().map(|k| k.1).sum())
    }

    /// Items from the root down to and including `item`.
    pub fn ancestry(&self, item: ItemId) -> Vec<ItemId> {
        let st = self.state.read();
        let mut v = vec![item];
        let mut d = if item == ROOT { None } else { locate(&st, item).map(|(d, _)| d) };
        while let Some(dir) = d {
            let x = &st.dirs[dir as usize];
            v.push(x.item);
            d = (x.parent != NONE).then_some(x.parent);
        }
        v.reverse();
        v
    }

    pub fn path(&self, item: ItemId) -> PathBuf {
        if item == ROOT {
            return self.root.clone();
        }
        if let Some(d) = self.dir_of(item) {
            return self.dir_path(d);
        }
        match self.locate(item) {
            Some((dir, _)) => self.dir_path(dir).join(self.name(item)),
            None => self.root.clone(),
        }
    }

    /// The item at absolute path `path`, if it is in this scan.
    pub fn find(&self, path: &Path) -> Option<ItemId> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut cur = ROOT;
        for part in rel.components() {
            let dir = self.dir_of(cur)?;
            let listing = self.listing(dir)?;
            let i = (0..listing.entries.len()).find(|&i| listing.name(i) == part.as_os_str())?;
            cur = listing.first_item + i as u32;
            if self.state.read().removed.contains(&cur) {
                return None;
            }
        }
        Some(cur)
    }

    /// False once `item` or a folder holding it was removed or replaced by a rescan.
    pub fn is_attached(&self, item: ItemId) -> bool {
        let st = self.state.read();
        if item == ROOT {
            return true;
        }
        if st.removed.contains(&item) {
            return false;
        }
        locate(&st, item).is_some_and(|(d, _)| dir_attached(&st, d))
    }

    /// Calls `visit` for every item below `from` with its node and name; descends into a
    /// folder when `visit` returns true. Reads the listings from disk as needed.
    pub fn walk(&self, from: ItemId, mut visit: impl FnMut(ItemId, &Node, &OsStr) -> bool) {
        let mut stack: Vec<DirId> = self.dir_of(from).into_iter().collect();
        while let Some(dir) = stack.pop() {
            let Some(listing) = self.listing(dir) else {
                continue;
            };
            let children: Vec<(ItemId, Node, DirId)> = {
                let st = self.state.read();
                listing
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (listing.first_item + i as u32, e))
                    .filter(|(id, _)| !st.removed.contains(id))
                    .map(|(id, e)| {
                        let size = if e.child == NONE { e.size } else { st.dirs[e.child as usize].size };
                        let node = Node { size, mtime: e.mtime, kind: Kind::from_u8(e.kind), flags: e.flags & !flags::LINKED };
                        (id, node, e.child)
                    })
                    .collect()
            };
            for (i, (id, node, child)) in children.into_iter().enumerate() {
                if visit(id, &node, listing.name(i)) && child != NONE {
                    stack.push(child);
                }
            }
        }
    }

    /// The `n` largest items below `from`. Packages count as one item, like in Finder.
    pub fn biggest(&self, from: ItemId, n: usize) -> Vec<ItemId> {
        let mut heap: BinaryHeap<Reverse<(u64, ItemId)>> = BinaryHeap::with_capacity(n + 1);
        self.walk(from, |id, node, _| {
            if node.is_dir() && !node.has(flags::PACKAGE) {
                return true;
            }
            heap.push(Reverse((node.size, id)));
            if heap.len() > n {
                heap.pop();
            }
            false
        });
        let mut v: Vec<(u64, ItemId)> = heap.into_iter().map(|Reverse(e)| e).collect();
        v.sort_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, id)| id).collect()
    }

    /// Items below `from` whose name contains `query` (case-insensitive), largest first.
    pub fn search(&self, from: ItemId, query: &str, limit: usize) -> Vec<ItemId> {
        let query = query.to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let ascii = query.is_ascii();
        let q = query.as_bytes();
        let matches = |name: &[u8]| {
            if ascii {
                name.len() >= q.len() && name.windows(q.len()).any(|w| w.eq_ignore_ascii_case(q))
            } else {
                String::from_utf8_lossy(name).to_lowercase().contains(&query)
            }
        };
        let mut heap: BinaryHeap<Reverse<(u64, ItemId)>> = BinaryHeap::new();
        self.walk(from, |id, node, name| {
            if matches(name.as_bytes()) {
                heap.push(Reverse((node.size, id)));
                if heap.len() > limit {
                    heap.pop();
                }
            }
            true
        });
        let mut v: Vec<(u64, ItemId)> = heap.into_iter().map(|Reverse(e)| e).collect();
        v.sort_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, id)| id).collect()
    }
}

impl Writer {
    /// Adds a record to the block being collected and returns its location.
    fn append(&mut self, tag: u8, payload: &[u8]) -> io::Result<u64> {
        let at = self.buf.len() as u64;
        let loc = (self.end << IN_BLOCK_BITS) | at;
        self.buf.push(tag);
        self.buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(payload);
        if self.buf.len() >= BLOCK {
            self.flush_block()?;
        }
        Ok(loc)
    }

    fn flush_block(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let mut z = zstd::Encoder::new(Vec::new(), 1)?;
        z.include_checksum(true)?;
        z.write_all(&self.buf)?;
        let compressed = z.finish()?;
        let mut block = Vec::with_capacity(8 + compressed.len());
        block.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        block.extend_from_slice(&(self.buf.len() as u32).to_le_bytes());
        block.extend_from_slice(&compressed);
        self.file.write_all_at(&block, self.end)?;
        self.end += block.len() as u64;
        self.buf.clear();
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn write_header(file: &File, h: &Header) -> io::Result<()> {
    let root = h.root.as_os_str().as_bytes();
    let name = h.root_name.as_bytes();
    let mut buf = Vec::with_capacity(HEADER_LEN as usize);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&h.total.to_le_bytes());
    buf.extend_from_slice(&h.scanned_at.to_le_bytes());
    buf.push(u8::from(h.complete));
    buf.extend_from_slice(&[0; 3]);
    buf.extend_from_slice(&h.root_mtime.to_le_bytes());
    buf.extend_from_slice(&(root.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
    buf.extend_from_slice(root);
    buf.extend_from_slice(name);
    if buf.len() > HEADER_LEN as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long for a scan file"));
    }
    buf.resize(HEADER_LEN as usize, 0);
    file.write_all_at(&buf, 0)
}

fn read_header(file: &File) -> io::Result<Header> {
    let mut buf = vec![0u8; HEADER_LEN as usize];
    file.read_exact_at(&mut buf, 0)?;
    if &buf[..8] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a scan file"));
    }
    let u64_at = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
    let u16_at = |i: usize| u16::from_le_bytes(buf[i..i + 2].try_into().unwrap()) as usize;
    let (root_len, name_len) = (u16_at(32), u16_at(34));
    if 36 + root_len + name_len > buf.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged scan file header"));
    }
    Ok(Header {
        total: u64_at(8),
        scanned_at: u64_at(16),
        complete: buf[24] != 0,
        root_mtime: u32::from_le_bytes(buf[28..32].try_into().unwrap()),
        root: PathBuf::from(OsStr::from_bytes(&buf[36..36 + root_len])),
        root_name: String::from_utf8_lossy(&buf[36 + root_len..36 + root_len + name_len]).into_owned(),
    })
}

/// The block at `off` uncompressed, and where the next block starts; `None` if it is torn.
fn read_block(file: &File, off: u64, len: u64) -> Option<(Vec<u8>, u64)> {
    let mut head = [0u8; 8];
    file.read_exact_at(&mut head, off).ok()?;
    let compressed_len = u64::from(u32::from_le_bytes(head[..4].try_into().unwrap()));
    let raw_len = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
    let next = off + 8 + compressed_len;
    if next > len {
        return None;
    }
    let mut compressed = vec![0u8; compressed_len as usize];
    file.read_exact_at(&mut compressed, off + 8).ok()?;
    let raw = zstd::bulk::decompress(&compressed, raw_len).ok()?;
    (raw.len() == raw_len).then_some((raw, next))
}

/// The record at `at`: its tag, payload and where the next record starts.
fn split_record(raw: &[u8], at: usize) -> Option<(u8, &[u8], usize)> {
    let tag = *raw.get(at)?;
    let len = u32::from_le_bytes(raw.get(at + 1..at + 5)?.try_into().ok()?) as usize;
    let payload = raw.get(at + 5..at + 5 + len)?;
    Some((tag, payload, at + 5 + len))
}

#[allow(clippy::too_many_arguments)]
fn encode_entry(out: &mut Vec<u8>, kind: Kind, flags: u8, name: &[u8], mtime: u32, size: u64, child: DirId, link: Option<(u64, u64)>) {
    out.push(kind as u8);
    out.push(flags);
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&mtime.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&child.to_le_bytes());
    if let Some((dev, ino)) = link {
        out.extend_from_slice(&dev.to_le_bytes());
        out.extend_from_slice(&ino.to_le_bytes());
    }
    out.extend_from_slice(name);
}

struct Decoded<'a> {
    dir: DirId,
    first_item: ItemId,
    entries: Vec<DecodedEntry<'a>>,
}

struct DecodedEntry<'a> {
    entry: Entry,
    link: Option<(u64, u64)>,
    name: &'a [u8],
}

fn decode_entries(payload: &[u8]) -> Option<Decoded<'_>> {
    let u32_at = |i: usize| -> Option<u32> { Some(u32::from_le_bytes(payload.get(i..i + 4)?.try_into().ok()?)) };
    let (dir, first_item, count) = (u32_at(0)?, u32_at(4)?, u32_at(8)? as usize);
    let mut entries = Vec::with_capacity(count);
    let mut at = 12;
    for _ in 0..count {
        let head = payload.get(at..at + 20)?;
        let kind = head[0];
        let flags = head[1];
        let name_len = u16::from_le_bytes(head[2..4].try_into().ok()?);
        let mtime = u32::from_le_bytes(head[4..8].try_into().ok()?);
        let size = u64::from_le_bytes(head[8..16].try_into().ok()?);
        let child = u32::from_le_bytes(head[16..20].try_into().ok()?);
        at += 20;
        let link = if flags & flags::LINKED != 0 {
            let l = payload.get(at..at + 16)?;
            at += 16;
            Some((u64::from_le_bytes(l[..8].try_into().ok()?), u64::from_le_bytes(l[8..].try_into().ok()?)))
        } else {
            None
        };
        let name = payload.get(at..at + name_len as usize)?;
        at += name_len as usize;
        let entry = Entry { size, mtime, child, name_off: 0, name_len, kind, flags };
        entries.push(DecodedEntry { entry, link, name });
    }
    Some(Decoded { dir, first_item, entries })
}

fn decode_listing(payload: &[u8]) -> Option<Listing> {
    let d = decode_entries(payload)?;
    let mut names = Vec::new();
    let entries = d
        .entries
        .into_iter()
        .map(|d| {
            let mut e = d.entry;
            e.name_off = names.len() as u32;
            names.extend_from_slice(d.name);
            e
        })
        .collect();
    Some(Listing { first_item: d.first_item, entries, names })
}

fn push_dir(st: &mut State, parent: DirId, item: ItemId, name: &[u8], own: u32, flags: u8) -> DirId {
    let id = st.dirs.len() as DirId;
    let name_off = st.names.len() as u32;
    st.names.extend_from_slice(name);
    st.dirs.push(Dir {
        size: u64::from(own),
        loc: UNLISTED,
        parent,
        item,
        first_item: 0,
        count: 0,
        own,
        name_off,
        name_len: name.len() as u16,
        flags,
    });
    id
}

fn dir_name(st: &State, dir: DirId) -> &OsStr {
    let d = &st.dirs[dir as usize];
    OsStr::from_bytes(&st.names[d.name_off as usize..d.name_off as usize + d.name_len as usize])
}

/// Adds `delta` to `dir` and every folder holding it. A rescan's shadow is not in the tree
/// yet, so its totals stop there.
fn add_up(st: &mut State, mut dir: DirId, delta: i64) {
    while dir != NONE {
        let d = &mut st.dirs[dir as usize];
        d.size = d.size.saturating_add_signed(delta);
        if d.flags & flags::SHADOW != 0 {
            break;
        }
        dir = d.parent;
    }
}

fn locate(st: &State, item: ItemId) -> Option<(DirId, usize)> {
    let i = st.listings.partition_point(|&(first, _)| first <= item).checked_sub(1)?;
    let (first, dir) = st.listings[i];
    let d = st.dirs.get(dir as usize)?;
    (d.first_item == first && item < first + d.count && d.loc != UNLISTED).then_some((dir, (item - first) as usize))
}

fn dir_of(st: &State, item: ItemId) -> Option<DirId> {
    let i = st.dirs.partition_point(|d| d.item < item);
    (st.dirs.get(i)?.item == item).then_some(i as DirId)
}

/// True if `dir` is reachable from the root through current listings.
fn dir_attached(st: &State, mut dir: DirId) -> bool {
    loop {
        if dir == 0 {
            return true;
        }
        let d = &st.dirs[dir as usize];
        if d.parent == NONE || d.flags & flags::SHADOW != 0 || st.removed.contains(&d.item) {
            return false;
        }
        let p = &st.dirs[d.parent as usize];
        if p.loc == UNLISTED || d.item < p.first_item || d.item >= p.first_item + p.count {
            return false;
        }
        dir = d.parent;
    }
}

/// True if `dir` is `shadow` or inside it.
fn within(st: &State, mut dir: DirId, shadow: DirId) -> bool {
    loop {
        if dir == shadow {
            return true;
        }
        let d = &st.dirs[dir as usize];
        if d.parent == NONE || d.flags & flags::SHADOW != 0 {
            return false;
        }
        dir = d.parent;
    }
}

/// Applies one record to the folder table, when it is written and when the file is opened.
fn replay(st: &mut State, tag: u8, payload: &[u8], loc: u64) {
    let u32_at = |i: usize| payload.get(i..i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    match tag {
        TAG_LISTING => {
            let Some(d) = decode_entries(payload) else {
                return;
            };
            let Some(dir) = st.dirs.get(d.dir as usize) else {
                return;
            };
            if dir.loc != UNLISTED {
                return;
            }
            let mut total: u64 = 0;
            for (i, DecodedEntry { entry: e, link, name }) in d.entries.iter().enumerate() {
                total += e.size;
                if let Some(link) = link {
                    st.counted.insert(*link);
                }
                if e.child != NONE {
                    // Folders are numbered in the order they are found, so this one is next.
                    debug_assert_eq!(e.child as usize, st.dirs.len());
                    let own = u32::try_from(e.size).unwrap_or(u32::MAX);
                    let id = push_dir(st, d.dir, d.first_item + i as u32, name, own, e.flags & (flags::HIDDEN | flags::PACKAGE));
                    // Its own size is already in `total`; counted once.
                    st.dirs[id as usize].size = e.size;
                }
            }
            let count = d.entries.len() as u32;
            let x = &mut st.dirs[d.dir as usize];
            x.loc = loc;
            x.first_item = d.first_item;
            x.count = count;
            st.listings.push((d.first_item, d.dir));
            st.next_item = st.next_item.max(d.first_item + count);
            add_up(st, d.dir, total as i64);
        }
        TAG_REMOVE => {
            let (Some(item), Some(size)) = (u32_at(0), payload.get(4..12).map(|b| u64::from_le_bytes(b.try_into().unwrap()))) else {
                return;
            };
            if !st.removed.insert(item) {
                return;
            }
            if let Some((dir, _)) = locate(st, item) {
                add_up(st, dir, -(size as i64));
            }
        }
        TAG_SHADOW => {
            let (Some(shadow), Some(of), Some(item)) = (u32_at(0), u32_at(4), u32_at(8)) else {
                return;
            };
            if shadow as usize != st.dirs.len() || of as usize >= st.dirs.len() {
                return;
            }
            let o = st.dirs[of as usize];
            let name = dir_name(st, of).as_bytes().to_vec();
            let id = push_dir(st, o.parent, item, &name, o.own, (o.flags & (flags::HIDDEN | flags::PACKAGE)) | flags::SHADOW);
            st.shadows.insert(id, of);
            st.next_item = st.next_item.max(item + 1);
        }
        TAG_GRAFT => {
            let (Some(target), Some(shadow)) = (u32_at(0), u32_at(4)) else {
                return;
            };
            let (Some(t), Some(s)) = (st.dirs.get(target as usize).copied(), st.dirs.get(shadow as usize).copied()) else {
                return;
            };
            if st.shadows.get(&shadow) != Some(&target) || s.loc == UNLISTED {
                return;
            }
            st.shadows.remove(&shadow);
            // The target takes over the shadow's listing and the folders found in it; its old
            // folders are no longer in its listing, so they count as detached.
            for d in st.dirs.iter_mut().filter(|d| d.parent == shadow) {
                d.parent = target;
            }
            if let Some(l) = st.listings.iter_mut().rev().find(|l| l.1 == shadow) {
                l.1 = target;
            }
            let x = &mut st.dirs[target as usize];
            x.loc = s.loc;
            x.first_item = s.first_item;
            x.count = s.count;
            x.size = s.size;
            let parent = x.parent;
            let sh = &mut st.dirs[shadow as usize];
            sh.parent = NONE;
            sh.loc = UNLISTED;
            sh.flags &= !flags::SHADOW;
            add_up(st, parent, s.size as i64 - t.size as i64);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> NewEntry {
        NewEntry { name: name.into(), kind: Kind::File, flags: 0, size, mtime: 7, link: None, descend: false }
    }

    fn folder(name: &str) -> NewEntry {
        NewEntry { name: name.into(), kind: Kind::Dir, flags: 0, size: 0, mtime: 9, link: None, descend: true }
    }

    /// /data: a/ (x.bin 100, y.txt 10), b/ (not read yet), c.bin 1000
    fn sample(dir: &Path) -> Store {
        let s = Store::create(&dir.join("t.scan"), Path::new("/data"), "Data", 3).unwrap();
        let found = s.add_listing(0, vec![folder("a"), folder("b"), file("c.bin", 1000)]).unwrap();
        s.add_listing(found[0], vec![file("x.bin", 100), file("y.txt", 10)]).unwrap();
        s
    }

    fn listing(s: &Store) -> Vec<(PathBuf, u64)> {
        let mut out = Vec::new();
        s.walk(ROOT, |id, node, _| {
            out.push((s.path(id), node.size));
            true
        });
        out.sort();
        out
    }

    #[test]
    fn totals_grow_while_folders_are_read() {
        // The interface shows live totals during a scan, so every listing must add to all
        // folders holding it at once.
        let tmp = tempfile::tempdir().unwrap();
        let s = sample(tmp.path());
        let a = s.find(Path::new("/data/a")).unwrap();
        assert_eq!(s.size(a), 110);
        assert_eq!(s.size(ROOT), 1110);
        assert_eq!(s.display_name(ROOT), "Data");
        assert_eq!(s.path(s.find(Path::new("/data/a/y.txt")).unwrap()), PathBuf::from("/data/a/y.txt"));
        let names: Vec<String> = s.sorted_children(ROOT).into_iter().map(|c| s.display_name(c)).collect();
        assert_eq!(names, ["c.bin", "a", "b"]);
        assert_eq!(s.node(s.find(Path::new("/data/c.bin")).unwrap()).mtime, 7);
        assert!(s.node(a).is_dir());
        assert_eq!(s.ancestry(s.find(Path::new("/data/a/x.bin")).unwrap()), vec![ROOT, a, s.find(Path::new("/data/a/x.bin")).unwrap()]);
    }

    #[test]
    fn a_reopened_scan_is_identical_and_knows_what_is_left_to_read() {
        // Quitting mid-scan must lose nothing: the file alone holds the results and the
        // folders still to read.
        let tmp = tempfile::tempdir().unwrap();
        let before = {
            let s = sample(tmp.path());
            s.checkpoint(false, None).unwrap();
            (listing(&s), s.pending())
        };
        let s = Store::open(&tmp.path().join("t.scan")).unwrap();
        assert_eq!(listing(&s), before.0);
        assert_eq!(s.pending(), before.1);
        assert_eq!(s.pending().iter().map(|&d| s.dir_path(d)).collect::<Vec<_>>(), vec![PathBuf::from("/data/b")]);
        assert!(!s.summary().complete);
        // Continuing fills in the pending folder.
        let b = s.pending()[0];
        s.add_listing(b, vec![file("z", 5)]).unwrap();
        assert_eq!(s.size(ROOT), 1115);
        assert!(s.pending().is_empty());
    }

    #[test]
    fn records_not_yet_written_are_readable_and_a_torn_tail_is_dropped() {
        // A crash while a block is written must leave a usable file with everything before it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.scan");
        {
            let s = sample(tmp.path());
            // Still in memory, not flushed: readable all the same.
            assert_eq!(s.size(s.find(Path::new("/data/a/x.bin")).unwrap()), 100);
            s.checkpoint(false, None).unwrap();
            let b = s.pending()[0];
            s.add_listing(b, vec![file("late", 5)]).unwrap();
            s.checkpoint(false, None).unwrap();
        }
        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 3).unwrap();
        drop(f);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.size(ROOT), 1110, "the torn last block is dropped");
        assert_eq!(s.pending().len(), 1, "its folder is pending again");
        assert!(std::fs::metadata(&path).unwrap().len() < len - 3, "the torn bytes are cut off");
        let b = s.pending()[0];
        s.add_listing(b, vec![file("again", 6)]).unwrap();
        s.checkpoint(true, None).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.size(ROOT), 1116);
        assert!(s.summary().complete);
    }

    #[test]
    fn removed_items_are_gone_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.scan");
        {
            let s = sample(tmp.path());
            s.remove(s.find(Path::new("/data/a/x.bin")).unwrap()).unwrap();
            assert_eq!(s.size(ROOT), 1010);
            let a = s.find(Path::new("/data/a")).unwrap();
            s.remove(a).unwrap();
            assert_eq!(s.size(ROOT), 1000);
            assert!(!s.is_attached(a));
            assert!(s.find(Path::new("/data/a")).is_none());
            s.checkpoint(false, None).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.size(ROOT), 1000);
        assert_eq!(s.children(ROOT).len(), 2);
        assert_eq!(s.summary().total, 1000);
    }

    #[test]
    fn a_rescanned_folder_keeps_its_old_contents_until_the_rescan_is_grafted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.scan");
        {
            let s = sample(tmp.path());
            let a = s.find(Path::new("/data/a")).unwrap();
            let shadow = s.shadow(a).unwrap().unwrap();
            let inner = s.add_listing(shadow, vec![file("new.bin", 500), folder("deep")]).unwrap();
            // Not grafted yet: the old contents still count and the shadow is not in the tree.
            assert_eq!(s.size(ROOT), 1110);
            assert!(s.find(Path::new("/data/a/x.bin")).is_some());
            assert!(!s.pending().contains(&inner[0]));
            assert_eq!(s.pending_in(shadow), vec![inner[0]]);
            assert_eq!(s.dir_path(inner[0]), PathBuf::from("/data/a/deep"));
            s.add_listing(inner[0], vec![file("d", 1)]).unwrap();
            s.graft(shadow).unwrap();
            assert_eq!(s.size(ROOT), 1501);
            assert!(s.find(Path::new("/data/a/x.bin")).is_none());
            assert_eq!(s.size(s.find(Path::new("/data/a")).unwrap()), 501);
            assert_eq!(s.path(s.find(Path::new("/data/a/deep/d")).unwrap()), PathBuf::from("/data/a/deep/d"));
            s.checkpoint(false, None).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.size(ROOT), 1501);
        assert_eq!(s.size(s.find(Path::new("/data/a/deep")).unwrap()), 1);
        assert_eq!(s.pending().iter().map(|&d| s.dir_path(d)).collect::<Vec<_>>(), vec![PathBuf::from("/data/b")]);
    }

    #[test]
    fn hard_links_count_once_even_across_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.scan");
        let linked = |name: &str| NewEntry { link: Some((1, 42)), ..file(name, 4096) };
        {
            let s = Store::create(&path, Path::new("/r"), "r", 0).unwrap();
            let found = s.add_listing(0, vec![linked("one"), folder("later")]).unwrap();
            assert_eq!(s.size(ROOT), 4096);
            s.checkpoint(false, None).unwrap();
            drop(found);
        }
        let s = Store::open(&path).unwrap();
        let later = s.pending()[0];
        s.add_listing(later, vec![linked("two")]).unwrap();
        assert_eq!(s.size(ROOT), 4096);
        let two = s.find(Path::new("/r/later/two")).unwrap();
        assert!(s.node(two).has(flags::HARDLINK));
    }

    #[test]
    fn large_and_many_listings_read_back_through_small_caches() {
        // A folder with more entries than fit in a block, and more blocks than the caches hold.
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::create(&tmp.path().join("t.scan"), Path::new("/r"), "r", 0).unwrap();
        let mut entries: Vec<NewEntry> = (0..20_000).map(|i| file(&format!("f{i}"), i)).collect();
        entries.push(folder("sub"));
        let sub = s.add_listing(0, entries).unwrap()[0];
        let mut dirs = vec![sub];
        for level in 0..300 {
            let d = dirs[level];
            let mut e: Vec<NewEntry> = (0..50).map(|i| file(&format!("l{level}-{i}"), 1)).collect();
            e.push(folder("next"));
            dirs.push(s.add_listing(d, e).unwrap()[0]);
        }
        s.checkpoint(false, None).unwrap();
        let expected: u64 = (0..20_000u64).sum::<u64>() + 300 * 50;
        assert_eq!(s.size(ROOT), expected);
        let s = Store::open(s.file()).unwrap();
        assert_eq!(s.size(ROOT), expected);
        assert_eq!(s.display_name(s.find(Path::new("/r/f19999")).unwrap()), "f19999");
        let mut count = 0;
        s.walk(ROOT, |_, _, _| {
            count += 1;
            true
        });
        assert_eq!(count, 20_001 + 300 * 51);
        assert_eq!(s.biggest(ROOT, 2).iter().map(|&i| s.display_name(i)).collect::<Vec<_>>(), ["f19999", "f19998"]);
        assert_eq!(s.search(ROOT, "L299-49", 10).len(), 1, "case-insensitive, and l299-4 alone would also match l299-40..49");
        let (top, rest, rest_size) = s.largest_children(ROOT, 3);
        assert_eq!(top.len(), 3);
        assert_eq!(rest, 20_001 - 3);
        assert_eq!(rest_size + top.iter().map(|&t| s.size(t)).sum::<u64>(), expected);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.scan");
        std::fs::write(&path, b"garbage").unwrap();
        assert!(Store::open(&path).is_err());
    }
}
