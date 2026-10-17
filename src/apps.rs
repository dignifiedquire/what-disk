//! Applications and the data they keep in `~/Library`. Works on a scan tree; only the
//! bundle metadata is read from disk.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use objc2_foundation::{NSBundle, NSString};

use crate::tree::{NodeId, Tree, flags};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleInfo {
    pub id: Option<String>,
    pub name: String,
    pub version: Option<String>,
}

/// Bundle identifier, display name and version from the app's Info.plist.
pub fn bundle_info(path: &Path) -> BundleInfo {
    let fallback = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let Some(bundle) = NSBundle::bundleWithPath(&NSString::from_str(&path.to_string_lossy())) else {
        return BundleInfo { id: None, name: fallback, version: None };
    };
    let info = |key: &str| {
        let value = bundle.objectForInfoDictionaryKey(&NSString::from_str(key))?;
        value.downcast::<NSString>().ok().map(|s| s.to_string())
    };
    BundleInfo {
        id: bundle.bundleIdentifier().map(|s| s.to_string()),
        name: info("CFBundleDisplayName")
            .or_else(|| info("CFBundleName"))
            .filter(|n| !n.is_empty())
            .unwrap_or(fallback),
        version: info("CFBundleShortVersionString"),
    }
}

/// `.app` bundles in `folder` and its subfolders, but not apps inside other bundles.
pub fn find_apps(tree: &Tree, folder: &Path) -> Vec<NodeId> {
    let Some(start) = tree.find(folder) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        for c in tree.children(dir) {
            let n = tree.node(c);
            if !n.is_dir() {
                continue;
            }
            if n.has(flags::PACKAGE) {
                if tree.name(c).as_bytes().ends_with(b".app") {
                    out.push(c);
                }
            } else {
                stack.push(c);
            }
        }
    }
    out
}

/// How a folder or file in a `~/Library` subfolder is recognised as an app's.
#[derive(Clone, Copy)]
enum Match {
    /// Named exactly like the bundle identifier.
    Id,
    /// Named like the bundle identifier or the app.
    IdOrName,
    /// Named like the bundle identifier plus this suffix.
    IdWith(&'static str),
    /// A group container: the identifier, optionally prefixed by a team ID or "group.".
    Group,
}

/// Where apps keep data in `~/Library`, with a short label for each.
const DATA: &[(&str, &str, Match)] = &[
    ("Application Support", "Application Support", Match::IdOrName),
    ("Caches", "Caches", Match::IdOrName),
    ("Container", "Containers", Match::Id),
    ("Group container", "Group Containers", Match::Group),
    ("Application scripts", "Application Scripts", Match::Id),
    ("WebKit data", "WebKit", Match::Id),
    ("HTTP storage", "HTTPStorages", Match::Id),
    ("Cookies", "HTTPStorages", Match::IdWith(".binarycookies")),
    ("Cookies", "Cookies", Match::IdWith(".binarycookies")),
    ("Logs", "Logs", Match::IdOrName),
    ("Saved window state", "Saved Application State", Match::IdWith(".savedState")),
    ("Preferences", "Preferences", Match::IdWith(".plist")),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataFolder {
    pub id: NodeId,
    pub label: &'static str,
}

/// Items in `library` (a user's `~/Library`) that belong to the app with bundle identifier
/// `bundle_id` and name `name`. Names must match exactly (ignoring case), since uninstalling
/// removes everything listed here; very short names are not matched to avoid collisions.
pub fn data_folders(tree: &Tree, library: &Path, bundle_id: &str, name: &str) -> Vec<DataFolder> {
    let id = bundle_id.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let matches = |item: &str, rule: Match| match rule {
        Match::Id => item == id,
        Match::IdOrName => item == id || (name.len() >= 3 && item == name),
        Match::IdWith(suffix) => item.strip_suffix(&suffix.to_ascii_lowercase()) == Some(id.as_str()),
        Match::Group => {
            item == id || item.strip_suffix(id.as_str()).is_some_and(|p| p.ends_with('.'))
        }
    };
    let mut out = Vec::new();
    for &(label, sub, rule) in DATA {
        let Some(dir) = tree.find(&library.join(sub)) else {
            continue;
        };
        for c in tree.children(dir) {
            let item = tree.display_name(c).to_ascii_lowercase();
            if matches(&item, rule) && !out.iter().any(|d: &DataFolder| d.id == c) {
                out.push(DataFolder { id: c, label });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Kind, ROOT};
    use std::path::PathBuf;

    fn dir(t: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        t.add(parent, name.as_bytes(), Kind::Dir, 0, 0, 0)
    }

    fn file(t: &mut Tree, parent: NodeId, name: &str) -> NodeId {
        t.add(parent, name.as_bytes(), Kind::File, 0, 1, 0)
    }

    #[test]
    fn apps_are_found_in_subfolders_but_not_inside_other_apps() {
        let mut t = Tree::new("/".into(), "Macintosh HD", 0);
        let apps = dir(&mut t, ROOT, "Applications");
        let foo = t.add(apps, b"Foo.app", Kind::Dir, flags::PACKAGE, 0, 0);
        let helpers = dir(&mut t, foo, "Helpers");
        t.add(helpers, b"Inner.app", Kind::Dir, flags::PACKAGE, 0, 0);
        let utilities = dir(&mut t, apps, "Utilities");
        let bar = t.add(utilities, b"Bar.app", Kind::Dir, flags::PACKAGE, 0, 0);
        t.add(apps, b"Notes.rtfd", Kind::Dir, flags::PACKAGE, 0, 0);
        let mut found = find_apps(&t, Path::new("/Applications"));
        found.sort();
        assert_eq!(found, vec![foo, bar]);
        assert!(find_apps(&t, Path::new("/Missing")).is_empty());
    }

    #[test]
    fn an_apps_data_is_found_in_every_library_location() {
        let mut t = Tree::new("/Users/me".into(), "me", 0);
        let lib = dir(&mut t, ROOT, "Library");
        let support = dir(&mut t, lib, "Application Support");
        let by_id = dir(&mut t, support, "com.foo.Bar");
        let by_name = dir(&mut t, support, "Bar");
        let caches = dir(&mut t, lib, "Caches");
        let cache = dir(&mut t, caches, "com.foo.bar");
        let containers = dir(&mut t, lib, "Containers");
        let container = dir(&mut t, containers, "com.foo.Bar");
        let groups = dir(&mut t, lib, "Group Containers");
        let group = dir(&mut t, groups, "ABCDE12345.com.foo.Bar");
        let prefs = dir(&mut t, lib, "Preferences");
        let plist = file(&mut t, prefs, "com.foo.Bar.plist");
        let saved = dir(&mut t, lib, "Saved Application State");
        let state = dir(&mut t, saved, "com.foo.Bar.savedState");
        let http = dir(&mut t, lib, "HTTPStorages");
        let cookies = file(&mut t, http, "com.foo.Bar.binarycookies");

        let found = data_folders(&t, Path::new("/Users/me/Library"), "com.foo.Bar", "Bar");
        let mut ids: Vec<NodeId> = found.iter().map(|d| d.id).collect();
        ids.sort();
        let mut expected = vec![by_id, by_name, cache, container, group, plist, state, cookies];
        expected.sort();
        assert_eq!(ids, expected);
        assert_eq!(found.iter().find(|d| d.id == container).unwrap().label, "Container");
    }

    #[test]
    fn similar_names_are_not_attributed_to_the_app() {
        // Uninstalling removes everything matched, so a helper or another vendor's folder
        // with a similar name must never be included.
        let mut t = Tree::new("/Users/me".into(), "me", 0);
        let lib = dir(&mut t, ROOT, "Library");
        let support = dir(&mut t, lib, "Application Support");
        dir(&mut t, support, "com.foo.BarHelper");
        dir(&mut t, support, "Barista");
        let groups = dir(&mut t, lib, "Group Containers");
        dir(&mut t, groups, "X.com.foo.BarHelper");
        dir(&mut t, groups, "Xcom.foo.Bar");
        let prefs = dir(&mut t, lib, "Preferences");
        file(&mut t, prefs, "com.foo.Bar.helper.plist");
        let caches = dir(&mut t, lib, "Caches");
        dir(&mut t, caches, "Go");
        assert!(data_folders(&t, Path::new("/Users/me/Library"), "com.foo.Bar", "Bar").is_empty());
        // Two-letter names would match too much.
        assert!(data_folders(&t, Path::new("/Users/me/Library"), "com.go", "Go").is_empty());
    }

    #[test]
    fn bundle_info_reads_the_info_plist() {
        let info = bundle_info(&PathBuf::from("/System/Applications/Calculator.app"));
        assert_eq!(info.id.as_deref(), Some("com.apple.calculator"));
        assert!(!info.name.is_empty());
        let missing = bundle_info(Path::new("/nonexistent/Ghost.app"));
        assert_eq!(missing, BundleInfo { id: None, name: "Ghost".into(), version: None });
    }
}
