//! `docker ps` and `podman ps` (also as `container ls`). Both get `--format json`. Docker
//! writes one object per line with flat strings, podman one array with lists and numbers, so
//! the parser maps both shapes to the same `container` record.

use chrono::{DateTime, NaiveDateTime};
use meersh_value::{Record, Value};
use serde_json::Value as Json;

use crate::util::{Object, Opt, Spec, date_from_secs, has, insert_at, json_objects, local_date, scan, text, value_of};

/// Global options of docker that take the next word as their value.
const DOCKER_GLOBAL: &[&str] =
    &["-c", "--context", "-H", "--host", "-l", "--log-level", "--config", "--tlscacert", "--tlscert", "--tlskey"];

/// Global options of podman that take the next word as their value.
const PODMAN_GLOBAL: &[&str] = &[
    "-c",
    "--connection",
    "--url",
    "--identity",
    "--root",
    "--runroot",
    "--storage-driver",
    "--storage-opt",
    "--log-level",
    "--cgroup-manager",
    "--events-backend",
    "--network-cmd-path",
    "--network-config-dir",
    "--runtime",
    "--runtime-flag",
    "--tmpdir",
    "--imagestore",
    "--hooks-dir",
    "--config",
    "--module",
    "--ssh",
    "--conmon",
    "--volumepath",
    "--db-backend",
    "--out",
];

const PS: Spec = Spec { short: "fn", short_optional: "", long: &["filter", "format", "last", "sort", "pod-id"] };

pub fn docker_rewrite(args: &[String]) -> Option<Vec<String>> {
    rewrite(args, DOCKER_GLOBAL)
}

pub fn podman_rewrite(args: &[String]) -> Option<Vec<String>> {
    rewrite(args, PODMAN_GLOBAL)
}

fn rewrite(args: &[String], global_values: &[&str]) -> Option<Vec<String>> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if global_values.contains(&arg.as_str()) {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let at = match (args.get(i).map(String::as_str), args.get(i + 1).map(String::as_str)) {
        (Some("ps"), _) => i + 1,
        (Some("container"), Some("ls" | "ps" | "list")) => i + 2,
        _ => return None,
    };
    let opts = scan(&args[at..], &PS);
    if has(&opts, Some('q'), &["quiet", "help"]) || has(&opts, Some('w'), &["watch"]) || has(&opts, Some('h'), &[]) {
        return None;
    }
    if opts.iter().any(|(_, o)| matches!(o, Opt::Operand(_))) {
        return None;
    }
    match value_of(&opts, None, &["format"]) {
        Some(Some("json" | "{{json .}}")) => Some(args.to_vec()),
        Some(_) => None,
        None => Some(insert_at(args, at, &["--format", "json"])),
    }
}

pub fn parse(stdout: &str) -> Result<Vec<Value>, String> {
    Ok(json_objects(stdout)?
        .iter()
        .map(|obj| Value::Record(if obj.contains_key("Id") { podman(obj) } else { docker(obj) }))
        .collect())
}

fn docker(obj: &Object) -> Record {
    // Docker quotes and shortens the command, as in `"uvicorn app.main:ap…"`.
    let command = match text(obj, "Command") {
        Value::String(s) => Value::String(s.trim_matches('"').to_string()),
        other => other,
    };
    Record::typed("container")
        .with("id", text(obj, "ID"))
        .with("name", text(obj, "Names"))
        .with("image", text(obj, "Image"))
        .with("command", command)
        .with("state", text(obj, "State"))
        .with("status", text(obj, "Status"))
        .with("created", obj.get("CreatedAt").and_then(Json::as_str).map_or(Value::Nothing, docker_date))
        .with("ports", text(obj, "Ports"))
}

/// `2026-10-29 17:35:29 +0100 CET` as a date. The zone name at the end is ignored.
fn docker_date(s: &str) -> Value {
    let mut words = s.split_whitespace();
    let (Some(day), Some(time)) = (words.next(), words.next()) else { return Value::Nothing };
    let stamp = format!("{day} {time}");
    match words.next() {
        Some(offset) => DateTime::parse_from_str(&format!("{stamp} {offset}"), "%Y-%m-%d %H:%M:%S %z")
            .map_or(Value::Nothing, Value::Date),
        None => NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S").map_or(Value::Nothing, local_date),
    }
}

fn podman(obj: &Object) -> Record {
    let joined = |key: &str, sep: &str| match obj.get(key) {
        Some(Json::Array(items)) => {
            let parts: Vec<&str> = items.iter().filter_map(Json::as_str).collect();
            if parts.is_empty() { Value::Nothing } else { Value::String(parts.join(sep)) }
        }
        _ => text(obj, key),
    };
    let created = match obj.get("Created") {
        Some(Json::Number(n)) => n.as_i64().map_or(Value::Nothing, date_from_secs),
        Some(Json::String(s)) => DateTime::parse_from_rfc3339(s).map_or(Value::Nothing, Value::Date),
        _ => Value::Nothing,
    };
    let ports = match obj.get("Ports") {
        Some(Json::Array(items)) => {
            let ports: Vec<String> = items.iter().filter_map(Json::as_object).map(podman_port).collect();
            if ports.is_empty() { Value::Nothing } else { Value::String(ports.join(", ")) }
        }
        _ => Value::Nothing,
    };
    Record::typed("container")
        .with("id", text(obj, "Id"))
        .with("name", joined("Names", ","))
        .with("image", text(obj, "Image"))
        .with("command", joined("Command", " "))
        .with("state", text(obj, "State"))
        .with("status", text(obj, "Status"))
        .with("created", created)
        .with("ports", ports)
}

/// A podman port mapping in docker's notation, `0.0.0.0:8080->80/tcp`.
fn podman_port(port: &Object) -> String {
    let field = |key: &str| match port.get(key) {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let host_ip = field("host_ip");
    let host = if host_ip.is_empty() { "0.0.0.0".to_string() } else { host_ip };
    let protocol = field("protocol");
    let protocol = if protocol.is_empty() { "tcp".to_string() } else { protocol };
    format!("{host}:{}->{}/{protocol}", field("host_port"), field("container_port"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn rewrites() {
        assert_eq!(docker_rewrite(&args("ps")).unwrap(), args("ps --format json"));
        assert_eq!(
            docker_rewrite(&args("ps -a -f status=exited")).unwrap(),
            args("ps --format json -a -f status=exited")
        );
        assert_eq!(
            docker_rewrite(&args("--context prod container ls")).unwrap(),
            args("--context prod container ls --format json")
        );
        assert_eq!(docker_rewrite(&args("ps --format json")).unwrap(), args("ps --format json"));
        assert_eq!(docker_rewrite(&args("ps --format {{.Names}}")), None);
        assert_eq!(docker_rewrite(&args("ps -q")), None);
        assert_eq!(docker_rewrite(&args("images")), None);
        assert_eq!(podman_rewrite(&args("--remote ps -a")).unwrap(), args("--remote ps --format json -a"));
        assert_eq!(podman_rewrite(&args("ps --watch 2")), None);
    }

    // `docker ps -a --format json`, trimmed and anonymized.
    const DOCKER: &str = r#"{"Command":"\"uvicorn app.main:ap…\"","CreatedAt":"2026-10-29 17:35:29 +0100 CET","HealthStatus":"healthy","ID":"fe125be7d89b","Image":"example-server","Labels":"com.docker.compose.project=example,com.docker.compose.service=server","LocalVolumes":"1","Mounts":"example_data","Names":"example-server-1","Networks":"example_default","Platform":null,"Ports":"127.0.0.1:8000->8000/tcp","RunningFor":"3 hours ago","Size":"0B","State":"running","Status":"Up 3 hours (healthy)"}
{"Command":"\"docker-entrypoint.s…\"","CreatedAt":"2026-10-20 09:01:02 +0200 CEST","ID":"99acbb5146fc","Image":"postgres:16","Labels":"","LocalVolumes":"1","Mounts":"pgdata","Names":"db","Networks":"bridge","Ports":"","RunningFor":"9 days ago","Size":"0B","State":"exited","Status":"Exited (0) 2 days ago"}
"#;

    #[test]
    fn docker_lines() {
        let rows = parse(DOCKER).unwrap();
        assert_eq!(rows.len(), 2);
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.type_name(), Some("container"));
        assert_eq!(r.get("name"), Some(&"example-server-1".into()));
        assert_eq!(r.get("command"), Some(&"uvicorn app.main:ap…".into()));
        assert_eq!(r.get("ports"), Some(&"127.0.0.1:8000->8000/tcp".into()));
        let Some(Value::Date(created)) = r.get("created") else { panic!("created is not a date") };
        assert_eq!(created.to_rfc3339(), "2026-10-29T17:35:29+01:00");
        assert_eq!(rows[1].as_record().unwrap().get("state"), Some(&"exited".into()));
        assert_eq!(rows[1].as_record().unwrap().get("ports"), Some(&Value::Nothing));
    }

    // `podman ps -a --format json` from podman 5.
    const PODMAN: &str = r#"[
  {
    "AutoRemove": false,
    "Command": ["nginx", "-g", "daemon off;"],
    "Created": "2026-10-29T17:35:29.123456789+01:00",
    "CreatedAt": "3 hours ago",
    "Exited": false,
    "ExitCode": 0,
    "Id": "5f2b7c1a9e0d4c3b2a1f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f2e1d0c9b",
    "Image": "docker.io/library/nginx:latest",
    "Labels": null,
    "Mounts": [],
    "Names": ["web"],
    "Networks": ["podman"],
    "Pid": 4242,
    "Pod": "",
    "Ports": [{"host_ip": "", "container_port": 80, "host_port": 8080, "range": 1, "protocol": "tcp"}],
    "State": "running",
    "Status": "Up 3 hours"
  },
  {"Command": null, "Created": 1793000000, "Id": "abc", "Image": "alpine", "Names": ["one", "two"], "Ports": null, "State": "exited", "Status": "Exited (1) 1 hour ago"}
]"#;

    #[test]
    fn podman_array() {
        let rows = parse(PODMAN).unwrap();
        assert_eq!(rows.len(), 2);
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.get("name"), Some(&"web".into()));
        assert_eq!(r.get("command"), Some(&"nginx -g daemon off;".into()));
        assert_eq!(r.get("ports"), Some(&"0.0.0.0:8080->80/tcp".into()));
        assert!(matches!(r.get("created"), Some(Value::Date(_))));
        let r = rows[1].as_record().unwrap();
        assert_eq!(r.get("name"), Some(&"one,two".into()));
        let Some(Value::Date(created)) = r.get("created") else { panic!("created is not a date") };
        assert_eq!(created.timestamp(), 1793000000);
        assert_eq!(parse("[]").unwrap(), vec![]);
        assert!(parse("CONTAINER ID   IMAGE").is_err());
    }
}
