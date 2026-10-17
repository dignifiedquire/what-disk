use dioxus::prelude::*;

use super::format;
use super::icons::{Icon, Symbol};
use super::state::{AppState, Busy, Mode, use_app_state};

/// Rescans what the user is looking at: the whole scan at its root, otherwise the folder.
pub fn rescan_current(state: AppState) {
    let selected = state.selected.peek().clone();
    match state.resolve(&selected) {
        Some((session, id)) => state.rescan_folder(session, id),
        None => state.start_scan(&selected),
    }
}

#[component]
pub fn Toolbar() -> Element {
    let mut state = use_app_state();
    let _ = (state.rev)();
    let selected = (state.selected)();
    let resolved = state.resolve(&selected);
    let location = state.location(&selected);
    let title = location.as_ref().map_or_else(
        || crate::volumes::display_name(&selected),
        |l| l.name.clone(),
    );
    let mut subtitle = Vec::new();
    if let Some(v) = state.volume_of(&selected) {
        subtitle.push(format!(
            "{} available of {}",
            format::bytes(v.available),
            format::bytes(v.capacity)
        ));
    }
    let busy = resolved.as_ref().map(|(s, _)| s.busy());
    if let Some((session, _)) = &resolved
        && busy == Some(Busy::Idle)
    {
        match *session.scanned_at.lock() {
            Some(at) => subtitle.push(format!("scanned {}", format::ago(at))),
            None => subtitle.push("scan stopped, results incomplete".into()),
        }
    }
    let subtitle = subtitle.join(" · ");
    let view = state.view.read();
    let mode = view.mode;
    let query = view.query.clone();
    drop(view);

    rsx! {
        header {
            class: "toolbar",
            onmousedown: move |_| dioxus::desktop::window().drag(),
            ondoubleclick: move |_| dioxus::desktop::window().toggle_maximized(),
            if resolved.is_some() {
                div {
                    class: "toolbar-controls",
                    onmousedown: move |e| e.stop_propagation(),
                    ondoubleclick: move |e| e.stop_propagation(),
                    label { class: "search-field",
                        Symbol { icon: Icon::Search, size: 14 }
                        input {
                            id: "search",
                            r#type: "text",
                            placeholder: "Search",
                            spellcheck: false,
                            autocomplete: "off",
                            value: "{query}",
                            oninput: move |e| state.view.with_mut(|v| v.query = e.value()),
                            onkeydown: move |e| {
                                if e.key() == Key::Escape {
                                    state.view.with_mut(|v| v.query.clear());
                                }
                            },
                        }
                        if !query.is_empty() {
                            button {
                                class: "search-clear",
                                onclick: move |_| state.view.with_mut(|v| v.query.clear()),
                                Symbol { icon: Icon::Clear, size: 14 }
                            }
                        }
                    }
                    div { class: "segmented",
                        button {
                            class: if mode == Mode::Outline { "selected" },
                            onclick: move |_| state.view.with_mut(|v| v.mode = Mode::Outline),
                            "Folders Outline"
                        }
                        button {
                            class: if mode == Mode::Biggest { "selected" },
                            onclick: move |_| state.view.with_mut(|v| v.mode = Mode::Biggest),
                            "Biggest Files"
                        }
                    }
                }
            }
            div { class: "toolbar-title",
                div { class: "title", "{title}" }
                if !subtitle.is_empty() {
                    div { class: "subtitle", "{subtitle}" }
                }
            }
            div {
                class: "toolbar-controls end",
                onmousedown: move |e| e.stop_propagation(),
                ondoubleclick: move |e| e.stop_propagation(),
                if let Some((session, _)) = resolved.clone() {
                    if matches!(busy, Some(Busy::Scanning | Busy::Rescanning(_))) {
                        button {
                            class: "toolbar-button",
                            title: "Stop Scanning",
                            onclick: move |_| state.stop(&session),
                            Symbol { icon: Icon::Stop }
                        }
                    } else {
                        button {
                            class: "toolbar-button",
                            title: "Rescan (⌘R)",
                            disabled: busy != Some(Busy::Idle),
                            onclick: move |_| rescan_current(state),
                            Symbol { icon: Icon::Rescan }
                        }
                    }
                }
            }
        }
    }
}
