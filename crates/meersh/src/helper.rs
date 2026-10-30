//! `meersh __exec`. The engine starts external programs through this helper when they must
//! run in an overlay sandbox or under Landlock. The helper is a fresh single threaded
//! process, which is what `unshare` and `landlock_restrict_self` need. It sets up the
//! confinement and then replaces itself with the program.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use landlock::{ABI, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, path_beneath_rules};
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};

#[derive(Default)]
struct Spec {
    overlay: Option<(PathBuf, PathBuf, PathBuf)>,
    restrict_write: bool,
    write: Vec<PathBuf>,
    no_net: bool,
    cwd: Option<PathBuf>,
    program: Vec<String>,
}

fn parse(args: &[String]) -> Result<Spec, String> {
    let mut spec = Spec::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--overlay" => spec.overlay = Some((value()?.into(), value()?.into(), value()?.into())),
            "--restrict-write" => spec.restrict_write = true,
            "--write" => spec.write.push(value()?.into()),
            "--no-net" => spec.no_net = true,
            "--cwd" => spec.cwd = Some(value()?.into()),
            "--" => {
                spec.program = it.cloned().collect();
                break;
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    if spec.program.is_empty() {
        return Err("no program given".into());
    }
    Ok(spec)
}

/// A private user and mount namespace in which `lower` is an overlay. The uid and gid map
/// to themselves, so programs see their normal user and files in the upper directory
/// belong to the user.
fn enter_overlay(lower: &Path, upper: &Path, work: &Path) -> Result<(), String> {
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS).map_err(|e| format!("unshare ({e})"))?;
    std::fs::write("/proc/self/setgroups", "deny").map_err(|e| format!("setgroups ({e})"))?;
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).map_err(|e| format!("uid_map ({e})"))?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).map_err(|e| format!("gid_map ({e})"))?;
    let options = format!("lowerdir={},upperdir={},workdir={}", lower.display(), upper.display(), work.display());
    mount(Some("overlay"), lower, Some("overlay"), MsFlags::empty(), Some(options.as_str()))
        .map_err(|e| format!("mounting the overlay ({e})"))
}

/// Writes are limited to the given roots plus /dev and the temp directory, which most
/// programs need. Reads stay allowed everywhere. `no_net` blocks TCP bind and connect.
fn restrict(spec: &Spec) -> Result<(), String> {
    let abi = ABI::V5;
    let mut ruleset = Ruleset::default();
    if spec.restrict_write {
        ruleset = ruleset.handle_access(AccessFs::from_write(abi)).map_err(|e| e.to_string())?;
    }
    if spec.no_net {
        ruleset = ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp).map_err(|e| e.to_string())?;
    }
    let mut created = ruleset.create().map_err(|e| e.to_string())?;
    if spec.restrict_write {
        let mut roots = spec.write.clone();
        roots.push("/dev".into());
        roots.push(std::env::temp_dir());
        roots.retain(|r| r.exists());
        created =
            created.add_rules(path_beneath_rules(&roots, AccessFs::from_write(abi))).map_err(|e| e.to_string())?;
    }
    created.restrict_self().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn run(args: &[String]) -> ! {
    let fail = |msg: String| -> ! {
        eprintln!("meersh: {msg}");
        std::process::exit(126);
    };
    let spec = parse(args).unwrap_or_else(|e| fail(e));
    if let Some((lower, upper, work)) = &spec.overlay {
        enter_overlay(lower, upper, work).unwrap_or_else(|e| fail(format!("cannot create the sandbox, {e}")));
    }
    // Enter the directory again, so the path resolves through the overlay.
    if let Some(cwd) = &spec.cwd {
        std::env::set_current_dir(cwd).unwrap_or_else(|e| fail(format!("cannot enter {} ({e})", cwd.display())));
    }
    if spec.restrict_write || spec.no_net {
        restrict(&spec).unwrap_or_else(|e| fail(format!("cannot apply Landlock, {e}")));
    }
    let error = std::process::Command::new(&spec.program[0]).args(&spec.program[1..]).exec();
    fail(format!("{}: {error}", spec.program[0]))
}
