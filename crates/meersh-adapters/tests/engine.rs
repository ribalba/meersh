//! Runs adapters through the engine against the real programs. A test whose program is not
//! installed is skipped.

use std::sync::Arc;

use meersh_engine::value::Value;
use meersh_engine::{Engine, Stack};

fn engine() -> Arc<Engine> {
    let mut engine = Engine::new();
    meersh_cmds::register(&mut engine);
    meersh_adapters::register(&mut engine);
    Arc::new(engine)
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn run(engine: &Arc<Engine>, src: &str) -> Value {
    let mut out = Value::Nothing;
    engine
        .run_script(&mut Stack::default(), src, false, &mut |data| {
            out = data.into_value()?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{src}: {e}"));
    out
}

fn rows(value: &Value) -> &[Value] {
    match value {
        Value::List(items) => items,
        other => panic!("expected a table, got {other:?}"),
    }
}

#[test]
fn ip_addr_yields_typed_interfaces() {
    if !installed("ip") {
        eprintln!("skipped, ip is not installed");
        return;
    }
    let engine = engine();
    let all = run(&engine, "^ip addr");
    let interfaces = rows(&all);
    assert!(!interfaces.is_empty(), "ip addr listed no interfaces");
    for row in interfaces {
        assert_eq!(row.as_record().map(|r| r.type_name()), Some(Some("net.interface")), "{row:?}");
    }
    assert_eq!(run(&engine, "^ip addr | length"), Value::Int(interfaces.len() as i64));
    assert_eq!(run(&engine, "^ip addr | where name == lo | length"), Value::Int(1));
    // A change is not rewritten, so its (empty or error) output stays text.
    assert!(engine.adapter("ip").unwrap().rewrite(&["addr".into(), "add".into()]).is_none());
}

/// Every adapter against the program on this machine. Only checks that the output parses
/// into records of the right type, since the content depends on the machine.
#[test]
fn real_programs_parse() {
    let engine = engine();
    let cases = [
        ("ip", "^ip link", "net.interface"),
        ("ip", "^ip route", "net.route"),
        ("ip", "^ip neigh", "net.neighbor"),
        ("ip", "^ip -br addr", "net.interface"),
        ("ss", "^ss -tunap", "net.socket"),
        ("ss", "^ss -tln", "net.socket"),
        ("ss", "^ss -x", "net.socket"),
        ("systemctl", "^systemctl list-units", "systemd.unit"),
        ("systemctl", "^systemctl list-timers", "systemd.timer"),
        ("systemctl", "^systemctl list-unit-files", "systemd.unit"),
        ("journalctl", "^journalctl -n 20", "log.entry"),
        ("git", "^git log -n 3", "git.commit"),
        ("git", "^git status", "git.status"),
        ("git", "^git branch -a", "git.branch"),
        ("lsblk", "^lsblk", "block.device"),
        ("findmnt", "^findmnt", "fs.mount"),
        ("findmnt", "^findmnt -D", "fs.mount"),
        ("docker", "^docker ps -a", "container"),
        ("podman", "^podman ps -a", "container"),
        ("who", "^who", "session.login"),
        ("who", "^who -u", "session.login"),
        ("free", "^free", "mem.info"),
        ("free", "^free -w -t", "mem.info"),
        ("uptime", "^uptime", "sys.uptime"),
        ("df", "^df", "fs.mount"),
        ("df", "^df -h", "fs.mount"),
    ];
    for (program, src, type_name) in cases {
        if !installed(program) {
            eprintln!("skipped {src}, {program} is not installed");
            continue;
        }
        let out = run(&engine, src);
        let rows = rows(&out);
        eprintln!("{src}: {} rows", rows.len());
        for row in rows {
            assert_eq!(row.as_record().and_then(|r| r.type_name()), Some(type_name), "{src}: {row:?}");
        }
    }
}

#[test]
fn ss_fills_the_columns_it_leaves_out() {
    if !installed("ss") {
        eprintln!("skipped, ss is not installed");
        return;
    }
    let engine = engine();
    let out = run(&engine, "^ss -tln");
    for row in rows(&out) {
        let r = row.as_record().unwrap();
        assert_eq!(r.get("proto"), Some(&"tcp".into()));
        assert_eq!(r.get("state"), Some(&"LISTEN".into()));
        assert!(matches!(r.get("port"), Some(Value::Int(_))), "{r:?}");
    }
}

#[test]
fn df_sizes_are_filesizes() {
    if !installed("df") {
        eprintln!("skipped, df is not installed");
        return;
    }
    let engine = engine();
    let root = run(&engine, "^df / | first");
    let record = root.as_record().expect("df / gave no row");
    assert!(matches!(record.get("size"), Some(Value::Filesize(n)) if *n > 0), "{record:?}");
    assert_eq!(record.get("target"), Some(&"/".into()));
}
