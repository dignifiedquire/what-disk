//! The Apps overview: installed applications and Homebrew packages with the space they and
//! their data use, and uninstalling them.

use std::collections::{BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dioxus::prelude::*;

use super::browser::show_context_menu;
use super::state::{BrewState, Busy, Session, home_dir, use_app_state, use_list_trigger};
use super::{format, icon_url};
use crate::apps::{self, BundleInfo};
use crate::brew::{self, Brew};
use crate::ops;
use crate::tree::NodeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// /Applications, installed for all users.
    Global,
    /// ~/Applications.
    User,
    Casks,
    Formulae,
    BrewCache,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::Global => "Applications",
            Group::User => "My Applications",
            Group::Casks => "Homebrew Casks",
            Group::Formulae => "Homebrew Formulae",
            Group::BrewCache => "Homebrew Cache",
        }
    }

    fn is_brew(self) -> bool {
        matches!(self, Group::Casks | Group::Formulae | Group::BrewCache)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    App,
    Cask { token: String },
    Formula { name: String },
    BrewCache,
}

/// A folder or file that belongs to an entry. `size` is `None` when no scan covers it.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub path: PathBuf,
    pub label: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppEntry {
    /// Identifies the entry: the app bundle, Cellar or Caskroom folder, or cache folder.
    pub key: PathBuf,
    pub group: Group,
    pub kind: EntryKind,
    pub name: String,
    pub detail: String,
    /// Shown with Finder's icon for this path.
    pub icon: PathBuf,
    /// The app itself: bundles, Cellar or Caskroom folders.
    pub install: Vec<Part>,
    /// What it keeps in ~/Library.
    pub data: Vec<Part>,
}

fn sum(parts: &[Part]) -> Option<u64> {
    parts.iter().map(|p| p.size).sum()
}

impl AppEntry {
    pub fn install_size(&self) -> Option<u64> {
        sum(&self.install)
    }

    pub fn data_size(&self) -> u64 {
        self.data.iter().filter_map(|p| p.size).sum()
    }

    pub fn total(&self) -> u64 {
        self.install_size().unwrap_or(0) + self.data_size()
    }
}

/// Locations the report could not see because no scan covers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    Applications,
    Library,
    Homebrew,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppsReport {
    pub entries: Vec<AppEntry>,
    pub missing: Vec<Missing>,
}

/// Loaded scans, newest first, for looking up sizes by path.
struct Scans(Vec<Arc<Session>>);

impl Scans {
    fn new(mut sessions: Vec<Arc<Session>>) -> Self {
        sessions.retain(|s| s.busy() != Busy::Loading);
        sessions.sort_by_key(|s| std::cmp::Reverse(*s.scanned_at.lock()));
        Scans(sessions)
    }

    /// The newest scan that contains `path`, and the node for it.
    fn locate(&self, path: &Path) -> Option<(&Arc<Session>, NodeId)> {
        self.0.iter().find_map(|s| {
            if !path.starts_with(&s.root) {
                return None;
            }
            let id = s.tree.read().find(path)?;
            Some((s, id))
        })
    }

    fn size(&self, path: &Path) -> Option<u64> {
        let (s, id) = self.locate(path)?;
        Some(s.tree.read().size(id))
    }

    fn covers(&self, path: &Path) -> bool {
        self.0.iter().any(|s| path.starts_with(&s.root))
    }
}

/// An app's folders in ~/Library, from the newest scan that contains ~/Library.
fn app_data(scans: &Scans, library: &Path, info: &BundleInfo) -> Vec<Part> {
    let (Some(id), Some((session, _))) = (&info.id, scans.locate(library)) else {
        return Vec::new();
    };
    let tree = session.tree.read();
    apps::data_folders(&tree, library, id, &info.name)
        .into_iter()
        .map(|d| Part { path: tree.path(d.id), label: d.label.into(), size: Some(tree.size(d.id)) })
        .collect()
}

fn app_detail(info: &BundleInfo) -> String {
    [info.id.clone(), info.version.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

pub fn collect(sessions: Vec<Arc<Session>>, home: &Path, brew: Option<&Brew>) -> AppsReport {
    let scans = Scans::new(sessions);
    let library = home.join("Library");
    let mut report = AppsReport::default();
    if !scans.covers(&library) {
        report.missing.push(Missing::Library);
    }

    // Apps installed by a cask are listed with the cask.
    let cask_apps: HashSet<PathBuf> = brew
        .into_iter()
        .flat_map(|b| &b.casks)
        .flat_map(|c| &c.apps)
        .map(|a| Path::new("/Applications").join(a))
        .collect();
    for (folder, group) in [
        (PathBuf::from("/Applications"), Group::Global),
        (home.join("Applications"), Group::User),
    ] {
        let Some((session, _)) = scans.locate(&folder) else {
            if group == Group::Global {
                report.missing.push(Missing::Applications);
            }
            continue;
        };
        let found: Vec<(PathBuf, u64)> = {
            let tree = session.tree.read();
            apps::find_apps(&tree, &folder).into_iter().map(|id| (tree.path(id), tree.size(id))).collect()
        };
        for (path, size) in found.into_iter().filter(|(p, _)| !cask_apps.contains(p)) {
            let info = apps::bundle_info(&path);
            report.entries.push(AppEntry {
                key: path.clone(),
                group,
                kind: EntryKind::App,
                detail: app_detail(&info),
                data: app_data(&scans, &library, &info),
                name: info.name,
                icon: path.clone(),
                install: vec![Part { path, label: "App".into(), size: Some(size) }],
            });
        }
    }

    if let Some(brew) = brew {
        if !scans.covers(&brew.prefix) {
            report.missing.push(Missing::Homebrew);
        }
        for cask in &brew.casks {
            let apps: Vec<PathBuf> = cask.apps.iter().map(|a| Path::new("/Applications").join(a)).collect();
            let mut install: Vec<Part> = apps
                .iter()
                .map(|p| Part { path: p.clone(), label: "App".into(), size: scans.size(p) })
                .collect();
            let room = brew.caskroom(cask);
            install.push(Part { size: scans.size(&room), path: room.clone(), label: "Caskroom".into() });
            let app = apps.into_iter().find(|p| p.exists());
            let data = app.as_ref().map_or_else(Vec::new, |a| app_data(&scans, &library, &apps::bundle_info(a)));
            report.entries.push(AppEntry {
                key: room.clone(),
                group: Group::Casks,
                kind: EntryKind::Cask { token: cask.token.clone() },
                name: cask.name.clone(),
                detail: format!("{} · {}", cask.token, cask.version),
                icon: app.unwrap_or(room),
                install,
                data,
            });
        }
        for formula in &brew.formulae {
            let cellar = brew.cellar(formula);
            let mut detail = formula.versions.join(", ");
            if formula.dependency {
                detail.push_str(" · dependency");
            }
            report.entries.push(AppEntry {
                key: cellar.clone(),
                group: Group::Formulae,
                kind: EntryKind::Formula { name: formula.name.clone() },
                name: formula.name.clone(),
                detail,
                icon: cellar.clone(),
                install: vec![Part { size: scans.size(&cellar), path: cellar, label: "Cellar".into() }],
                data: Vec::new(),
            });
        }
        let cache = library.join("Caches/Homebrew");
        if let Some(size) = scans.size(&cache) {
            report.entries.push(AppEntry {
                key: cache.clone(),
                group: Group::BrewCache,
                kind: EntryKind::BrewCache,
                name: "Download cache".into(),
                detail: "Downloaded again when needed".into(),
                icon: cache.clone(),
                install: vec![Part { path: cache, label: "Cache".into(), size: Some(size) }],
                data: Vec::new(),
            });
        }
    }
    report.entries.sort_by(|a, b| b.total().cmp(&a.total()));
    report
}

/// What uninstalling `entry` does, in words.
fn action(entry: &AppEntry) -> String {
    let data = match entry.data.len() {
        0 => String::new(),
        1 => ", then move its data folder to the Trash".into(),
        n => format!(", then move its {n} data folders to the Trash"),
    };
    match &entry.kind {
        EntryKind::App if entry.data.is_empty() => "Move the app to the Trash".into(),
        EntryKind::App => format!(
            "Move the app and {} to the Trash",
            if entry.data.len() == 1 { "its data folder".to_string() } else { format!("its {} data folders", entry.data.len()) }
        ),
        EntryKind::Cask { token } => format!("brew uninstall --cask {token}{data}"),
        EntryKind::Formula { name } => format!("brew uninstall {name}"),
        EntryKind::BrewCache => "Move the download cache to the Trash".into(),
    }
}

fn trash(path: &Path) -> io::Result<()> {
    match ops::move_to_trash(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound || !path.exists() => Ok(()),
        r => r,
    }
}

/// Uninstalls entries one by one. An app's data is only removed once the app itself is gone,
/// so a failed uninstall never leaves an installed app without its settings. Returns the
/// removed paths and the failures as (name, message).
fn uninstall_blocking(entries: Vec<AppEntry>, brew: Option<PathBuf>) -> (Vec<PathBuf>, Vec<(String, String)>) {
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    let no_brew = || io::Error::other("Homebrew is not installed.");
    for entry in entries {
        let installed_gone = match &entry.kind {
            EntryKind::Formula { name } => brew.as_deref().ok_or_else(no_brew).and_then(|b| brew::uninstall_formula(b, name)),
            EntryKind::Cask { token } => brew.as_deref().ok_or_else(no_brew).and_then(|b| brew::uninstall_cask(b, token)),
            EntryKind::App | EntryKind::BrewCache => entry.install.iter().try_for_each(|p| trash(&p.path)),
        };
        if let Err(e) = installed_gone {
            failed.push((entry.name.clone(), e.to_string()));
            continue;
        }
        removed.extend(entry.install.iter().map(|p| p.path.clone()));
        for part in &entry.data {
            match trash(&part.path) {
                Ok(()) => removed.push(part.path.clone()),
                Err(e) => failed.push((format!("{} ({})", entry.name, part.label), e.to_string())),
            }
        }
    }
    (removed, failed)
}

fn short_path(path: &Path) -> String {
    match path.strip_prefix(home_dir()) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

fn size_text(size: Option<u64>) -> String {
    size.map_or_else(|| "—".into(), format::bytes)
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Apps,
    Homebrew,
}

#[component]
pub fn AppsView() -> Element {
    let mut state = use_app_state();
    use_hook(move || {
        state.load_all_cached();
        state.load_brew(false);
    });
    let trigger = use_list_trigger(state);
    let mut report = use_signal(|| None::<AppsReport>);
    use_resource(move || {
        let _ = trigger();
        let sessions: Vec<Arc<Session>> = state.sessions.read().values().cloned().collect();
        let brew = match &*state.brew.read() {
            BrewState::Ready(b) => Some(b.clone()),
            _ => None,
        };
        async move {
            let home = home_dir();
            let found = tokio::task::spawn_blocking(move || collect(sessions, &home, brew.as_deref())).await;
            if let Ok(r) = found {
                report.set(Some(r));
            }
        }
    });
    let mut filter = use_signal(|| Filter::All);
    let expanded = use_signal(BTreeSet::<PathBuf>::new);
    let mut sheet_open = use_signal(|| false);
    let brew_state = state.brew.read().clone();
    let checked = state.apps_checked.read().clone();

    let report = report.read().clone();
    let entries: Vec<AppEntry> = report.as_ref().map_or_else(Vec::new, |r| r.entries.clone());
    let shown: Vec<&AppEntry> = entries
        .iter()
        .filter(|e| match filter() {
            Filter::All => true,
            Filter::Apps => !e.group.is_brew(),
            Filter::Homebrew => e.group.is_brew(),
        })
        .collect();
    let selected: Vec<AppEntry> = entries.iter().filter(|e| checked.contains(&e.key)).cloned().collect();
    let selected_size: u64 = selected.iter().map(AppEntry::total).sum();
    let missing = report.as_ref().map_or_else(Vec::new, |r| r.missing.clone());
    let groups = [Group::Global, Group::User, Group::Casks, Group::Formulae, Group::BrewCache];
    let segment = move |label: &'static str, value: Filter| {
        rsx! {
            button {
                class: if filter() == value { "selected" },
                onclick: move |_| filter.set(value),
                "{label}"
            }
        }
    };
    let brew_note = match &brew_state {
        BrewState::Loading => Some("Asking Homebrew what is installed…".to_string()),
        BrewState::Unavailable(e) => Some(format!("Homebrew: {e}")),
        _ => None,
    };

    rsx! {
        header {
            class: "toolbar",
            onmousedown: move |_| dioxus::desktop::window().drag(),
            ondoubleclick: move |_| dioxus::desktop::window().toggle_maximized(),
            div {
                class: "toolbar-controls",
                onmousedown: move |e| e.stop_propagation(),
                ondoubleclick: move |e| e.stop_propagation(),
                div { class: "segmented",
                    {segment("All", Filter::All)}
                    {segment("Applications", Filter::Apps)}
                    {segment("Homebrew", Filter::Homebrew)}
                }
            }
            div { class: "toolbar-title",
                div { class: "title", "Apps" }
                div { class: "subtitle", "{entries.len()} installed · {format::bytes(entries.iter().map(AppEntry::total).sum())}" }
            }
        }
        div { class: "browser",
            for m in missing {
                MissingBanner { missing: m }
            }
            if let Some(note) = brew_note {
                div { class: "banner",
                    if brew_state == BrewState::Loading {
                        span { class: "spinner" }
                    }
                    span { "{note}" }
                }
            }
            if report.is_none() {
                div { class: "placeholder", div { class: "spinner large" } }
            } else {
                div { class: "dev-list",
                    for group in groups {
                        if shown.iter().any(|e| e.group == group) {
                            AppSection {
                                group,
                                entries: shown.iter().filter(|e| e.group == group).map(|e| (*e).clone()).collect::<Vec<_>>(),
                                checked: checked.clone(),
                                expanded,
                            }
                        }
                    }
                }
            }
            footer { class: "bottombar",
                div { class: "bottombar-status",
                    if selected.is_empty() {
                        span { class: "status-text secondary",
                            "Check apps to uninstall them. Expand an app to see its data; right-click a folder for more."
                        }
                    } else {
                        span { class: "status-text",
                            strong { "{selected.len()} selected" }
                            " · {format::bytes(selected_size)}"
                        }
                        button {
                            class: "btn plain",
                            onclick: move |_| state.apps_checked.with_mut(|c| c.clear()),
                            "Clear Selection"
                        }
                    }
                }
                button {
                    class: "btn primary",
                    disabled: selected.is_empty(),
                    onclick: move |_| sheet_open.set(true),
                    "Uninstall…"
                }
            }
        }
        if sheet_open() {
            UninstallSheet { entries: selected.clone(), open: sheet_open }
        }
    }
}

#[component]
fn MissingBanner(missing: Missing) -> Element {
    let state = use_app_state();
    let (text, root, button) = match missing {
        Missing::Applications => ("Apps in /Applications are not scanned yet.", PathBuf::from("/"), "Scan Macintosh HD"),
        Missing::Homebrew => ("Homebrew sizes need a scan of Macintosh HD.", PathBuf::from("/"), "Scan Macintosh HD"),
        Missing::Library => ("App data in ~/Library is not scanned yet.", home_dir(), "Scan Home Folder"),
    };
    let busy = state.sessions.read().get(&root).is_some_and(|s| s.busy() == Busy::Scanning);
    rsx! {
        div { class: "banner",
            span { "{text}" }
            if busy {
                span { class: "spinner" }
            } else {
                button { class: "btn small", onclick: move |_| state.start_scan(&root), "{button}" }
            }
        }
    }
}

#[component]
fn AppSection(group: Group, entries: Vec<AppEntry>, checked: BTreeSet<PathBuf>, expanded: Signal<BTreeSet<PathBuf>>) -> Element {
    let total: u64 = entries.iter().map(AppEntry::total).sum();
    // Without a scan of where these live, a total of zero would be wrong.
    let known = entries.iter().any(|e| e.install_size().is_some() || !e.data.is_empty());
    let total = if known { format::bytes(total) } else { "—".into() };
    rsx! {
        section { class: "dev-section",
            h3 { class: "dev-heading apps-heading",
                span { class: "apps-title", "{group.title()}" }
                span { class: "apps-col", "App" }
                span { class: "apps-col", "Data" }
                span { class: "apps-col", "{entries.len()} · {total}" }
            }
            for entry in entries {
                AppItem {
                    key: "{entry.key.display()}",
                    checked: checked.contains(&entry.key),
                    open: expanded.read().contains(&entry.key),
                    expanded,
                    entry,
                }
            }
        }
    }
}

#[component]
fn AppItem(entry: AppEntry, checked: bool, open: bool, expanded: Signal<BTreeSet<PathBuf>>) -> Element {
    let mut state = use_app_state();
    let mut expanded = expanded;
    let key = entry.key.clone();
    let check_key = entry.key.clone();
    let has_details = entry.install.len() > 1 || !entry.data.is_empty();
    let menu_path = entry.key.clone();
    rsx! {
        div {
            class: if checked { "row dev-row app-row checked" } else { "row dev-row app-row" },
            onclick: move |_| {
                if has_details {
                    let k = key.clone();
                    expanded.with_mut(|e| if !e.remove(&k) { e.insert(k); });
                }
            },
            oncontextmenu: move |e| {
                e.prevent_default();
                show_context_menu(state, menu_path.clone());
            },
            input {
                r#type: "checkbox",
                checked,
                onclick: move |e| {
                    e.stop_propagation();
                    let k = check_key.clone();
                    state.apps_checked.with_mut(|c| if !c.remove(&k) { c.insert(k); });
                },
            }
            img { class: "icon", src: "{icon_url(&entry.icon)}?size=64", width: "28", height: "28", loading: "lazy" }
            div { class: "label",
                span { class: "name", "{entry.name}" }
                span { class: "detail", "{entry.detail}" }
            }
            span { class: "apps-col size", "{size_text(entry.install_size())}" }
            span { class: "apps-col size", if entry.data.is_empty() { "—" } else { "{format::bytes(entry.data_size())}" } }
            span { class: "apps-col size total",
                if entry.install_size().is_none() && entry.data.is_empty() { "—" } else { "{format::bytes(entry.total())}" }
            }
            span { class: if open { "disclosure open" } else { "disclosure" },
                if has_details {
                    super::icons::Symbol { icon: super::icons::Icon::Chevron, size: 12 }
                }
            }
        }
        if open {
            div { class: "app-parts",
                for part in entry.install.iter().chain(entry.data.iter()).cloned() {
                    PartRow { key: "{part.path.display()}", part }
                }
            }
        }
    }
}

#[component]
fn PartRow(part: Part) -> Element {
    let state = use_app_state();
    let path = part.path.clone();
    rsx! {
        div {
            class: "row part-row",
            title: "{part.path.display()}",
            oncontextmenu: move |e| {
                e.prevent_default();
                show_context_menu(state, path.clone());
            },
            img { class: "icon", src: "{icon_url(&part.path)}", width: "16", height: "16", loading: "lazy" }
            span { class: "part-label", "{part.label}" }
            span { class: "detail part-path", "{short_path(&part.path)}" }
            span { class: "size", "{size_text(part.size)}" }
        }
    }
}

#[component]
fn UninstallSheet(entries: Vec<AppEntry>, open: Signal<bool>) -> Element {
    let mut state = use_app_state();
    let mut open = open;
    let mut working = use_signal(|| false);
    let mut failures = use_signal(Vec::<(String, String)>::new);
    let total: u64 = entries.iter().map(AppEntry::total).sum();
    let uses_brew = entries.iter().any(|e| matches!(e.kind, EntryKind::Cask { .. } | EntryKind::Formula { .. }));
    let run_entries = entries.clone();
    let run = move |_: MouseEvent| {
        let entries = run_entries.clone();
        spawn(async move {
            working.set(true);
            let brew = brew::find();
            let used_brew = entries.iter().any(|e| matches!(e.kind, EntryKind::Cask { .. } | EntryKind::Formula { .. }));
            let (removed, failed) = tokio::task::spawn_blocking(move || uninstall_blocking(entries, brew))
                .await
                .unwrap_or_else(|e| (Vec::new(), vec![("Uninstall".into(), e.to_string())]));
            state.forget_paths(&removed);
            state.apps_checked.with_mut(|c| c.retain(|k| !removed.contains(k)));
            if used_brew {
                state.load_brew(true);
            }
            working.set(false);
            if failed.is_empty() {
                open.set(false);
            } else {
                failures.set(failed);
            }
        });
    };

    rsx! {
        div { class: "sheet-backdrop",
            div { class: "sheet",
                h2 { "Uninstall {entries.len()} " if entries.len() == 1 { "item" } else { "items" } }
                if failures.read().is_empty() {
                    ul { class: "sheet-list",
                        for e in entries.iter() {
                            li { class: "sheet-item", key: "{e.key.display()}",
                                img { src: "{icon_url(&e.icon)}", width: "16", height: "16" }
                                div { class: "label",
                                    span { class: "name", "{e.name}" }
                                    span { class: "detail", "{action(e)}" }
                                }
                                span { class: "size", "{format::bytes(e.total())}" }
                            }
                        }
                    }
                    div { class: "sheet-total",
                        span { "Total" }
                        strong { "{format::bytes(total)}" }
                    }
                    p { class: "secondary",
                        if uses_brew {
                            "Homebrew deletes packages immediately. Apps and data go to the Trash and free space once it is emptied."
                        } else {
                            "Apps and data go to the Trash and free space once it is emptied."
                        }
                    }
                    div { class: "sheet-buttons",
                        span { class: "spacer" }
                        if working() {
                            span { class: "spinner" }
                        }
                        button { class: "btn", disabled: working(), onclick: move |_| open.set(false), "Cancel" }
                        button { class: "btn primary", disabled: working(), onclick: run, "Uninstall" }
                    }
                } else {
                    p { "Some items could not be uninstalled:" }
                    ul { class: "sheet-list",
                        for (name, err) in failures.read().iter().cloned() {
                            li { class: "sheet-item",
                                div { class: "label",
                                    span { class: "name", "{name}" }
                                    span { class: "detail error", "{err}" }
                                }
                            }
                        }
                    }
                    div { class: "sheet-buttons",
                        span { class: "spacer" }
                        button { class: "btn primary", onclick: move |_| open.set(false), "Close" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brew::{Cask, Formula};
    use crate::tree::{Kind, ROOT, Tree, flags};
    use std::time::{Duration, SystemTime};

    fn session(root: &str, build: impl FnOnce(&mut Tree)) -> Arc<Session> {
        let s = Session::new(Path::new(root), "x");
        let mut tree = Tree::new(root.into(), "x", 0);
        build(&mut tree);
        *s.tree.write() = tree;
        *s.scanned_at.lock() = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        Arc::new(s)
    }

    fn dir(t: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        t.add(parent, name.as_bytes(), Kind::Dir, 0, 0, 0)
    }

    /// /Applications/Ghost.app (100), /Applications/kitty.app (40, from a cask),
    /// /opt/homebrew/Cellar/zstd (7), /opt/homebrew/Caskroom/kitty (3).
    fn disk() -> Arc<Session> {
        session("/", |t| {
            let apps = dir(t, ROOT, "Applications");
            let ghost = t.add(apps, b"Ghost.app", Kind::Dir, flags::PACKAGE, 0, 0);
            t.add(ghost, b"bin", Kind::File, 0, 100, 0);
            let kitty = t.add(apps, b"kitty.app", Kind::Dir, flags::PACKAGE, 0, 0);
            t.add(kitty, b"bin", Kind::File, 0, 40, 0);
            let opt = dir(t, ROOT, "opt");
            let hb = dir(t, opt, "homebrew");
            let cellar = dir(t, hb, "Cellar");
            let zstd = dir(t, cellar, "zstd");
            t.add(zstd, b"lib", Kind::File, 0, 7, 0);
            let caskroom = dir(t, hb, "Caskroom");
            let room = dir(t, caskroom, "kitty");
            t.add(room, b"meta", Kind::File, 0, 3, 0);
        })
    }

    fn brew() -> Brew {
        Brew {
            executable: "/opt/homebrew/bin/brew".into(),
            prefix: "/opt/homebrew".into(),
            formulae: vec![Formula { name: "zstd".into(), versions: vec!["1.5.7".into()], dependency: true }],
            casks: vec![Cask { token: "kitty".into(), name: "kitty".into(), version: "0.49".into(), apps: vec!["kitty.app".into()] }],
        }
    }

    #[test]
    fn apps_casks_and_formulae_are_listed_once_with_their_sizes() {
        // A cask's app must appear under the cask only, with the cask's own folder added,
        // so nothing is counted twice and uninstalling goes through brew.
        let report = collect(vec![disk()], Path::new("/Users/nobody"), Some(&brew()));
        let summary: Vec<(Group, &str, Option<u64>)> = report
            .entries
            .iter()
            .map(|e| (e.group, e.name.as_str(), e.install_size()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Group::Global, "Ghost", Some(100)),
                (Group::Casks, "kitty", Some(43)),
                (Group::Formulae, "zstd", Some(7)),
            ]
        );
        let zstd = &report.entries[2];
        assert_eq!(zstd.detail, "1.5.7 · dependency");
        assert_eq!(zstd.kind, EntryKind::Formula { name: "zstd".into() });
        // The disk scan covers everything, even a home folder that does not exist in it.
        assert!(report.missing.is_empty());
    }

    #[test]
    fn missing_scans_are_reported_instead_of_zero_sizes() {
        let home_only = session("/Users/me", |_| {});
        let report = collect(vec![home_only], Path::new("/Users/me"), Some(&brew()));
        assert_eq!(report.missing, vec![Missing::Applications, Missing::Homebrew]);
        let zstd = report.entries.iter().find(|e| e.name == "zstd").unwrap();
        assert_eq!(zstd.install_size(), None);
    }

    #[test]
    fn a_failed_uninstall_keeps_the_data() {
        // If the app or package cannot be removed (an app that needs an administrator
        // password, a formula others depend on), deleting its data would leave a broken
        // install behind. Here brew is missing, so the uninstall fails.
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let entry = AppEntry {
            key: "/x".into(),
            group: Group::Global,
            kind: EntryKind::Formula { name: "x".into() },
            name: "X".into(),
            detail: String::new(),
            icon: "/x".into(),
            install: vec![],
            data: vec![Part { path: data.clone(), label: "Caches".into(), size: Some(1) }],
        };
        let (removed, failed) = uninstall_blocking(vec![entry], None);
        assert!(removed.is_empty());
        assert_eq!(failed.len(), 1);
        assert!(data.exists());
    }

    #[test]
    fn the_sheet_says_what_each_uninstall_does() {
        let mut e = AppEntry {
            key: "/a".into(),
            group: Group::Casks,
            kind: EntryKind::Cask { token: "kitty".into() },
            name: "kitty".into(),
            detail: String::new(),
            icon: "/a".into(),
            install: vec![],
            data: vec![Part { path: "/d".into(), label: "Caches".into(), size: Some(1) }],
        };
        assert_eq!(action(&e), "brew uninstall --cask kitty, then move its data folder to the Trash");
        e.kind = EntryKind::App;
        assert_eq!(action(&e), "Move the app and its data folder to the Trash");
    }
}
