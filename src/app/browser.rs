//! The scanned view: column browser or item list on the left, sunburst on the right.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dioxus::desktop::muda::{ContextMenu, Menu, MenuItem, PredefinedMenuItem};
use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
use dioxus::prelude::*;

use super::icons::{Icon, Symbol};
use super::remove::BottomBar;
use super::state::{
    AppState, Busy, Mode, Overview, home_dir, is_protected, use_app_state, use_list_trigger,
};
use super::sunburst::Sunburst;
use super::{SessionRef, format, icon_url, type_icon_url};
use crate::ops;
use crate::tree::{NodeId, ROOT, Tree, flags};

/// Rows shown per folder before the rest is folded into one "smaller items" row.
const ROW_LIMIT: usize = 200;
/// Entries in the Biggest Files and search lists.
const LIST_LIMIT: usize = 200;

#[derive(Clone, PartialEq)]
struct Row {
    id: NodeId,
    name: String,
    size: u64,
    is_dir: bool,
    has_children: bool,
    hidden: bool,
    path: PathBuf,
    /// Second line in list mode: the folder containing the item.
    detail: Option<String>,
    protected: bool,
    icon: String,
}

fn row(tree: &Tree, id: NodeId, detail: Option<String>) -> Row {
    let n = tree.node(id);
    let path = tree.path(id);
    // Plain files share their type's icon; drawing one per file made large folders slow.
    let icon = if n.kind() == crate::tree::Kind::File {
        type_icon_url(&path)
    } else {
        icon_url(&path)
    };
    Row {
        id,
        name: tree.display_name(id).into_owned(),
        size: n.size,
        is_dir: n.is_dir(),
        has_children: n.first_child != crate::tree::NONE,
        hidden: n.has(flags::HIDDEN),
        path,
        detail,
        protected: is_protected(tree, id),
        icon,
    }
}

fn size_class(size: u64) -> &'static str {
    match size {
        s if s >= 10_000_000_000 => "size huge",
        s if s >= 1_000_000_000 => "size large",
        s if s >= 100_000_000 => "size medium",
        _ => "size",
    }
}

/// Selection without entries that were removed or no longer form a path below `root`.
fn valid_selection(tree: &Tree, root: NodeId, selection: &[NodeId]) -> Vec<NodeId> {
    let mut parent = root;
    selection
        .iter()
        .copied()
        .take_while(|&id| {
            let ok = tree.is_attached(id) && tree.node(id).parent == parent;
            parent = id;
            ok
        })
        .collect()
}

/// The folder at the center of the chart: the deepest selected folder.
fn current_folder(tree: &Tree, root: NodeId, selection: &[NodeId]) -> NodeId {
    selection
        .iter()
        .rev()
        .copied()
        .find(|&id| tree.node(id).is_dir())
        .unwrap_or(root)
}

/// Path from `root` (exclusive) to `id`, for selecting an item found in a list.
fn selection_to(tree: &Tree, root: NodeId, id: NodeId) -> Vec<NodeId> {
    let chain = tree.ancestry(id);
    let start = chain.iter().position(|&a| a == root).map_or(chain.len(), |p| p + 1);
    chain[start..].to_vec()
}

#[component]
pub fn Browser(session: SessionRef, root: NodeId) -> Element {
    let mut state = use_app_state();
    let _ = (state.rev)();
    let fda = use_hook(ops::has_full_disk_access);
    let view = state.view.read().clone();
    let tree = session.tree.read();
    if !tree.is_attached(root) {
        return rsx! {};
    }
    let selection = valid_selection(&tree, root, &view.selection);
    let center = current_folder(&tree, root, &selection);
    let highlighted = selection.last().copied();
    let root_name = tree.display_name(root).into_owned();
    drop(tree);

    let needs_fda = !fda
        && !view.fda_dismissed
        && (session.root.as_os_str() == "/" || session.root.starts_with(super::state::home_dir()));
    let key = format!("{:p}-{root}", Arc::as_ptr(&session.0));
    let list = if !view.query.is_empty() {
        Some(ListKind::Search(view.query.clone()))
    } else if view.mode == Mode::Biggest {
        Some(ListKind::Biggest)
    } else {
        None
    };

    rsx! {
        div { class: "browser",
            if needs_fda {
                div { class: "banner",
                    Symbol { icon: Icon::Lock }
                    span { class: "banner-text", "Without Full Disk Access some folders are skipped and their space shows as unaccounted." }
                    button { class: "btn small", onclick: |_| ops::open_full_disk_access_settings(), "Open Settings…" }
                    button {
                        class: "btn small plain",
                        onclick: move |_| state.view.with_mut(|v| v.fda_dismissed = true),
                        "Dismiss"
                    }
                }
            }
            div { class: "browser-body",
                {match list {
                    // The key includes the kind: the list computes its items once per mount.
                    Some(kind) => rsx! {
                        ItemList { key: "{key}-{kind:?}", session: session.clone(), root, kind }
                    },
                    None => rsx! {
                        Columns { session: session.clone(), root, selection: selection.clone() }
                    },
                }}
                div { class: "chart",
                    Sunburst { session: session.clone(), view_root: root, center, highlighted }
                }
            }
            PathBar { session: session.clone(), root, root_name, selection }
            BottomBar { session: session.clone(), root }
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
enum ListKind {
    Biggest,
    Search(String),
}

#[component]
fn Columns(session: SessionRef, root: NodeId, selection: Vec<NodeId>) -> Element {
    let mut state = use_app_state();
    let _ = (state.rev)();
    let view = state.view.read().clone();
    let tree = session.tree.read();
    let complete = session.is_complete();
    let unaccounted = (root == ROOT && complete)
        .then(|| state.volume_of(&session.root))
        .flatten()
        .map(|v| v.used().saturating_sub(tree.size(ROOT)))
        .filter(|&u| u > 0);

    struct Column {
        folder: NodeId,
        rows: Vec<Row>,
        more: Option<(usize, u64)>,
        selected: Option<NodeId>,
    }
    let mut columns = Vec::new();
    let mut preview = None;
    for i in 0..=selection.len() {
        let folder = if i == 0 { root } else { selection[i - 1] };
        if !tree.node(folder).is_dir() {
            preview = Some(row(&tree, folder, None));
            break;
        }
        let limit = if view.expanded.contains(&folder) { usize::MAX } else { ROW_LIMIT };
        let (kids, rest, rest_size) = tree.largest_children(folder, limit);
        let more = (rest > 0).then_some((rest, rest_size));
        columns.push(Column {
            folder,
            rows: kids.iter().map(|&k| row(&tree, k, None)).collect(),
            more,
            selected: selection.get(i).copied(),
        });
    }
    let modified = preview.as_ref().map_or_else(String::new, |p| format::date(tree.node(p.id).mtime));
    drop(tree);
    let focused = selection.last().copied();

    // Keep the newest column visible when a folder is opened.
    let column_count = columns.len();
    use_effect(use_reactive!(|column_count| {
        let _ = column_count;
        document::eval("const c=document.querySelector('.columns'); if(c){c.scrollLeft=c.scrollWidth}");
    }));

    let key_session = session.clone();
    rsx! {
        div {
            class: "columns",
            tabindex: "0",
            onkeydown: move |e| handle_key(state, &key_session, root, e),
            for (i, col) in columns.into_iter().enumerate() {
                div { class: "column", key: "{col.folder}",
                    for r in col.rows {
                        ItemRow {
                            key: "{r.id}",
                            selected: col.selected == Some(r.id),
                            focused: focused == Some(r.id),
                            checked: view.checked.contains(&r.id),
                            onselect: {
                                let prefix = selection[..i].to_vec();
                                move |id| state.view.with_mut(|v| {
                                    v.selection = prefix.clone();
                                    v.selection.push(id);
                                })
                            },
                            row: r,
                        }
                    }
                    if let Some((n, size)) = col.more {
                        button {
                            class: "row more",
                            onclick: move |_| state.view.with_mut(|v| { v.expanded.insert(col.folder); }),
                            span { class: "name", "{format::count(n as u64)} smaller items" }
                            span { class: "size", "{format::bytes(size)}" }
                        }
                    }
                    if let Some(u) = unaccounted.filter(|_| i == 0) {
                        div {
                            class: "row unaccounted",
                            title: "Space the volume reports as used that the scan could not attribute: APFS snapshots, purgeable files, other volumes in the container, and folders without read access.",
                            span { class: "name", "Unaccounted" }
                            span { class: "size", "{format::bytes(u)}" }
                        }
                    }
                }
            }
            if let Some(p) = preview {
                div { class: "column preview",
                    img { src: "{icon_url(&p.path)}?size=128", width: "96", height: "96" }
                    div { class: "preview-name", "{p.name}" }
                    dl {
                        dt { "Size" }
                        dd { "{format::bytes(p.size)}" }
                        dt { "Modified" }
                        dd { "{modified}" }
                    }
                }
            }
        }
    }
}

fn handle_key(mut state: AppState, session: &SessionRef, root: NodeId, e: KeyboardEvent) {
    let tree = session.tree.read();
    let selection = valid_selection(&tree, root, &state.view.peek().selection);
    let parent = selection.len().checked_sub(2).map_or(root, |i| selection[i]);
    let mut next = selection.clone();
    match e.key() {
        Key::ArrowDown | Key::ArrowUp => {
            let siblings = tree.sorted_children(parent);
            let pos = selection.last().and_then(|s| siblings.iter().position(|x| x == s));
            let target = match (e.key(), pos) {
                (Key::ArrowDown, Some(p)) => siblings.get(p + 1),
                (Key::ArrowUp, Some(p)) => p.checked_sub(1).and_then(|p| siblings.get(p)),
                _ => siblings.first(),
            };
            if let Some(&t) = target {
                next.pop();
                next.push(t);
            }
        }
        Key::ArrowRight => {
            if let Some(&first) = selection
                .last()
                .and_then(|&s| tree.sorted_children(s).first().copied())
                .as_ref()
            {
                next.push(first);
            }
        }
        Key::ArrowLeft if next.len() > 1 => {
            next.pop();
        }
        Key::Character(c) if c == " " => {
            if let Some(&s) = selection.last() {
                ops::quick_look(&tree.path(s));
            }
            e.prevent_default();
            return;
        }
        Key::Backspace if e.modifiers().meta() => {
            if let Some(&s) = selection.last()
                && !is_protected(&tree, s)
            {
                let path = tree.path(s);
                drop(tree);
                trash_one(state, path);
            }
            return;
        }
        _ => return,
    }
    e.prevent_default();
    drop(tree);
    state.view.with_mut(|v| v.selection = next);
    document::eval("document.querySelector('.row.focused')?.scrollIntoView({block:'nearest'})");
}

#[component]
fn ItemRow(
    row: Row,
    selected: bool,
    focused: bool,
    checked: bool,
    onselect: EventHandler<NodeId>,
) -> Element {
    let mut state = use_app_state();
    let id = row.id;
    let path = row.path.clone();
    let mut class = String::from("row");
    if selected {
        class.push_str(if focused { " focused" } else { " selected" });
    }
    if row.hidden {
        class.push_str(" hidden");
    }
    rsx! {
        div {
            class,
            onclick: move |_| onselect.call(id),
            oncontextmenu: move |e| {
                e.prevent_default();
                onselect.call(id);
                show_context_menu(state, path.clone());
            },
            input {
                r#type: "checkbox",
                checked,
                disabled: row.protected,
                onclick: move |e| {
                    e.stop_propagation();
                    state.view.with_mut(|v| {
                        if !v.checked.remove(&id) {
                            v.checked.insert(id);
                        }
                    });
                },
            }
            img { class: "icon", src: "{row.icon}", width: "16", height: "16", loading: "lazy" }
            div { class: "label",
                span { class: "name", title: "{row.name}", "{row.name}" }
                if let Some(d) = &row.detail {
                    span { class: "detail", "{d}" }
                }
            }
            span { class: size_class(row.size), "{format::bytes(row.size)}" }
            if row.is_dir && row.has_children && row.detail.is_none() {
                span { class: "disclosure", Symbol { icon: Icon::Chevron, size: 12 } }
            } else if row.detail.is_none() {
                span { class: "disclosure" }
            }
        }
    }
}

#[component]
fn ItemList(session: SessionRef, root: NodeId, kind: ListKind) -> Element {
    let mut state = use_app_state();
    let trigger = use_list_trigger(state);
    let mut ids = use_signal(Vec::<NodeId>::new);
    let mut computing = use_signal(|| true);
    let job_session = session.clone();
    let job_kind = kind.clone();
    use_resource(move || {
        let _ = trigger();
        let session = job_session.0.clone();
        let kind = job_kind.clone();
        async move {
            computing.set(true);
            let found = tokio::task::spawn_blocking(move || {
                let tree = session.tree.read();
                match kind {
                    ListKind::Biggest => tree.biggest(root, LIST_LIMIT),
                    ListKind::Search(q) => tree.search(root, &q, LIST_LIMIT),
                }
            })
            .await
            .unwrap_or_default();
            ids.set(found);
            computing.set(false);
        }
    });

    let _ = (state.rev)();
    let view = state.view.read().clone();
    let tree = session.tree.read();
    let selected = view.selection.last().copied();
    let rows: Vec<Row> = ids
        .read()
        .iter()
        .copied()
        .filter(|&id| tree.is_attached(id))
        .map(|id| {
            let parent = tree.node(id).parent;
            let rel = tree.path(parent);
            let base = tree.path(root);
            let detail = rel
                .strip_prefix(&base)
                .ok()
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| tree.display_name(root).into_owned(), |p| p.display().to_string());
            row(&tree, id, Some(detail))
        })
        .collect();
    drop(tree);
    let empty = rows.is_empty() && !computing();

    rsx! {
        div { class: "list",
            for r in rows {
                ItemRow {
                    key: "{r.id}",
                    selected: selected == Some(r.id),
                    focused: true,
                    checked: view.checked.contains(&r.id),
                    onselect: {
                        let session = session.clone();
                        move |id| {
                            let sel = selection_to(&session.tree.read(), root, id);
                            state.view.with_mut(|v| v.selection = sel);
                        }
                    },
                    row: r,
                }
            }
            if empty {
                div { class: "list-empty",
                    {match &kind {
                        ListKind::Search(q) => rsx! { "No items match “{q}”." },
                        ListKind::Biggest => rsx! { "No files." },
                    }}
                }
            }
        }
    }
}

#[component]
fn PathBar(session: SessionRef, root: NodeId, root_name: String, selection: Vec<NodeId>) -> Element {
    let mut state = use_app_state();
    let _ = (state.rev)();
    let tree = session.tree.read();
    let mut crumbs = vec![(0usize, root_name, tree.path(root))];
    for (i, &id) in selection.iter().enumerate() {
        crumbs.push((i + 1, tree.display_name(id).into_owned(), tree.path(id)));
    }
    drop(tree);
    let last = crumbs.len() - 1;
    rsx! {
        div { class: "pathbar",
            for (depth, name, path) in crumbs {
                button {
                    class: "crumb",
                    onclick: move |_| state.view.with_mut(|v| v.selection.truncate(depth)),
                    img { src: "{icon_url(&path)}", width: "14", height: "14" }
                    span { "{name}" }
                }
                if depth < last {
                    span { class: "crumb-sep", Symbol { icon: Icon::Chevron, size: 10 } }
                }
            }
        }
    }
}

/// The scan and node a path refers to: in a location, the scan on screen; in an overview,
/// the closest scan that contains it.
fn target_of(state: &AppState, path: &Path) -> Option<(Arc<super::Session>, NodeId)> {
    if state.overview.peek().is_some() {
        return state.resolve(path);
    }
    let (session, _) = state.resolve(&state.selected.peek().clone())?;
    let id = session.tree.read().find(path)?;
    Some((session, id))
}

fn is_checked(state: &AppState, path: &Path, id: NodeId) -> bool {
    match *state.overview.peek() {
        Some(Overview::Projects) => state.dev_checked.peek().contains(path),
        Some(Overview::Apps) => false,
        None => state.view.peek().checked.contains(&id),
    }
}

fn toggle_checked(mut state: AppState, path: &Path, id: NodeId) {
    match *state.overview.peek() {
        Some(Overview::Projects) => state.dev_checked.with_mut(|c| {
            if !c.remove(path) {
                c.insert(path.to_path_buf());
            }
        }),
        // Apps are checked for uninstalling as a whole, not folder by folder.
        Some(Overview::Apps) => {}
        None => state.view.with_mut(|v| {
            if !v.checked.remove(&id) {
                v.checked.insert(id);
            }
        }),
    }
}

/// Opens the native context menu for `path`; the choice arrives in [`context_action`].
pub fn show_context_menu(mut state: AppState, path: PathBuf) {
    let Some((session, id)) = target_of(&state, &path) else {
        return;
    };
    let tree = session.tree.read();
    let is_dir = tree.node(id).is_dir();
    let protected = is_protected(&tree, id);
    drop(tree);
    let checked = is_checked(&state, &path, id);
    let markable = !protected && *state.overview.peek() != Some(Overview::Apps);
    let idle = session.busy() == Busy::Idle;
    state.context_target.set(Some(path));

    let menu = Menu::new();
    let entries: Vec<Option<(&str, &str, bool)>> = vec![
        Some(("ctx-reveal", "Show in Finder", true)),
        Some(("ctx-quicklook", "Quick Look", true)),
        None,
        Some(("ctx-rescan", "Rescan Folder", is_dir && idle)),
        None,
        Some(("ctx-check", if checked { "Unmark for Removal" } else { "Mark for Removal" }, markable)),
        Some(("ctx-trash", "Move to Trash", !protected && idle)),
        Some(("ctx-delete", "Delete Immediately…", !protected && idle)),
    ];
    for e in entries {
        let _ = match e {
            Some((id, label, enabled)) => menu.append(&MenuItem::with_id(id, label, enabled, None)),
            None => menu.append(&PredefinedMenuItem::separator()),
        };
    }
    let window = dioxus::desktop::window();
    // SAFETY: the view belongs to this window, which outlives the synchronous menu loop.
    unsafe {
        menu.show_context_menu_for_nsview(window.window.ns_view() as *const _, None);
    }
}

pub fn context_action(state: AppState, action: &str) {
    let Some(path) = state.context_target.peek().clone() else {
        return;
    };
    let Some((session, id)) = target_of(&state, &path) else {
        return;
    };
    match action {
        "ctx-reveal" => ops::reveal_in_finder(&path),
        "ctx-quicklook" => ops::quick_look(&path),
        "ctx-rescan" => state.rescan_folder(session, id),
        "ctx-check" => toggle_checked(state, &path, id),
        "ctx-trash" => trash_one(state, path),
        "ctx-delete" => {
            spawn(async move {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let confirmed = rfd::AsyncMessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title(format!("Delete “{name}” immediately?"))
                    .set_description("This item will be deleted immediately. You can’t undo this action.")
                    .set_buttons(rfd::MessageButtons::OkCancelCustom("Delete".into(), "Cancel".into()))
                    .show()
                    .await;
                if confirmed == rfd::MessageDialogResult::Custom("Delete".into())
                    || confirmed == rfd::MessageDialogResult::Ok
                {
                    let failed = run_removal(state, vec![path], false).await;
                    report_failures(failed).await;
                }
            });
        }
        _ => {}
    }
}

fn trash_one(state: AppState, path: PathBuf) {
    spawn(async move {
        let failed = run_removal(state, vec![path], true).await;
        report_failures(failed).await;
    });
}

/// Shows removals that failed in a native alert, so nothing fails silently.
async fn report_failures(failed: Vec<(PathBuf, String)>) {
    let Some((path, err)) = failed.first() else {
        return;
    };
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(format!("“{name}” could not be removed"))
        .set_description(err.as_str())
        .set_buttons(rfd::MessageButtons::Ok)
        .show()
        .await;
}

/// Removes items on a background thread, then drops the ones that are gone from every scan.
/// Returns the failures as (path, error message).
pub async fn run_removal(
    state: AppState,
    paths: Vec<PathBuf>,
    to_trash: bool,
) -> Vec<(PathBuf, String)> {
    let results = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let r = if to_trash { ops::move_to_trash(&path) } else { ops::delete_now(&path) };
                (path, r)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for (path, r) in results {
        match r {
            Ok(()) => removed.push(path),
            // Already gone counts as removed, so the scans stop showing it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => removed.push(path),
            Err(e) => failed.push((path, e.to_string())),
        }
    }
    state.forget_paths(&removed);
    if to_trash && !removed.is_empty() {
        refresh_trash(state);
    }
    failed
}

/// Moving to the Trash keeps the bytes on disk, now inside the Trash folder. Rescan it in
/// every scan that contains it, so the trees show where the space went.
fn refresh_trash(state: AppState) {
    let trash = home_dir().join(".Trash");
    let sessions: Vec<Arc<super::Session>> = state.sessions.peek().values().cloned().collect();
    for session in sessions {
        let id = session.tree.read().find(&trash);
        if let Some(id) = id {
            state.rescan_folder(session, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Kind;

    #[test]
    fn selection_is_cut_at_the_first_removed_item() {
        // After a deletion or rescan the columns must not point at items that are gone.
        let mut t = Tree::new("/r".into(), "r", 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        let b = t.add(a, b"b", Kind::Dir, 0, 0, 0);
        let c = t.add(b, b"c", Kind::File, 0, 1, 0);
        assert_eq!(valid_selection(&t, ROOT, &[a, b, c]), vec![a, b, c]);
        t.remove(b);
        assert_eq!(valid_selection(&t, ROOT, &[a, b, c]), vec![a]);
        // A selection that does not start below the view root is dropped entirely.
        assert!(valid_selection(&t, a, &[c]).is_empty());
    }

    #[test]
    fn chart_centers_on_the_deepest_selected_folder() {
        let mut t = Tree::new("/r".into(), "r", 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        let f = t.add(a, b"f", Kind::File, 0, 1, 0);
        assert_eq!(current_folder(&t, ROOT, &[]), ROOT);
        assert_eq!(current_folder(&t, ROOT, &[a]), a);
        assert_eq!(current_folder(&t, ROOT, &[a, f]), a);
        assert_eq!(selection_to(&t, ROOT, f), vec![a, f]);
        assert_eq!(selection_to(&t, a, f), vec![f]);
    }
}
