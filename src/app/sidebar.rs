use dioxus::prelude::*;

use super::format;
use super::icons::{Icon, Symbol};
use super::state::{Busy, Location, LocationKind, Overview, use_app_state};
use super::{choose_folder, select_location};

fn icon_for(location: &Location) -> Icon {
    match &location.kind {
        LocationKind::Home => Icon::Home,
        LocationKind::Volume(v) if v.mount.as_os_str() == "/" => Icon::Drive,
        LocationKind::Volume(_) => Icon::ExternalDrive,
        LocationKind::Folder => match location.path.file_name().and_then(|n| n.to_str()) {
            Some("Desktop") => Icon::Desktop,
            Some("Documents") => Icon::Documents,
            Some("Downloads") => Icon::Downloads,
            _ => Icon::Folder,
        },
    }
}

#[component]
pub fn Sidebar() -> Element {
    let state = use_app_state();
    let locations = state.locations.read().clone();
    let home: Vec<&Location> = locations
        .iter()
        .filter(|l| l.kind == LocationKind::Home)
        .collect();
    let volumes: Vec<&Location> = locations
        .iter()
        .filter(|l| matches!(l.kind, LocationKind::Volume(_)))
        .collect();
    let folders: Vec<&Location> = locations
        .iter()
        .filter(|l| l.kind == LocationKind::Folder)
        .collect();

    rsx! {
        nav { class: "sidebar",
            div { class: "sidebar-scroll",
                for l in home {
                    SidebarItem { location: l.clone() }
                }
                div { class: "sidebar-heading", "Storage" }
                for l in volumes {
                    SidebarItem { location: l.clone() }
                }
                div { class: "sidebar-heading", "Overviews" }
                OverviewItem { overview: Overview::Apps, icon: Icon::Apps, label: "Apps" }
                OverviewItem { overview: Overview::Projects, icon: Icon::Code, label: "Projects & Toolchains" }
                div { class: "sidebar-heading", "Folders" }
                for l in folders {
                    SidebarItem { location: l.clone() }
                }
                button {
                    class: "sidebar-item sidebar-add",
                    onclick: move |_| {
                        spawn(choose_folder(state));
                    },
                    Symbol { icon: Icon::Add }
                    span { class: "sidebar-name", "Choose Folder…" }
                }
            }
        }
    }
}

#[component]
fn OverviewItem(overview: Overview, icon: Icon, label: &'static str) -> Element {
    let mut state = use_app_state();
    let selected = (state.overview)() == Some(overview);
    rsx! {
        button {
            class: if selected { "sidebar-item selected" } else { "sidebar-item" },
            onclick: move |_| state.overview.set(Some(overview)),
            Symbol { icon }
            span { class: "sidebar-name", "{label}" }
        }
    }
}

#[component]
fn SidebarItem(location: Location) -> Element {
    let state = use_app_state();
    let _ = (state.rev)();
    let selected = *state.selected.read() == location.path && (state.overview)().is_none();
    let scanning = state
        .sessions
        .read()
        .get(&location.path)
        .is_some_and(|s| matches!(s.busy(), Busy::Scanning | Busy::Loading));
    let size = state.summaries.read().get(&location.path).map(|s| s.total);
    let icon = icon_for(&location);
    let path = location.path.clone();

    rsx! {
        button {
            class: if selected { "sidebar-item selected" } else { "sidebar-item" },
            title: "{location.path.display()}",
            onclick: move |_| select_location(state, path.clone()),
            Symbol { icon }
            span { class: "sidebar-name", "{location.name}" }
            if scanning {
                span { class: "spinner" }
            } else if let Some(size) = size {
                span { class: "sidebar-size", "{format::bytes(size)}" }
            }
        }
    }
}
