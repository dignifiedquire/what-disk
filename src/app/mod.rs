//! The Dioxus user interface.

mod browser;
mod format;
mod icons;
mod overview;
mod remove;
mod sidebar;
mod state;
mod sunburst;
mod toolbar;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dioxus::desktop::muda::{
    AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu,
    accelerator::{Accelerator, Code, Modifiers},
};
use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;
use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::*;

use crate::{cache, ops};
use state::{AppState, Busy, Mode, View, default_locations, home_dir};

pub use state::Session;

const STYLE: &str = include_str!("../../assets/style.css");
pub const APP_NAME: &str = "What Disk";

pub fn launch() {
    let window = WindowBuilder::new()
        .with_title(APP_NAME)
        .with_inner_size(LogicalSize::new(1180.0, 760.0))
        .with_min_inner_size(LogicalSize::new(860.0, 520.0))
        .with_transparent(true)
        .with_titlebar_transparent(true)
        .with_title_hidden(true)
        .with_fullsize_content_view(true);
    let config = Config::new()
        .with_window(window)
        .with_menu(menu_bar())
        .with_background_color((0, 0, 0, 0))
        .with_disable_context_menu(!cfg!(debug_assertions))
        .with_custom_head(format!("<style>{STYLE}</style>"));
    dioxus::LaunchBuilder::desktop().with_cfg(config).launch(App);
}

/// Window chrome that has to be set up after wry has installed the webview.
///
/// The window is created transparent only so that wry makes the webview transparent. The
/// window itself is made opaque again, so AppKit keeps its rounded corner mask, and the
/// sidebar material is placed below the webview. Doing this earlier does not work: wry
/// replaces the window's content view when it creates the webview.
fn native_chrome() {
    use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
    use objc2_app_kit::{NSColor, NSWindow};
    use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState, apply_vibrancy};

    let desktop = dioxus::desktop::window();
    let window = &desktop.window;
    // SAFETY: tao returns the NSWindow it owns; we are on the main thread and the window
    // outlives this call.
    let ns_window = unsafe { &*(window.ns_window() as *const NSWindow) };
    ns_window.setOpaque(true);
    ns_window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
    if let Err(e) = apply_vibrancy(
        &**window,
        NSVisualEffectMaterial::Sidebar,
        Some(NSVisualEffectState::FollowsWindowActiveState),
        None,
    ) {
        eprintln!("sidebar material unavailable: {e}");
    }
    place_traffic_lights();
}

/// Centers the window buttons in the 52pt toolbar. tao's own inset is applied from its
/// content view's `drawRect:`, which never runs once wry has replaced that view, and AppKit
/// resets the layout on resize, so this is called again on window events.
fn place_traffic_lights() {
    use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
    use objc2_app_kit::{NSWindow, NSWindowButton, NSWindowStyleMask};
    const X: f64 = 20.0;
    const CENTER_Y: f64 = 26.0;

    let desktop = dioxus::desktop::window();
    // SAFETY: see `native_chrome`.
    let ns_window = unsafe { &*(desktop.window.ns_window() as *const NSWindow) };
    if ns_window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return;
    }
    let buttons = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ]
    .map(|b| ns_window.standardWindowButton(b));
    let [Some(close), Some(mini), Some(zoom)] = buttons else {
        return;
    };
    // SAFETY: walking up the view hierarchy of window buttons we hold strong references to.
    let Some(container) = (unsafe { close.superview().and_then(|v| v.superview()) }) else {
        return;
    };
    let button = close.frame();
    let spacing = mini.frame().origin.x - button.origin.x;
    // The container spans the top of the window; the buttons sit centered in it.
    let height = CENTER_Y * 2.0;
    let mut frame = container.frame();
    frame.size.height = height;
    frame.origin.y = ns_window.frame().size.height - height;
    container.setFrame(frame);
    for (i, b) in [close, mini, zoom].iter().enumerate() {
        let mut origin = b.frame().origin;
        origin.x = X + i as f64 * spacing;
        origin.y = (height - button.size.height) / 2.0;
        b.setFrameOrigin(origin);
    }
}

fn item(id: &str, label: &str, key: Option<Code>) -> MenuItem {
    let accel = key.map(|k| Accelerator::new(Some(Modifiers::SUPER), k));
    MenuItem::with_id(id, label, true, accel)
}

fn menu_bar() -> Menu {
    let about = AboutMetadata {
        name: Some(APP_NAME.into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        ..Default::default()
    };
    let app = Submenu::with_items(
        APP_NAME,
        true,
        &[
            &PredefinedMenuItem::about(None, Some(about)),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ],
    )
    .expect("app menu");
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &item("choose-folder", "Choose Folder…", Some(Code::KeyO)),
            &item("rescan", "Rescan", Some(Code::KeyR)),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::close_window(None),
        ],
    )
    .expect("file menu");
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
            &PredefinedMenuItem::separator(),
            &item("find", "Find", Some(Code::KeyF)),
        ],
    )
    .expect("edit menu");
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &item("mode-outline", "Folders Outline", Some(Code::Digit1)),
            &item("mode-biggest", "Biggest Files", Some(Code::Digit2)),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(None),
        ],
    )
    .expect("view menu");
    let window = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
        ],
    )
    .expect("window menu");
    Menu::with_items(&[&app, &file, &edit, &view, &window]).expect("menu bar")
}

/// Asks for a folder with the native open panel and selects it.
pub async fn choose_folder(state: AppState) {
    let Some(folder) = rfd::AsyncFileDialog::new()
        .set_directory(home_dir())
        .pick_folder()
        .await
    else {
        return;
    };
    open_folder(state, folder.path().to_path_buf());
}

/// Adds `path` to the sidebar if needed and selects it.
fn open_folder(mut state: AppState, path: PathBuf) {
    if state.location(&path).is_none() {
        state.locations.with_mut(|l| {
            l.push(state::Location {
                name: crate::volumes::display_name(&path),
                path: path.clone(),
                kind: state::LocationKind::Folder,
            })
        });
    }
    select_location(state, path);
}

pub fn select_location(mut state: AppState, path: PathBuf) {
    if *state.selected.peek() != path {
        state.selected.set(path.clone());
        state.view.set(View::default());
    }
    state.ensure_loaded(&path);
}

#[component]
fn App() -> Element {
    let summaries: HashMap<PathBuf, cache::Summary> = use_hook(|| {
        cache::list(&cache::dir())
            .into_iter()
            .map(|s| (s.root.clone(), s))
            .collect()
    });
    let locations = use_hook(|| default_locations(&summaries));
    let first = locations
        .iter()
        .find(|l| matches!(l.kind, state::LocationKind::Volume(_)))
        .map_or_else(home_dir, |l| l.path.clone());
    let state = use_context_provider(|| AppState {
        locations: Signal::new(locations),
        selected: Signal::new(first.clone()),
        sessions: Signal::new(HashMap::new()),
        summaries: Signal::new(summaries),
        rev: Signal::new(0),
        view: Signal::new(View::default()),
        review_open: Signal::new(false),
        context_target: Signal::new(None),
    });
    use_hook(native_chrome);
    #[cfg(debug_assertions)]
    use_debug_console(state);
    dioxus::desktop::use_wry_event_handler(|event, _| {
        use dioxus::desktop::tao::event::{Event, WindowEvent};
        if let Event::WindowEvent {
            event: WindowEvent::Resized(_) | WindowEvent::Focused(_) | WindowEvent::ScaleFactorChanged { .. },
            ..
        } = event
        {
            place_traffic_lights();
        }
    });
    use_hook(move || state.ensure_loaded(&first));

    // Background threads cannot touch signals, so poll their progress while they work.
    use_future(move || async move {
        let mut state = state;
        let mut last_versions = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let sessions: Vec<Arc<Session>> = state.sessions.peek().values().cloned().collect();
            let busy = sessions.iter().any(|s| s.busy() != Busy::Idle);
            let versions: u64 = sessions
                .iter()
                .map(|s| s.version.load(std::sync::atomic::Ordering::Relaxed))
                .sum();
            if busy || versions != last_versions {
                state.rev.with_mut(|r| *r += 1);
            }
            if versions != last_versions {
                last_versions = versions;
                state.refresh_disk_info();
            }
        }
    });

    dioxus::desktop::use_asset_handler("icon", move |request, responder| {
        let path = percent_decode(request.uri().path().trim_start_matches("/icon"));
        let pixels: isize = request
            .uri()
            .query()
            .and_then(|q| q.strip_prefix("size="))
            .and_then(|s| s.parse().ok())
            .unwrap_or(32)
            .clamp(16, 256);
        let png = ICONS.with(|cache| {
            cache
                .borrow_mut()
                .entry((path.clone(), pixels))
                .or_insert_with(|| ops::icon_png(std::path::Path::new(&path), pixels))
                .clone()
        });
        let response = match png {
            Some(bytes) => dioxus::desktop::wry::http::Response::builder()
                .header("Content-Type", "image/png")
                .header("Cache-Control", "max-age=3600")
                .body(bytes),
            None => dioxus::desktop::wry::http::Response::builder()
                .status(404)
                .body(Vec::new()),
        };
        responder.respond(response.expect("valid response"));
    });

    dioxus::desktop::use_muda_event_handler(move |event| {
        let mut state = state;
        match event.id.0.as_str() {
            "choose-folder" => {
                spawn(choose_folder(state));
            }
            "rescan" => toolbar::rescan_current(state),
            "find" => {
                document::eval("document.getElementById('search')?.focus()");
            }
            "mode-outline" => state.view.with_mut(|v| v.mode = Mode::Outline),
            "mode-biggest" => state.view.with_mut(|v| v.mode = Mode::Biggest),
            id if id.starts_with("ctx-") => browser::context_action(state, id),
            _ => {}
        }
    });

    rsx! {
        div { class: "window",
            sidebar::Sidebar {}
            main { class: "main",
                toolbar::Toolbar {}
                Content {}
            }
            if (state.review_open)() {
                remove::ReviewSheet {}
            }
        }
    }
}

/// Debug builds only: drives the UI from scripts, since this view cannot be scripted through
/// accessibility without extra permissions. Each line on stdin is evaluated as JavaScript in
/// the page and its result printed to stdout; `:open <path>` adds and selects a folder, which
/// otherwise needs the native open panel.
#[cfg(debug_assertions)]
fn use_debug_console(state: AppState) {
    use_future(move || async move {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        while let Some(line) = rx.recv().await {
            if let Some(path) = line.strip_prefix(":open ") {
                open_folder(state, PathBuf::from(path.trim()));
                println!("ok");
            } else {
                let result = document::eval(&line).await;
                println!("{result:?}");
            }
        }
    });
}

thread_local! {
    static ICONS: std::cell::RefCell<HashMap<(String, isize), Option<Vec<u8>>>> = Default::default();
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |i: usize| bytes.get(i).and_then(|&b| (b as char).to_digit(16));
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (hex(i + 1), hex(i + 2))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// URL under which the icon handler serves Finder's icon for `path`.
pub fn icon_url(path: &std::path::Path) -> String {
    let mut out = String::from("/icon");
    for b in path.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The main area below the toolbar for the selected location.
#[component]
fn Content() -> Element {
    let state = state::use_app_state();
    let _ = (state.rev)();
    let selected = (state.selected)();
    let resolved = state.resolve(&selected);
    let loading = state
        .sessions
        .read()
        .values()
        .any(|s| s.busy() == Busy::Loading && selected.starts_with(&s.root));
    match resolved {
        Some((session, root)) => rsx! {
            browser::Browser { session: SessionRef(session), root }
        },
        None if loading => rsx! {
            div { class: "placeholder", div { class: "spinner large" } }
        },
        None => rsx! {
            overview::Overview { path: selected }
        },
    }
}

/// `Arc<Session>` compared by identity, so it can be a component property.
#[derive(Clone)]
pub struct SessionRef(pub Arc<Session>);

impl PartialEq for SessionRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for SessionRef {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_urls_round_trip_any_file_name() {
        let path = std::path::Path::new("/Users/me/My Files/caf\u{e9} #1%.txt");
        let url = icon_url(path);
        assert!(!url.contains(' ') && !url.contains('#'));
        assert_eq!(percent_decode(url.trim_start_matches("/icon")), path.to_str().unwrap());
    }
}
