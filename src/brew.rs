//! Homebrew: what is installed, and uninstalling it with `brew` itself, so Homebrew's own
//! records stay correct.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Clone, Debug, PartialEq)]
pub struct Formula {
    pub name: String,
    pub versions: Vec<String>,
    /// Installed only because another formula needs it.
    pub dependency: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cask {
    pub token: String,
    pub name: String,
    pub version: String,
    /// File names of the apps it puts in /Applications.
    pub apps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Brew {
    pub executable: PathBuf,
    /// For example /opt/homebrew; formulae live in `Cellar`, casks in `Caskroom`.
    pub prefix: PathBuf,
    pub formulae: Vec<Formula>,
    pub casks: Vec<Cask>,
}

impl Brew {
    pub fn cellar(&self, formula: &Formula) -> PathBuf {
        self.prefix.join("Cellar").join(&formula.name)
    }

    pub fn caskroom(&self, cask: &Cask) -> PathBuf {
        self.prefix.join("Caskroom").join(&cask.token)
    }
}

/// The brew executable. Apps started from Finder do not get the shell's PATH, so look in
/// the standard prefixes for Apple silicon and Intel Macs.
pub fn find() -> Option<PathBuf> {
    ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn command(brew: &Path) -> Command {
    let mut cmd = Command::new(brew);
    // Never update Homebrew as a side effect of looking at it or removing something.
    cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1");
    cmd
}

/// Installed formulae and casks. Takes a few seconds; call it off the main thread.
pub fn load(brew: &Path) -> io::Result<Brew> {
    let out = command(brew).args(["info", "--json=v2", "--installed"]).output()?;
    if !out.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    let prefix = brew.parent().and_then(Path::parent).unwrap_or(Path::new("/opt/homebrew"));
    parse(&out.stdout, brew, prefix)
}

#[derive(Deserialize)]
struct Info {
    formulae: Vec<RawFormula>,
    casks: Vec<RawCask>,
}

#[derive(Deserialize)]
struct RawFormula {
    name: String,
    installed: Vec<RawInstall>,
}

#[derive(Deserialize)]
struct RawInstall {
    version: String,
    #[serde(default)]
    installed_on_request: bool,
}

#[derive(Deserialize)]
struct RawCask {
    token: String,
    #[serde(default)]
    name: Vec<String>,
    installed: Option<String>,
    #[serde(default)]
    artifacts: Vec<serde_json::Value>,
}

fn parse(json: &[u8], brew: &Path, prefix: &Path) -> io::Result<Brew> {
    let info: Info = serde_json::from_slice(json).map_err(io::Error::other)?;
    let formulae = info
        .formulae
        .into_iter()
        .filter(|f| !f.installed.is_empty())
        .map(|f| Formula {
            dependency: !f.installed.iter().any(|i| i.installed_on_request),
            versions: f.installed.into_iter().map(|i| i.version).collect(),
            name: f.name,
        })
        .collect();
    let casks = info
        .casks
        .into_iter()
        .map(|c| Cask {
            name: c.name.into_iter().next().unwrap_or_else(|| c.token.clone()),
            version: c.installed.unwrap_or_default(),
            apps: c.artifacts.iter().filter_map(app_artifact).collect(),
            token: c.token,
        })
        .collect();
    Ok(Brew { executable: brew.to_path_buf(), prefix: prefix.to_path_buf(), formulae, casks })
}

/// `{"app": ["Foo.app"]}` installs Foo.app; `{"app": ["Foo.app", {"target": "Bar.app"}]}`
/// installs it as Bar.app.
fn app_artifact(artifact: &serde_json::Value) -> Option<String> {
    let app = artifact.get("app")?.as_array()?;
    let target = app.get(1).and_then(|o| o.get("target")).and_then(|t| t.as_str());
    let name = target.or_else(|| app.first()?.as_str())?;
    Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned())
}

fn run(brew: &Path, args: &[&str]) -> io::Result<()> {
    let out = command(brew).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.lines().find(|l| l.starts_with("Error:")).unwrap_or(msg.trim());
        Err(io::Error::other(msg.trim_start_matches("Error:").trim().to_string()))
    }
}

/// Fails when other formulae depend on it; brew's message says which.
pub fn uninstall_formula(brew: &Path, name: &str) -> io::Result<()> {
    run(brew, &["uninstall", "--formula", name])
}

pub fn uninstall_cask(brew: &Path, token: &str) -> io::Result<()> {
    run(brew, &["uninstall", "--cask", token])
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = r#"{
      "formulae": [
        {"name": "zstd", "installed": [{"version": "1.5.7_1", "installed_on_request": false}]},
        {"name": "ripgrep", "installed": [
          {"version": "14.0.0", "installed_on_request": true},
          {"version": "14.1.1", "installed_on_request": true}]},
        {"name": "gone", "installed": []}
      ],
      "casks": [
        {"token": "kitty", "name": ["kitty"], "installed": "0.49.2",
         "artifacts": [{"app": ["kitty.app"]}, {"zap": [{"trash": ["~/.config/kitty"]}]}]},
        {"token": "blender@lts", "name": ["Blender LTS"], "installed": "5.2.2",
         "artifacts": [{"app": ["Blender.app", {"target": "Blender LTS.app"}]}]},
        {"token": "codex", "name": ["Codex"], "installed": "0.163.0",
         "artifacts": [{"binary": ["codex"]}]}
      ]
    }"#;

    fn brew() -> Brew {
        parse(INFO.as_bytes(), Path::new("/opt/homebrew/bin/brew"), Path::new("/opt/homebrew"))
            .unwrap()
    }

    #[test]
    fn formulae_list_every_installed_version_and_whether_they_were_asked_for() {
        // Old versions and leftover dependencies are what `brew cleanup` and
        // `brew autoremove` reclaim, so both must be visible.
        let b = brew();
        assert_eq!(
            b.formulae,
            vec![
                Formula { name: "zstd".into(), versions: vec!["1.5.7_1".into()], dependency: true },
                Formula {
                    name: "ripgrep".into(),
                    versions: vec!["14.0.0".into(), "14.1.1".into()],
                    dependency: false,
                },
            ]
        );
        assert_eq!(b.cellar(&b.formulae[0]), PathBuf::from("/opt/homebrew/Cellar/zstd"));
    }

    #[test]
    fn casks_know_which_apps_they_installed() {
        // A cask's app must be listed under the cask, not a second time as a plain app.
        let b = brew();
        let apps: Vec<(&str, Vec<String>)> =
            b.casks.iter().map(|c| (c.token.as_str(), c.apps.clone())).collect();
        assert_eq!(
            apps,
            vec![
                ("kitty", vec!["kitty.app".to_string()]),
                ("blender@lts", vec!["Blender LTS.app".to_string()]),
                ("codex", vec![]),
            ]
        );
        assert_eq!(b.casks[1].name, "Blender LTS");
        assert_eq!(b.caskroom(&b.casks[2]), PathBuf::from("/opt/homebrew/Caskroom/codex"));
    }

    /// A stand-in for brew that records its arguments and fails like brew does when a
    /// formula is still needed, so uninstalling can be tested without touching Homebrew.
    fn fake_brew(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("brew");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@ $HOMEBREW_NO_AUTO_UPDATE\" > {}\n\
                 if [ \"$3\" = needed ]; then\n\
                 echo 'Warning: something' >&2\n\
                 echo 'Error: Refusing to uninstall needed because it is required by app' >&2\n\
                 exit 1\nfi\n",
                dir.join("args").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn uninstalling_runs_brew_without_updating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let brew = fake_brew(tmp.path());
        uninstall_formula(&brew, "ripgrep").unwrap();
        let args = std::fs::read_to_string(tmp.path().join("args")).unwrap();
        assert_eq!(args.trim(), "uninstall --formula ripgrep 1");
        uninstall_cask(&brew, "kitty").unwrap();
        let args = std::fs::read_to_string(tmp.path().join("args")).unwrap();
        assert_eq!(args.trim(), "uninstall --cask kitty 1");
    }

    #[test]
    fn a_refused_uninstall_reports_brews_reason() {
        // The user must learn why nothing was removed, e.g. which formula still needs it.
        let tmp = tempfile::tempdir().unwrap();
        let brew = fake_brew(tmp.path());
        let err = uninstall_formula(&brew, "needed").unwrap_err();
        assert_eq!(err.to_string(), "Refusing to uninstall needed because it is required by app");
    }

    #[test]
    fn malformed_output_is_an_error() {
        assert!(parse(b"not json", Path::new("brew"), Path::new("/x")).is_err());
    }
}
