//! Where developer tools keep their data on this Mac.
//!
//! Every location starts from the tool's documented macOS default. The user's shell
//! environment overrides it (variables such as `CARGO_HOME` or `XDG_CACHE_HOME` are usually
//! set in shell startup files, which an app opened from Finder never reads), and the tool's
//! own answer (`go env GOMODCACHE`, `npm config get cache`, ...) overrides both.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A folder whose location depends on the tool's configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Root {
    Home,
    CargoHome,
    RustupHome,
    Sccache,
    Kache,
    NpmCache,
    PnpmStore,
    PnpmCache,
    YarnCache,
    YarnBerryCache,
    BunCache,
    DenoDir,
    NvmDir,
    FnmDir,
    VoltaHome,
    Corepack,
    GoModCache,
    GoBuildCache,
    GolangciLint,
    GradleHome,
    MavenRepository,
    SdkmanDir,
    Coursier,
    Ccache,
    ConanHome,
    VcpkgCache,
    /// Bazel 9 and later.
    Bazel,
    /// Bazel 8 and earlier.
    BazelLegacy,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Locations {
    home: PathBuf,
    env: HashMap<String, String>,
    /// Answers from the tools themselves.
    reported: HashMap<Root, PathBuf>,
}

/// Waiting longer than this for the shell or a tool means it is broken or busy; the defaults
/// are used instead.
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// How to ask a tool for a location: the program, its arguments, and how to read its answer.
type Query = (Root, &'static str, &'static [&'static str], fn(&str) -> Option<PathBuf>);

const QUERIES: &[Query] = &[
    (Root::GoModCache, "go", &["env", "GOMODCACHE"], first_line),
    (Root::GoBuildCache, "go", &["env", "GOCACHE"], first_line),
    (Root::NpmCache, "npm", &["config", "get", "cache"], first_line),
    (Root::PnpmStore, "pnpm", &["store", "path"], first_line),
    (Root::YarnCache, "yarn", &["cache", "dir"], first_line),
    (Root::DenoDir, "deno", &["info", "--json"], deno_dir),
    (Root::Ccache, "ccache", &["-k", "cache_dir"], first_line),
    (Root::ConanHome, "conan", &["config", "home"], first_line),
];

impl Locations {
    /// Documented defaults only.
    pub fn defaults(home: &Path) -> Self {
        Locations { home: home.to_path_buf(), ..Default::default() }
    }

    pub fn with_env(home: &Path, env: HashMap<String, String>) -> Self {
        Locations { home: home.to_path_buf(), env, ..Default::default() }
    }

    /// Reads the user's shell environment and asks the installed tools. Takes seconds; call
    /// it off the main thread.
    pub fn discover(home: &Path) -> Self {
        let mut locations = Locations::with_env(home, user_environment());
        locations.reported = ask_tools(&locations.env, home);
        locations
    }

    /// What the tools reported, for diagnostics.
    #[cfg(debug_assertions)]
    pub fn reported(&self) -> &HashMap<Root, PathBuf> {
        &self.reported
    }

    /// An absolute path from an environment variable, or the first entry of a list such as
    /// GOPATH; empty or relative values are ignored.
    fn var(&self, name: &str) -> Option<PathBuf> {
        let value = self.env.get(name)?;
        let path = PathBuf::from(value.split(':').next()?);
        path.is_absolute().then_some(path)
    }

    fn xdg(&self, var: &str, rel: &str) -> Option<PathBuf> {
        self.var(var).map(|base| base.join(rel))
    }

    fn user(&self) -> String {
        self.env.get("USER").cloned().unwrap_or_else(|| {
            self.home.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        })
    }

    pub fn root(&self, root: Root) -> PathBuf {
        if let Some(path) = self.reported.get(&root) {
            return path.clone();
        }
        let home = |rel: &str| self.home.join(rel);
        let var = |name: &str| self.var(name);
        match root {
            Root::Home => self.home.clone(),
            Root::CargoHome => var("CARGO_HOME").unwrap_or_else(|| home(".cargo")),
            Root::RustupHome => var("RUSTUP_HOME").unwrap_or_else(|| home(".rustup")),
            Root::Sccache => var("SCCACHE_DIR").unwrap_or_else(|| home("Library/Caches/Mozilla.sccache")),
            Root::Kache => var("KACHE_CACHE_DIR").unwrap_or_else(|| home("Library/Caches/kache")),
            Root::NpmCache => var("npm_config_cache")
                .or_else(|| var("NPM_CONFIG_CACHE"))
                .unwrap_or_else(|| home(".npm")),
            Root::PnpmStore => var("PNPM_HOME")
                .map(|p| p.join("store"))
                .or_else(|| self.xdg("XDG_DATA_HOME", "pnpm/store"))
                .unwrap_or_else(|| home("Library/pnpm/store")),
            Root::PnpmCache => self.xdg("XDG_CACHE_HOME", "pnpm").unwrap_or_else(|| home("Library/Caches/pnpm")),
            Root::YarnCache => var("YARN_CACHE_FOLDER")
                .or_else(|| self.xdg("XDG_CACHE_HOME", "yarn"))
                .unwrap_or_else(|| home("Library/Caches/Yarn")),
            Root::YarnBerryCache => var("YARN_GLOBAL_FOLDER")
                .map(|p| p.join("cache"))
                .or_else(|| self.xdg("XDG_DATA_HOME", "yarn/berry/cache"))
                .unwrap_or_else(|| home(".yarn/berry/cache")),
            Root::BunCache => var("BUN_INSTALL_CACHE_DIR")
                .or_else(|| var("BUN_INSTALL").map(|p| p.join("install/cache")))
                .unwrap_or_else(|| home(".bun/install/cache")),
            Root::DenoDir => var("DENO_DIR").unwrap_or_else(|| home("Library/Caches/deno")),
            Root::NvmDir => var("NVM_DIR")
                .or_else(|| self.xdg("XDG_CONFIG_HOME", "nvm"))
                .unwrap_or_else(|| home(".nvm")),
            Root::FnmDir => var("FNM_DIR").unwrap_or_else(|| {
                // fnm uses the first of these that exists.
                let xdg = self.xdg("XDG_DATA_HOME", "fnm").unwrap_or_else(|| home(".local/share/fnm"));
                [xdg.clone(), home(".fnm"), home("Library/Application Support/fnm")]
                    .into_iter()
                    .find(|p| p.is_dir())
                    .unwrap_or(xdg)
            }),
            Root::VoltaHome => var("VOLTA_HOME").unwrap_or_else(|| home(".volta")),
            Root::Corepack => var("COREPACK_HOME")
                .or_else(|| self.xdg("XDG_CACHE_HOME", "node/corepack"))
                .unwrap_or_else(|| home(".cache/node/corepack")),
            Root::GoModCache => var("GOMODCACHE")
                .or_else(|| var("GOPATH").map(|p| p.join("pkg/mod")))
                .unwrap_or_else(|| home("go/pkg/mod")),
            // Go ignores XDG_CACHE_HOME on macOS.
            Root::GoBuildCache => var("GOCACHE").unwrap_or_else(|| home("Library/Caches/go-build")),
            Root::GolangciLint => var("GOLANGCI_LINT_CACHE").unwrap_or_else(|| home("Library/Caches/golangci-lint")),
            Root::GradleHome => var("GRADLE_USER_HOME").unwrap_or_else(|| home(".gradle")),
            Root::MavenRepository => maven_repository(&self.home).unwrap_or_else(|| home(".m2/repository")),
            Root::SdkmanDir => var("SDKMAN_DIR").unwrap_or_else(|| home(".sdkman")),
            Root::Coursier => var("COURSIER_CACHE").unwrap_or_else(|| home("Library/Caches/Coursier/v1")),
            Root::Ccache => var("CCACHE_DIR").unwrap_or_else(|| {
                // ccache 4 keeps using ~/.ccache only if it already exists.
                let legacy = home(".ccache");
                if legacy.is_dir() {
                    legacy
                } else {
                    self.xdg("XDG_CACHE_HOME", "ccache").unwrap_or_else(|| home("Library/Caches/ccache"))
                }
            }),
            Root::ConanHome => var("CONAN_HOME").unwrap_or_else(|| home(".conan2")),
            Root::VcpkgCache => var("VCPKG_DEFAULT_BINARY_CACHE")
                .or_else(|| self.xdg("XDG_CACHE_HOME", "vcpkg/archives"))
                .unwrap_or_else(|| home(".cache/vcpkg/archives")),
            Root::Bazel => self
                .xdg("XDG_CACHE_HOME", "bazel")
                .unwrap_or_else(|| home("Library/Caches/bazel"))
                .join(format!("_bazel_{}", self.user())),
            Root::BazelLegacy => PathBuf::from(format!("/private/var/tmp/_bazel_{}", self.user())),
        }
    }

    /// `rel` inside `root`; an empty `rel` is the root itself.
    pub fn path(&self, root: Root, rel: &str) -> PathBuf {
        let base = self.root(root);
        if rel.is_empty() { base } else { base.join(rel) }
    }
}

/// A custom `<localRepository>` from Maven's user settings, if any.
fn maven_repository(home: &Path) -> Option<PathBuf> {
    let settings = std::fs::read_to_string(home.join(".m2/settings.xml")).ok()?;
    let start = settings.find("<localRepository>")? + "<localRepository>".len();
    let end = start + settings[start..].find("</localRepository>")?;
    let value = settings[start..end].trim().replace("${user.home}", &home.to_string_lossy());
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn first_line(output: &str) -> Option<PathBuf> {
    let path = PathBuf::from(output.lines().map(str::trim).find(|l| !l.is_empty())?);
    path.is_absolute().then_some(path)
}

fn deno_dir(output: &str) -> Option<PathBuf> {
    let info: serde_json::Value = serde_json::from_str(output).ok()?;
    let path = PathBuf::from(info.get("denoDir")?.as_str()?);
    path.is_absolute().then_some(path)
}

/// Runs `command`, returning its standard output if it succeeds within `timeout`.
fn run(mut command: Command, timeout: Duration) -> Option<Vec<u8>> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = reader.join().ok()?;
    status.success().then_some(out)
}

/// `KEY=value` pairs separated by NUL bytes, as printed by `env -0`.
fn parse_env(bytes: &[u8]) -> HashMap<String, String> {
    bytes
        .split(|&b| b == 0)
        .filter_map(|pair| {
            let (key, value) = std::str::from_utf8(pair).ok()?.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// The environment of a new login shell, as a terminal would have it. Falls back to this
/// process's environment.
fn user_environment() -> HashMap<String, String> {
    let fallback = || std::env::vars().collect();
    let Some(shell) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) else {
        return fallback();
    };
    // Login and interactive, so both kinds of startup files run (.zprofile and .zshrc).
    let mut command = Command::new(shell);
    command.args(["-l", "-i", "-c", "command env -0"]);
    match run(command, SHELL_TIMEOUT) {
        Some(out) => {
            let env = parse_env(&out);
            if env.contains_key("PATH") { env } else { fallback() }
        }
        None => fallback(),
    }
}

/// The executable `name` on the user's PATH.
fn which(env: &HashMap<String, String>, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    env.get("PATH")?.split(':').map(|dir| Path::new(dir).join(name)).find(|p| {
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// Asks every installed tool where its data is, all at once.
fn ask_tools(env: &HashMap<String, String>, home: &Path) -> HashMap<Root, PathBuf> {
    let jobs: Vec<_> = QUERIES
        .iter()
        .filter_map(|&(root, program, args, parse)| {
            let program = which(env, program)?;
            let mut command = Command::new(program);
            // From the home folder, so no project's configuration applies.
            command.args(args).env_clear().envs(env).current_dir(home);
            Some(std::thread::spawn(move || {
                let out = run(command, TOOL_TIMEOUT)?;
                Some((root, parse(&String::from_utf8_lossy(&out))?))
            }))
        })
        .collect();
    jobs.into_iter().filter_map(|job| job.join().ok().flatten()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults_follow_the_tools_documentation() {
        let l = Locations::defaults(Path::new("/Users/me"));
        assert_eq!(l.root(Root::GoModCache), PathBuf::from("/Users/me/go/pkg/mod"));
        assert_eq!(l.root(Root::GoBuildCache), PathBuf::from("/Users/me/Library/Caches/go-build"));
        assert_eq!(l.root(Root::PnpmStore), PathBuf::from("/Users/me/Library/pnpm/store"));
        assert_eq!(l.root(Root::Corepack), PathBuf::from("/Users/me/.cache/node/corepack"));
        assert_eq!(l.root(Root::Bazel), PathBuf::from("/Users/me/Library/Caches/bazel/_bazel_me"));
        assert_eq!(l.root(Root::BazelLegacy), PathBuf::from("/private/var/tmp/_bazel_me"));
        assert_eq!(l.path(Root::CargoHome, "registry"), PathBuf::from("/Users/me/.cargo/registry"));
        assert_eq!(l.path(Root::Kache, ""), PathBuf::from("/Users/me/Library/Caches/kache"));
    }

    #[test]
    fn the_users_environment_moves_the_folders() {
        // Many developers relocate these in their shell startup files; the defaults would
        // then show nothing, or the wrong folder.
        let l = Locations::with_env(
            Path::new("/Users/me"),
            env(&[
                ("CARGO_HOME", "/data/cargo"),
                ("XDG_CACHE_HOME", "/data/cache"),
                ("GOPATH", "/data/gopath:/other"),
                ("USER", "dev"),
                ("RUSTUP_HOME", "relative/ignored"),
                ("DENO_DIR", ""),
            ]),
        );
        assert_eq!(l.path(Root::CargoHome, "git"), PathBuf::from("/data/cargo/git"));
        assert_eq!(l.root(Root::GoModCache), PathBuf::from("/data/gopath/pkg/mod"));
        assert_eq!(l.root(Root::YarnCache), PathBuf::from("/data/cache/yarn"));
        assert_eq!(l.root(Root::PnpmCache), PathBuf::from("/data/cache/pnpm"));
        assert_eq!(l.root(Root::Bazel), PathBuf::from("/data/cache/bazel/_bazel_dev"));
        // Go ignores XDG_CACHE_HOME on macOS.
        assert_eq!(l.root(Root::GoBuildCache), PathBuf::from("/Users/me/Library/Caches/go-build"));
        // Relative and empty values are not paths.
        assert_eq!(l.root(Root::RustupHome), PathBuf::from("/Users/me/.rustup"));
        assert_eq!(l.root(Root::DenoDir), PathBuf::from("/Users/me/Library/Caches/deno"));
    }

    #[test]
    fn a_tools_own_answer_wins() {
        let mut l = Locations::with_env(Path::new("/Users/me"), env(&[("GOMODCACHE", "/env/mod")]));
        l.reported.insert(Root::GoModCache, PathBuf::from("/tool/mod"));
        assert_eq!(l.root(Root::GoModCache), PathBuf::from("/tool/mod"));
    }

    #[test]
    fn maven_reads_a_custom_repository_from_its_settings() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".m2")).unwrap();
        std::fs::write(
            home.path().join(".m2/settings.xml"),
            "<settings>\n  <localRepository> ${user.home}/m2repo </localRepository>\n</settings>",
        )
        .unwrap();
        let l = Locations::defaults(home.path());
        assert_eq!(l.root(Root::MavenRepository), home.path().join("m2repo"));
    }

    #[test]
    fn tool_answers_are_parsed_strictly() {
        assert_eq!(first_line("\n  /Users/me/.npm\n"), Some(PathBuf::from("/Users/me/.npm")));
        assert_eq!(first_line("npm WARN something"), None);
        assert_eq!(deno_dir(r#"{"denoDir": "/x/deno", "version": 1}"#), Some(PathBuf::from("/x/deno")));
        assert_eq!(deno_dir("not json"), None);
        assert_eq!(
            parse_env(b"A=1\0PATH=/bin:/usr/bin\0MULTI=x=y\0"),
            env(&[("A", "1"), ("PATH", "/bin:/usr/bin"), ("MULTI", "x=y")])
        );
    }

    #[test]
    fn a_hanging_command_is_abandoned() {
        let started = Instant::now();
        let mut command = Command::new("/bin/sleep");
        command.arg("10");
        assert_eq!(run(command, Duration::from_millis(200)), None);
        assert!(started.elapsed() < Duration::from_secs(3));
        let mut echo = Command::new("/bin/echo");
        echo.arg("/ok");
        assert_eq!(run(echo, TOOL_TIMEOUT), Some(b"/ok\n".to_vec()));
    }
}
