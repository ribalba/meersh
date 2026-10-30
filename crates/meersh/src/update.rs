//! The update check. An interactive meersh asks its repository once a day for the version on
//! the main branch and offers to build and install it when it is newer. Raising the version in
//! Cargo.toml on main is the release, the way `make install` from a checkout is the install.

use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::unistd::{AccessFlags, access};

use crate::consent;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The repository is asked at most once a day, and an update is offered at most once a day.
const DAY: u64 = 24 * 60 * 60;
/// A slow answer still arrives in the background and is kept for the next start.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The first prompt waits this long for the answer at most.
const WAIT: Duration = Duration::from_secs(1);

/// What the last check found and what the user answered, in `~/.local/share/meersh/update.txt`.
#[derive(Default, Debug, PartialEq)]
struct State {
    /// Seconds since the epoch of the last request to the repository, answered or not.
    checked: u64,
    latest: Option<String>,
    /// Seconds since the epoch of the last offer.
    asked: u64,
    /// A version the user does not want, or was told about because meersh cannot install it.
    skip: Option<String>,
}

impl State {
    fn path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("meersh").join("update.txt"))
    }

    fn parse(text: &str) -> State {
        let mut state = State::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once(' ')) {
            let value = value.trim();
            match key {
                "checked" => state.checked = value.parse().unwrap_or(0),
                "latest" => state.latest = Some(value.to_string()),
                "asked" => state.asked = value.parse().unwrap_or(0),
                "skip" => state.skip = Some(value.to_string()),
                _ => {}
            }
        }
        state
    }

    fn text(&self) -> String {
        let mut text = format!("checked {}\nasked {}\n", self.checked, self.asked);
        for (key, value) in [("latest", &self.latest), ("skip", &self.skip)] {
            if let Some(value) = value {
                text.push_str(&format!("{key} {value}\n"));
            }
        }
        text
    }

    fn load() -> State {
        State::parse(&State::path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default())
    }

    /// Rereads the file before it changes it, because another meersh may have written it since.
    fn change(change: impl FnOnce(&mut State)) {
        let Some(path) = State::path() else { return };
        let mut state = State::load();
        change(&mut state);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, state.text());
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The leading numbers of a version, so `v0.2.0-rc1` is `[0, 2, 0]`. `None` without any.
fn numbers(version: &str) -> Option<Vec<u64>> {
    let mut numbers = Vec::new();
    for part in version.trim().trim_start_matches('v').split('.') {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        numbers.push(digits.parse().ok()?);
        if digits.len() != part.len() {
            break;
        }
    }
    (!numbers.is_empty()).then_some(numbers)
}

/// Whether `latest` is newer than `current`. A version that cannot be read never is, so a
/// broken answer cannot make meersh ask on every start.
fn is_newer(latest: &str, current: &str) -> bool {
    let (Some(mut latest), Some(mut current)) = (numbers(latest), numbers(current)) else { return false };
    let width = latest.len().max(current.len());
    latest.resize(width, 0);
    current.resize(width, 0);
    latest > current
}

/// The workspace version in a Cargo.toml.
fn manifest_version(text: &str) -> Option<String> {
    let manifest: toml::Table = toml::from_str(text).ok()?;
    manifest.get("workspace")?.get("package")?.get("version")?.as_str().map(str::to_string)
}

/// The version in Cargo.toml on the main branch of the repository.
fn fetch_latest() -> Option<String> {
    let raw = REPOSITORY.replace("https://github.com/", "https://raw.githubusercontent.com/");
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
    let mut response = agent.get(&format!("{raw}/main/Cargo.toml")).call().ok()?;
    manifest_version(&response.body_mut().read_to_string().ok()?)
}

/// Asks the repository in a thread of its own. The answer is saved when it comes, so one that
/// takes longer than `WAIT` is offered at the next start instead of holding up this one. The
/// request counts before it is sent, so a network that never answers holds up one start a day.
fn refresh() -> Option<String> {
    State::change(|state| state.checked = now());
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let latest = fetch_latest();
        if let Some(latest) = &latest {
            State::change(|state| state.latest = Some(latest.clone()));
        }
        let _ = send.send(latest);
    });
    receive.recv_timeout(WAIT).ok().flatten()
}

/// `[update] check = false` in the config or `MEERSH_NO_UPDATE_CHECK` in the environment turn
/// the check off.
fn enabled(config: &Path) -> bool {
    if std::env::var_os("MEERSH_NO_UPDATE_CHECK").is_some() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(config) else { return true };
    let check = toml::from_str::<toml::Table>(&text).ok().and_then(|t| t.get("update")?.get("check")?.as_bool());
    check.unwrap_or(true)
}

/// The prefix `make install` put this meersh into, when the user can write to it. A package, an
/// install by root or a build in a checkout is updated the way it was installed instead.
fn prefix(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" || access(bin, AccessFlags::W_OK).is_err() {
        return None;
    }
    bin.parent().map(Path::to_path_buf)
}

/// Offers a newer meersh before the first prompt. It stays quiet unless stdin and stdout are a
/// terminal.
pub fn offer(config: &Path) {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() || !enabled(config) {
        return;
    }
    let state = State::load();
    let now = now();
    if now < state.asked + DAY {
        return;
    }
    let latest = if now < state.checked + DAY { state.latest } else { refresh().or(state.latest) };
    let Some(latest) = latest.filter(|l| is_newer(l, CURRENT) && state.skip.as_deref() != Some(l.as_str())) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else { return };
    State::change(|state| state.asked = now);
    let Some(prefix) = prefix(&exe) else {
        println!(
            "meersh {latest} is available, this is {CURRENT}. meersh cannot write to {}, so update it the way you \
             installed it.",
            exe.parent().unwrap_or(&exe).display()
        );
        return State::change(|state| state.skip = Some(latest));
    };
    println!(
        "meersh {latest} is available, this is {CURRENT}. y builds it and installs it into {}, s skips this version.",
        prefix.display()
    );
    match consent::read_answer("update now? [y/N/s]").as_str() {
        "y" | "yes" => {}
        "s" | "skip" => return State::change(|state| state.skip = Some(latest)),
        _ => return,
    }
    if let Err(e) = install(&prefix) {
        return eprintln!("meersh: the update failed, {e}. This is still meersh {CURRENT}.");
    }
    // Nothing has run yet, so the new meersh can take over this process.
    println!("meersh {latest} is installed and starts now.");
    let e = Command::new(&exe).args(std::env::args_os().skip(1)).exec();
    eprintln!("meersh: cannot start {} ({e}), start it again yourself.", exe.display());
}

/// Fetches main into a clone of its own and runs `make install` there. The clone stays in
/// `~/.cache/meersh/source`, so the next update builds only what changed.
fn install(prefix: &Path) -> Result<(), String> {
    let source = dirs::cache_dir().ok_or("there is no cache directory")?.join("meersh").join("source");
    println!("Building in {}. The first build takes a few minutes.", source.display());
    if source.join(".git").is_dir() {
        run(Command::new("git").arg("-C").arg(&source).args(["fetch", "--depth", "1", "origin", "main"]))?;
        run(Command::new("git").arg("-C").arg(&source).args(["checkout", "--force", "--detach", "FETCH_HEAD"]))?;
    } else {
        run(Command::new("git").args(["clone", "--depth", "1", "--branch", "main", REPOSITORY]).arg(&source))?;
    }
    run(Command::new("make").arg("-C").arg(&source).arg("install").arg(format!("PREFIX={}", prefix.display())))
}

fn run(command: &mut Command) -> Result<(), String> {
    let name = command.get_program().to_string_lossy().into_owned();
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{name} failed ({status})")),
        Err(e) => Err(format!("cannot run {name} ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::{State, is_newer, manifest_version, numbers};

    #[test]
    fn versions() {
        assert_eq!(numbers("0.2.0"), Some(vec![0, 2, 0]));
        assert_eq!(numbers("v1.10"), Some(vec![1, 10]));
        assert_eq!(numbers("0.2.0-rc1"), Some(vec![0, 2, 0]));
        assert_eq!(numbers("unknown"), None);
        assert_eq!(numbers(""), None);
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.2.0-rc1", "0.2.0"));
        assert!(!is_newer("<!DOCTYPE html>", "0.1.0"));
    }

    #[test]
    fn manifests() {
        let manifest = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.3.1\"\nedition = \"2024\"\n";
        assert_eq!(manifest_version(manifest), Some("0.3.1".to_string()));
        assert_eq!(manifest_version("[package]\nversion = \"0.3.1\"\n"), None);
        assert_eq!(manifest_version("404: Not Found"), None);
    }

    #[test]
    fn state() {
        let state = State { checked: 10, latest: Some("0.2.0".into()), asked: 5, skip: None };
        assert_eq!(State::parse(&state.text()), state);
        assert_eq!(State::parse("checked x\nnonsense\n"), State::default());
    }
}
