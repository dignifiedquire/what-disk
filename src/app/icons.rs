//! Line icons in the style of SF Symbols, drawn with `currentColor`.

use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Home,
    Drive,
    ExternalDrive,
    Folder,
    Desktop,
    Documents,
    Downloads,
    Add,
    Search,
    Rescan,
    Chevron,
    Clear,
    Lock,
    Stop,
    Code,
    Apps,
}

impl Icon {
    fn path(self) -> &'static str {
        match self {
            Icon::Home => "M3 8.5 10 3l7 5.5V16a1 1 0 0 1-1 1h-3.5v-5h-5v5H4a1 1 0 0 1-1-1z",
            Icon::Drive => {
                "M3 11.5 5 5h10l2 6.5M3 11.5V15a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-3.5M3 11.5h14M13.5 14h.01"
            }
            Icon::ExternalDrive => {
                "M4 4h12a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM3 12h14M13.5 14h.01"
            }
            Icon::Folder => "M2.5 5.5a1 1 0 0 1 1-1h4l1.5 1.5h7.5a1 1 0 0 1 1 1V15a1 1 0 0 1-1 1h-13a1 1 0 0 1-1-1z",
            Icon::Desktop => "M3 4h14a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zM7.5 17h5M10 14v3",
            Icon::Documents => "M5 2.5h6.5L15 6v10.5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-13a1 1 0 0 1 1-1zM11.5 2.5V6H15M7 10h6M7 13h6",
            Icon::Downloads => "M10 2.5a7.5 7.5 0 1 0 0 15 7.5 7.5 0 0 0 0-15zM10 6v7M7 10.5l3 3 3-3",
            Icon::Add => "M10 2.5a7.5 7.5 0 1 0 0 15 7.5 7.5 0 0 0 0-15zM10 6.5v7M6.5 10h7",
            Icon::Search => "M8.5 3a5.5 5.5 0 1 0 0 11 5.5 5.5 0 0 0 0-11zM12.5 12.5 17 17",
            Icon::Rescan => "M16 10a6 6 0 1 1-1.8-4.3M16 3v3.5h-3.5",
            Icon::Chevron => "M7.5 4.5 13 10l-5.5 5.5",
            Icon::Clear => "M10 2.5a7.5 7.5 0 1 0 0 15 7.5 7.5 0 0 0 0-15zM7.3 7.3l5.4 5.4M12.7 7.3l-5.4 5.4",
            Icon::Lock => "M5 9h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-6a1 1 0 0 1 1-1zM7 9V6.5a3 3 0 0 1 6 0V9",
            Icon::Stop => "M10 2.5a7.5 7.5 0 1 0 0 15 7.5 7.5 0 0 0 0-15zM7.5 7.5h5v5h-5z",
            Icon::Code => "M6.5 5.5 2 10l4.5 4.5M13.5 5.5 18 10l-4.5 4.5M11.5 3.5l-3 13",
            Icon::Apps => "M3 3h5.5v5.5H3zM11.5 3H17v5.5h-5.5zM3 11.5h5.5V17H3zM11.5 11.5H17V17h-5.5z",
        }
    }
}

#[component]
pub fn Symbol(icon: Icon, #[props(default = 16)] size: u32) -> Element {
    rsx! {
        svg {
            class: "symbol",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 20 20",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.5",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: icon.path() }
        }
    }
}
