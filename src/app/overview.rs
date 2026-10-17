use std::path::PathBuf;

use dioxus::prelude::*;

use super::state::{LocationKind, use_app_state};
use super::{format, icon_url};

#[component]
pub fn Overview(path: PathBuf) -> Element {
    let state = use_app_state();
    let location = state.location(&path);
    let name = location
        .as_ref()
        .map_or_else(|| crate::volumes::display_name(&path), |l| l.name.clone());
    let mut facts: Vec<(&str, String)> = Vec::new();
    match location.as_ref().map(|l| &l.kind) {
        Some(LocationKind::Volume(v)) => {
            facts.push(("Kind", "Volume".into()));
            facts.push(("Capacity", format::bytes(v.capacity)));
            facts.push(("Available", format::bytes(v.available)));
            facts.push(("Format", v.fs_type.to_uppercase()));
        }
        _ => {
            facts.push(("Kind", "Folder".into()));
            facts.push(("Where", path.parent().map_or_else(String::new, |p| p.display().to_string())));
        }
    }
    let exists = path.exists();
    let scan_path = path.clone();

    rsx! {
        div { class: "overview",
            div { class: "overview-card",
                img { class: "overview-icon", src: "{icon_url(&path)}?size=128", width: "96", height: "96" }
                div { class: "overview-body",
                    h1 { "{name}" }
                    dl {
                        for (k, v) in facts {
                            dt { "{k}" }
                            dd { "{v}" }
                        }
                    }
                }
            }
            if exists {
                p { class: "overview-hint", "Scan to see which folders and files take up space." }
                button {
                    class: "btn primary large",
                    onclick: move |_| state.start_scan(&scan_path),
                    "Start Scanning"
                }
            } else {
                p { class: "overview-hint", "This folder does not exist." }
            }
        }
    }
}
