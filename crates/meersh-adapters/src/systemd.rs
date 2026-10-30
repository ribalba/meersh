//! `systemctl` and `journalctl`. Both write JSON when asked, so the adapters only add the
//! output option and map the fields to `systemd.unit`, `systemd.timer` and `log.entry`.

use meersh_value::{Record, Value};
use serde_json::Value as Json;

use crate::util::{Object, Opt, Spec, date_from_micros, has, insert_at, json_i64, json_objects, scan, text, value_of};

const SYSTEMCTL: Spec = Spec {
    short: "tspHMno",
    short_optional: "",
    long: &[
        "type",
        "state",
        "property",
        "signal",
        "host",
        "machine",
        "lines",
        "output",
        "root",
        "image",
        "image-policy",
        "job-mode",
        "kill-whom",
        "kill-value",
        "what",
        "preset-mode",
        "timestamp",
        "boot-loader-entry",
        "boot-loader-menu",
        "reboot-argument",
        "message",
        "when",
        "drop-in",
        "legend",
        "check-inhibitors",
    ],
};

/// `list-units`, `list-timers` and `list-unit-files` get `--output=json`. A plain `systemctl`
/// lists units too. Other verbs stay text.
pub fn systemctl_rewrite(args: &[String]) -> Option<Vec<String>> {
    let opts = scan(args, &SYSTEMCTL);
    if has(&opts, Some('h'), &["help", "version"]) {
        return None;
    }
    let verb = opts.iter().find_map(|(_, o)| if let Opt::Operand(w) = o { Some(*w) } else { None });
    if !matches!(verb, None | Some("list-units" | "list-timers" | "list-unit-files")) {
        return None;
    }
    match value_of(&opts, Some('o'), &["output"]) {
        Some(Some(mode)) if mode.starts_with("json") => Some(args.to_vec()),
        Some(_) => None,
        None => Some(insert_at(args, 0, &["--output=json"])),
    }
}

/// Parses the JSON of `systemctl list-*`. The listing is told apart by its fields.
pub fn systemctl_parse(stdout: &str) -> Result<Vec<Value>, String> {
    Ok(json_objects(stdout)?
        .iter()
        .map(|obj| {
            Value::Record(if obj.contains_key("unit_file") {
                Record::typed("systemd.unit")
                    .with("unit", text(obj, "unit_file"))
                    .with("state", text(obj, "state"))
                    .with("preset", text(obj, "preset"))
            } else if obj.contains_key("activates") || obj.contains_key("next") {
                // `left` and `passed` are left out. Their meaning differs between systemd
                // versions, and both follow from `next`, `last` and `date now`.
                Record::typed("systemd.timer")
                    .with("unit", text(obj, "unit"))
                    .with("activates", text(obj, "activates"))
                    .with("next", micros(obj, "next"))
                    .with("last", micros(obj, "last"))
            } else {
                Record::typed("systemd.unit")
                    .with("unit", text(obj, "unit"))
                    .with("load", text(obj, "load"))
                    .with("active", text(obj, "active"))
                    .with("sub", text(obj, "sub"))
                    .with("description", text(obj, "description"))
            })
        })
        .collect())
}

fn micros(obj: &Object, key: &str) -> Value {
    obj.get(key).and_then(json_i64).map_or(Value::Nothing, date_from_micros)
}

const JOURNALCTL: Spec = Spec {
    short: "DMScUutTpgoFi",
    short_optional: "",
    long: &[
        "directory",
        "file",
        "root",
        "image",
        "image-policy",
        "namespace",
        "since",
        "until",
        "cursor",
        "after-cursor",
        "cursor-file",
        "unit",
        "user-unit",
        "invocation",
        "identifier",
        "exclude-identifier",
        "priority",
        "facility",
        "grep",
        "output",
        "output-fields",
        "field",
        "machine",
        "interval",
        "verify-key",
        "vacuum-size",
        "vacuum-files",
        "vacuum-time",
        "synchronize-on-exit",
    ],
};

/// Options of journalctl that do something other than print entries.
const NOT_ENTRIES: &[&str] = &[
    "fields",
    "field",
    "list-boots",
    "list-invocations",
    "list-namespaces",
    "disk-usage",
    "vacuum-size",
    "vacuum-files",
    "vacuum-time",
    "verify",
    "sync",
    "relinquish-var",
    "smart-relinquish-var",
    "flush",
    "rotate",
    "header",
    "list-catalog",
    "dump-catalog",
    "update-catalog",
    "setup-keys",
    "new-id128",
    "help",
    "version",
];

/// Adds `--output=json` unless the user chose an output mode. `-f` stays text, because the
/// adapter reads the whole output before it parses and a followed journal never ends.
pub fn journalctl_rewrite(args: &[String]) -> Option<Vec<String>> {
    let opts = scan(args, &JOURNALCTL);
    let other = [Some('f'), Some('N'), Some('F'), Some('h')].iter().any(|c| has(&opts, *c, &[]));
    if other || has(&opts, None, &["follow"]) || has(&opts, None, NOT_ENTRIES) {
        return None;
    }
    match value_of(&opts, Some('o'), &["output"]) {
        Some(Some("json" | "json-pretty" | "json-seq")) => Some(args.to_vec()),
        Some(_) => None,
        None => Some(insert_at(args, 0, &["--output=json"])),
    }
}

/// Parses `journalctl -o json` (and `json-pretty` or `json-seq`) into `log.entry` records.
pub fn journalctl_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let clean;
    let stdout = if stdout.contains('\u{1e}') {
        clean = stdout.replace('\u{1e}', "");
        clean.as_str()
    } else {
        stdout
    };
    Ok(json_objects(stdout)?.iter().map(|obj| Value::Record(entry(obj))).collect())
}

fn entry(obj: &Object) -> Record {
    let field = |key: &str| obj.get(key).and_then(journal_text);
    let time =
        field("__REALTIME_TIMESTAMP").and_then(|t| t.parse::<i64>().ok()).map_or(Value::Nothing, date_from_micros);
    let unit = field("_SYSTEMD_USER_UNIT").or_else(|| field("_SYSTEMD_UNIT"));
    let identifier = field("SYSLOG_IDENTIFIER").or_else(|| field("_COMM"));
    let number = |key: &str| field(key).and_then(|v| v.trim().parse::<i64>().ok()).map_or(Value::Nothing, Value::Int);
    let text = |v: Option<String>| v.map_or(Value::Nothing, Value::String);
    Record::typed("log.entry")
        .with("time", time)
        .with("unit", text(unit))
        .with("identifier", text(identifier))
        .with("priority", number("PRIORITY"))
        .with("pid", number("_PID"))
        .with("message", text(field("MESSAGE")))
}

/// A journal field as text. Binary fields arrive as arrays of bytes, fields that occur more
/// than once as arrays of values, and fields that are too large as null.
fn journal_text(json: &Json) -> Option<String> {
    match json {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Array(items) if items.iter().all(Json::is_number) => {
            let bytes: Vec<u8> = items.iter().filter_map(|b| b.as_u64()).map(|b| b as u8).collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        Json::Array(items) => items.first().and_then(journal_text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn systemctl_rewrite_adds_json_to_listings() {
        assert_eq!(systemctl_rewrite(&args("")).unwrap(), args("--output=json"));
        assert_eq!(
            systemctl_rewrite(&args("--user list-units --type service")).unwrap(),
            args("--output=json --user list-units --type service")
        );
        assert_eq!(systemctl_rewrite(&args("list-timers --all")).unwrap(), args("--output=json list-timers --all"));
        assert_eq!(
            systemctl_rewrite(&args("-t service list-unit-files")).unwrap(),
            args("--output=json -t service list-unit-files")
        );
        assert_eq!(systemctl_rewrite(&args("list-units -o json-pretty")).unwrap(), args("list-units -o json-pretty"));
        assert_eq!(systemctl_rewrite(&args("list-units -o cat")), None);
        assert_eq!(systemctl_rewrite(&args("status sshd")), None);
        assert_eq!(systemctl_rewrite(&args("restart nginx")), None);
    }

    #[test]
    fn units_timers_and_unit_files() {
        let units = r#"[{"unit":"mnt-data.automount","load":"loaded","active":"active","sub":"running","description":"mnt-data.automount"},{"unit":"sshd.service","load":"loaded","active":"failed","sub":"failed","description":"OpenSSH server daemon"}]"#;
        let rows = systemctl_parse(units).unwrap();
        let r = rows[1].as_record().unwrap();
        assert_eq!(r.type_name(), Some("systemd.unit"));
        assert_eq!(r.get("unit"), Some(&"sshd.service".into()));
        assert_eq!(r.get("active"), Some(&"failed".into()));

        let timers = r#"[{"next":1793313894261240,"left":1793313894261240,"last":1793299794646222,"passed":53762839041,"unit":"dnf-makecache.timer","activates":"dnf-makecache.service"},{"next":0,"left":0,"last":null,"passed":0,"unit":"idle.timer","activates":"idle.service"}]"#;
        let rows = systemctl_parse(timers).unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.type_name(), Some("systemd.timer"));
        let Some(Value::Date(next)) = r.get("next") else { panic!("next is not a date") };
        assert_eq!(next.timestamp_micros(), 1793313894261240);
        assert_eq!(rows[1].as_record().unwrap().get("next"), Some(&Value::Nothing));
        assert_eq!(rows[1].as_record().unwrap().get("last"), Some(&Value::Nothing));

        let files = r#"[{"unit_file":"-.mount","state":"generated","preset":null},{"unit_file":"sshd.service","state":"enabled","preset":"enabled"}]"#;
        let rows = systemctl_parse(files).unwrap();
        let r = rows[1].as_record().unwrap();
        assert_eq!(r.get("unit"), Some(&"sshd.service".into()));
        assert_eq!(r.get("preset"), Some(&"enabled".into()));
        assert_eq!(rows[0].as_record().unwrap().get("preset"), Some(&Value::Nothing));

        assert!(systemctl_parse("UNIT LOAD ACTIVE SUB DESCRIPTION").is_err());
    }

    #[test]
    fn journalctl_rewrite_respects_output_and_follow() {
        assert_eq!(journalctl_rewrite(&args("-u nginx -n 20")).unwrap(), args("--output=json -u nginx -n 20"));
        assert_eq!(journalctl_rewrite(&args("-b -1 -p err")).unwrap(), args("--output=json -b -1 -p err"));
        assert_eq!(journalctl_rewrite(&args("-o json-pretty -k")).unwrap(), args("-o json-pretty -k"));
        assert_eq!(journalctl_rewrite(&args("-ojson")).unwrap(), args("-ojson"));
        assert_eq!(journalctl_rewrite(&args("-o short-iso")), None);
        assert_eq!(journalctl_rewrite(&args("-fu nginx")), None);
        assert_eq!(journalctl_rewrite(&args("-u foo-f")).unwrap(), args("--output=json -u foo-f"));
        assert_eq!(journalctl_rewrite(&args("--list-boots")), None);
        assert_eq!(journalctl_rewrite(&args("--disk-usage")), None);
    }

    // `journalctl -n 2 -o json`, trimmed and anonymized.
    const JOURNAL: &str = r#"{"_HOSTNAME":"fedora","_SYSTEMD_SLICE":"system.slice","_SYSTEMD_UNIT":"ollama.service","__REALTIME_TIMESTAMP":"1793302890760895","_BOOT_ID":"c1de36ef121341a7aaa912cbb016b344","_COMM":"ollama","SYSLOG_IDENTIFIER":"ollama","_TRANSPORT":"stdout","PRIORITY":"6","_PID":"2077","_UID":"963","SYSLOG_FACILITY":"3","MESSAGE":"[GIN] 2026/10/29 - 20:41:30 | 200 |     249.629µs |       127.0.0.1 | GET      \"/api/tags\""}
{"MESSAGE":"overlayfs: try mounting with 'userxattr' option","__SEQNUM":"5353138","SYSLOG_IDENTIFIER":"kernel","PRIORITY":"6","__REALTIME_TIMESTAMP":"1793301917193552","_HOSTNAME":"fedora","_TRANSPORT":"kernel","SYSLOG_FACILITY":"0"}
{"__REALTIME_TIMESTAMP":"1793301917193553","PRIORITY":"4","_PID":"1","_SYSTEMD_UNIT":"init.scope","_SYSTEMD_USER_UNIT":"app.service","MESSAGE":[104,105,255],"SYSLOG_IDENTIFIER":["systemd","systemd"]}
"#;

    #[test]
    fn journal_entries() {
        let rows = journalctl_parse(JOURNAL).unwrap();
        assert_eq!(rows.len(), 3);
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.type_name(), Some("log.entry"));
        let Some(Value::Date(time)) = r.get("time") else { panic!("time is not a date") };
        assert_eq!(time.timestamp_micros(), 1793302890760895);
        assert_eq!(r.get("unit"), Some(&"ollama.service".into()));
        assert_eq!(r.get("priority"), Some(&Value::Int(6)));
        assert_eq!(r.get("pid"), Some(&Value::Int(2077)));
        let kernel = rows[1].as_record().unwrap();
        assert_eq!(kernel.get("unit"), Some(&Value::Nothing));
        assert_eq!(kernel.get("identifier"), Some(&"kernel".into()));
        let odd = rows[2].as_record().unwrap();
        assert_eq!(odd.get("unit"), Some(&"app.service".into()));
        assert_eq!(odd.get("message"), Some(&"hi\u{fffd}".into()));
        assert_eq!(odd.get("identifier"), Some(&"systemd".into()));
    }

    #[test]
    fn journal_pretty_and_seq() {
        let pretty = "{\n\t\"MESSAGE\" : \"a\",\n\t\"PRIORITY\" : \"3\"\n}\n{\n\t\"MESSAGE\" : \"b\"\n}\n";
        assert_eq!(journalctl_parse(pretty).unwrap().len(), 2);
        let seq = "\u{1e}{\"MESSAGE\":\"a\"}\n\u{1e}{\"MESSAGE\":\"b\"}\n";
        assert_eq!(journalctl_parse(seq).unwrap().len(), 2);
        assert!(journalctl_parse("Oct 29 20:41:30 fedora ollama[2077]: hi").is_err());
    }
}
