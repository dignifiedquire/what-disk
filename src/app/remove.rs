//! The bar below the browser and the sheet that removes checked items.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use dioxus::prelude::*;

use super::browser::run_removal;
use super::state::{Busy, effective_checked, is_protected, use_app_state};
use super::{SessionRef, format, icon_url};
use crate::tree::{NodeId, ROOT};

#[component]
pub fn BottomBar(session: SessionRef, root: NodeId) -> Element {
    let mut state = use_app_state();
    let _ = (state.rev)();
    let busy = session.busy();
    let tree = session.tree.read();
    let checked = effective_checked(&tree, &state.view.read().checked);
    let checked_size: u64 = checked.iter().map(|&id| tree.size(id)).sum();
    let scanned = tree.size(ROOT);
    let rescanning_name = match busy {
        Busy::Rescanning(id) => tree.display_name(id).into_owned(),
        _ => String::new(),
    };
    drop(tree);
    let items = session.progress.files.load(Ordering::Relaxed) + session.progress.dirs.load(Ordering::Relaxed);
    let used = state.volume_of(&session.root).map(|v| v.used());

    let status = match busy {
        Busy::Scanning => {
            let percent = used.filter(|&u| u > 0).map(|u| (scanned as f64 / u as f64 * 100.0).min(99.0));
            rsx! {
                div { class: "progress",
                    div {
                        class: if percent.is_some() { "progress-track" } else { "progress-track indeterminate" },
                        div { class: "progress-fill", style: "width: {percent.unwrap_or(30.0)}%" }
                    }
                    span { class: "status-text",
                        "Scanning… {format::count(items)} items · {format::bytes(scanned)}"
                        if let Some(u) = used {
                            " of {format::bytes(u)} used"
                        }
                    }
                }
            }
        }
        Busy::Rescanning(_) => rsx! {
            span { class: "spinner" }
            span { class: "status-text", "Rescanning “{rescanning_name}”…" }
        },
        Busy::Loading => rsx! { span { class: "status-text", "Loading…" } },
        Busy::Idle if !checked.is_empty() => rsx! {
            span { class: "status-text",
                strong { "{checked.len()} " if checked.len() == 1 { "item" } else { "items" } }
                " selected · {format::bytes(checked_size)}"
            }
            button {
                class: "btn plain",
                onclick: move |_| state.view.with_mut(|v| v.checked.clear()),
                "Clear Selection"
            }
        },
        Busy::Idle => rsx! {
            span { class: "status-text secondary", "Check items to review and remove them." }
        },
    };

    rsx! {
        footer { class: "bottombar",
            div { class: "bottombar-status", {status} }
            button {
                class: "btn primary",
                disabled: checked.is_empty() || busy != Busy::Idle,
                onclick: move |_| state.review_open.set(true),
                "Review and Remove…"
            }
        }
    }
}

#[derive(Clone, PartialEq)]
enum Phase {
    Review,
    Working,
    Failed(Vec<(PathBuf, String)>),
}

#[component]
pub fn ReviewSheet() -> Element {
    let mut state = use_app_state();
    let mut phase = use_signal(|| Phase::Review);
    let selected = state.selected.peek().clone();
    let Some((session, _)) = state.resolve(&selected) else {
        state.review_open.set(false);
        return rsx! {};
    };
    let tree = session.tree.read();
    let items: Vec<(NodeId, String, PathBuf, u64)> = effective_checked(&tree, &state.view.read().checked)
        .into_iter()
        .filter(|&id| !is_protected(&tree, id))
        .map(|id| (id, tree.display_name(id).into_owned(), tree.path(id), tree.size(id)))
        .collect();
    drop(tree);
    let total: u64 = items.iter().map(|i| i.3).sum();
    let targets: Vec<(NodeId, PathBuf)> = items.iter().map(|i| (i.0, i.2.clone())).collect();
    let working = phase() == Phase::Working;

    let remove = move |to_trash: bool| {
        let session = session.clone();
        let targets = targets.clone();
        async move {
            if !to_trash {
                let n = targets.len();
                let confirmed = rfd::AsyncMessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title(format!(
                        "Delete {n} {} immediately?",
                        if n == 1 { "item" } else { "items" }
                    ))
                    .set_description("These items will be deleted immediately. You can’t undo this action.")
                    .set_buttons(rfd::MessageButtons::OkCancelCustom("Delete".into(), "Cancel".into()))
                    .show()
                    .await;
                if !matches!(confirmed, rfd::MessageDialogResult::Ok)
                    && confirmed != rfd::MessageDialogResult::Custom("Delete".into())
                {
                    return;
                }
            }
            phase.set(Phase::Working);
            let failed = run_removal(state, session, targets, to_trash).await;
            if failed.is_empty() {
                state.review_open.set(false);
            } else {
                phase.set(Phase::Failed(failed));
            }
        }
    };
    let trash = remove.clone();

    rsx! {
        div {
            class: "sheet-backdrop",
            onkeydown: move |e| {
                if e.key() == Key::Escape && !working {
                    state.review_open.set(false);
                }
            },
            div { class: "sheet", tabindex: "-1",
                h2 { "Review and Remove" }
                {match phase() {
                    Phase::Failed(failures) => rsx! {
                        p { "Some items could not be removed:" }
                        ul { class: "sheet-list",
                            for (path, err) in failures {
                                li { class: "sheet-item",
                                    img { src: "{icon_url(&path)}", width: "16", height: "16" }
                                    div { class: "label",
                                        span { class: "name", "{path.display()}" }
                                        span { class: "detail error", "{err}" }
                                    }
                                }
                            }
                        }
                        div { class: "sheet-buttons",
                            button { class: "btn primary", onclick: move |_| state.review_open.set(false), "Close" }
                        }
                    },
                    _ => rsx! {
                        p { class: "secondary",
                            "Moving to the Trash frees the space once the Trash is emptied."
                        }
                        ul { class: "sheet-list",
                            for (id, name, path, size) in items.clone() {
                                li { class: "sheet-item", key: "{id}",
                                    img { src: "{icon_url(&path)}", width: "16", height: "16" }
                                    div { class: "label",
                                        span { class: "name", "{name}" }
                                        span { class: "detail", "{path.parent().map(|p| p.display().to_string()).unwrap_or_default()}" }
                                    }
                                    span { class: "size", "{format::bytes(size)}" }
                                }
                            }
                        }
                        div { class: "sheet-total",
                            span { "{items.len()} " if items.len() == 1 { "item" } else { "items" } }
                            strong { "{format::bytes(total)}" }
                        }
                        div { class: "sheet-buttons",
                            button {
                                class: "btn destructive",
                                disabled: working || items.is_empty(),
                                onclick: move |_| { spawn(remove(false)); },
                                "Delete Immediately…"
                            }
                            span { class: "spacer" }
                            if working {
                                span { class: "spinner" }
                            }
                            button {
                                class: "btn",
                                disabled: working,
                                onclick: move |_| state.review_open.set(false),
                                "Cancel"
                            }
                            button {
                                class: "btn primary",
                                autofocus: true,
                                disabled: working || items.is_empty(),
                                onclick: move |_| { spawn(trash(true)); },
                                "Move to Trash"
                            }
                        }
                    },
                }}
            }
        }
    }
}
