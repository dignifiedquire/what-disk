//! Temporary folders, and which app or tool each item in them belongs to.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempKind {
    /// The user's $TMPDIR, emptied of items older than three days and at restart.
    Temp,
    /// The user's cache folder next to it; macOS does not clean it.
    Cache,
    /// /private/tmp and /private/var/tmp, shared by all users.
    Shared,
}

impl TempKind {
    pub fn label(self) -> &'static str {
        match self {
            TempKind::Temp | TempKind::Shared => "Temporary files",
            TempKind::Cache => "Temporary cache",
        }
    }
}

/// The user's temporary folders, with symbolic links resolved so they match scanned paths.
pub fn roots() -> Vec<(PathBuf, TempKind)> {
    let mut out = Vec::new();
    for (name, kind) in [(libc::_CS_DARWIN_USER_TEMP_DIR, TempKind::Temp), (libc::_CS_DARWIN_USER_CACHE_DIR, TempKind::Cache)] {
        if let Some(path) = confstr(name) {
            out.push((path, kind));
        }
    }
    for shared in ["/private/tmp", "/private/var/tmp"] {
        out.push((PathBuf::from(shared), TempKind::Shared));
    }
    out
}

fn confstr(name: libc::c_int) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the buffer is valid for its full length; confstr writes a NUL-terminated string.
    let len = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), buf.len()) };
    if len == 0 || len > buf.len() {
        return None;
    }
    buf.truncate(len - 1);
    let path = PathBuf::from(String::from_utf8(buf).ok()?);
    std::fs::canonicalize(path).ok()
}

/// Who a temporary item belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// An app, by its bundle identifier.
    App(String),
    /// A command-line tool or framework, by name.
    Tool(&'static str),
    /// macOS itself (com.apple.*).
    System,
    /// Named like an app's bundle identifier, but no such app is installed.
    Leftover(String),
    /// Unknown; grouped by the start of the name.
    Other(String),
}

/// Name prefixes of tools that do not use a bundle identifier.
const TOOLS: &[(&str, &str)] = &[
    ("claude-", "Claude Code"),
    ("codex-", "Codex"),
    ("kache", "kache"),
    ("go-build", "Go"),
    ("node-compile-cache", "Node.js"),
    ("v8-compile-cache", "Node.js"),
    ("xcrun_db", "Xcode"),
    ("clang", "Xcode"),
    ("uv-", "uv"),
    ("typescript-language-server", "TypeScript"),
    ("BlobRegistryFiles", "WebKit"),
    ("TemporaryItems", "macOS"),
];

/// The owner of the temporary item `name`, given the bundle identifiers of installed apps.
pub fn owner(name: &str, bundle_ids: &[String]) -> Owner {
    // Chromium hides some items behind a leading dot; some apps prefix their team ID.
    let name = name.trim_start_matches('.');
    let name = match name.split_once('.') {
        Some((team, rest)) if team.len() == 10 && team.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) => {
            rest
        }
        _ => name,
    };
    let lower = name.to_ascii_lowercase();
    let starts_with_id = |id: &str| {
        let id = id.to_ascii_lowercase();
        lower.strip_prefix(&id).is_some_and(|rest| rest.is_empty() || rest.starts_with(['.', '-', '_']))
    };
    if let Some(id) = bundle_ids.iter().filter(|id| starts_with_id(id)).max_by_key(|id| id.len()) {
        return Owner::App(id.clone());
    }
    if lower.starts_with("com.apple.") {
        return Owner::System;
    }
    // A helper of an app, named after the app's vendor: com.vendor.something-helper.
    let vendor: Vec<&str> = lower.splitn(3, '.').collect();
    if vendor.len() == 3 {
        let prefix = format!("{}.{}.", vendor[0], vendor[1]);
        let mut matching = bundle_ids.iter().filter(|id| id.to_ascii_lowercase().starts_with(&prefix));
        if let (Some(id), None) = (matching.next(), matching.next()) {
            return Owner::App(id.clone());
        }
    }
    if let Some(&(_, tool)) = TOOLS.iter().find(|(prefix, _)| name.starts_with(prefix)) {
        return Owner::Tool(tool);
    }
    if let Some(id) = reverse_dns(name) {
        return Owner::Leftover(id);
    }
    Owner::Other(stem(name))
}

/// "us.zoom.xos.helper" -> "us.zoom.xos": the bundle identifier a reverse-DNS name starts with.
fn reverse_dns(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split('.').take(3).collect();
    let domain = parts.first()?;
    let looks_like_id = parts.len() == 3
        && (2..=4).contains(&domain.len())
        && domain.bytes().all(|b| b.is_ascii_lowercase())
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    looks_like_id.then(|| parts.join("."))
}

/// The start of a name, before numbers or random characters: "nosh-redir-123-pid" -> "nosh".
/// Short random names such as "w0pQaE" have no meaningful stem.
fn stem(name: &str) -> String {
    let end = name.find(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '_')).unwrap_or(name.len());
    let stem = &name[..end];
    let random = name.len() <= 8 && end == name.len();
    if stem.len() < 3 || random { "Other".into() } else { stem.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        ["com.google.Chrome", "com.anthropic.claudefordesktop", "com.1password.1password", "com.google.Chrome.canary"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn items_are_attributed_to_the_app_whose_bundle_id_starts_their_name() {
        // Browsers and Electron apps leave hundreds of items named after their bundle ID,
        // with random suffixes, helper suffixes and sometimes a leading dot.
        let ids = ids();
        let app = |id: &str| Owner::App(id.into());
        assert_eq!(owner("com.google.Chrome.XyZ123", &ids), app("com.google.Chrome"));
        assert_eq!(owner(".com.google.Chrome.aB3dEf", &ids), app("com.google.Chrome"));
        assert_eq!(owner("com.google.Chrome.chrome_chrome_url_fetcher_1", &ids), app("com.google.Chrome"));
        assert_eq!(owner("com.google.Chrome.canary.tmp", &ids), app("com.google.Chrome.canary"));
        assert_eq!(owner("com.anthropic.claudefordesktop.helper", &ids), app("com.anthropic.claudefordesktop"));
        // A team ID prefix and a helper named after the vendor.
        assert_eq!(owner("2BUA8C4S2C.com.1password.browser-helper", &ids), app("com.1password.1password"));
        // A different app that merely shares the start of the ID.
        assert_ne!(owner("com.google.Chromecast", &ids), app("com.google.Chrome"));
    }

    #[test]
    fn tools_system_items_and_the_rest_are_told_apart() {
        let ids = ids();
        assert_eq!(owner("claude-501", &ids), Owner::Tool("Claude Code"));
        assert_eq!(owner("codex-daemon-501", &ids), Owner::Tool("Codex"));
        assert_eq!(owner("kache-x7Yz", &ids), Owner::Tool("kache"));
        assert_eq!(owner("com.apple.Safari", &ids), Owner::System);
        // Named after an app that is not installed: likely left behind by an uninstall.
        assert_eq!(owner("us.zoom.xos", &ids), Owner::Leftover("us.zoom.xos".into()));
        assert_eq!(owner("chat.delta.desktop.electron.helper", &ids), Owner::Leftover("chat.delta.desktop".into()));
        assert_eq!(owner("nosh-redir-1234-pid", &ids), Owner::Other("nosh".into()));
        assert_eq!(owner("tmp.ZM2FViCggO", &ids), Owner::Other("tmp".into()));
        assert_eq!(owner("TemporaryDirectory.xCV8PA", &ids), Owner::Other("TemporaryDirectory".into()));
        // Random mkstemp-style names are not each a group of their own.
        assert_eq!(owner("unySS", &ids), Owner::Other("Other".into()));
        assert_eq!(owner("w0pQaE", &ids), Owner::Other("Other".into()));
    }

    #[test]
    fn the_users_temporary_folders_are_found() {
        let roots = roots();
        let temp = roots.iter().find(|(_, k)| *k == TempKind::Temp).expect("TMPDIR");
        assert!(temp.0.starts_with("/private/var/folders"), "{:?}", temp.0);
        assert!(roots.iter().any(|(_, k)| *k == TempKind::Cache));
    }
}
