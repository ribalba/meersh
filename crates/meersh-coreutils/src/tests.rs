//! Every test works inside its own directory under the system temp dir and only uses
//! absolute paths, because tests run in parallel and share one working directory. `rm` is
//! only ever called with `--permanent`, `--dry-run` or in plan mode, so nothing lands in the
//! real trash. Existing files are only touched or overwritten in plan mode, because an
//! executed write keeps a snapshot of the old content in the user's cache directory. The
//! overwrite itself is tested on the copy mechanism directly.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex, PoisonError};

use meersh_engine::authority::{Plan, proc_id};
use meersh_engine::value::{Record, Value};
use meersh_engine::{Engine, ExecMode, PipelineData, Principal, Result, Stack, Verdict};

use crate::mechanism;
use crate::nav::mode_string;
use crate::proc::single_process;
use crate::register;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("meersh-coreutils-{}-{name}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// The path as a quoted meersh string literal.
    fn q(&self, rel: &str) -> String {
        format!("\"{}\"", self.path(rel).display())
    }

    fn file(&self, rel: &str, content: &str) {
        fs::write(self.path(rel), content).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An engine with the coreutils and the generic commands such as `each` and `where`. Its
/// ledger lives in memory, so tests never touch the shared one.
fn engine() -> Arc<Engine> {
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    register(&mut engine);
    Arc::new(engine)
}

fn run_in(engine: &Arc<Engine>, stack: &mut Stack, src: &str) -> Result<Value> {
    let mut out = Value::Nothing;
    engine.run_script(stack, src, false, &mut |data: PipelineData| {
        out = data.into_value()?;
        Ok(())
    })?;
    Ok(out)
}

fn run(src: &str) -> Result<Value> {
    run_in(&engine(), &mut Stack::default(), src)
}

/// Runs `src` in plan mode, where nothing changes, and returns the plan and the output.
fn plan(engine: &Arc<Engine>, stack: &Stack, src: &str) -> (Plan, Value) {
    let plan = Arc::new(Mutex::new(Plan::default()));
    let out = run_in(engine, &mut stack.with_mode(ExecMode::Plan(plan.clone())), src).unwrap();
    (std::mem::take(&mut *plan.lock().unwrap()), out)
}

fn apply(engine: &Arc<Engine>, stack: &Stack, plan: &Plan, src: &str) -> Value {
    run_in(engine, &mut stack.with_mode(ExecMode::Apply(Arc::new(plan.approvable_keys()))), src).unwrap()
}

fn actions(value: &Value) -> Vec<&str> {
    rows(value).iter().map(|r| text(r, "action")).collect()
}

fn rows(value: &Value) -> Vec<&Record> {
    match value {
        Value::List(items) => items.iter().map(|v| v.as_record().expect("a table")).collect(),
        other => panic!("expected a table, got {other:?}"),
    }
}

fn text<'a>(row: &'a Record, field: &str) -> &'a str {
    row.get(field).and_then(Value::as_str).unwrap_or_else(|| panic!("no string field {field} in {row:?}"))
}

fn error(src: &str) -> String {
    run(src).expect_err("the command should fail").to_string()
}

fn shown(path: PathBuf) -> String {
    path.display().to_string()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Kind, permission bits and content or link target of every path below `root`, relative
/// to it, so that two copies of a tree can be compared.
fn snapshot(root: &Path) -> Vec<String> {
    let mut entries: Vec<String> = mechanism::tree(root)
        .iter()
        .map(|path| {
            let rel = path.strip_prefix(root).unwrap().display();
            let meta = fs::symlink_metadata(path).unwrap();
            if meta.is_symlink() {
                format!("{rel} link {}", fs::read_link(path).unwrap().display())
            } else if meta.is_dir() {
                format!("{rel} dir {:o}", mode(path))
            } else {
                format!("{rel} file {:o} {}", mode(path), fs::read_to_string(path).unwrap())
            }
        })
        .collect();
    entries.sort();
    entries
}

/// Permission bits do not stop root, so the tests about unreadable files skip themselves.
fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

#[test]
fn mode_strings() {
    assert_eq!(mode_string(0o755), "rwxr-xr-x");
    assert_eq!(mode_string(0o100644), "rw-r--r--");
    assert_eq!(mode_string(0o4755), "rwsr-xr-x");
    assert_eq!(mode_string(0o2644), "rw-r-Sr--");
    assert_eq!(mode_string(0o1777), "rwxrwxrwt");
    assert_eq!(mode_string(0), "---------");
}

#[test]
fn ls_lists_relative_names_sorted_and_typed() {
    let t = TempDir::new("ls");
    t.file("b.txt", "bb");
    t.file("a.txt", "a");
    t.file(".hidden", "");
    fs::create_dir(t.path("sub")).unwrap();
    symlink("a.txt", t.path("link")).unwrap();

    let out = run(&format!("ls {}", t.q(""))).unwrap();
    let names: Vec<&str> = rows(&out).iter().map(|r| text(r, "name")).collect();
    let expected: Vec<String> = ["a.txt", "b.txt", "link", "sub"].iter().map(|n| shown(t.path(n))).collect();
    assert_eq!(names, expected);
    let types: Vec<&str> = rows(&out).iter().map(|r| text(r, "type")).collect();
    assert_eq!(types, ["file", "file", "symlink", "dir"]);
    let first = rows(&out)[0];
    assert_eq!(first.type_name(), Some("fs.entry"));
    assert_eq!(first.fields.keys().collect::<Vec<_>>(), ["name", "type", "size", "modified"]);
    assert_eq!(first.get("size"), Some(&Value::Filesize(1)));
    assert!(matches!(first.get("modified"), Some(Value::Date(_))));
    assert!(first.id().is_some_and(|id| id.starts_with("file:")), "{first:?}");

    assert_eq!(rows(&run(&format!("ls -a {}", t.q(""))).unwrap()).len(), 5);
    assert_eq!(rows(&run(&format!("ls {}", t.q("*.txt"))).unwrap()).len(), 2);

    let long = run(&format!("ls -l {}", t.q("link"))).unwrap();
    let link = rows(&long)[0];
    assert_eq!(text(link, "name"), shown(t.path("link")));
    assert_eq!(text(link, "target"), "a.txt");
    assert_eq!(link.fields.keys().last().map(String::as_str), Some("target"));
    assert!(error(&format!("ls {}", t.q("missing"))).contains("cannot access"));
}

#[test]
fn mkdir_and_touch_report_what_they_did() {
    let t = TempDir::new("mkdir");
    let action = |src: String| text(rows(&run(&src).unwrap())[0], "action").to_string();
    assert_eq!(action(format!("mkdir {}", t.q("p/q"))), "created");
    assert_eq!(action(format!("mkdir {}", t.q("p/q"))), "exists");
    assert!(t.path("p/q").is_dir());
    assert_eq!(action(format!("touch {}", t.q("f"))), "created");
    let (plan, out) = plan(&engine(), &Stack::default(), &format!("touch {}", t.q("f")));
    assert_eq!(actions(&out), ["would touch"]);
    assert_eq!(plan.actions[0].kind, "fs.write");
}

#[test]
fn mkdir_creates_and_owns_every_missing_directory() {
    let t = TempDir::new("mkdir-own");
    let engine = engine();
    let src = format!("mkdir {}", t.q("a/b/c"));
    let (plan_before, out) = plan(&engine, &Stack::default(), &src);
    assert_eq!(plan_before.actions.len(), 3);
    assert_eq!(actions(&out), ["would create", "would create", "would create"]);
    assert!(!t.path("a").exists());

    let out = run_in(&engine, &mut Stack::default(), &src).unwrap();
    let paths: Vec<&str> = rows(&out).iter().map(|r| text(r, "path")).collect();
    assert_eq!(paths, [shown(t.path("a")), shown(t.path("a/b")), shown(t.path("a/b/c"))]);
    // The whole tree was created by the session, so a model may remove it.
    let actor = Stack::default().with_principal(Principal::Actor);
    let (p, _) = plan(&engine, &actor, &format!("rm -r {}", t.q("a")));
    assert_eq!(p.actions[0].verdict, Verdict::Allow, "{:?}", p.actions);
}

#[test]
fn rm_refuses_a_directory_without_recursive() {
    let t = TempDir::new("rm-dir");
    fs::create_dir(t.path("dir")).unwrap();
    t.file("dir/keep", "x");
    t.file("file", "x");
    // The file comes first and must survive, because rm checks every path before acting.
    assert!(error(&format!("rm -p {} {}", t.q("file"), t.q("dir"))).contains("is a directory"));
    assert!(t.path("dir/keep").exists());
    assert!(t.path("file").exists());
}

#[test]
fn rm_dry_run_deletes_nothing() {
    let t = TempDir::new("rm-dry");
    fs::create_dir(t.path("dir")).unwrap();
    t.file("file", "12345");
    let out = run(&format!("rm -n -r {} {}", t.q("dir"), t.q("file"))).unwrap();
    let actions: Vec<&str> = rows(&out).iter().map(|r| text(r, "action")).collect();
    assert_eq!(actions, ["would trash", "would trash"]);
    assert_eq!(rows(&out)[0].get("size"), Some(&Value::Nothing));
    assert_eq!(rows(&out)[1].get("size"), Some(&Value::Filesize(5)));
    let out = run(&format!("rm --dry-run --permanent {}", t.q("file"))).unwrap();
    assert_eq!(text(rows(&out)[0], "action"), "would delete");
    assert!(t.path("dir").is_dir());
    assert!(t.path("file").exists());
}

#[test]
fn rm_permanent_takes_piped_entries() {
    let t = TempDir::new("rm-pipe");
    fs::create_dir(t.path("sub")).unwrap();
    t.file("sub/a.rs", "a");
    t.file("sub/b.rs", "b");
    let out = run(&format!("ls {} | rm -p", t.q("sub"))).unwrap();
    assert_eq!(rows(&out).len(), 2);
    assert_eq!(text(rows(&out)[0], "action"), "deleted");
    assert!(!t.path("sub/a.rs").exists());
    assert!(!t.path("sub/b.rs").exists());

    assert!(error(&format!("rm -p {}", t.q("missing"))).contains("no such file"));
    assert!(rows(&run(&format!("rm -p -f {}", t.q("missing"))).unwrap()).is_empty());
}

#[test]
fn rm_refuses_protected_paths() {
    let t = TempDir::new("rm-protected");
    for target in ["/".to_string(), ".".into(), "..".into(), "~".into(), t.q(".."), t.q("sub/.")] {
        let msg = error(&format!("rm -n -r {target}"));
        assert!(msg.contains("refusing"), "{target} gave {msg}");
    }
}

#[test]
fn cp_copies_files_and_trees() {
    let t = TempDir::new("cp");
    t.file("a.txt", "hello");
    fs::create_dir(t.path("dir")).unwrap();
    t.file("dir/inner.txt", "inner");

    let out = run(&format!("cp {} {}", t.q("a.txt"), t.q("dir"))).unwrap();
    let row = rows(&out)[0];
    assert_eq!(row.fields.keys().collect::<Vec<_>>(), ["path", "from", "action"]);
    assert_eq!(text(row, "path"), shown(t.path("dir/a.txt")));
    assert_eq!(text(row, "from"), shown(t.path("a.txt")));
    assert_eq!(fs::read_to_string(t.path("dir/a.txt")).unwrap(), "hello");

    assert!(error(&format!("cp {} {}", t.q("dir"), t.q("copy"))).contains("Use -r"));
    run(&format!("cp -r {} {}", t.q("dir"), t.q("copy"))).unwrap();
    assert_eq!(fs::read_to_string(t.path("copy/inner.txt")).unwrap(), "inner");

    assert!(error(&format!("cp {} {}", t.q("a.txt"), t.q("a.txt"))).contains("same file"));
    assert_eq!(fs::read_to_string(t.path("a.txt")).unwrap(), "hello");
    assert!(error(&format!("cp -r {} {}", t.q("dir"), t.q("dir/nested"))).contains("inside itself"));
}

#[test]
fn cp_owns_the_whole_copied_tree_but_not_the_source() {
    let t = TempDir::new("cp-own");
    fs::create_dir_all(t.path("src/deep")).unwrap();
    t.file("src/deep/file.txt", "written outside meersh");
    let engine = engine();
    let (p, out) = plan(&engine, &Stack::default(), &format!("cp -r {} {}", t.q("src"), t.q("copy")));
    assert_eq!(p.actions.len(), 1, "only the top level target is authorized");
    assert_eq!(actions(&out), ["would copy"]);
    run_in(&engine, &mut Stack::default(), &format!("cp -r {} {}", t.q("src"), t.q("copy"))).unwrap();

    let actor = Stack::default().with_principal(Principal::Actor);
    let (p, _) = plan(&engine, &actor, &format!("rm -r {} {}", t.q("copy"), t.q("src")));
    let verdicts: Vec<Verdict> = p.actions.iter().map(|a| a.verdict).collect();
    assert_eq!(verdicts[0], Verdict::Allow, "{:?}", p.actions);
    assert_ne!(verdicts[1], Verdict::Allow, "{:?}", p.actions);
}

#[test]
fn mv_renames_and_moves_into_directories() {
    let t = TempDir::new("mv");
    t.file("a.txt", "a");
    t.file("b.txt", "b");
    fs::create_dir(t.path("dir")).unwrap();

    let out = run(&format!("mv {} {}", t.q("a.txt"), t.q("renamed.txt"))).unwrap();
    assert_eq!(text(rows(&out)[0], "action"), "moved");
    assert!(!t.path("a.txt").exists());
    assert!(t.path("renamed.txt").exists());

    assert!(error(&format!("mv {} {} {}", t.q("renamed.txt"), t.q("b.txt"), t.q("nodir"))).contains("not a directory"));
    let out = run(&format!("ls {} | mv {}", t.q("*.txt"), t.q("dir"))).unwrap();
    assert_eq!(rows(&out).len(), 2);
    assert!(t.path("dir/renamed.txt").exists());
    assert!(t.path("dir/b.txt").exists());
}

#[test]
fn cp_keeps_symlinks_and_permissions_of_a_tree() {
    let t = TempDir::new("cp-tree");
    fs::create_dir_all(t.path("src/sub")).unwrap();
    t.file("src/run.sh", "#!/bin/sh\n");
    t.file("src/sub/shared", "group writable");
    symlink("run.sh", t.path("src/link")).unwrap();
    symlink("../missing", t.path("src/sub/dangling")).unwrap();
    chmod(&t.path("src/run.sh"), 0o750);
    chmod(&t.path("src/sub/shared"), 0o664);
    chmod(&t.path("src/sub"), 0o700);

    let engine = engine();
    let out = run_in(&engine, &mut Stack::default(), &format!("cp -r {} {}", t.q("src"), t.q("copy"))).unwrap();
    assert_eq!(actions(&out), ["copied"]);
    assert_eq!(fs::read_link(t.path("copy/link")).unwrap(), Path::new("run.sh"));
    assert_eq!(fs::read_link(t.path("copy/sub/dangling")).unwrap(), Path::new("../missing"));
    let modes = [mode(&t.path("copy/run.sh")), mode(&t.path("copy/sub")), mode(&t.path("copy/sub/shared"))];
    assert_eq!(modes, [0o750, 0o700, 0o664]);
    // uutils makes the same tree as the std implementation it replaced.
    mechanism::std_impl::copy(&t.path("src"), &t.path("by-std"), true).unwrap();
    assert_eq!(snapshot(&t.path("copy")), snapshot(&t.path("by-std")));

    // Every path the copy created belongs to the session, the links included.
    let actor = Stack::default().with_principal(Principal::Actor);
    let src = format!("rm {} {} {}", t.q("copy/link"), t.q("copy/sub/dangling"), t.q("src/link"));
    let (p, _) = plan(&engine, &actor, &src);
    let verdicts: Vec<Verdict> = p.actions.iter().map(|a| a.verdict).collect();
    assert_eq!(verdicts[..2], [Verdict::Allow, Verdict::Allow], "{:?}", p.actions);
    assert_ne!(verdicts[2], Verdict::Allow, "{:?}", p.actions);
}

#[test]
fn cp_onto_an_existing_file() {
    let t = TempDir::new("cp-onto");
    t.file("new.txt", "new");
    chmod(&t.path("new.txt"), 0o640);
    for name in ["old.txt", "by-std.txt"] {
        t.file(name, "old and longer");
        chmod(&t.path(name), 0o600);
    }
    let (p, out) = plan(&engine(), &Stack::default(), &format!("cp {} {}", t.q("new.txt"), t.q("old.txt")));
    assert_eq!((p.actions[0].kind.as_str(), actions(&out)), ("fs.write", vec!["would copy"]));
    assert_eq!(fs::read_to_string(t.path("old.txt")).unwrap(), "old and longer");

    // The content is replaced and the permissions are taken over, as std did.
    mechanism::copy(&t.path("new.txt"), &t.path("old.txt"), false).unwrap();
    mechanism::std_impl::copy(&t.path("new.txt"), &t.path("by-std.txt"), false).unwrap();
    for name in ["old.txt", "by-std.txt"] {
        assert_eq!(fs::read_to_string(t.path(name)).unwrap(), "new", "{name}");
        assert_eq!(mode(&t.path(name)), 0o640, "{name}");
    }

    // A file never replaces a directory and a directory never replaces a file.
    fs::create_dir_all(t.path("dir/new.txt")).unwrap();
    let msg = error(&format!("cp {} {}", t.q("new.txt"), t.q("dir")));
    assert!(msg.contains("(the target is a directory and the source is not)"), "{msg}");
    assert!(t.path("dir/new.txt").is_dir());
    let why = mechanism::copy(&t.path("dir"), &t.path("old.txt"), true).unwrap_err();
    assert_eq!(why, "the source is a directory and the target is not");
}

#[test]
fn a_copy_that_left_files_out_fails_and_names_them() {
    if is_root() {
        return;
    }
    let t = TempDir::new("cp-left-out");
    fs::create_dir(t.path("tree")).unwrap();
    t.file("tree/open", "open");
    t.file("tree/locked", "secret");
    chmod(&t.path("tree/locked"), 0o000);
    let engine = engine();
    let result = run_in(&engine, &mut Stack::default(), &format!("cp -r {} {}", t.q("tree"), t.q("copy")));
    chmod(&t.path("tree/locked"), 0o600);

    // uu_cp itself skips the file, prints why on stderr and reports success.
    let msg = result.expect_err("an unreadable file").to_string();
    let expected = format!("'{}' was left out, uutils printed why on stderr", shown(t.path("tree/locked")));
    assert!(msg.contains(&expected), "{msg}");
    // What did arrive belongs to the session, so it can be cleaned up.
    assert_eq!(fs::read_to_string(t.path("copy/open")).unwrap(), "open");
    let actor = Stack::default().with_principal(Principal::Actor);
    let (p, _) = plan(&engine, &actor, &format!("rm -r {}", t.q("copy")));
    assert_eq!(p.actions[0].verdict, Verdict::Allow, "{:?}", p.actions);
}

#[test]
fn mv_moves_trees_across_directories() {
    let t = TempDir::new("mv-across");
    for dir in ["a", "std"] {
        fs::create_dir_all(t.path(&format!("{dir}/tree/sub"))).unwrap();
        t.file(&format!("{dir}/tree/sub/data"), "data");
        symlink("sub/data", t.path(&format!("{dir}/tree/link"))).unwrap();
        chmod(&t.path(&format!("{dir}/tree/sub")), 0o750);
    }
    t.file("a/note.txt", "note");
    fs::create_dir(t.path("b")).unwrap();
    let before = snapshot(&t.path("a/tree"));

    let out = run(&format!("mv {} {} {}", t.q("a/tree"), t.q("a/note.txt"), t.q("b"))).unwrap();
    let moved: Vec<[&str; 3]> =
        rows(&out).iter().map(|r| [text(r, "from"), text(r, "path"), text(r, "action")]).collect();
    let (tree, note) = (shown(t.path("b/tree")), shown(t.path("b/note.txt")));
    let (from_tree, from_note) = (shown(t.path("a/tree")), shown(t.path("a/note.txt")));
    assert_eq!(moved, [[from_tree.as_str(), &tree, "moved"], [from_note.as_str(), &note, "moved"]]);
    assert!(!t.path("a/tree").exists() && !t.path("a/note.txt").exists());
    assert_eq!(snapshot(&t.path("b/tree")), before);
    assert_eq!(fs::read_to_string(t.path("b/note.txt")).unwrap(), "note");
    mechanism::std_impl::move_path(&t.path("std/tree"), &t.path("b/by-std")).unwrap();
    assert_eq!(snapshot(&t.path("b/by-std")), before);

    // A file never replaces a directory, and a directory only replaces an empty one.
    fs::create_dir_all(t.path("a/note.txt")).unwrap();
    let msg = error(&format!("mv {} {}", t.q("b/note.txt"), t.q("a")));
    assert!(msg.contains("(the target is a directory and the source is not)"), "{msg}");
    fs::create_dir_all(t.path("a/tree/keep")).unwrap();
    let msg = error(&format!("mv {} {}", t.q("b/tree"), t.q("a")));
    assert!(msg.contains("(the target is a directory that is not empty)"), "{msg}");
    assert!(t.path("b/note.txt").is_file() && t.path("b/tree/sub/data").is_file());
}

#[test]
fn mv_keeps_a_moved_file_owned() {
    let t = TempDir::new("mv-own");
    fs::create_dir(t.path("elsewhere")).unwrap();
    t.file("foreign", "written outside meersh");
    let engine = engine();
    let mut user = Stack::default();
    run_in(&engine, &mut user, &format!("touch {}", t.q("mine"))).unwrap();
    run_in(&engine, &mut user, &format!("mv {} {} {}", t.q("mine"), t.q("foreign"), t.q("elsewhere"))).unwrap();

    let actor = Stack::default().with_principal(Principal::Actor);
    let (p, _) = plan(&engine, &actor, &format!("rm {} {}", t.q("elsewhere/mine"), t.q("elsewhere/foreign")));
    assert_eq!(p.actions[0].verdict, Verdict::Allow, "{:?}", p.actions);
    assert_ne!(p.actions[1].verdict, Verdict::Allow, "{:?}", p.actions);
}

#[test]
fn mv_gives_the_reason_without_uutils_noise() {
    if is_root() {
        return;
    }
    let t = TempDir::new("mv-denied");
    t.file("a.txt", "a");
    fs::create_dir(t.path("locked")).unwrap();
    chmod(&t.path("locked"), 0o500);
    let result = run(&format!("mv {} {}", t.q("a.txt"), t.q("locked")));
    chmod(&t.path("locked"), 0o700);
    let msg = result.expect_err("a read only directory").to_string();
    let (from, to) = (shown(t.path("a.txt")), shown(t.path("locked/a.txt")));
    assert!(msg.contains(&format!("cannot move '{from}' to '{to}' (permission denied)")), "{msg}");
    assert!(t.path("a.txt").is_file());
}

#[test]
fn open_parses_by_extension() {
    let t = TempDir::new("open");
    t.file("data.json", r#"{"b": 1, "a": [true, null]}"#);
    t.file("conf.toml", "[package]\nname = \"x\"\nwhen = 1979-05-27T07:32:00Z\n[deps]\nz = 1\n");
    fs::write(t.path("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();

    let json = run(&format!("open {}", t.q("data.json"))).unwrap();
    let json = json.as_record().unwrap();
    assert_eq!(json.fields.keys().collect::<Vec<_>>(), ["b", "a"]);
    assert_eq!(json.get("a"), Some(&Value::List(vec![Value::Bool(true), Value::Nothing])));

    let toml = run(&format!("open {}", t.q("conf.toml"))).unwrap();
    let toml = toml.as_record().unwrap();
    assert_eq!(toml.fields.keys().collect::<Vec<_>>(), ["package", "deps"]);
    assert!(matches!(toml.get("package").and_then(|p| p.follow("when")), Some(Value::Date(_))));

    assert!(matches!(run(&format!("open --raw {}", t.q("conf.toml"))).unwrap(), Value::String(_)));
    assert_eq!(run(&format!("open {}", t.q("blob.bin"))).unwrap(), Value::Binary(vec![0xff, 0xfe, 0x00]));
}

#[test]
fn which_tells_builtins_from_programs() {
    let which = |name: &str| run(&format!("which {name}")).unwrap();
    assert_eq!(which("ls").as_record().unwrap().get("kind"), Some(&Value::from("builtin")));
    let sh = which("sh");
    assert_eq!(sh.as_record().unwrap().get("kind"), Some(&Value::from("external")));
    assert!(matches!(sh.as_record().unwrap().get("path"), Some(Value::String(_))));
    let none = which("meersh-no-such-program");
    assert_eq!(none.as_record().unwrap().get("kind"), Some(&Value::from("not found")));
}

#[test]
fn ps_lists_this_process() {
    let out = run("ps").unwrap();
    let me = rows(&out).into_iter().find(|r| r.get("pid") == Some(&Value::Int(std::process::id().into()))).unwrap();
    assert_eq!(me.type_name(), Some("proc"));
    let keys: Vec<&String> = me.fields.keys().collect();
    assert_eq!(keys, ["pid", "ppid", "name", "user", "cpu", "mem", "status", "command"]);
    assert!(matches!(me.get("mem"), Some(Value::Filesize(_))));
    assert_eq!(me.id(), proc_id(std::process::id()).as_deref());
}

/// Waits for the child before cleaning up. Killing it first would race with the signal
/// under test, since Linux delivers a pending SIGKILL before SIGTERM.
fn reap(mut child: Child, ok: bool) -> Option<i32> {
    if !ok {
        let _ = child.kill();
    }
    child.wait().unwrap().signal()
}

#[test]
fn kill_signals_a_spawned_sleep() {
    let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let out = run(&format!("kill {}", child.id()));
    let ok = out.as_ref().is_ok_and(|v| rows(v)[0].get("ok") == Some(&Value::Bool(true)));
    assert_eq!(reap(child, ok), Some(nix::sys::signal::Signal::SIGTERM as i32));
    let out = out.unwrap();
    let row = rows(&out)[0];
    assert_eq!(row.fields.keys().collect::<Vec<_>>(), ["pid", "name", "signal", "action", "ok", "error"]);
    assert_eq!(text(row, "action"), "signaled");
    assert_eq!(text(row, "signal"), "SIGTERM");
    assert_eq!(row.get("error"), Some(&Value::Nothing));
}

#[test]
fn kill_takes_proc_records_and_signal_names() {
    let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let out = run(&format!("{{pid: {}, name: sleep}} | kill -s kill", child.id()));
    let ok = out.as_ref().is_ok_and(|v| rows(v)[0].get("ok") == Some(&Value::Bool(true)));
    assert_eq!(reap(child, ok), Some(nix::sys::signal::Signal::SIGKILL as i32));
    assert_eq!(text(rows(&out.unwrap())[0], "name"), "sleep");
}

/// Checked as a function and not through `kill 0`, which would hit the test runner's own
/// process group if the check were broken.
#[test]
fn kill_only_accepts_single_processes() {
    assert!(!single_process(0));
    assert!(!single_process(-1));
    assert!(!single_process(-9));
    assert!(!single_process(i64::from(i32::MAX) + 1));
    assert!(single_process(1234));
}

#[test]
fn kill_rejects_unknown_signals_before_sending() {
    let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let msg = run(&format!("kill -s NOPE {}", child.id())).map_err(|e| e.to_string());
    let survived = reap_if_alive(child);
    assert!(msg.expect_err("unknown signal").contains("unknown signal"));
    assert!(survived);
}

fn reap_if_alive(mut child: Child) -> bool {
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    alive
}

#[test]
fn an_actor_may_plan_to_remove_only_what_the_session_created() {
    let t = TempDir::new("actor-own");
    t.file("foreign", "written outside meersh");
    let engine = engine();
    run_in(&engine, &mut Stack::default(), &format!("touch {}", t.q("mine"))).unwrap();

    let actor = Stack::default().with_principal(Principal::Actor);
    let (p, out) = plan(&engine, &actor, &format!("rm {} {}", t.q("mine"), t.q("foreign")));
    assert_eq!(p.actions.len(), 2);
    assert_eq!(p.actions[0].verdict, Verdict::Allow, "{:?}", p.actions);
    assert_ne!(p.actions[1].verdict, Verdict::Allow, "{:?}", p.actions);
    assert_eq!(actions(&out)[0], "would trash");
    assert!(t.path("mine").exists() && t.path("foreign").exists(), "planning changes nothing");
}

#[test]
fn applying_a_plan_only_removes_the_approved_files() {
    let t = TempDir::new("actor-apply");
    t.file("foreign", "written outside meersh");
    let engine = engine();
    run_in(&engine, &mut Stack::default(), &format!("touch {}", t.q("mine"))).unwrap();

    let actor = Stack::default().with_principal(Principal::Actor);
    let src = format!("rm --permanent {} {}", t.q("mine"), t.q("foreign"));
    // A model may not act without a plan.
    assert!(run_in(&engine, &mut actor.clone(), &src).is_err());
    assert!(t.path("mine").exists());

    let (p, out) = plan(&engine, &actor, &src);
    assert_eq!(actions(&out), ["would delete", "refused"]);
    let out = apply(&engine, &actor, &p, &src);
    assert_eq!(actions(&out), ["deleted", "refused"]);
    assert!(rows(&out)[1].get("because").is_some());
    assert!(!t.path("mine").exists());
    assert!(t.path("foreign").exists());
}

#[test]
fn delete_method_through_each() {
    let t = TempDir::new("method-delete");
    t.file("junk.tmp", "x");
    let out = run(&format!("ls {} | each {{ $it.delete(--permanent) }}", t.q("junk.tmp"))).unwrap();
    let row = rows(&out)[0];
    assert_eq!(row.type_name(), Some("fs.change"));
    assert_eq!(text(row, "action"), "deleted");
    assert!(!t.path("junk.tmp").exists());
}

#[test]
fn a_stale_entry_is_refused() {
    let t = TempDir::new("stale");
    t.file("notes.md", "first");
    let engine = engine();
    let mut stack = Stack::default();
    let listed = run_in(&engine, &mut stack, &format!("ls {}", t.q("notes.md"))).unwrap();
    stack.set("f", Value::Record(rows(&listed)[0].clone()));
    fs::write(t.path("notes.md"), "three pages written after ls").unwrap();

    for src in ["$f | rm --permanent", "$f.delete(--permanent)", "$f | mv /nonexistent"] {
        let msg = run_in(&engine, &mut stack, src).expect_err(src).to_string();
        assert!(msg.contains("changed or disappeared"), "{src} gave {msg}");
    }
    assert_eq!(fs::read_to_string(t.path("notes.md")).unwrap(), "three pages written after ls");
}

#[test]
fn entry_methods_navigate_move_and_copy() {
    let t = TempDir::new("methods");
    fs::create_dir(t.path("dir")).unwrap();
    t.file("dir/a.json", r#"{"x": 1}"#);
    t.file("dir/.hidden", "");
    let engine = engine();
    let mut stack = Stack::default();
    let listed = run_in(&engine, &mut stack, &format!("ls {}", t.q("dir"))).unwrap();
    stack.set("f", Value::Record(rows(&listed)[0].clone()));

    let parent = run_in(&engine, &mut stack, "$f.parent()").unwrap();
    let parent = parent.as_record().unwrap();
    assert_eq!((text(parent, "name"), text(parent, "type")), (shown(t.path("dir")).as_str(), "dir"));
    assert!(parent.id().is_some());
    stack.set("d", Value::Record(parent.clone()));
    assert_eq!(rows(&run_in(&engine, &mut stack, "$d.children()").unwrap()).len(), 1);
    assert_eq!(rows(&run_in(&engine, &mut stack, "$d.children(--all)").unwrap()).len(), 2);
    let opened = run_in(&engine, &mut stack, "$f.open()").unwrap();
    assert_eq!(opened.as_record().and_then(|r| r.get("x")), Some(&Value::Int(1)));

    let copied = run_in(&engine, &mut stack, &format!("$f.copy({})", t.q("b.json"))).unwrap();
    assert_eq!(text(copied.as_record().unwrap(), "action"), "copied");
    let moved = run_in(&engine, &mut stack, &format!("$f.move({})", t.q(""))).unwrap();
    assert_eq!(text(moved.as_record().unwrap(), "path"), shown(t.path("a.json")));
    assert!(t.path("a.json").exists() && t.path("b.json").exists() && !t.path("dir/a.json").exists());
    assert!(run_in(&engine, &mut stack, "$f.move(somewhere)").unwrap_err().to_string().contains("changed or"));
    assert!(
        run_in(&engine, &mut stack, &format!("ls {} | fs.entry.parent", t.q("dir"))).is_err(),
        "a table is not one object"
    );
}

/// Kills and reaps every job of the engine, also when an assertion failed.
struct Reaper(Arc<Engine>);

impl Drop for Reaper {
    fn drop(&mut self) {
        for job in self.0.jobs.lock().unwrap_or_else(PoisonError::into_inner).iter_mut() {
            if let Some(child) = job.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[test]
fn an_actor_may_stop_a_process_the_session_started() {
    let engine = engine();
    let _reaper = Reaper(engine.clone());
    let mut user = Stack::default();
    let job = run_in(&engine, &mut user, "spawn sleep 30").unwrap();
    let job = job.as_record().unwrap();
    let (id, pid) = (job.get("id").and_then(Value::as_int).unwrap(), job.get("pid").and_then(Value::as_int).unwrap());
    let listed = run_in(&engine, &mut user, "jobs").unwrap();
    let listed = rows(&listed).into_iter().find(|r| r.get("id") == Some(&Value::Int(id))).unwrap();
    assert_eq!(text(listed, "state"), "running");

    // A record whose start time does not match is a different process and is refused.
    user.set("p", Value::Record(Record::typed("proc").with("pid", pid).with_id(format!("proc:{pid}:1"))));
    assert!(run_in(&engine, &mut user, "$p | kill").unwrap_err().to_string().contains("changed or disappeared"));

    let actor = Stack::default().with_principal(Principal::Actor);
    let src = format!("ps | where pid == {pid} | kill");
    let (p, out) = plan(&engine, &actor, &src);
    assert_eq!(p.actions.len(), 1);
    assert_eq!(p.actions[0].verdict, Verdict::Allow, "{:?}", p.actions);
    assert_eq!(actions(&out), ["would signal"]);
    let out = apply(&engine, &actor, &p, &src);
    assert_eq!(actions(&out), ["signaled"]);

    let done = run_in(&engine, &mut user, &format!("fg {id}")).unwrap();
    let done = done.as_record().unwrap();
    assert_eq!(text(done, "state"), "killed");
    assert_eq!(done.get("exit"), Some(&Value::Int(128 + nix::sys::signal::Signal::SIGTERM as i64)));
}

#[test]
fn proc_methods_see_open_files_and_ports() {
    let t = TempDir::new("proc-methods");
    t.file("held.txt", "x");
    let _held = fs::File::open(t.path("held.txt")).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = i64::from(listener.local_addr().unwrap().port());
    let me = std::process::id();
    let engine = engine();
    let mut stack = Stack::default();
    stack.set("p", Value::Record(Record::typed("proc").with("pid", i64::from(me)).with_id(proc_id(me).unwrap())));

    let files = run_in(&engine, &mut stack, "$p.files()").unwrap();
    let held = shown(fs::canonicalize(t.path("held.txt")).unwrap());
    assert!(rows(&files).iter().any(|r| text(r, "name") == held), "{files:?}");
    let ports = run_in(&engine, &mut stack, "$p.ports()").unwrap();
    let socket = rows(&ports).into_iter().find(|r| r.get("port") == Some(&Value::Int(port))).expect("the listener");
    assert_eq!((text(socket, "proto"), text(socket, "local"), text(socket, "state")), ("tcp", "127.0.0.1", "listen"));
    assert_eq!(socket.get("remote"), Some(&Value::Nothing));
}

#[test]
fn undo_moves_back_and_is_for_humans_only() {
    let t = TempDir::new("undo");
    t.file("a.txt", "a");
    let engine = engine();
    let mut user = Stack::default();
    run_in(&engine, &mut user, &format!("mv {} {}", t.q("a.txt"), t.q("b.txt"))).unwrap();
    assert!(t.path("b.txt").exists());

    let mut actor = Stack::default().with_principal(Principal::Actor);
    assert!(run_in(&engine, &mut actor, "undo").unwrap_err().to_string().contains("Only a human"));
    let (p, out) = plan(&engine, &user, "undo");
    assert_eq!(p.actions[0].kind, "fs.move");
    assert_eq!(text(rows(&out)[0], "result"), "would undo");
    assert!(t.path("b.txt").exists(), "planning changes nothing");

    let out = run_in(&engine, &mut user, "undo").unwrap();
    assert_eq!(text(rows(&out)[0], "action"), "move");
    assert!(t.path("a.txt").exists() && !t.path("b.txt").exists());
    assert!(run_in(&engine, &mut user, "undo").unwrap_err().to_string().contains("nothing to undo"));
    let again = run_in(&engine, &mut user, "undo 1").unwrap();
    assert_eq!((text(rows(&again)[0], "result"), text(rows(&again)[0], "because")), ("refused", "already undone"));
}

#[test]
fn types_and_methods_are_registered() {
    let engine = engine();
    for name in ["fs.entry", "fs.change", "proc", "proc.signal", "job", "net.socket"] {
        let info = engine.type_info(name).unwrap_or_else(|| panic!("{name} is not registered"));
        assert!(info.fields.iter().all(|(_, _, desc)| !desc.is_empty()), "{name}");
    }
    let methods = |ty: &str| -> Vec<String> { engine.methods_of(ty).iter().map(|s| s.name.clone()).collect() };
    for name in ["delete", "trash", "move", "copy", "parent", "children", "open"] {
        assert!(methods("fs.entry").contains(&format!("fs.entry.{name}")), "{:?}", methods("fs.entry"));
    }
    for name in ["kill", "parent", "children", "files", "ports"] {
        assert!(methods("proc").contains(&format!("proc.{name}")), "{:?}", methods("proc"));
    }
    for sig in engine.methods_of("fs.entry").iter().chain(&engine.methods_of("proc")) {
        assert!(!sig.examples.is_empty() && !sig.description.is_empty(), "{}", sig.name);
    }
}
