//! Disk usage caused by software development: build and dependency folders of projects,
//! and the global caches of the toolchains. Works on a scan tree; touches no files.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::tree::{NodeId, ROOT, Tree, flags};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    JavaScript,
}

impl Language {
    pub fn label(self) -> &'static str {
        match self {
            Language::Rust => "Rust",
            Language::JavaScript => "JavaScript",
        }
    }
}

/// A folder that a build or install recreates in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    /// Cargo's `target/`.
    CargoTarget,
    /// npm, pnpm, yarn or bun `node_modules/`.
    NodeModules,
}

impl ArtifactKind {
    pub fn language(self) -> Language {
        match self {
            ArtifactKind::CargoTarget => Language::Rust,
            ArtifactKind::NodeModules => Language::JavaScript,
        }
    }

    pub fn folder_name(self) -> &'static str {
        match self {
            ArtifactKind::CargoTarget => "target",
            ArtifactKind::NodeModules => "node_modules",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub id: NodeId,
    pub kind: ArtifactKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub dir: NodeId,
    pub artifacts: Vec<Artifact>,
}

/// Folders that hold software rather than the user's projects. A `node_modules` with a
/// `package.json` next to it in there belongs to an installed tool (a global npm package,
/// an editor extension), and deleting it would break that tool.
const SYSTEM_FOLDERS: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/usr",
    "/opt",
    "/private",
    "/bin",
    "/sbin",
    "/cores",
    "/dev",
    "/Volumes",
];

/// Projects below the root of `tree`, found by their build and dependency folders.
///
/// A Rust project is a folder with a `target/` next to `Cargo.toml`, or a `target/` that
/// Cargo tagged with `CACHEDIR.TAG`. A JavaScript project is a folder with `node_modules/`
/// next to `package.json`. Packages, hidden folders, `~/Library` and system folders are not
/// searched, and neither are artifact folders themselves, so nested `node_modules` count as
/// part of the outer one.
pub fn find_projects(tree: &Tree) -> Vec<Project> {
    let skip = skipped_folders(tree);
    let mut out = Vec::new();
    let mut stack = vec![ROOT];
    while let Some(dir) = stack.pop() {
        let (mut cargo_toml, mut package_json) = (false, false);
        let (mut target, mut node_modules) = (None, None);
        for c in tree.children(dir) {
            let is_dir = tree.node(c).is_dir();
            match tree.name(c).as_bytes() {
                b"Cargo.toml" if !is_dir => cargo_toml = true,
                b"package.json" if !is_dir => package_json = true,
                b"target" if is_dir => target = Some(c),
                b"node_modules" if is_dir => node_modules = Some(c),
                _ => {}
            }
        }
        let mut artifacts = Vec::new();
        if let Some(t) = target
            && (cargo_toml || has_child(tree, t, b"CACHEDIR.TAG"))
        {
            artifacts.push(Artifact { id: t, kind: ArtifactKind::CargoTarget });
        }
        if let Some(m) = node_modules
            && package_json
        {
            artifacts.push(Artifact { id: m, kind: ArtifactKind::NodeModules });
        }
        for c in tree.children(dir) {
            let n = tree.node(c);
            let searchable = n.is_dir()
                && !n.has(flags::PACKAGE | flags::HIDDEN)
                && Some(c) != node_modules
                && !artifacts.iter().any(|a| a.id == c)
                && !skip.contains(&c);
            if searchable {
                stack.push(c);
            }
        }
        if !artifacts.is_empty() {
            out.push(Project { dir, artifacts });
        }
    }
    out
}

fn has_child(tree: &Tree, dir: NodeId, name: &[u8]) -> bool {
    tree.children(dir).any(|c| tree.name(c).as_bytes() == name)
}

/// System folders and every user's `Library` that occur in this tree.
fn skipped_folders(tree: &Tree) -> HashSet<NodeId> {
    let mut skip: HashSet<NodeId> = SYSTEM_FOLDERS
        .iter()
        .filter_map(|p| tree.find(Path::new(p)))
        .collect();
    if let Some(users) = tree.find(Path::new("/Users")) {
        skip.extend(tree.children(users).filter_map(|home| {
            tree.children(home).find(|&c| tree.name(c).as_bytes() == b"Library")
        }));
    }
    if let Some(home) = std::env::var_os("HOME")
        && let Some(library) = tree.find(&Path::new(&home).join("Library"))
    {
        skip.insert(library);
    }
    skip
}

/// A toolchain cache or installed toolchain outside any project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalCache {
    pub id: NodeId,
    pub language: Language,
    pub label: String,
    /// What deleting it costs.
    pub note: &'static str,
}

const REDOWNLOADED: &str = "Downloaded again when needed";
const REBUILT: &str = "Rebuilt by the next builds";

/// Known cache folders, relative to the home folder.
const GLOBAL_CACHES: &[(&str, Language, &str, &str)] = &[
    (".cargo/registry", Language::Rust, "Cargo registry", REDOWNLOADED),
    (".cargo/git", Language::Rust, "Cargo git checkouts", REDOWNLOADED),
    (".rustup/downloads", Language::Rust, "rustup downloads", REDOWNLOADED),
    ("Library/Caches/Mozilla.sccache", Language::Rust, "sccache", REBUILT),
    ("Library/Caches/kache", Language::Rust, "kache", REBUILT),
    (".npm", Language::JavaScript, "npm cache", REDOWNLOADED),
    ("Library/pnpm/store", Language::JavaScript, "pnpm store", REDOWNLOADED),
    (".pnpm-store", Language::JavaScript, "pnpm store", REDOWNLOADED),
    ("Library/Caches/Yarn", Language::JavaScript, "Yarn cache", REDOWNLOADED),
    (".yarn/berry/cache", Language::JavaScript, "Yarn cache", REDOWNLOADED),
    (".bun/install/cache", Language::JavaScript, "Bun cache", REDOWNLOADED),
    (".nvm/.cache", Language::JavaScript, "nvm downloads", REDOWNLOADED),
    ("Library/Caches/deno", Language::JavaScript, "Deno cache", REDOWNLOADED),
];

/// Folders whose subfolders are installed toolchain versions, listed one by one since old
/// versions tend to pile up.
const TOOLCHAINS: &[(&str, Language, &str, &str)] = &[
    (".rustup/toolchains", Language::Rust, "rustup toolchain", "Reinstall with rustup"),
    (".nvm/versions/node", Language::JavaScript, "Node.js", "Reinstall with nvm"),
];

/// Global caches and installed toolchain versions of the user whose home folder is `home`,
/// if this tree contains them. Default locations only: a GUI app does not see overrides such
/// as `SCCACHE_DIR` or `DENO_DIR` from the shell.
pub fn find_global_caches(tree: &Tree, home: &Path) -> Vec<GlobalCache> {
    let mut out: Vec<GlobalCache> = GLOBAL_CACHES
        .iter()
        .filter_map(|&(rel, language, label, note)| {
            let id = tree.find(&home.join(rel))?;
            Some(GlobalCache { id, language, label: label.into(), note })
        })
        .collect();
    for &(rel, language, label, note) in TOOLCHAINS {
        if let Some(parent) = tree.find(&home.join(rel)) {
            out.extend(tree.children(parent).filter(|&c| tree.node(c).is_dir()).map(|id| {
                GlobalCache {
                    id,
                    language,
                    label: format!("{label} {}", tree.display_name(id)),
                    note,
                }
            }));
        }
    }
    out
}

/// Newest modification time in the subtree of `id`: when it was last built or installed.
pub fn newest_mtime(tree: &Tree, id: NodeId) -> u32 {
    let mut newest = tree.node(id).mtime;
    let mut stack = vec![id];
    while let Some(n) = stack.pop() {
        for c in tree.children(n) {
            newest = newest.max(tree.node(c).mtime);
            stack.push(c);
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Kind;
    use std::path::PathBuf;

    fn dir(t: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        t.add(parent, name.as_bytes(), Kind::Dir, 0, 0, 0)
    }

    fn file(t: &mut Tree, parent: NodeId, name: &str, size: u64, mtime: u32) -> NodeId {
        t.add(parent, name.as_bytes(), Kind::File, 0, size, mtime)
    }

    fn found(t: &Tree) -> Vec<(PathBuf, Vec<ArtifactKind>)> {
        let mut v: Vec<_> = find_projects(t)
            .into_iter()
            .map(|p| (t.path(p.dir), p.artifacts.iter().map(|a| a.kind).collect()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[test]
    fn rust_and_js_projects_are_found_by_their_build_folders() {
        let mut t = Tree::new("/w".into(), "w", 0);
        let rs = dir(&mut t, ROOT, "rs");
        file(&mut t, rs, "Cargo.toml", 1, 0);
        let target = dir(&mut t, rs, "target");
        file(&mut t, target, "big", 100, 0);
        let web = dir(&mut t, ROOT, "web");
        file(&mut t, web, "package.json", 1, 0);
        dir(&mut t, web, "node_modules");
        assert_eq!(
            found(&t),
            vec![
                ("/w/rs".into(), vec![ArtifactKind::CargoTarget]),
                ("/w/web".into(), vec![ArtifactKind::NodeModules]),
            ]
        );
    }

    #[test]
    fn a_folder_is_only_a_build_folder_next_to_its_manifest() {
        // "target" and "node_modules" are ordinary names elsewhere; deleting those would
        // destroy user data, so they must not be offered.
        let mut t = Tree::new("/w".into(), "w", 0);
        let photos = dir(&mut t, ROOT, "photos");
        dir(&mut t, photos, "target");
        let stray = dir(&mut t, ROOT, "stray");
        dir(&mut t, stray, "node_modules");
        assert!(found(&t).is_empty());
    }

    #[test]
    fn a_cargo_tagged_target_counts_without_a_manifest_next_to_it() {
        // A shared CARGO_TARGET_DIR has no Cargo.toml beside it but is still Cargo's.
        let mut t = Tree::new("/w".into(), "w", 0);
        let shared = dir(&mut t, ROOT, "builds");
        let target = dir(&mut t, shared, "target");
        file(&mut t, target, "CACHEDIR.TAG", 1, 0);
        assert_eq!(found(&t), vec![("/w/builds".into(), vec![ArtifactKind::CargoTarget])]);
    }

    #[test]
    fn workspaces_and_nested_dependencies_are_not_listed_twice() {
        // Workspace members share the root's target; packages inside node_modules have
        // their own package.json and node_modules but belong to the outer project.
        let mut t = Tree::new("/w".into(), "w", 0);
        let ws = dir(&mut t, ROOT, "ws");
        file(&mut t, ws, "Cargo.toml", 1, 0);
        dir(&mut t, ws, "target");
        let member = dir(&mut t, ws, "crate-a");
        file(&mut t, member, "Cargo.toml", 1, 0);
        file(&mut t, ws, "package.json", 1, 0);
        let nm = dir(&mut t, ws, "node_modules");
        let pkg = dir(&mut t, nm, "left-pad");
        file(&mut t, pkg, "package.json", 1, 0);
        dir(&mut t, pkg, "node_modules");
        assert_eq!(
            found(&t),
            vec![("/w/ws".into(), vec![ArtifactKind::CargoTarget, ArtifactKind::NodeModules])]
        );
    }

    #[test]
    fn installed_software_is_not_offered_for_cleaning() {
        // Electron apps, editor extensions and global npm tools ship node_modules next to a
        // package.json; deleting them breaks the software.
        let mut t = Tree::new("/".into(), "Macintosh HD", 0);
        let apps = dir(&mut t, ROOT, "Applications");
        let app = t.add(apps, b"Slack.app", Kind::Dir, flags::PACKAGE, 0, 0);
        file(&mut t, app, "package.json", 1, 0);
        dir(&mut t, app, "node_modules");
        let users = dir(&mut t, ROOT, "Users");
        let me = dir(&mut t, users, "me");
        let vscode = t.add(me, b".vscode", Kind::Dir, flags::HIDDEN, 0, 0);
        file(&mut t, vscode, "package.json", 1, 0);
        dir(&mut t, vscode, "node_modules");
        let library = dir(&mut t, me, "Library");
        file(&mut t, library, "package.json", 1, 0);
        dir(&mut t, library, "node_modules");
        let opt = dir(&mut t, ROOT, "opt");
        file(&mut t, opt, "package.json", 1, 0);
        dir(&mut t, opt, "node_modules");
        let code = dir(&mut t, me, "code");
        file(&mut t, code, "package.json", 1, 0);
        dir(&mut t, code, "node_modules");
        assert_eq!(found(&t), vec![("/Users/me/code".into(), vec![ArtifactKind::NodeModules])]);
    }

    #[test]
    fn global_caches_and_each_toolchain_version_are_listed() {
        // Compiler caches and old toolchain versions are where Rust and Node.js space hides
        // outside projects; each version is its own entry so old ones can be removed alone.
        let mut t = Tree::new("/Users/me".into(), "me", 0);
        let cargo = t.add(ROOT, b".cargo", Kind::Dir, flags::HIDDEN, 0, 0);
        let registry = dir(&mut t, cargo, "registry");
        let rustup = t.add(ROOT, b".rustup", Kind::Dir, flags::HIDDEN, 0, 0);
        let toolchains = dir(&mut t, rustup, "toolchains");
        let stable = dir(&mut t, toolchains, "stable-aarch64-apple-darwin");
        let nightly = dir(&mut t, toolchains, "nightly-2024-01-01-aarch64-apple-darwin");
        let library = dir(&mut t, ROOT, "Library");
        let caches_dir = dir(&mut t, library, "Caches");
        let kache = dir(&mut t, caches_dir, "kache");
        let deno = dir(&mut t, caches_dir, "deno");
        let nvm = t.add(ROOT, b".nvm", Kind::Dir, flags::HIDDEN, 0, 0);
        let versions = dir(&mut t, nvm, "versions");
        let node = dir(&mut t, versions, "node");
        let v23 = dir(&mut t, node, "v23.3.0");
        let npm = t.add(ROOT, b".npm", Kind::Dir, flags::HIDDEN, 0, 0);

        let caches = find_global_caches(&t, Path::new("/Users/me"));
        let mut ids: Vec<NodeId> = caches.iter().map(|c| c.id).collect();
        ids.sort();
        let mut expected = vec![registry, stable, nightly, kache, deno, v23, npm];
        expected.sort();
        assert_eq!(ids, expected);
        let label = |id| caches.iter().find(|c| c.id == id).map(|c| (c.label.as_str(), c.language));
        assert_eq!(label(kache), Some(("kache", Language::Rust)));
        assert_eq!(label(v23), Some(("Node.js v23.3.0", Language::JavaScript)));
        assert_eq!(label(nightly), Some(("rustup toolchain nightly-2024-01-01-aarch64-apple-darwin", Language::Rust)));
    }

    #[test]
    fn age_comes_from_the_newest_file_inside() {
        // The folder's own date changes rarely; the last build touches files deep inside.
        let mut t = Tree::new("/w".into(), "w", 0);
        let target = t.add(ROOT, b"target", Kind::Dir, 0, 0, 10);
        let debug = t.add(target, b"debug", Kind::Dir, 0, 0, 20);
        file(&mut t, debug, "old", 1, 15);
        file(&mut t, debug, "new", 1, 99);
        assert_eq!(newest_mtime(&t, target), 99);
    }
}
