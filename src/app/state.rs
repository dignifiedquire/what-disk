//! Application state shared by all components, and the background work that changes it.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use dioxus::prelude::*;
use parking_lot::{Mutex, RwLock};

use crate::brew::{self, Brew};
use crate::cache;
use crate::dev::Language;
use crate::locations::Locations;
use crate::scan::{self, CountedLinks, Outcome, Progress};
use crate::tree::{NodeId, ROOT, Tree, flags};
use crate::volumes::{self, Boundary, Volume};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    Idle,
    Loading,
    Scanning,
    /// A single folder is being rescanned and will replace its old contents when done.
    Rescanning(NodeId),
}

/// One scanned root: its tree and the state of work on it.
pub struct Session {
    pub root: PathBuf,
    pub tree: RwLock<Tree>,
    pub progress: Progress,
    /// Asks a running scan to pause, or a folder rescan to stop, after the folders it is
    /// reading.
    pub stop: AtomicBool,
    pub busy: Mutex<Busy>,
    /// When the tree was last scanned completely; for an unfinished scan, when it started.
    /// `None` before the first scan.
    pub scanned_at: Mutex<Option<SystemTime>>,
    /// Folders a paused scan still has to read. Empty once the scan is complete, and while
    /// a scan runs, since the scan holds them.
    pub pending: Mutex<Vec<NodeId>>,
    /// Hard links a paused scan has counted, so continuing it does not count them again.
    pub counted: Mutex<CountedLinks>,
    /// Incremented by background threads whenever they finish changing the tree.
    pub version: AtomicU64,
}

impl Session {
    pub(super) fn new(root: &Path, name: &str) -> Self {
        Session {
            root: root.to_path_buf(),
            tree: RwLock::new(Tree::new(root.to_path_buf(), name, 0)),
            progress: Progress::default(),
            stop: AtomicBool::new(false),
            busy: Mutex::new(Busy::Idle),
            scanned_at: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            counted: Mutex::new(CountedLinks::new()),
            version: AtomicU64::new(0),
        }
    }

    pub fn busy(&self) -> Busy {
        *self.busy.lock()
    }

    pub fn is_paused(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// The tree holds a whole, finished scan.
    pub fn is_complete(&self) -> bool {
        self.busy() == Busy::Idle && self.scanned_at.lock().is_some() && !self.is_paused()
    }

    fn finish(&self) {
        *self.busy.lock() = Busy::Idle;
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Writes the tree to the cache; failures only cost a rescan next time. Must not be
    /// called by others while a scan runs: the scan holds the folders it still has to read,
    /// so the cache would record the partial tree as complete.
    pub fn save(&self) {
        if let Some(at) = *self.scanned_at.lock() {
            let _ = cache::save(&cache::dir(), &self.tree.read(), at, &self.pending.lock(), &self.counted.lock());
        }
    }
}

/// How often a running scan saves its progress, so quitting or a crash loses little.
const CHECKPOINT: Duration = Duration::from_secs(120);

/// Scans that are running. Quitting pauses and saves them; by then the window and its state
/// are gone, so they are tracked here.
static RUNNING: Mutex<Vec<Arc<Session>>> = Mutex::new(Vec::new());

/// Scans `folders` of the session's tree in the background until it is complete or paused.
fn spawn_scan(session: Arc<Session>, folders: Vec<NodeId>) {
    *session.busy.lock() = Busy::Scanning;
    RUNNING.lock().push(session.clone());
    std::thread::spawn(move || {
        let boundary = Boundary::new(&session.root);
        let mut folders = folders;
        let mut counted = std::mem::take(&mut *session.counted.lock());
        loop {
            let deadline = Some(Instant::now() + CHECKPOINT);
            let outcome = scan::scan(&session.tree, &boundary, &session.progress, &session.stop, folders, deadline, &mut counted);
            match outcome {
                Outcome::Complete => {
                    *session.scanned_at.lock() = Some(SystemTime::now());
                    session.save();
                    break;
                }
                Outcome::Paused(rest) => {
                    *session.pending.lock() = rest;
                    *session.counted.lock() = counted;
                    session.save();
                    if session.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    folders = std::mem::take(&mut *session.pending.lock());
                    counted = std::mem::take(&mut *session.counted.lock());
                }
            }
        }
        session.stop.store(false, Ordering::Relaxed);
        RUNNING.lock().retain(|s| !Arc::ptr_eq(s, &session));
        session.finish();
    });
}

/// Pauses running scans and waits up to `limit` for them to save, so they can continue at
/// the next launch.
pub fn pause_scans_before_quit(limit: Duration) {
    let started = Instant::now();
    for session in RUNNING.lock().iter() {
        session.stop.store(true, Ordering::Relaxed);
    }
    while !RUNNING.lock().is_empty() && started.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LocationKind {
    Home,
    Volume(Volume),
    Folder,
}

/// An entry in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub name: String,
    pub kind: LocationKind,
}

/// A screen that combines all scans instead of showing one location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overview {
    Projects,
    Apps,
}

/// Homebrew's installed packages, loaded once per run since `brew` takes seconds.
#[derive(Clone, Debug, PartialEq)]
pub enum BrewState {
    NotLoaded,
    Loading,
    Ready(Arc<Brew>),
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Outline,
    Biggest,
}

/// What the user is looking at inside the selected location.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub mode: Mode,
    /// Path from the view root (exclusive) down to the selected item.
    pub selection: Vec<NodeId>,
    pub checked: BTreeSet<NodeId>,
    pub query: String,
    /// Folders whose list of small items was expanded.
    pub expanded: BTreeSet<NodeId>,
    pub fda_dismissed: bool,
}

impl Default for View {
    fn default() -> Self {
        View {
            mode: Mode::Outline,
            selection: Vec::new(),
            checked: BTreeSet::new(),
            query: String::new(),
            expanded: BTreeSet::new(),
            fda_dismissed: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct AppState {
    pub locations: Signal<Vec<Location>>,
    pub selected: Signal<PathBuf>,
    pub sessions: Signal<HashMap<PathBuf, Arc<Session>>>,
    pub summaries: Signal<HashMap<PathBuf, cache::Summary>>,
    /// Bumped whenever any tree changes, so components that read trees re-render.
    pub rev: Signal<u64>,
    pub view: Signal<View>,
    pub review_open: Signal<bool>,
    /// The item the open context menu acts on.
    pub context_target: Signal<Option<PathBuf>>,
    /// An overview is shown instead of the selected location.
    pub overview: Signal<Option<Overview>>,
    /// Folders checked in the Projects overview. Paths, since they come from several scans.
    pub dev_checked: Signal<BTreeSet<PathBuf>>,
    pub dev_filter: Signal<Option<Language>>,
    /// Entries checked for uninstalling in the Apps overview, by their key path.
    pub apps_checked: Signal<BTreeSet<PathBuf>>,
    pub brew: Signal<BrewState>,
    /// Where developer tools keep their data: the documented defaults until discovery has
    /// asked the user's shell and the tools.
    pub tool_locations: Signal<Arc<Locations>>,
}

pub fn use_app_state() -> AppState {
    use_context::<AppState>()
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

pub fn default_locations(summaries: &HashMap<PathBuf, cache::Summary>) -> Vec<Location> {
    let home = home_dir();
    let mut v = vec![Location {
        name: volumes::display_name(&home),
        path: home.clone(),
        kind: LocationKind::Home,
    }];
    for vol in volumes::list() {
        v.push(Location {
            path: vol.mount.clone(),
            name: vol.name.clone(),
            kind: LocationKind::Volume(vol),
        });
    }
    let mut folders: Vec<PathBuf> = ["Desktop", "Documents", "Downloads"]
        .iter()
        .map(|f| home.join(f))
        .collect();
    // Folders scanned in earlier sessions stay in the sidebar, except temporary folders that
    // the Apps overview scanned for itself.
    let mut cached: Vec<&PathBuf> = summaries
        .keys()
        .filter(|p| !p.starts_with("/private/var/folders") && !p.starts_with("/private/tmp") && !p.starts_with("/private/var/tmp"))
        .collect();
    cached.sort();
    for p in cached {
        if !folders.contains(p) && !v.iter().any(|l| &l.path == p) {
            folders.push(p.clone());
        }
    }
    for p in folders {
        v.push(Location {
            name: volumes::display_name(&p),
            path: p,
            kind: LocationKind::Folder,
        });
    }
    v
}

impl AppState {
    pub fn location(&self, path: &Path) -> Option<Location> {
        self.locations.read().iter().find(|l| l.path == path).cloned()
    }

    pub fn volume_of(&self, path: &Path) -> Option<Volume> {
        self.locations.read().iter().find_map(|l| match &l.kind {
            LocationKind::Volume(v) if l.path == path => Some(v.clone()),
            _ => None,
        })
    }

    /// The session showing `path`, and the node for `path` inside it. A folder that is part
    /// of a larger scan (for example Documents inside the startup disk) reuses that scan.
    pub fn resolve(&self, path: &Path) -> Option<(Arc<Session>, NodeId)> {
        let sessions = self.sessions.read();
        if let Some(s) = sessions.get(path) {
            return Some((s.clone(), ROOT));
        }
        let mut candidates: Vec<&Arc<Session>> = sessions
            .values()
            .filter(|s| path.starts_with(&s.root) && s.busy() != Busy::Loading)
            .collect();
        // Prefer the closest enclosing scan.
        candidates.sort_by_key(|s| std::cmp::Reverse(s.root.components().count()));
        candidates
            .into_iter()
            .find_map(|s| s.tree.read().find(path).map(|id| (s.clone(), id)))
    }

    /// Cached root that can show `path`: its own scan, else the closest enclosing one.
    fn cached_root_for(&self, path: &Path) -> Option<PathBuf> {
        let summaries = self.summaries.read();
        if summaries.contains_key(path) {
            return Some(path.to_path_buf());
        }
        summaries
            .keys()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.components().count())
            .cloned()
    }

    /// Loads the cached scan for `path` if it is not shown by a loaded session yet.
    pub fn ensure_loaded(mut self, path: &Path) {
        if self.resolve(path).is_some() {
            return;
        }
        let Some(root) = self.cached_root_for(path) else {
            return;
        };
        if self.sessions.read().contains_key(&root) {
            return;
        }
        let session = Arc::new(Session::new(&root, ""));
        *session.busy.lock() = Busy::Loading;
        self.sessions.write().insert(root.clone(), session.clone());
        std::thread::spawn(move || {
            match cache::load(&cache::dir(), &root) {
                Ok((tree, summary, pending, counted)) => {
                    // A continued scan counts on from the items it already found.
                    let found = tree.parts().0.len() as u64 - 1;
                    session.progress.files.store(found, Ordering::Relaxed);
                    *session.tree.write() = tree;
                    *session.scanned_at.lock() = Some(summary.scanned_at);
                    *session.pending.lock() = pending;
                    *session.counted.lock() = counted;
                }
                // An unreadable cache is treated as "not scanned"; the next scan replaces it.
                Err(_) => {
                    let _ = cache::remove(&cache::dir(), &root);
                }
            }
            session.finish();
        });
    }

    /// Starts a full scan of `path`, replacing any previous result for it. Does nothing while
    /// `path` is being scanned: both scans would save to the same cache.
    pub fn start_scan(mut self, path: &Path) {
        let running = self
            .sessions
            .peek()
            .get(path)
            .is_some_and(|s| matches!(s.busy(), Busy::Scanning | Busy::Rescanning(_)));
        if running {
            return;
        }
        let name = self
            .location(path)
            .map_or_else(|| volumes::display_name(path), |l| l.name);
        let session = Arc::new(Session::new(path, &name));
        *session.scanned_at.lock() = Some(SystemTime::now());
        self.sessions.write().insert(path.to_path_buf(), session.clone());
        self.view.set(View::default());
        spawn_scan(session, vec![ROOT]);
    }

    /// Continues a paused scan with the folders it has not read yet.
    pub fn resume_scan(self, session: Arc<Session>) {
        if session.busy() != Busy::Idle {
            return;
        }
        let folders = std::mem::take(&mut *session.pending.lock());
        if !folders.is_empty() {
            spawn_scan(session, folders);
        }
    }

    /// Rescans one folder of an existing scan and replaces its contents.
    pub fn rescan_folder(self, session: Arc<Session>, id: NodeId) {
        if id == ROOT {
            self.start_scan(&session.root);
            return;
        }
        if session.busy() != Busy::Idle {
            return;
        }
        let (path, name) = {
            let t = session.tree.read();
            (t.path(id), t.display_name(id).into_owned())
        };
        *session.busy.lock() = Busy::Rescanning(id);
        std::thread::spawn(move || {
            let sub = RwLock::new(Tree::new(path, &name, 0));
            let boundary = Boundary::new(&session.root);
            let outcome = scan::scan(
                &sub,
                &boundary,
                &Progress::default(),
                &session.stop,
                vec![ROOT],
                None,
                &mut CountedLinks::new(),
            );
            session.stop.store(false, Ordering::Relaxed);
            // A stopped rescan keeps the folder's previous contents.
            if outcome == Outcome::Complete {
                let sub = sub.into_inner();
                let mut tree = session.tree.write();
                if tree.is_attached(id) {
                    tree.graft(id, &sub);
                }
                drop(tree);
                // In a paused scan the folder is now read; folders below it were replaced.
                session.pending.lock().retain(|&p| p != id);
                session.save();
            }
            session.finish();
        });
    }

    /// Pauses a scan, or stops a folder rescan.
    pub fn stop(self, session: &Session) {
        session.stop.store(true, Ordering::Relaxed);
    }

    /// Drops paths that were deleted on disk from every loaded scan that contains them, so
    /// overlapping scans (a volume and a folder on it) stay consistent, and saves those scans.
    /// A running scan saves the change itself when it pauses or finishes.
    pub fn forget_paths(mut self, paths: &[PathBuf]) {
        let sessions: Vec<Arc<Session>> = self.sessions.peek().values().cloned().collect();
        for session in sessions {
            let changed = {
                let mut tree = session.tree.write();
                let ids: Vec<NodeId> = paths
                    .iter()
                    .filter_map(|p| tree.find(p))
                    .filter(|&id| id != ROOT)
                    .collect();
                for &id in &ids {
                    tree.remove(id);
                }
                !ids.is_empty()
            };
            if changed && session.busy() != Busy::Scanning {
                std::thread::spawn(move || {
                    session.save();
                    // Lets the sidebar pick up the new total from the saved scan.
                    session.version.fetch_add(1, Ordering::Relaxed);
                });
            }
        }
        if let Some((session, _)) = self.resolve(&self.selected.peek().clone()) {
            let tree = session.tree.read();
            self.view.with_mut(|v| v.checked.retain(|&id| tree.is_attached(id)));
        }
        self.dev_checked.with_mut(|c| c.retain(|p| !paths.contains(p)));
        self.rev.with_mut(|r| *r += 1);
    }

    /// Asks Homebrew what is installed, in the background. `force` reloads after changes.
    pub fn load_brew(mut self, force: bool) {
        let current = self.brew.peek().clone();
        if !force && current != BrewState::NotLoaded || current == BrewState::Loading {
            return;
        }
        let Some(executable) = brew::find() else {
            self.brew.set(BrewState::Unavailable("Homebrew is not installed.".into()));
            return;
        };
        self.brew.set(BrewState::Loading);
        spawn(async move {
            let loaded = tokio::task::spawn_blocking(move || brew::load(&executable)).await;
            self.brew.set(match loaded {
                Ok(Ok(b)) => BrewState::Ready(Arc::new(b)),
                Ok(Err(e)) => BrewState::Unavailable(e.to_string()),
                Err(e) => BrewState::Unavailable(e.to_string()),
            });
        });
    }

    /// Finds where developer tools keep their data, in the background.
    pub fn discover_locations(mut self) {
        spawn(async move {
            let home = home_dir();
            if let Ok(found) = tokio::task::spawn_blocking(move || Locations::discover(&home)).await {
                self.tool_locations.set(Arc::new(found));
            }
        });
    }

    /// Loads every cached scan that no loaded scan covers yet.
    pub fn load_all_cached(self) {
        let roots: Vec<PathBuf> = self.summaries.peek().keys().cloned().collect();
        for root in roots {
            self.ensure_loaded(&root);
        }
    }

    /// Re-reads cache summaries and volume capacities after background work finished.
    pub fn refresh_disk_info(mut self) {
        let summaries: HashMap<PathBuf, cache::Summary> = cache::list(&cache::dir())
            .into_iter()
            .map(|s| (s.root.clone(), s))
            .collect();
        let mut locations = default_locations(&summaries);
        // Keep folders the user opened that have no scan yet.
        for l in self.locations.peek().iter() {
            if !locations.iter().any(|n| n.path == l.path) {
                locations.push(l.clone());
            }
        }
        self.summaries.set(summaries);
        if *self.locations.peek() != locations {
            self.locations.set(locations);
        }
    }
}

/// Changes when lists computed from the trees should be recomputed: on every change while
/// idle, but at most every two seconds while a scan runs, since walking millions of nodes
/// four times a second would stall the interface.
pub fn use_list_trigger(state: AppState) -> Memo<(bool, u64)> {
    use_memo(move || {
        let rev = (state.rev)();
        let busy = state
            .sessions
            .peek()
            .values()
            .any(|s| matches!(s.busy(), Busy::Scanning | Busy::Rescanning(_)));
        if busy { (true, rev / 8) } else { (false, rev) }
    })
}

/// Checked items, without items whose ancestor is also checked (deleting the ancestor
/// deletes them too) and without items that no longer exist.
pub fn effective_checked(tree: &Tree, checked: &BTreeSet<NodeId>) -> Vec<NodeId> {
    checked
        .iter()
        .copied()
        .filter(|&id| tree.is_attached(id))
        .filter(|&id| {
            let chain = tree.ancestry(id);
            chain[..chain.len() - 1].iter().all(|a| !checked.contains(a))
        })
        .collect()
}

/// Items that must never be offered for deletion.
pub fn is_protected(tree: &Tree, id: NodeId) -> bool {
    id == ROOT || tree.node(id).has(flags::PRUNED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Kind;

    #[test]
    fn checking_a_folder_and_its_contents_counts_the_contents_once() {
        // The removal total must equal the space actually freed, so items inside a checked
        // folder must not be added again.
        let mut t = Tree::new("/r".into(), "r", 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        let inner = t.add(a, b"inner", Kind::File, 0, 10, 0);
        let other = t.add(ROOT, b"other", Kind::File, 0, 5, 0);
        let checked: BTreeSet<NodeId> = [a, inner, other].into();
        assert_eq!(effective_checked(&t, &checked), vec![a, other]);

        t.remove(other);
        assert_eq!(effective_checked(&t, &checked), vec![a]);
    }
}
