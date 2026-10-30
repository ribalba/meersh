//! The host against plugins written as shell scripts, so the protocol is tested without the
//! Rust helper. Scripts and registries live in a temp directory.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meersh_engine::value::Value;
use meersh_engine::{Engine, Result, Stack};

const ECHO: &str = r#"#!/bin/sh
read -r request
case "$request" in
  *signatures*) echo '{"signatures":[{"name":"sizes","input":"list<filesize>","output":"list<filesize>","effect":"pure"},{"name":"fail","effect":"read"}]}' ;;
  *'"name":"fail"'*) printf '\036{"@error":"it failed"}\n' ;;
  *) cat ;;
esac
"#;

const CLASH: &str = r#"#!/bin/sh
read -r request
echo '{"signatures":[{"name":"plugin list","effect":"read"}]}'
"#;

const MUTE: &str = "#!/bin/sh\nexit 3\n";

fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn run(engine: &Arc<Engine>, src: &str) -> Result<Value> {
    let mut out = Value::Nothing;
    engine.run_script(&mut Stack::default(), src, false, &mut |data| {
        out = data.into_value()?;
        Ok(())
    })?;
    Ok(out)
}

#[test]
fn scripts_can_be_plugins() {
    let dir = std::env::temp_dir().join(format!("meersh-plugin-host-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let echo = script(&dir, "meersh-plugin-echo", ECHO);
    let clash = script(&dir, "clash", CLASH);
    let mute = script(&dir, "mute", MUTE);
    let mut engine = Engine::new();
    meersh_plugin::register(&mut engine);
    engine.set_env(meersh_plugin::ENV_VAR, Some(dir.join("plugins.json").display().to_string()));
    let engine = Arc::new(engine);

    run(&engine, &format!("plugin add '{}'", echo.display())).unwrap();
    // The script echoes its input. The filesizes arrive as plain numbers and get their type
    // back from the declared output type.
    assert_eq!(run(&engine, "[1kb 2kb] | sizes").unwrap(), run(&engine, "[1kb 2kb]").unwrap());
    assert!(matches!(run(&engine, "[1kb] | sizes").unwrap(), Value::List(v) if v == [Value::Filesize(1000)]));
    assert_eq!(run(&engine, "fail").unwrap_err().to_string(), "fail: it failed");

    let clashing = run(&engine, &format!("plugin add '{}'", clash.display())).unwrap_err().to_string();
    assert!(clashing.contains("plugin list is already a command"), "{clashing}");
    let silent = run(&engine, &format!("plugin add '{}'", mute.display())).unwrap_err().to_string();
    assert!(silent.contains("exited with status 3"), "{silent}");
    assert!(run(&engine, "plugin add ./does-not-exist").is_err());

    let Value::List(rows) = run(&engine, "plugin list").unwrap() else { panic!("plugin list is a table") };
    let names: Vec<_> = rows.iter().filter_map(|r| r.as_record()?.get("plugin").cloned()).collect();
    assert_eq!(names, vec![Value::from("echo")]);
    std::fs::remove_dir_all(dir).unwrap();
}
