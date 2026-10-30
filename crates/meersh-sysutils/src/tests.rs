//! Every test works inside its own directory under the system temp dir and only uses
//! absolute paths, because tests run in parallel and share one working directory.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use meersh_engine::authority::Plan;
use meersh_engine::{Engine, ExecMode, PipelineData, Result, Stack};
use meersh_value::{Record, Value};

use crate::disk::join_name;
use crate::register;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("meersh-sysutils-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
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

    fn mode(&self, rel: &str) -> u32 {
        fs::metadata(self.path(rel)).unwrap().permissions().mode() & 0o7777
    }

    fn set_mode(&self, rel: &str, mode: u32) {
        fs::set_permissions(self.path(rel), fs::Permissions::from_mode(mode)).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn engine() -> Arc<Engine> {
    let mut engine = Engine::new();
    register(&mut engine);
    Arc::new(engine)
}

fn run_with(stack: &mut Stack, src: &str) -> Result<Value> {
    let mut out = Value::Nothing;
    engine().run_script(stack, src, false, &mut |data: PipelineData| {
        out = data.into_value()?;
        Ok(())
    })?;
    Ok(out)
}

fn run(src: &str) -> Result<Value> {
    run_with(&mut Stack::default(), src)
}

fn ok(src: &str) -> Value {
    run(src).unwrap_or_else(|e| panic!("`{src}` failed ({e})"))
}

fn rows(value: &Value) -> Vec<&Record> {
    match value {
        Value::List(items) => items.iter().map(|v| v.as_record().expect("a table")).collect(),
        other => panic!("expected a table, got {other:?}"),
    }
}

fn rec(value: &Value) -> &Record {
    value.as_record().unwrap_or_else(|| panic!("expected a record, got {value:?}"))
}

fn text<'a>(row: &'a Record, field: &str) -> &'a str {
    row.get(field).and_then(Value::as_str).unwrap_or_else(|| panic!("no string field {field} in {row:?}"))
}

fn int(row: &Record, field: &str) -> i64 {
    match row.get(field) {
        Some(Value::Int(n) | Value::Filesize(n) | Value::Duration(n)) => *n,
        other => panic!("no number field {field} in {row:?}, found {other:?}"),
    }
}

fn float(row: &Record, field: &str) -> f64 {
    match row.get(field) {
        Some(Value::Float(f)) => *f,
        other => panic!("no float field {field} in {row:?}, found {other:?}"),
    }
}

fn error(src: &str) -> String {
    run(src).expect_err("the command should fail").to_string()
}

fn string(s: &str) -> Value {
    Value::String(s.into())
}

fn planning() -> (Stack, Arc<Mutex<Plan>>) {
    let plan = Arc::new(Mutex::new(Plan::default()));
    (Stack::default().with_mode(ExecMode::Plan(plan.clone())), plan)
}

#[test]
fn every_command_has_a_full_signature_and_its_types() {
    let engine = engine();
    for name in [
        "cat", "head", "tail", "wc", "du", "df", "find", "stat", "chmod", "ln", "free", "uptime", "whoami", "hostname",
        "uname",
    ] {
        let sig = engine.signature(name).unwrap_or_else(|| panic!("{name} is not registered"));
        assert!(!sig.description.is_empty(), "{name} has no description");
        assert!(!sig.examples.is_empty(), "{name} has no example");
    }
    for ty in [
        "fs.entry",
        "fs.change",
        "fs.usage",
        "fs.mount",
        "fs.stat",
        "mem.info",
        "sys.uptime",
        "sys.uname",
        "text.count",
    ] {
        let info = engine.type_info(ty).unwrap_or_else(|| panic!("type {ty} is not registered"));
        assert!(!info.fields.is_empty());
    }
}

#[test]
fn cat_concatenates_files_and_reads_piped_entries() {
    let t = TempDir::new("cat");
    t.file("a.txt", "one\n");
    t.file("b.txt", "two\n");
    fs::write(t.path("blob.bin"), [0xff, 0xfe, b'\n', 0x00]).unwrap();

    assert_eq!(ok(&format!("cat {} {}", t.q("a.txt"), t.q("b.txt"))), string("one\ntwo\n"));
    assert_eq!(ok(&format!("cat {}", t.q("*.txt"))), string("one\ntwo\n"));
    assert_eq!(ok(&format!("cat {}", t.q("blob.bin"))), Value::Binary(vec![0xff, 0xfe, b'\n', 0x00]));
    assert_eq!(ok(&format!("find {} --name b.txt | cat", t.q(""))), string("two\n"));
    assert_eq!(ok("\"piped text\" | cat"), string("piped text"));
    assert!(error(&format!("cat {}", t.q("missing"))).contains("cannot read"));
    assert!(error(&format!("cat {}", t.q(""))).contains("is a directory"));
    assert!(error("cat").contains("nothing to read"));
}

#[test]
fn head_and_tail_take_lines_of_text_and_items_of_lists() {
    let t = TempDir::new("headtail");
    let twelve: String = (1..=12).map(|i| format!("{i}\n")).collect();
    t.file("lines.txt", &twelve);

    assert_eq!(ok("\"a\\nb\\nc\\n\" | head -n 2"), string("a\nb\n"));
    assert_eq!(ok("\"a\\nb\\nc\" | tail -n 1"), string("c"));
    assert_eq!(ok("\"a\\nb\\nc\\n\" | tail -n 2"), string("b\nc\n"));
    assert_eq!(ok("\"a\\nb\" | head -n 0"), string(""));

    let first10: String = (1..=10).map(|i| format!("{i}\n")).collect();
    assert_eq!(ok(&format!("cat {} | head", t.q("lines.txt"))), string(&first10));
    let last10: String = (3..=12).map(|i| format!("{i}\n")).collect();
    assert_eq!(ok(&format!("cat {} | tail", t.q("lines.txt"))), string(&last10));

    assert_eq!(ok("[1 2 3 4] | head -n 2"), Value::List(vec![Value::Int(1), Value::Int(2)]));
    assert_eq!(ok("[1 2 3 4] | tail -n 3"), Value::List(vec![Value::Int(2), Value::Int(3), Value::Int(4)]));

    // Binary keeps its line structure and stays binary.
    fs::write(t.path("blob.bin"), [0xff, b'\n', 0xfe, b'\n', 0xfd]).unwrap();
    assert_eq!(ok(&format!("cat {} | head -n 1", t.q("blob.bin"))), Value::Binary(vec![0xff, b'\n']));
    assert_eq!(ok(&format!("cat {} | tail -n 2", t.q("blob.bin"))), Value::Binary(vec![0xfe, b'\n', 0xfd]));

    // Streams are cut lazily by head and buffered by tail.
    for name in ["a", "b", "c"] {
        t.file(name, "");
    }
    let head = ok(&format!("find {} --type f | head -n 2", t.q("")));
    let names: Vec<_> = rows(&head).iter().map(|r| text(r, "name").to_string()).collect();
    assert_eq!(names, [t.path("a"), t.path("b")].map(|p| p.display().to_string()));
    let tail = ok(&format!("find {} --type f --name \"?\" | tail -n 1", t.q("")));
    assert_eq!(text(rows(&tail)[0], "name"), t.path("c").display().to_string());

    assert!(error("\"x\" | head -n -1").contains("whole number"));
    assert!(error("{a: 1} | head").contains("not on a record"));
}

#[test]
fn output_of_external_programs_counts_as_bytes_and_lines() {
    // `head` stops reading, so an endless producer ends.
    assert_eq!(ok("^yes | head -n 2"), Value::List(vec![string("y"), string("y")]));
    assert_eq!(ok("^printf 'a\\nb\\nc\\n' | tail -n 1"), Value::List(vec![string("c")]));
    // cat and wc see the exact bytes, including the last newline.
    assert_eq!(ok("^printf 'a b\\n' | cat"), string("a b\n"));
    let counted = ok("^printf 'a b\\nc\\n' | wc");
    let r = rec(&counted);
    assert_eq!((int(r, "lines"), int(r, "words"), int(r, "bytes")), (2, 3, 6));
}

#[test]
fn wc_counts_text_and_files() {
    let t = TempDir::new("wc");
    t.file("a.txt", "one two\nthree\n");
    t.file("b.txt", "grüße\n");

    let counted = ok("\"one two\\nthree\\n\" | wc");
    let r = rec(&counted);
    assert_eq!(r.type_name(), Some("text.count"));
    assert_eq!((int(r, "lines"), int(r, "words"), int(r, "bytes"), int(r, "chars")), (2, 3, 14, 14));

    let files = ok(&format!("wc {} {}", t.q("a.txt"), t.q("b.txt")));
    let counts = rows(&files);
    assert_eq!(counts.len(), 2);
    assert_eq!(text(counts[0], "path"), t.path("a.txt").display().to_string());
    assert_eq!(counts[0].fields.keys().collect::<Vec<_>>(), ["path", "lines", "words", "bytes", "chars"]);
    assert_eq!((int(counts[1], "lines"), int(counts[1], "words")), (1, 1));
    assert_eq!((int(counts[1], "bytes"), int(counts[1], "chars")), (8, 6));

    let piped = ok(&format!("find {} --name a.txt | wc", t.q("")));
    assert_eq!(int(rows(&piped)[0], "words"), 3);
    assert!(error("wc").contains("nothing to count"));
}

#[test]
fn du_sums_apparent_sizes_and_counts_hard_links_once() {
    let t = TempDir::new("du");
    fs::create_dir_all(t.path("sub/deeper")).unwrap();
    t.file("top.txt", "12345");
    t.file("sub/a.txt", "1234567890");
    t.file("sub/deeper/b.txt", "123");
    fs::hard_link(t.path("sub/a.txt"), t.path("sub/a-again.txt")).unwrap();
    symlink("top.txt", t.path("link")).unwrap();

    let summary = ok(&format!("du {}", t.q("")));
    let usage = rows(&summary);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].type_name(), Some("fs.usage"));
    assert_eq!(usage[0].fields.keys().collect::<Vec<_>>(), ["path", "size", "files"]);
    // The symlink counts with its own length, the target name "top.txt".
    assert_eq!(usage[0].get("size"), Some(&Value::Filesize(5 + 10 + 3 + 7)));
    assert_eq!(int(usage[0], "files"), 4);

    let deep = ok(&format!("du -d 1 {}", t.q("")));
    let paths: Vec<_> = rows(&deep).iter().map(|r| text(r, "path").to_string()).collect();
    assert_eq!(paths, [t.path("").display().to_string(), t.path("sub").display().to_string()]);
    assert_eq!(rows(&deep)[1].get("size"), Some(&Value::Filesize(13)));

    let all = ok(&format!("du --max-depth 5 {}", t.q("")));
    assert_eq!(rows(&all).len(), 3);
    let file = ok(&format!("du {}", t.q("top.txt")));
    assert_eq!(rows(&file)[0].get("size"), Some(&Value::Filesize(5)));
    assert!(error(&format!("du {}", t.q("missing"))).contains("cannot access"));
}

#[test]
fn find_streams_entries_with_filters_and_ids() {
    let t = TempDir::new("find");
    fs::create_dir_all(t.path("src/nested")).unwrap();
    fs::create_dir(t.path(".git")).unwrap();
    t.file("src/main.rs", "fn main() {}");
    t.file("src/nested/lib.rs", "");
    t.file("src/notes.md", "");
    t.file(".git/config", "");
    t.file(".env", "");
    symlink("src/main.rs", t.path("main-link")).unwrap();
    let shown = |rel: &str| t.path(rel).display().to_string();

    let all = ok(&format!("find {}", t.q("")));
    let names: Vec<_> = rows(&all).iter().map(|r| text(r, "name").to_string()).collect();
    let expected = ["main-link", "src", "src/main.rs", "src/nested", "src/nested/lib.rs", "src/notes.md"].map(shown);
    assert_eq!(names, expected);
    let first = rows(&all)[2];
    assert_eq!(first.type_name(), Some("fs.entry"));
    assert_eq!(first.fields.keys().collect::<Vec<_>>(), ["name", "type", "size", "modified"]);
    assert_eq!(first.get("size"), Some(&Value::Filesize(12)));
    for row in rows(&all) {
        let id = row.id().unwrap_or_else(|| panic!("no id on {row:?}"));
        assert!(id.starts_with("file:"), "{id}");
        assert_eq!(Some(id), meersh_engine::authority::file_id(&PathBuf::from(text(row, "name"))).as_deref());
    }

    let rust = ok(&format!("find {} --name *.rs", t.q("")));
    let names: Vec<_> = rows(&rust).iter().map(|r| text(r, "name").to_string()).collect();
    assert_eq!(names, [shown("src/main.rs"), shown("src/nested/lib.rs")]);

    let dirs = ok(&format!("find {} --type d", t.q("")));
    let types: Vec<_> = rows(&dirs).iter().map(|r| text(r, "type")).collect();
    assert_eq!(types, ["dir", "dir"]);
    let links = ok(&format!("find {} --type l", t.q("")));
    assert_eq!(text(rows(&links)[0], "type"), "symlink");
    assert_eq!(rows(&ok(&format!("find {} --type f", t.q("")))).len(), 3);

    let shallow = ok(&format!("find {} --max-depth 1", t.q("")));
    assert_eq!(rows(&shallow).len(), 2);
    let hidden = ok(&format!("find {} -a --max-depth 1", t.q("")));
    assert_eq!(rows(&hidden).len(), 4);
    assert_eq!(rows(&ok(&format!("find {} --hidden --type f", t.q("")))).len(), 5);

    assert!(error(&format!("find {} --type x", t.q(""))).contains("--type takes f, d or l"));
    assert!(error(&format!("find {}", t.q("missing"))).contains("cannot access"));
    assert_eq!(join_name(".", "a"), "a");
    assert_eq!(join_name("src/", "a"), "src/a");
}

#[test]
fn stat_describes_a_file_and_a_symlink() {
    let t = TempDir::new("stat");
    t.file("data.txt", "hello");
    t.set_mode("data.txt", 0o640);
    symlink("data.txt", t.path("link")).unwrap();

    let out = ok(&format!("stat {}", t.q("data.txt")));
    let r = rec(&out);
    assert_eq!(r.type_name(), Some("fs.stat"));
    let keys: Vec<_> = r.fields.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "path",
            "type",
            "size",
            "mode",
            "mode_octal",
            "uid",
            "user",
            "gid",
            "group",
            "inode",
            "device",
            "links",
            "accessed",
            "modified",
            "changed"
        ]
    );
    let meta = fs::metadata(t.path("data.txt")).unwrap();
    assert_eq!(text(r, "type"), "file");
    assert_eq!(r.get("size"), Some(&Value::Filesize(5)));
    assert_eq!(text(r, "mode"), "rw-r-----");
    assert_eq!(text(r, "mode_octal"), "640");
    assert_eq!(int(r, "uid"), i64::from(meta.uid()));
    assert_eq!(int(r, "inode"), meta.ino() as i64);
    assert_eq!(int(r, "links"), 1);
    assert!(!text(r, "user").is_empty() && !text(r, "group").is_empty());
    for field in ["accessed", "modified", "changed"] {
        assert!(matches!(r.get(field), Some(Value::Date(_))), "{field}");
    }
    assert!(r.id().is_some_and(|id| id.starts_with("file:")));

    let link = ok(&format!("stat {}", t.q("link")));
    assert_eq!(text(rec(&link), "type"), "symlink");
    assert!(error(&format!("stat {}", t.q("missing"))).contains("cannot stat"));
}

#[test]
fn symbolic_and_octal_modes() {
    use crate::files::new_mode;
    let umask = 0o022;
    let m = |spec: &str, current: u32| new_mode(spec, current, false, umask).unwrap();
    assert_eq!(m("644", 0o777), 0o644);
    assert_eq!(m("0755", 0), 0o755);
    assert_eq!(m("4755", 0), 0o4755);
    assert_eq!(m("u+x", 0o600), 0o700);
    assert_eq!(m("go-w", 0o666), 0o644);
    assert_eq!(m("a=r", 0o755), 0o444);
    assert_eq!(m("u=rw,go=r", 0o777), 0o644);
    assert_eq!(m("+x", 0o644), 0o755);
    // Without who, the umask protects group and other from w.
    assert_eq!(m("+w", 0o444), 0o644);
    assert_eq!(m("=r", 0o777), 0o444);
    assert_eq!(m("g+s", 0o755), 0o2755);
    assert_eq!(m("o+t", 0o777), 0o1777);
    assert_eq!(m("go=u", 0o700), 0o777);
    assert_eq!(m("a+X", 0o644), 0o644);
    assert_eq!(m("a+X", 0o744), 0o755);
    assert_eq!(new_mode("a+X", 0o644, true, umask).unwrap(), 0o755);
    for bad in ["", "u", "u+q", "z+x", "88", "u+x,"] {
        assert!(new_mode(bad, 0o644, false, umask).is_err(), "{bad} should be invalid");
    }
}

#[test]
fn chmod_changes_modes_and_reports_them() {
    let t = TempDir::new("chmod");
    t.file("a.sh", "");
    t.file("b.sh", "");
    t.file("notes.txt", "");
    t.set_mode("a.sh", 0o644);
    t.set_mode("b.sh", 0o644);
    t.set_mode("notes.txt", 0o666);

    let out = ok(&format!("chmod 600 {}", t.q("notes.txt")));
    let row = rows(&out)[0];
    assert_eq!(row.type_name(), Some("fs.change"));
    assert_eq!(row.fields.keys().collect::<Vec<_>>(), ["path", "action", "mode"]);
    assert_eq!(text(row, "action"), "chmod");
    assert_eq!(text(row, "mode"), "600");
    assert_eq!(t.mode("notes.txt"), 0o600);

    ok(&format!("chmod go+r {}", t.q("notes.txt")));
    assert_eq!(t.mode("notes.txt"), 0o644);

    let piped = ok(&format!("find {} --name *.sh | chmod u+x", t.q("")));
    assert_eq!(rows(&piped).len(), 2);
    assert_eq!((t.mode("a.sh"), t.mode("b.sh")), (0o744, 0o744));

    assert!(error(&format!("chmod u+q {}", t.q("a.sh"))).contains("invalid mode"));
    // A missing path stops the command before anything changes.
    assert!(error(&format!("chmod 600 {} {}", t.q("a.sh"), t.q("missing"))).contains("cannot access"));
    assert_eq!(t.mode("a.sh"), 0o744);
}

#[test]
fn chmod_in_plan_mode_changes_nothing() {
    let t = TempDir::new("chmodplan");
    t.file("a.txt", "");
    t.set_mode("a.txt", 0o644);

    let (mut stack, plan) = planning();
    let out = run_with(&mut stack, &format!("chmod 777 {}", t.q("a.txt"))).unwrap();
    let row = rows(&out)[0];
    assert_eq!(text(row, "action"), "would chmod");
    assert_eq!(text(row, "mode"), "777");
    assert_eq!(t.mode("a.txt"), 0o644);
    let plan = plan.lock().unwrap();
    assert_eq!(plan.actions.len(), 1);
    assert_eq!(plan.actions[0].kind, "fs.chmod");

    // Applying a plan that does not contain the change refuses it and says why.
    let mut applying = Stack::default().with_mode(ExecMode::Apply(Arc::default()));
    let out = run_with(&mut applying, &format!("chmod 777 {}", t.q("a.txt"))).unwrap();
    let row = rows(&out)[0];
    assert_eq!(text(row, "action"), "refused");
    assert!(!text(row, "because").is_empty());
    assert_eq!(t.mode("a.txt"), 0o644);
}

#[test]
fn ln_creates_symbolic_and_hard_links() {
    let t = TempDir::new("ln");
    t.file("target.txt", "content");
    fs::create_dir(t.path("dir")).unwrap();

    let out = ok(&format!("ln -s target.txt {}", t.q("soft")));
    let row = rows(&out)[0];
    assert_eq!(row.fields.keys().collect::<Vec<_>>(), ["path", "action", "target"]);
    assert_eq!(text(row, "action"), "symlink");
    assert_eq!(text(row, "target"), "target.txt");
    assert_eq!(fs::read_link(t.path("soft")).unwrap(), PathBuf::from("target.txt"));
    assert_eq!(fs::read_to_string(t.path("soft")).unwrap(), "content");

    let out = ok(&format!("ln {} {}", t.q("target.txt"), t.q("hard")));
    assert_eq!(text(rows(&out)[0], "action"), "link");
    let (a, b) = (fs::metadata(t.path("target.txt")).unwrap(), fs::metadata(t.path("hard")).unwrap());
    assert_eq!(a.ino(), b.ino());

    // An existing directory receives the link under the target's name.
    let out = ok(&format!("ln {} {}", t.q("target.txt"), t.q("dir")));
    assert_eq!(text(rows(&out)[0], "path"), t.path("dir/target.txt").display().to_string());
    assert!(t.path("dir/target.txt").exists());

    assert!(error(&format!("ln -s x {}", t.q("soft"))).contains("already exists"));
    assert!(error(&format!("ln {} {}", t.q("missing"), t.q("new"))).contains("cannot link"));
    assert!(error(&format!("ln {} {}", t.q("dir"), t.q("new"))).contains("is a directory"));

    let (mut stack, plan) = planning();
    let out = run_with(&mut stack, &format!("ln -s target.txt {}", t.q("planned"))).unwrap();
    assert_eq!(text(rows(&out)[0], "action"), "would symlink");
    assert!(fs::symlink_metadata(t.path("planned")).is_err());
    assert_eq!(plan.lock().unwrap().actions[0].kind, "fs.create");
}

#[test]
fn df_lists_mounted_filesystems() {
    let out = ok("df");
    let mounts = rows(&out);
    assert!(!mounts.is_empty());
    for row in &mounts {
        assert_eq!(row.type_name(), Some("fs.mount"));
        assert_eq!(
            row.fields.keys().collect::<Vec<_>>(),
            ["mount", "source", "fstype", "size", "used", "available", "use"]
        );
        assert!(text(row, "mount").starts_with('/'));
        let (size, used, available) = (int(row, "size"), int(row, "used"), int(row, "available"));
        assert!(size > 0 && used <= size && available <= size, "{row:?}");
        assert!((0.0..=100.0).contains(&float(row, "use")), "{row:?}");
    }
    assert!(rows(&ok("df --all")).len() >= mounts.len());
}

#[test]
fn free_reports_memory() {
    let out = ok("free");
    let r = rec(&out);
    assert_eq!(r.type_name(), Some("mem.info"));
    let total = int(r, "total");
    assert!(total > 0);
    assert!(int(r, "available") <= total && int(r, "free") <= total);
    assert_eq!(int(r, "used") + int(r, "available"), total);
    assert!(int(r, "swap_free") <= int(r, "swap_total"));
    assert!(matches!(r.get("cached"), Some(Value::Filesize(_))));
}

#[test]
fn uptime_reports_boot_and_load() {
    let out = ok("uptime");
    let r = rec(&out);
    assert_eq!(r.type_name(), Some("sys.uptime"));
    assert!(int(r, "uptime") > 0);
    let Some(Value::Date(boot)) = r.get("boot") else { panic!("no boot date in {r:?}") };
    assert!(*boot < chrono::Local::now().fixed_offset());
    for load in ["load1", "load5", "load15"] {
        assert!(float(r, load) >= 0.0);
    }
    if r.get("users").is_some() {
        assert!(int(r, "users") >= 0);
    }
}

#[test]
fn identity_of_the_machine() {
    let Value::String(user) = ok("whoami") else { panic!("whoami is not a string") };
    assert!(!user.is_empty());
    let Value::String(host) = ok("hostname") else { panic!("hostname is not a string") };
    assert!(!host.is_empty());
    let out = ok("uname");
    let r = rec(&out);
    assert_eq!(r.type_name(), Some("sys.uname"));
    assert_eq!(text(r, "sysname"), "Linux");
    assert!(!text(r, "release").is_empty() && !text(r, "machine").is_empty());
}
