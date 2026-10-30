//! `lsblk` and `findmnt` from util-linux. Both get `-J -b`, so sizes arrive as byte counts.
//! The device and mount trees are flattened into tables. Columns chosen with `-o` are kept
//! under plain names (`maj:min` becomes `maj_min`, `use%` becomes `use_pct`).

use meersh_value::{Record, Value};
use serde_json::Value as Json;

use crate::util::{Object, Spec, boolean, column_name, has, insert_at, json_i64, scan, size, strings};

const LSBLK: Spec = Spec {
    short: "oeIxQEw",
    short_optional: "T",
    long: &[
        "output",
        "exclude",
        "include",
        "sort",
        "filter",
        "highlight",
        "dedup",
        "sysroot",
        "width",
        "properties-by",
        "ct",
        "ct-filter",
    ],
};

const FINDMNT: Spec = Spec {
    short: "FNOotSTwdMQ",
    short_optional: "",
    long: &[
        "tab-file",
        "task",
        "options",
        "output",
        "source",
        "target",
        "types",
        "timeout",
        "direction",
        "mountpoint",
        "filter",
        "id",
        "uniq-id",
    ],
};

/// Adds `-J` and `-b` unless they are there. The other output modes stay text.
fn add_json_bytes(args: &[String], spec: &Spec, unsupported: &[(Option<char>, &str)]) -> Option<Vec<String>> {
    let opts = scan(args, spec);
    if unsupported.iter().any(|(short, long)| has(&opts, *short, &[long])) {
        return None;
    }
    let mut extra = Vec::new();
    if !has(&opts, Some('J'), &["json"]) {
        extra.push("-J");
    }
    if !has(&opts, Some('b'), &["bytes"]) {
        extra.push("-b");
    }
    Some(insert_at(args, 0, &extra))
}

pub fn lsblk_rewrite(args: &[String]) -> Option<Vec<String>> {
    let unsupported = [
        (Some('P'), "pairs"),
        (Some('r'), "raw"),
        (Some('h'), "help"),
        (Some('V'), "version"),
        (Some('H'), "list-columns"),
    ];
    add_json_bytes(args, &LSBLK, &unsupported)
}

pub fn findmnt_rewrite(args: &[String]) -> Option<Vec<String>> {
    let unsupported = [
        (Some('P'), "pairs"),
        (Some('r'), "raw"),
        (Some('p'), "poll"),
        (Some('x'), "verify"),
        (Some('h'), "help"),
        (Some('V'), "version"),
        (Some('H'), "list-columns"),
    ];
    add_json_bytes(args, &FINDMNT, &unsupported)
}

/// The array under `key` of the single JSON object that lsblk and findmnt write.
fn top_level<'a>(json: &'a Json, key: &str, program: &str) -> Result<&'a [Json], String> {
    match json.get(key) {
        Some(Json::Array(items)) => Ok(items),
        _ => Err(format!("expected the JSON of {program} -J with a '{key}' list")),
    }
}

fn parse_json(stdout: &str) -> Result<Option<Json>, String> {
    if stdout.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(stdout).map(Some).map_err(|e| format!("not valid JSON ({e})"))
}

/// Columns that hold byte counts once `-b` is given.
const SIZES: &[&str] = &[
    "size",
    "fssize",
    "fsavail",
    "fsused",
    "avail",
    "used",
    "disc_gran",
    "disc_max",
    "min_io",
    "opt_io",
    "phy_sec",
    "log_sec",
    "zone_sz",
    "zone_wgran",
    "zone_app",
];
/// Columns that are flags. Older lsblk versions write them as `"0"` and `"1"`.
const FLAGS: &[&str] = &["rm", "ro", "rota", "hotplug", "rand", "disc_zero", "dax"];

/// One record with every field of `obj` except the children, typed where the column is known.
fn columns(type_name: &str, obj: &Object) -> Record {
    let mut record = Record::typed(type_name);
    for (key, json) in obj {
        if key == "children" {
            continue;
        }
        let name = column_name(key);
        let value = if SIZES.contains(&name.as_str()) {
            match size(obj, key) {
                Value::Nothing => Value::from_json(json),
                v => v,
            }
        } else if FLAGS.contains(&name.as_str()) {
            boolean(obj, key)
        } else if name == "mountpoints" {
            strings(obj, key)
        } else if name.ends_with("pct") {
            json.as_str()
                .and_then(|s| s.trim().trim_end_matches('%').parse::<i64>().ok())
                .or_else(|| json_i64(json))
                .map_or(Value::Nothing, Value::Int)
        } else {
            Value::from_json(json)
        };
        record.insert(name, value);
    }
    record
}

fn flatten(items: &[Json], parent: Option<&str>, out: &mut Vec<Value>, row: &dyn Fn(&Object, Option<&str>) -> Record) {
    for obj in items.iter().filter_map(Json::as_object) {
        out.push(Value::Record(row(obj, parent)));
        if let Some(Json::Array(children)) = obj.get("children") {
            let name = obj.get("name").and_then(Json::as_str);
            flatten(children, name, out, row);
        }
    }
}

/// Parses `lsblk -J -b` into `block.device` records. Children follow their parent and name
/// it in `parent`.
pub fn lsblk_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let Some(json) = parse_json(stdout)? else { return Ok(vec![]) };
    let mut rows = Vec::new();
    flatten(top_level(&json, "blockdevices", "lsblk")?, None, &mut rows, &|obj, parent| {
        columns("block.device", obj).with("parent", parent.map_or(Value::Nothing, Value::from))
    });
    Ok(rows)
}

/// Parses `findmnt -J -b` into `fs.mount` records, one per mount in tree order.
pub fn findmnt_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let Some(json) = parse_json(stdout)? else { return Ok(vec![]) };
    let mut rows = Vec::new();
    flatten(top_level(&json, "filesystems", "findmnt")?, None, &mut rows, &|obj, _| columns("fs.mount", obj));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn rewrites() {
        assert_eq!(lsblk_rewrite(&args("")).unwrap(), args("-J -b"));
        assert_eq!(lsblk_rewrite(&args("-f")).unwrap(), args("-J -b -f"));
        assert_eq!(lsblk_rewrite(&args("-Jo NAME,SIZE")).unwrap(), args("-b -Jo NAME,SIZE"));
        assert_eq!(lsblk_rewrite(&args("-o NAME,RO")).unwrap(), args("-J -b -o NAME,RO"));
        assert_eq!(lsblk_rewrite(&args("-P")), None);
        assert_eq!(lsblk_rewrite(&args("--raw")), None);
        assert_eq!(findmnt_rewrite(&args("-D")).unwrap(), args("-J -b -D"));
        assert_eq!(findmnt_rewrite(&args("-t ext4,btrfs")).unwrap(), args("-J -b -t ext4,btrfs"));
        assert_eq!(findmnt_rewrite(&args("--poll")), None);
    }

    // `lsblk -J -b` on a laptop with zram and an encrypted NVMe disk.
    const LSBLK_OUT: &str = r#"{
   "blockdevices": [
      {"name": "zram0", "maj:min": "251:0", "rm": false, "size": 8589934592, "ro": false, "type": "disk", "mountpoints": ["[SWAP]"]},
      {"name": "nvme0n1", "maj:min": "259:0", "rm": false, "size": 500107862016, "ro": false, "type": "disk", "mountpoints": [],
         "children": [
            {"name": "nvme0n1p1", "maj:min": "259:1", "rm": false, "size": 629145600, "ro": false, "type": "part", "mountpoints": ["/boot/efi"]},
            {"name": "nvme0n1p3", "maj:min": "259:3", "rm": false, "size": 498403901440, "ro": false, "type": "part", "mountpoints": [],
               "children": [
                  {"name": "luks-2538f95d", "maj:min": "252:0", "rm": false, "size": 498387124224, "ro": false, "type": "crypt", "mountpoints": ["/home", "/"]}
               ]
            }
         ]
      }
   ]
}"#;

    #[test]
    fn devices_are_flattened() {
        let rows = lsblk_parse(LSBLK_OUT).unwrap();
        let names: Vec<_> = rows.iter().map(|r| r.as_record().unwrap().get("name").cloned().unwrap()).collect();
        assert_eq!(
            names,
            vec!["zram0".into(), "nvme0n1".into(), "nvme0n1p1".into(), "nvme0n1p3".into(), "luks-2538f95d".into()]
        );
        let crypt = rows[4].as_record().unwrap();
        assert_eq!(crypt.type_name(), Some("block.device"));
        assert_eq!(crypt.get("size"), Some(&Value::Filesize(498387124224)));
        assert_eq!(crypt.get("ro"), Some(&Value::Bool(false)));
        assert_eq!(crypt.get("maj_min"), Some(&"252:0".into()));
        assert_eq!(crypt.get("parent"), Some(&"nvme0n1p3".into()));
        assert_eq!(crypt.get("mountpoints"), Some(&Value::List(vec!["/home".into(), "/".into()])));
        assert_eq!(rows[0].as_record().unwrap().get("parent"), Some(&Value::Nothing));
    }

    #[test]
    fn older_lsblk_writes_strings() {
        // util-linux 2.33 writes every value as a string and has `mountpoint`.
        let old = r#"{"blockdevices": [{"name": "sda", "maj:min": "8:0", "rm": "0", "size": "256060514304", "ro": "0", "type": "disk", "mountpoint": null, "mountpoints": [null]}]}"#;
        let rows = lsblk_parse(old).unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.get("size"), Some(&Value::Filesize(256060514304)));
        assert_eq!(r.get("rm"), Some(&Value::Bool(false)));
        assert_eq!(r.get("mountpoint"), Some(&Value::Nothing));
        assert_eq!(r.get("mountpoints"), Some(&Value::List(vec![])));
    }

    #[test]
    fn mounts() {
        let tree = r#"{"filesystems": [{"target": "/", "source": "/dev/mapper/luks-2538f95d[/root]", "fstype": "btrfs", "options": "rw,relatime,ssd",
            "children": [{"target": "/dev", "source": "devtmpfs", "fstype": "devtmpfs", "options": "rw,nosuid",
                "children": [{"target": "/dev/shm", "source": "tmpfs", "fstype": "tmpfs", "options": "rw,nosuid,nodev"}]}]}]}"#;
        let rows = findmnt_parse(tree).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].as_record().unwrap().get("target"), Some(&"/dev/shm".into()));
        assert_eq!(rows[2].as_record().unwrap().type_name(), Some("fs.mount"));

        // `findmnt -J -b -D`
        let df = r#"{"filesystems": [{"source": "/dev/mapper/luks-2538f95d[/root]", "fstype": "btrfs", "size": 498387124224, "used": 354341646336, "avail": 136647675904, "use%": "71%", "target": "/"}]}"#;
        let r = findmnt_parse(df).unwrap().remove(0);
        let r = r.as_record().unwrap();
        assert_eq!(r.get("used"), Some(&Value::Filesize(354341646336)));
        assert_eq!(r.get("use_pct"), Some(&Value::Int(71)));

        assert_eq!(findmnt_parse("").unwrap(), vec![]);
        assert!(findmnt_parse("TARGET SOURCE").is_err());
        assert!(findmnt_parse(r#"{"other": []}"#).unwrap_err().contains("filesystems"));
    }
}
