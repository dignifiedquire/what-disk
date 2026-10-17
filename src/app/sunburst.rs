//! Sunburst chart: the center is a folder, each ring one level deeper.

use std::f64::consts::TAU;

use dioxus::prelude::*;

use super::state::{Busy, use_app_state};
use super::{SessionRef, format};
use crate::tree::{NodeId, ROOT, Tree};

const RINGS: usize = 4;
const INNER: f64 = 30.0;
const OUTER: f64 = 97.0;
/// Segments narrower than this (radians) are not drawn; their space stays empty.
const MIN_ANGLE: f64 = 0.006;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Segment {
    pub id: NodeId,
    pub depth: usize,
    pub start: f64,
    pub end: f64,
    pub is_dir: bool,
}

/// Lays out the subtree of `center` over `total` bytes (at least the size of `center`).
pub fn layout(tree: &Tree, center: NodeId, total: u64) -> Vec<Segment> {
    let mut out = Vec::new();
    if total > 0 {
        place(tree, center, 0.0, TAU * tree.size(center) as f64 / total as f64, 1, &mut out);
    }
    out
}

fn place(tree: &Tree, parent: NodeId, start: f64, span: f64, depth: usize, out: &mut Vec<Segment>) {
    let parent_size = tree.size(parent);
    if parent_size == 0 || depth > RINGS {
        return;
    }
    // Only children wide enough to draw can appear. Pick them before sorting: a folder can
    // hold hundreds of thousands of entries, and sorting all of them on every redraw blocked
    // the interface.
    let min_size = (MIN_ANGLE / span * parent_size as f64) as u64;
    let mut kids: Vec<NodeId> = tree.children(parent).filter(|&c| tree.size(c) > min_size).collect();
    kids.sort_unstable_by(|&a, &b| tree.size(b).cmp(&tree.size(a)).then(a.cmp(&b)));
    let mut angle = start;
    for child in kids {
        let child_span = span * tree.size(child) as f64 / parent_size as f64;
        if child_span < MIN_ANGLE {
            break;
        }
        let is_dir = tree.node(child).is_dir();
        out.push(Segment { id: child, depth, start: angle, end: angle + child_span, is_dir });
        if is_dir {
            place(tree, child, angle, child_span, depth + 1, out);
        }
        angle += child_span;
    }
}

fn ring(depth: usize) -> (f64, f64) {
    let w = (OUTER - INNER) / RINGS as f64;
    (INNER + w * (depth - 1) as f64, INNER + w * depth as f64)
}

fn polar(r: f64, a: f64) -> (f64, f64) {
    (r * a.sin(), -r * a.cos())
}

/// Ring segment from angle `a0` to `a1`. Each edge is drawn as two half arcs: SVG drops an
/// arc whose end equals its start, so a single arc could not draw a full ring.
fn arc_path(r0: f64, r1: f64, a0: f64, a1: f64) -> String {
    let mid = (a0 + a1) / 2.0;
    let pt = |r: f64, a: f64| {
        let (x, y) = polar(r, a);
        format!("{x:.3} {y:.3}")
    };
    format!(
        "M{}A{r1:.3} {r1:.3} 0 0 1 {}A{r1:.3} {r1:.3} 0 0 1 {}L{}A{r0:.3} {r0:.3} 0 0 0 {}A{r0:.3} {r0:.3} 0 0 0 {}Z",
        pt(r1, a0),
        pt(r1, mid),
        pt(r1, a1),
        pt(r0, a1),
        pt(r0, mid),
        pt(r0, a0),
    )
}

fn hue(segment: &Segment) -> u32 {
    (((segment.start + segment.end) / 2.0 / TAU) * 330.0) as u32
}

#[derive(Clone, PartialEq)]
struct Hover {
    /// `None` for the unaccounted segment, whose size is given in `size`.
    id: Option<NodeId>,
    name: String,
    size: u64,
    modified: u32,
    x: f64,
    y: f64,
}

#[component]
pub fn Sunburst(
    session: SessionRef,
    view_root: NodeId,
    center: NodeId,
    highlighted: Option<NodeId>,
) -> Element {
    let mut state = use_app_state();
    // Components that read a tree must re-render when background work changes it.
    let _ = (state.rev)();
    let mut hover = use_signal(|| None::<Hover>);
    let tree = session.tree.read();
    let complete = session.busy() == Busy::Idle && session.scanned_at.lock().is_some();
    let unaccounted = (center == ROOT && complete)
        .then(|| state.volume_of(&session.root))
        .flatten()
        .map_or(0, |v| v.used().saturating_sub(tree.size(ROOT)));
    let total = tree.size(center) + unaccounted;
    let segments = layout(&tree, center, total);
    let highlight_path = highlighted.map(|h| tree.ancestry(h)).unwrap_or_default();
    let center_name = tree.display_name(center).into_owned();
    let center_size = tree.size(center);
    let parent = tree.node(center).parent;
    let names: Vec<(String, u32)> = segments
        .iter()
        .map(|s| (tree.display_name(s.id).into_owned(), tree.node(s.id).mtime))
        .collect();
    drop(tree);
    let window_width = {
        let w = dioxus::desktop::window();
        let size = w.window.inner_size();
        size.width as f64 / w.window.scale_factor()
    };

    let select_session = session.clone();
    let select = use_callback(move |id: NodeId| {
        let sel = {
            let tree = select_session.tree.read();
            let chain = tree.ancestry(id);
            let start = chain.iter().position(|&a| a == view_root).map_or(chain.len(), |p| p + 1);
            chain[start..].to_vec()
        };
        state.view.with_mut(|v| v.selection = sel);
    });
    let can_go_up = center != view_root;

    rsx! {
        svg {
            class: "sunburst",
            view_box: "-100 -100 200 200",
            onmouseleave: move |_| hover.set(None),
            for (s, (name, mtime)) in segments.iter().copied().zip(names) {
                path {
                    key: "{s.id}",
                    class: format!(
                        "arc d{}{}{}",
                        s.depth,
                        if s.is_dir { "" } else { " file" },
                        if highlight_path.contains(&s.id) { " active" } else { "" },
                    ),
                    style: "--h: {hue(&s)}",
                    d: arc_path(ring(s.depth).0, ring(s.depth).1, s.start, s.end),
                    onclick: move |_| select.call(s.id),
                    onmousemove: {
                        let name = name.clone();
                        move |e: MouseEvent| {
                            let p = e.client_coordinates();
                            hover.set(Some(Hover { id: Some(s.id), name: name.clone(), size: 0, modified: mtime, x: p.x, y: p.y }));
                        }
                    },
                }
            }
            if unaccounted > 0 {
                path {
                    class: "arc unaccounted",
                    d: arc_path(ring(1).0, ring(1).1, TAU * center_size as f64 / total as f64, TAU),
                    onmousemove: move |e: MouseEvent| {
                        let p = e.client_coordinates();
                        hover.set(Some(Hover { id: None, name: "Unaccounted".into(), size: unaccounted, modified: 0, x: p.x, y: p.y }));
                    },
                }
            }
            g {
                class: if can_go_up { "center up" } else { "center" },
                onclick: move |_| {
                    if can_go_up {
                        select.call(parent);
                    }
                },
                circle { r: INNER - 1.5 }
                text { class: "center-name", y: "-1", "{ellipsize(&center_name, 16)}" }
                text { class: "center-size", y: "9", "{format::bytes(center_size)}" }
            }
        }
        if let Some(h) = hover() {
            HoverCard { hover: h.clone(), session: session.clone(), window_width }
        }
    }
}

#[component]
fn HoverCard(hover: Hover, session: SessionRef, window_width: f64) -> Element {
    let _ = (use_app_state().rev)();
    // Look the size up at render time so it stays live during a scan.
    let size = hover.id.map_or(hover.size, |id| session.tree.read().size(id));
    let left = if hover.x + 260.0 > window_width { hover.x - 250.0 } else { hover.x + 14.0 };
    rsx! {
        div { class: "hover-card", style: "left: {left}px; top: {hover.y + 14.0}px",
            div { class: "hover-name", "{hover.name}" }
            dl {
                dt { "Size" }
                dd { "{format::bytes(size)}" }
                if hover.modified > 0 {
                    dt { "Modified" }
                    dd { "{format::date(hover.modified)}" }
                }
            }
        }
    }
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Kind;

    #[test]
    fn segment_angles_are_proportional_to_size() {
        // The chart is only useful if a segment's share of the circle is its share of the
        // space.
        let mut t = Tree::new("/r".into(), "r", 0);
        let a = t.add(ROOT, b"a", Kind::Dir, 0, 0, 0);
        t.add(a, b"x", Kind::File, 0, 300, 0);
        t.add(ROOT, b"b", Kind::File, 0, 100, 0);
        let segs = layout(&t, ROOT, t.size(ROOT));
        let span = |name: &[u8]| {
            let s = segs.iter().find(|s| t.name(s.id).as_encoded_bytes() == name).unwrap();
            (s.end - s.start, s.depth)
        };
        let (a_span, a_depth) = span(b"a");
        assert!((a_span - TAU * 0.75).abs() < 1e-9 && a_depth == 1);
        assert!((span(b"b").0 - TAU * 0.25).abs() < 1e-9);
        assert_eq!(span(b"x").1, 2);
    }

    #[test]
    fn unaccounted_space_shrinks_the_scanned_part_of_the_circle() {
        let mut t = Tree::new("/r".into(), "r", 0);
        t.add(ROOT, b"b", Kind::File, 0, 100, 0);
        let segs = layout(&t, ROOT, 400);
        assert!((segs[0].end - TAU * 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_child_filling_its_whole_folder_draws_a_visible_ring() {
        // A folder with a single child is common; its segment is a full circle and must
        // not collapse to nothing because an arc starts and ends at the same point.
        let path = arc_path(30.0, 46.75, 0.0, TAU);
        let mut current = (f64::NAN, f64::NAN);
        for cmd in path.split(['M', 'L', 'A', 'Z']).filter(|c| !c.is_empty()) {
            let nums: Vec<f64> = cmd.split(' ').map(|n| n.parse().unwrap()).collect();
            let end = (nums[nums.len() - 2], nums[nums.len() - 1]);
            if nums.len() == 7 {
                let moved = (end.0 - current.0).hypot(end.1 - current.1);
                assert!(moved > 1.0, "zero-length arc in {path}");
            }
            current = end;
        }
    }

    #[test]
    fn tiny_items_are_not_drawn() {
        let mut t = Tree::new("/r".into(), "r", 0);
        t.add(ROOT, b"big", Kind::File, 0, 1_000_000, 0);
        t.add(ROOT, b"tiny", Kind::File, 0, 1, 0);
        assert_eq!(layout(&t, ROOT, t.size(ROOT)).len(), 1);
    }
}
