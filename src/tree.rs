//! Arena tree holding the result of one scan.
//!
//! Nodes live in a single `Vec` and refer to each other by index, so a tree with
//! millions of entries stays compact (32 bytes per node plus the name bytes) and
//! can be written to the cache as raw memory.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use bytemuck::{Pod, Zeroable};

pub type NodeId = u32;
pub const NONE: NodeId = u32::MAX;
pub const ROOT: NodeId = 0;

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

    fn to_u8(self) -> u8 {
        self as u8
    }
}

pub mod flags {
    /// Name starts with a dot.
    pub const HIDDEN: u8 = 1;
    /// Directory that Finder presents as a single item (`.app`, `.photoslibrary`, ...).
    pub const PACKAGE: u8 = 2;
    /// Directory that was not descended into (another volume).
    pub const PRUNED: u8 = 4;
    /// Additional hard link to an inode that is already counted elsewhere.
    pub const HARDLINK: u8 = 8;
    /// Node was removed from the tree; its id must no longer be used.
    pub const DETACHED: u8 = 16;
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct Node {
    /// Allocated bytes on disk; for directories the total of the whole subtree.
    pub size: u64,
    pub parent: NodeId,
    pub first_child: NodeId,
    pub next_sibling: NodeId,
    name_off: u32,
    /// Modification time in seconds since the Unix epoch, 0 if unknown.
    pub mtime: u32,
    name_len: u16,
    kind: u8,
    pub flags: u8,
}

impl Node {
    pub fn kind(&self) -> Kind {
        Kind::from_u8(self.kind)
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == Kind::Dir
    }

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

pub struct Tree {
    root_path: PathBuf,
    nodes: Vec<Node>,
    names: Vec<u8>,
}

impl Tree {
    /// Creates a tree containing only the root directory. `root_name` is what the UI shows
    /// for the root (for example "Macintosh HD" for `/`).
    pub fn new(root_path: PathBuf, root_name: &str, mtime: u32) -> Self {
        let mut tree = Tree {
            root_path,
            nodes: Vec::new(),
            names: Vec::new(),
        };
        tree.push(NONE, root_name.as_bytes(), Kind::Dir, 0, 0, mtime);
        tree
    }

    pub(crate) fn from_parts(root_path: PathBuf, nodes: Vec<Node>, names: Vec<u8>) -> Self {
        Tree {
            root_path,
            nodes,
            names,
        }
    }

    pub(crate) fn parts(&self) -> (&[Node], &[u8]) {
        (&self.nodes, &self.names)
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn size(&self, id: NodeId) -> u64 {
        self.node(id).size
    }

    pub fn name(&self, id: NodeId) -> &OsStr {
        let n = self.node(id);
        let start = n.name_off as usize;
        OsStr::from_bytes(&self.names[start..start + n.name_len as usize])
    }

    pub fn display_name(&self, id: NodeId) -> Cow<'_, str> {
        self.name(id).to_string_lossy()
    }

    fn push(
        &mut self,
        parent: NodeId,
        name: &[u8],
        kind: Kind,
        flags: u8,
        size: u64,
        mtime: u32,
    ) -> NodeId {
        let id = NodeId::try_from(self.nodes.len()).expect("tree exceeds u32::MAX nodes");
        let name_off = u32::try_from(self.names.len()).expect("name buffer exceeds 4 GiB");
        // APFS limits names to 255 UTF-8 characters, so a name always fits in u16 bytes.
        let name_len = u16::try_from(name.len()).expect("file name longer than 65535 bytes");
        self.names.extend_from_slice(name);
        self.nodes.push(Node {
            size,
            parent,
            first_child: NONE,
            next_sibling: NONE,
            name_off,
            mtime,
            name_len,
            kind: kind.to_u8(),
            flags,
        });
        id
    }

    /// Adds a child and adds its `size` to every ancestor, so totals are correct at any
    /// point during a scan.
    pub fn add(
        &mut self,
        parent: NodeId,
        name: &[u8],
        kind: Kind,
        flags: u8,
        size: u64,
        mtime: u32,
    ) -> NodeId {
        let id = self.push(parent, name, kind, flags, size, mtime);
        self.nodes[id as usize].next_sibling = self.nodes[parent as usize].first_child;
        self.nodes[parent as usize].first_child = id;
        self.add_to_ancestors(parent, size as i64);
        id
    }

    pub fn set_mtime(&mut self, id: NodeId, mtime: u32) {
        self.nodes[id as usize].mtime = mtime;
    }

    /// Adds `delta` to `id` and all of its ancestors.
    fn add_to_ancestors(&mut self, mut id: NodeId, delta: i64) {
        while id != NONE {
            let n = &mut self.nodes[id as usize];
            n.size = n.size.saturating_add_signed(delta);
            id = n.parent;
        }
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            tree: self,
            next: self.node(id).first_child,
        }
    }

    /// Children ordered largest first, ties broken by name.
    pub fn sorted_children(&self, id: NodeId) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self.children(id).collect();
        v.sort_by(|&a, &b| {
            self.size(b)
                .cmp(&self.size(a))
                .then_with(|| self.name(a).cmp(self.name(b)))
        });
        v
    }

    /// Ids from the root down to and including `id`.
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut v = Vec::new();
        let mut cur = id;
        while cur != NONE {
            v.push(cur);
            cur = self.node(cur).parent;
        }
        v.reverse();
        v
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut path = self.root_path.clone();
        for &a in &self.ancestry(id)[1..] {
            path.push(self.name(a));
        }
        path
    }

    /// Looks up the node for an absolute path inside this tree.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let rel = path.strip_prefix(&self.root_path).ok()?;
        let mut cur = ROOT;
        for comp in rel.components() {
            cur = self.children(cur).find(|&c| self.name(c) == comp.as_os_str())?;
        }
        Some(cur)
    }

    /// False once `id` or one of its ancestors was removed or replaced.
    pub fn is_attached(&self, id: NodeId) -> bool {
        if id as usize >= self.nodes.len() {
            return false;
        }
        let chain = self.ancestry(id);
        chain[0] == ROOT && chain.iter().all(|&a| !self.node(a).has(flags::DETACHED))
    }

    /// Unlinks `id` from its parent and subtracts its size from all ancestors.
    pub fn remove(&mut self, id: NodeId) {
        assert_ne!(id, ROOT, "the root cannot be removed");
        let n = *self.node(id);
        if n.has(flags::DETACHED) {
            return;
        }
        let parent = n.parent;
        if self.node(parent).first_child == id {
            self.nodes[parent as usize].first_child = n.next_sibling;
        } else {
            let mut cur = self.node(parent).first_child;
            while self.node(cur).next_sibling != id {
                cur = self.node(cur).next_sibling;
            }
            self.nodes[cur as usize].next_sibling = n.next_sibling;
        }
        self.nodes[id as usize].flags |= flags::DETACHED;
        self.add_to_ancestors(parent, -(n.size as i64));
    }

    /// Replaces the contents of directory `at` with the contents of `sub`, which is a fresh
    /// scan of the same directory. Ancestor sizes are adjusted by the difference.
    pub fn graft(&mut self, at: NodeId, sub: &Tree) {
        let old_children: Vec<NodeId> = self.children(at).collect();
        for c in old_children {
            self.nodes[c as usize].flags |= flags::DETACHED;
        }
        let base = self.nodes.len() as NodeId;
        let map = |i: NodeId| match i {
            NONE => NONE,
            ROOT => at,
            i => base + i - 1,
        };
        for (i, n) in sub.nodes.iter().enumerate().skip(1) {
            let name_off = self.names.len() as u32;
            self.names.extend_from_slice(sub.name(i as NodeId).as_bytes());
            self.nodes.push(Node {
                parent: map(n.parent),
                first_child: map(n.first_child),
                next_sibling: map(n.next_sibling),
                name_off,
                ..*n
            });
        }
        let old_size = self.size(at);
        let new_root = sub.node(ROOT);
        let target = &mut self.nodes[at as usize];
        target.first_child = map(new_root.first_child);
        target.mtime = new_root.mtime;
        target.flags &= !flags::PRUNED;
        self.add_to_ancestors(at, new_root.size as i64 - old_size as i64);
    }

    /// The `n` largest items below `from`. Packages count as one item, like in Finder.
    pub fn biggest(&self, from: NodeId, n: usize) -> Vec<NodeId> {
        let mut heap: BinaryHeap<Reverse<(u64, NodeId)>> = BinaryHeap::with_capacity(n + 1);
        let mut stack = vec![from];
        while let Some(id) = stack.pop() {
            let node = self.node(id);
            let is_item = id != from && (!node.is_dir() || node.has(flags::PACKAGE));
            if is_item {
                if heap.len() < n {
                    heap.push(Reverse((node.size, id)));
                } else if heap.peek().is_some_and(|Reverse((s, _))| node.size > *s) {
                    heap.pop();
                    heap.push(Reverse((node.size, id)));
                }
            } else {
                stack.extend(self.children(id));
            }
        }
        let mut v: Vec<(u64, NodeId)> = heap.into_iter().map(|Reverse(e)| e).collect();
        v.sort_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, id)| id).collect()
    }

    /// Items below `from` whose name contains `query` (case-insensitive), largest first.
    pub fn search(&self, from: NodeId, query: &str, limit: usize) -> Vec<NodeId> {
        let query = query.to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let ascii = query.is_ascii();
        let q = query.as_bytes();
        let matches = |name: &[u8]| {
            if ascii {
                name.len() >= q.len()
                    && name
                        .windows(q.len())
                        .any(|w| w.eq_ignore_ascii_case(q))
            } else {
                String::from_utf8_lossy(name).to_lowercase().contains(&query)
            }
        };
        let mut heap: BinaryHeap<Reverse<(u64, NodeId)>> = BinaryHeap::new();
        let mut stack: Vec<NodeId> = self.children(from).collect();
        while let Some(id) = stack.pop() {
            if matches(self.name(id).as_bytes()) {
                heap.push(Reverse((self.size(id), id)));
                if heap.len() > limit {
                    heap.pop();
                }
            }
            stack.extend(self.children(id));
        }
        let mut v: Vec<(u64, NodeId)> = heap.into_iter().map(|Reverse(e)| e).collect();
        v.sort_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, id)| id).collect()
    }

    /// A copy without removed or replaced nodes, for writing to the cache.
    pub fn compacted(&self) -> Tree {
        let root = self.node(ROOT);
        let mut out = Tree::new(
            self.root_path.clone(),
            &self.display_name(ROOT),
            root.mtime,
        );
        out.nodes[0].size = root.size;
        out.nodes[0].flags = root.flags;
        // (old id, new parent id). Siblings are popped in reverse and prepended, which
        // restores their original order.
        let mut stack: Vec<(NodeId, NodeId)> = Vec::new();
        let push_children = |stack: &mut Vec<(NodeId, NodeId)>, old: NodeId, new: NodeId| {
            let kids: Vec<NodeId> = self.children(old).collect();
            stack.extend(kids.into_iter().map(|c| (c, new)));
        };
        push_children(&mut stack, ROOT, ROOT);
        while let Some((old, new_parent)) = stack.pop() {
            let n = self.node(old);
            let id = out.push(
                new_parent,
                self.name(old).as_bytes(),
                n.kind(),
                n.flags,
                n.size,
                n.mtime,
            );
            out.nodes[id as usize].next_sibling = out.nodes[new_parent as usize].first_child;
            out.nodes[new_parent as usize].first_child = id;
            push_children(&mut stack, old, id);
        }
        out
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.tree.node(id).next_sibling;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// /root
    ///   a/          (dir)
    ///     big.bin   100
    ///     small.txt 10
    ///   Foo.app/    (package)
    ///     inner     50
    ///   c.txt       30
    fn sample() -> (Tree, [NodeId; 6]) {
        let mut t = Tree::new("/root".into(), "root", 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        let big = t.add(a, b"big.bin", Kind::File, 0, 100, 0);
        let small = t.add(a, b"small.txt", Kind::File, 0, 10, 0);
        let app = t.add(ROOT, b"Foo.app", Kind::Dir, flags::PACKAGE, 0, 0);
        let inner = t.add(app, b"inner", Kind::File, 0, 50, 0);
        let c = t.add(ROOT, b"c.txt", Kind::File, 0, 30, 0);
        (t, [a, big, small, app, inner, c])
    }

    #[test]
    fn directory_sizes_are_live_totals_of_their_subtree() {
        // The UI shows totals while the scan is still running, so every add must
        // update all ancestors immediately rather than in a final pass.
        let (t, [a, _, _, app, _, _]) = sample();
        assert_eq!(t.size(a), 110);
        assert_eq!(t.size(app), 50);
        assert_eq!(t.size(ROOT), 190);
    }

    #[test]
    fn sorted_children_are_largest_first() {
        let (t, [a, _, _, app, _, c]) = sample();
        assert_eq!(t.sorted_children(ROOT), vec![a, app, c]);
    }

    #[test]
    fn path_and_find_round_trip() {
        let (t, [_, big, ..]) = sample();
        let p = t.path(big);
        assert_eq!(p, PathBuf::from("/root/a/big.bin"));
        assert_eq!(t.find(&p), Some(big));
        assert_eq!(t.find(Path::new("/root")), Some(ROOT));
        assert_eq!(t.find(Path::new("/root/missing")), None);
        assert_eq!(t.find(Path::new("/elsewhere")), None);
    }

    #[test]
    fn remove_frees_space_in_every_ancestor_and_invalidates_the_id() {
        // After a deletion the UI must show the reclaimed space immediately and must not
        // keep offering the deleted item for selection.
        let (mut t, [a, big, small, ..]) = sample();
        t.remove(big);
        assert_eq!(t.size(a), 10);
        assert_eq!(t.size(ROOT), 90);
        assert_eq!(t.children(a).collect::<Vec<_>>(), vec![small]);
        assert!(!t.is_attached(big));
        assert!(t.is_attached(small));
        // Removing twice must not subtract twice.
        t.remove(big);
        assert_eq!(t.size(ROOT), 90);
    }

    #[test]
    fn removing_a_middle_sibling_keeps_the_others() {
        let (mut t, [a, _, _, app, _, c]) = sample();
        t.remove(app);
        let mut kids: Vec<_> = t.children(ROOT).collect();
        kids.sort();
        assert_eq!(kids, vec![a, c]);
    }

    #[test]
    fn graft_replaces_contents_and_adjusts_totals() {
        // A folder rescan must replace stale contents and correct every total above it.
        let (mut t, [a, big, small, ..]) = sample();
        let mut fresh = Tree::new("/root/a".into(), "a", 7);
        let d = fresh.add(ROOT, b"d", Kind::Dir, 0, 0, 0);
        fresh.add(d, b"new.bin", Kind::File, 0, 400, 0);
        t.graft(a, &fresh);

        assert_eq!(t.size(a), 400);
        assert_eq!(t.size(ROOT), 400 + 50 + 30);
        assert_eq!(t.node(a).mtime, 7);
        assert!(!t.is_attached(big) && !t.is_attached(small));
        let new = t.find(Path::new("/root/a/d/new.bin")).expect("grafted node");
        assert_eq!(t.size(new), 400);
        assert_eq!(t.path(new), PathBuf::from("/root/a/d/new.bin"));
    }

    #[test]
    fn biggest_treats_packages_as_single_items() {
        // Users delete apps and libraries as a whole, so a package must be listed as one
        // entry and its contents must not be listed separately.
        let (t, [_, big, small, app, inner, c]) = sample();
        let top = t.biggest(ROOT, 10);
        assert_eq!(top, vec![big, app, c, small]);
        assert!(!top.contains(&inner));
        assert_eq!(t.biggest(ROOT, 2), vec![big, app]);
    }

    #[test]
    fn search_is_case_insensitive_and_largest_first() {
        let (t, [_, big, small, _, _, c]) = sample();
        assert_eq!(t.search(ROOT, "BIG", 10), vec![big]);
        assert_eq!(t.search(ROOT, "txt", 10), vec![c, small]);
        assert_eq!(t.search(ROOT, "txt", 1), vec![c]);
        assert!(t.search(ROOT, "", 10).is_empty());
    }

    #[test]
    fn compacted_drops_removed_nodes_but_keeps_everything_else() {
        let (mut t, [_, big, ..]) = sample();
        t.remove(big);
        let c = t.compacted();
        assert_eq!(c.parts().0.len(), t.parts().0.len() - 1);
        assert_eq!(c.size(ROOT), t.size(ROOT));
        assert!(c.find(Path::new("/root/a/big.bin")).is_none());
        let inner = c.find(Path::new("/root/Foo.app/inner")).expect("kept");
        assert_eq!(c.size(inner), 50);
        assert!(c.node(c.find(Path::new("/root/Foo.app")).unwrap()).has(flags::PACKAGE));
    }
}
