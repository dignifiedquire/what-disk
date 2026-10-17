//! The Developer view: build folders of Rust and JavaScript projects, plus toolchain caches
//! and installed toolchain versions, from all scans, largest first.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use dioxus::prelude::*;

use super::browser::show_context_menu;
use super::state::{AppState, Busy, Session, home_dir, use_app_state, use_list_trigger};
use super::{format, icon_url};
use crate::dev::{self, ArtifactKind, Language};
use crate::locations::Locations;

/// One removable folder: a project's build folder or a global cache.
#[derive(Clone, Debug, PartialEq)]
pub struct DevRow {
    /// The folder that would be removed.
    pub path: PathBuf,
    /// Project name, or the cache's label.
    pub title: String,
    /// Where it is: the project folder, or the cache folder.
    pub location: PathBuf,
    pub language: Language,
    /// "target" or "node_modules" for a project; for a cache, what removing it costs.
    pub what: String,
    pub kind: Option<ArtifactKind>,
    pub size: u64,
    /// Newest modification time inside, seconds since the epoch.
    pub newest: u32,
}

impl DevRow {
    fn is_cache(&self) -> bool {
        self.kind.is_none()
    }
}

/// Rows from all loaded scans. Where scans overlap, the most recent scan's numbers win.
pub fn collect(mut sessions: Vec<Arc<Session>>, locations: &Locations) -> Vec<DevRow> {
    sessions.retain(|s| s.busy() != Busy::Loading);
    sessions.sort_by_key(|s| std::cmp::Reverse(*s.scanned_at.lock()));
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for session in sessions {
        let tree = session.tree.read();
        for project in dev::find_projects(&tree) {
            let location = tree.path(project.dir);
            let title = tree.display_name(project.dir).into_owned();
            for a in project.artifacts {
                let path = tree.path(a.id);
                // The folder's own name, with what it is when the name does not say.
                let name = tree.display_name(a.id);
                let what = if name == a.kind.folder_name() {
                    name.into_owned()
                } else {
                    format!("{name} · {}", a.kind.folder_name())
                };
                if seen.insert(path.clone()) {
                    rows.push(DevRow {
                        path,
                        title: title.clone(),
                        location: location.clone(),
                        language: a.kind.language(),
                        what,
                        kind: Some(a.kind),
                        size: tree.size(a.id),
                        newest: dev::newest_mtime(&tree, a.id),
                    });
                }
            }
        }
        for c in dev::find_global_caches(&tree, locations) {
            let path = tree.path(c.id);
            if tree.size(c.id) > 0 && seen.insert(path.clone()) {
                rows.push(DevRow {
                    location: path.clone(),
                    path,
                    title: c.label,
                    language: c.language,
                    what: c.note.into(),
                    kind: None,
                    size: tree.size(c.id),
                    newest: dev::newest_mtime(&tree, c.id),
                });
            }
        }
    }
    rows.sort_by(|a, b| b.size.cmp(&a.size));
    rows
}

/// `~/work/x` instead of `/Users/me/work/x`.
fn short_path(path: &Path) -> String {
    match path.strip_prefix(home_dir()) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".into(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

fn age(row: &DevRow) -> String {
    if row.newest == 0 {
        return String::new();
    }
    let when = format::ago(UNIX_EPOCH + Duration::from_secs(u64::from(row.newest)));
    match row.kind {
        Some(ArtifactKind::NodeModules) => format!("installed {when}"),
        Some(_) => format!("built {when}"),
        None => format!("changed {when}"),
    }
}

/// The rows of the current scans, recomputed in the background when the scans change.
fn use_rows(state: AppState) -> (Signal<Vec<DevRow>>, Signal<bool>) {
    let trigger = use_list_trigger(state);
    let mut rows = use_signal(Vec::<DevRow>::new);
    let mut computing = use_signal(|| true);
    use_resource(move || {
        let _ = trigger();
        let sessions: Vec<Arc<Session>> = state.sessions.read().values().cloned().collect();
        let locations = state.tool_locations.read().clone();
        async move {
            computing.set(true);
            let found = tokio::task::spawn_blocking(move || collect(sessions, &locations))
                .await
                .unwrap_or_default();
            rows.set(found);
            computing.set(false);
        }
    });
    (rows, computing)
}

#[component]
pub fn DevView() -> Element {
    let mut state = use_app_state();
    use_hook(move || state.load_all_cached());
    let (rows, computing) = use_rows(state);
    let filter = (state.dev_filter)();
    let checked = state.dev_checked.read().clone();
    let all = rows.read();
    let shown: Vec<&DevRow> = all
        .iter()
        .filter(|r| filter.is_none_or(|l| r.language == l))
        .collect();
    let (projects, caches): (Vec<&DevRow>, Vec<&DevRow>) = shown.iter().partition(|r| !r.is_cache());
    let no_scans = state.summaries.read().is_empty() && state.sessions.read().is_empty();
    let loading = state.sessions.read().values().any(|s| s.busy() == Busy::Loading);
    let selected: Vec<&DevRow> = all.iter().filter(|r| checked.contains(&r.path)).collect();
    let selected_size: u64 = selected.iter().map(|r| r.size).sum();

    let body = if no_scans {
        let home = home_dir();
        rsx! {
            div { class: "overview",
                p { class: "overview-hint",
                    "Scan your home folder to find Rust and JavaScript projects and toolchain caches."
                }
                button {
                    class: "btn primary large",
                    // Stay in this view: the list fills in while the scan runs.
                    onclick: move |_| state.start_scan(&home),
                    "Scan Home Folder"
                }
            }
        }
    } else if shown.is_empty() {
        rsx! {
            div { class: "placeholder",
                if computing() || loading {
                    div { class: "spinner large" }
                } else {
                    p { class: "overview-hint", "No build folders or caches found in the scanned locations." }
                }
            }
        }
    } else {
        rsx! {
            div { class: "dev-list",
                if !projects.is_empty() {
                    Section { title: "Projects", rows: projects.iter().map(|r| (*r).clone()).collect::<Vec<_>>(), checked: checked.clone() }
                }
                if !caches.is_empty() {
                    Section { title: "Toolchains and Caches", rows: caches.iter().map(|r| (*r).clone()).collect::<Vec<_>>(), checked: checked.clone() }
                }
            }
        }
    };

    rsx! {
        div { class: "browser",
            {body}
            footer { class: "bottombar",
                div { class: "bottombar-status",
                    if selected.is_empty() {
                        span { class: "status-text secondary", "Check folders to review and remove them." }
                    } else {
                        span { class: "status-text",
                            strong { "{selected.len()} " if selected.len() == 1 { "folder" } else { "folders" } }
                            " selected · {format::bytes(selected_size)}"
                        }
                        button {
                            class: "btn plain",
                            onclick: move |_| state.dev_checked.with_mut(|c| c.clear()),
                            "Clear Selection"
                        }
                    }
                }
                button {
                    class: "btn primary",
                    disabled: selected.is_empty(),
                    onclick: move |_| state.review_open.set(true),
                    "Review and Remove…"
                }
            }
        }
    }
}

#[component]
fn Section(title: &'static str, rows: Vec<DevRow>, checked: BTreeSet<PathBuf>) -> Element {
    let total: u64 = rows.iter().map(|r| r.size).sum();
    rsx! {
        section { class: "dev-section",
            h3 { class: "dev-heading",
                span { "{title}" }
                span { class: "secondary", "{rows.len()} · {format::bytes(total)}" }
            }
            for row in rows {
                DevItem { key: "{row.path.display()}", checked: checked.contains(&row.path), row }
            }
        }
    }
}

#[component]
fn DevItem(row: DevRow, checked: bool) -> Element {
    let mut state = use_app_state();
    let toggle_path = row.path.clone();
    let menu_path = row.path.clone();
    let toggle = move || {
        let p = toggle_path.clone();
        state.dev_checked.with_mut(|c| {
            if !c.remove(&p) {
                c.insert(p);
            }
        });
    };
    let mut toggle_row = toggle.clone();
    let mut toggle_box = toggle;
    let badge = format!("badge {}", row.language.css_class());
    let size_class = match row.size {
        s if s >= 10_000_000_000 => "size huge",
        s if s >= 1_000_000_000 => "size large",
        s if s >= 100_000_000 => "size medium",
        _ => "size",
    };
    rsx! {
        div {
            class: if checked { "row dev-row checked" } else { "row dev-row" },
            onclick: move |_| toggle_row(),
            oncontextmenu: move |e| {
                e.prevent_default();
                show_context_menu(state, menu_path.clone());
            },
            input {
                r#type: "checkbox",
                checked,
                onclick: move |e| {
                    e.stop_propagation();
                    toggle_box();
                },
            }
            img { class: "icon", src: "{icon_url(&row.location)}?size=64", width: "28", height: "28", loading: "lazy" }
            div { class: "label",
                span { class: "name", "{row.title}" }
                span { class: "detail", title: "{row.path.display()}", "{short_path(&row.location)}" }
            }
            span { class: badge, "{row.language.label()}" }
            span { class: "dev-what", "{row.what}" }
            span { class: "dev-age", "{age(&row)}" }
            span { class: size_class, "{format::bytes(row.size)}" }
        }
    }
}

#[component]
pub fn DevToolbar() -> Element {
    let mut state = use_app_state();
    let filter = (state.dev_filter)();
    let roots: Vec<String> = {
        let sessions = state.sessions.read();
        let mut names: Vec<String> = sessions
            .values()
            .map(|s| s.tree.read().display_name(crate::tree::ROOT).into_owned())
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        names
    };
    let subtitle = if roots.is_empty() {
        "no scans yet".to_string()
    } else {
        format!("from scans of {}", roots.join(", "))
    };
    let segment = move |label: &'static str, value: Option<Language>| {
        rsx! {
            button {
                class: if filter == value { "selected" },
                onclick: move |_| state.dev_filter.set(value),
                "{label}"
            }
        }
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
                    {segment("All", None)}
                    for &language in Language::ALL {
                        {segment(language.label(), Some(language))}
                    }
                }
            }
            div { class: "toolbar-title",
                div { class: "title", "Developer" }
                div { class: "subtitle", "{subtitle}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Kind, ROOT, Tree};
    use std::time::SystemTime;

    fn session(root: &str, scanned_at: u64, build: impl FnOnce(&mut Tree)) -> Arc<Session> {
        let s = Session::new(Path::new(root), "x");
        let mut tree = Tree::new(root.into(), "x", 0);
        build(&mut tree);
        *s.tree.write() = tree;
        *s.scanned_at.lock() = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(scanned_at));
        Arc::new(s)
    }

    fn rust_project(t: &mut Tree, parent: crate::tree::NodeId, name: &str, size: u64) {
        let p = t.add(parent, name.as_bytes(), Kind::Dir, 0, 0, 0);
        t.add(p, b"Cargo.toml", Kind::File, 0, 1, 0);
        let target = t.add(p, b"target", Kind::Dir, 0, 0, 0);
        t.add(target, b"blob", Kind::File, 0, size, 7);
    }

    #[test]
    fn overlapping_scans_list_each_folder_once_with_the_newest_numbers() {
        // A volume scan and a folder scan can both contain a project; it must appear once,
        // and with the size from the more recent scan.
        let old = session("/Users/me", 100, |t| {
            let work = t.add(ROOT, b"work", Kind::Dir, 0, 0, 0);
            rust_project(t, work, "app", 5_000);
        });
        let new = session("/Users/me/work", 200, |t| rust_project(t, ROOT, "app", 9_000));
        let rows = collect(vec![old, new], &Locations::defaults(Path::new("/nonexistent-home")));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, PathBuf::from("/Users/me/work/app/target"));
        assert_eq!((rows[0].size, rows[0].newest), (9_000, 7));
        assert_eq!(rows[0].title, "app");
    }

    #[test]
    fn rows_are_sorted_largest_first() {
        let s = session("/w", 1, |t| {
            rust_project(t, ROOT, "small", 10);
            rust_project(t, ROOT, "big", 1_000);
        });
        let rows = collect(vec![s], &Locations::defaults(Path::new("/nonexistent-home")));
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["big", "small"]);
    }

    #[test]
    fn scans_that_are_still_loading_are_skipped() {
        let s = session("/w", 1, |t| rust_project(t, ROOT, "app", 10));
        *s.busy.lock() = Busy::Loading;
        assert!(collect(vec![s], &Locations::defaults(Path::new("/nonexistent-home"))).is_empty());
    }
}
