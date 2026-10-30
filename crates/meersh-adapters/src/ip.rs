//! `ip` from iproute2. Listing commands of `address`, `link`, `route` and `neigh` get `-j`
//! and their JSON becomes `net.interface`, `net.route` and `net.neighbor` records.

use meersh_value::{Record, Value};
use serde_json::Value as Json;

use crate::util::{Object, int, json_objects, strings, text};

/// The objects of `ip` in the order ip itself matches abbreviations, so that `ip r` is
/// `route` and not `rule`. Only the first part of the table matters for that.
const OBJECTS: &[&str] =
    &["address", "addrlabel", "maddress", "route", "rule", "neighbor", "neighbour", "ntable", "ntbl", "link", "l2tp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Value,
    Flag,
    Json,
    Unsupported,
}

/// Global options of ip in the order ip matches their abbreviations.
const OPTIONS: &[(&str, Kind)] = &[
    ("-loops", Kind::Value),
    ("-family", Kind::Value),
    ("-4", Kind::Flag),
    ("-6", Kind::Flag),
    ("-0", Kind::Flag),
    ("-M", Kind::Flag),
    ("-B", Kind::Flag),
    ("-human-readable", Kind::Flag),
    ("-iec", Kind::Flag),
    ("-statistics", Kind::Flag),
    ("-details", Kind::Flag),
    ("-resolve", Kind::Flag),
    ("-oneline", Kind::Flag),
    ("-timestamp", Kind::Flag),
    ("-tshort", Kind::Flag),
    ("-Version", Kind::Unsupported),
    ("-force", Kind::Flag),
    ("-batch", Kind::Unsupported),
    ("-brief", Kind::Flag),
    ("-json", Kind::Json),
    ("-pretty", Kind::Flag),
    ("-rcvbuf", Kind::Value),
    ("-color", Kind::Flag),
    ("-help", Kind::Unsupported),
    ("-netns", Kind::Value),
    ("-Numeric", Kind::Flag),
    ("-all", Kind::Flag),
    ("-echo", Kind::Flag),
];

fn option_kind(word: &str) -> Kind {
    // ip accepts `--json` as well as `-json`.
    let word = if word.starts_with("--") { &word[1..] } else { word };
    if word.starts_with("-c") && word.contains('=') {
        return Kind::Flag;
    }
    OPTIONS.iter().find(|(name, _)| name.starts_with(word)).map_or(Kind::Unsupported, |(_, kind)| *kind)
}

/// Adds `-j` to listing commands. Changes such as `ip addr add` and other objects stay text.
pub fn rewrite(args: &[String]) -> Option<Vec<String>> {
    let mut json = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        match option_kind(arg) {
            Kind::Value => i += 1,
            Kind::Json => json = true,
            Kind::Flag => {}
            Kind::Unsupported => return None,
        }
        i += 1;
    }
    let object = args.get(i)?;
    let object = OBJECTS.iter().find(|name| name.starts_with(object.as_str()))?;
    let verbs: &[&str] = match *object {
        "address" | "link" => &["show", "list", "lst"],
        "route" | "neighbor" | "neighbour" => &["show", "list", "lst", "get"],
        _ => return None,
    };
    if let Some(verb) = args.get(i + 1)
        && !(verb == "lst" || verb == "ls" || verbs.iter().any(|v| verb.len() >= 2 && v.starts_with(verb.as_str())))
    {
        // `ip addr dev lo` is not valid either, so anything else is a change or a typo.
        return None;
    }
    if json {
        return Some(args.to_vec());
    }
    let mut out = vec!["-j".to_string()];
    out.extend_from_slice(args);
    Some(out)
}

/// Parses the JSON of `ip -j`. The kind of record is taken from the fields of each entry,
/// since `parse` does not see the arguments.
pub fn parse(stdout: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for obj in json_objects(stdout)? {
        if obj.is_empty() {
            // `ip -j addr show to <prefix>` writes `{}` for interfaces without a match.
            continue;
        }
        let row = if obj.contains_key("ifname") {
            interface(&obj)
        } else if obj.contains_key("lladdr") || obj.get("state").is_some_and(Json::is_array) {
            neighbor(&obj)
        } else {
            route(&obj)
        };
        rows.push(Value::Record(row));
    }
    Ok(rows)
}

fn interface(obj: &Object) -> Record {
    let addresses = match obj.get("addr_info") {
        Some(Json::Array(items)) => {
            Value::List(items.iter().filter_map(Json::as_object).filter(|a| !a.is_empty()).map(address).collect())
        }
        _ => Value::Nothing,
    };
    Record::typed("net.interface")
        .with("name", text(obj, "ifname"))
        .with("index", int(obj, "ifindex"))
        .with("state", text(obj, "operstate"))
        .with("mtu", int(obj, "mtu"))
        .with("mac", text(obj, "address"))
        .with("type", text(obj, "link_type"))
        .with("flags", strings(obj, "flags"))
        .with("addresses", addresses)
}

fn address(obj: &Object) -> Value {
    let local = text(obj, "local");
    // Brief output (`ip -br -j addr`) leaves out the family.
    let family = match (text(obj, "family"), &local) {
        (Value::Nothing, Value::String(a)) => Value::String(if a.contains(':') { "inet6" } else { "inet" }.into()),
        (family, _) => family,
    };
    Value::Record(
        Record::typed("net.address")
            .with("family", family)
            .with("address", local)
            .with("prefix", int(obj, "prefixlen"))
            .with("scope", text(obj, "scope"))
            .with("broadcast", text(obj, "broadcast")),
    )
}

fn route(obj: &Object) -> Record {
    Record::typed("net.route")
        .with("dst", text(obj, "dst"))
        .with("gateway", text(obj, "gateway"))
        .with("dev", text(obj, "dev"))
        .with("protocol", text(obj, "protocol"))
        .with("scope", text(obj, "scope"))
        .with("src", text(obj, "prefsrc"))
        .with("metric", int(obj, "metric"))
        .with("flags", strings(obj, "flags"))
}

fn neighbor(obj: &Object) -> Record {
    let state = match obj.get("state") {
        Some(Json::Array(items)) => {
            let names: Vec<&str> = items.iter().filter_map(Json::as_str).collect();
            if names.is_empty() { Value::Nothing } else { Value::String(names.join(",")) }
        }
        _ => text(obj, "state"),
    };
    Record::typed("net.neighbor")
        .with("address", text(obj, "dst"))
        .with("dev", text(obj, "dev"))
        .with("mac", text(obj, "lladdr"))
        .with("state", state)
        // ip writes flags such as `"router": null`, so the key alone carries the meaning.
        .with("router", obj.contains_key("router"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn rw(s: &str) -> Option<String> {
        rewrite(&args(s)).map(|a| a.join(" "))
    }

    #[test]
    fn rewrite_adds_json_to_listings_only() {
        assert_eq!(rw("addr").as_deref(), Some("-j addr"));
        assert_eq!(rw("a").as_deref(), Some("-j a"));
        assert_eq!(rw("-4 address show dev lo").as_deref(), Some("-j -4 address show dev lo"));
        assert_eq!(rw("r").as_deref(), Some("-j r"));
        assert_eq!(rw("route get 1.1.1.1").as_deref(), Some("-j route get 1.1.1.1"));
        assert_eq!(rw("-br link").as_deref(), Some("-j -br link"));
        assert_eq!(rw("neigh").as_deref(), Some("-j neigh"));
        assert_eq!(rw("-n blue addr").as_deref(), Some("-j -n blue addr"));
        assert_eq!(rw("-j addr").as_deref(), Some("-j addr"));
        assert_eq!(rw("--json -p link").as_deref(), Some("--json -p link"));
        assert_eq!(rw("addr add 10.0.0.1/24 dev eth0"), None);
        assert_eq!(rw("link set eth0 up"), None);
        assert_eq!(rw("ru"), None);
        assert_eq!(rw("monitor"), None);
        assert_eq!(rw("-batch file"), None);
        assert_eq!(rw(""), None);
    }

    // `ip -j addr`, trimmed and anonymized.
    const ADDR: &str = r#"[{"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"mtu":65536,"qdisc":"noqueue","operstate":"UNKNOWN","group":"default","txqlen":1000,"link_type":"loopback","address":"00:00:00:00:00:00","broadcast":"00:00:00:00:00:00","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host","label":"lo","valid_life_time":4294967295,"preferred_life_time":4294967295},{"family":"inet6","local":"::1","prefixlen":128,"scope":"host","noprefixroute":true,"valid_life_time":4294967295,"preferred_life_time":4294967295}]},{"ifindex":2,"ifname":"wlp166s0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"mtu":1500,"qdisc":"noqueue","operstate":"UP","group":"default","txqlen":1000,"link_type":"ether","address":"52:54:00:12:34:56","broadcast":"ff:ff:ff:ff:ff:ff","altnames":["wlx525400123456"],"addr_info":[{"family":"inet","local":"192.168.1.243","prefixlen":24,"broadcast":"192.168.1.255","scope":"global","dynamic":true,"noprefixroute":true,"label":"wlp166s0","valid_life_time":71468,"preferred_life_time":71468},{"family":"inet6","local":"fe80::11a8:2f04:bc75:81cf","prefixlen":64,"scope":"link","noprefixroute":true,"valid_life_time":4294967295,"preferred_life_time":4294967295}]},{"ifindex":3,"ifname":"wg2","flags":["POINTOPOINT","NOARP","UP","LOWER_UP"],"mtu":1420,"qdisc":"noqueue","operstate":"UNKNOWN","group":"default","txqlen":1000,"link_type":"none","addr_info":[{"family":"inet","local":"10.10.0.3","prefixlen":32,"scope":"global","noprefixroute":true,"label":"wg2","valid_life_time":4294967295,"preferred_life_time":4294967295}]},{}]"#;

    #[test]
    fn addresses_become_interfaces() {
        let rows = parse(ADDR).unwrap();
        assert_eq!(rows.len(), 3);
        let wifi = rows[1].as_record().unwrap();
        assert_eq!(wifi.type_name(), Some("net.interface"));
        assert_eq!(wifi.get("name"), Some(&"wlp166s0".into()));
        assert_eq!(wifi.get("state"), Some(&"UP".into()));
        assert_eq!(wifi.get("mtu"), Some(&Value::Int(1500)));
        assert_eq!(wifi.get("mac"), Some(&"52:54:00:12:34:56".into()));
        let Some(Value::List(addrs)) = wifi.get("addresses") else { panic!("no addresses") };
        let first = addrs[0].as_record().unwrap();
        assert_eq!(first.type_name(), Some("net.address"));
        assert_eq!(first.get("address"), Some(&"192.168.1.243".into()));
        assert_eq!(first.get("prefix"), Some(&Value::Int(24)));
        assert_eq!(first.get("family"), Some(&"inet".into()));
        let wg = rows[2].as_record().unwrap();
        assert_eq!(wg.get("mac"), Some(&Value::Nothing));
    }

    #[test]
    fn brief_output_and_links() {
        let brief = r#"[{"ifname":"lo","operstate":"UNKNOWN","addr_info":[{"local":"127.0.0.1","prefixlen":8},{"local":"::1","prefixlen":128}]}]"#;
        let rows = parse(brief).unwrap();
        let Some(Value::List(addrs)) = rows[0].as_record().unwrap().get("addresses") else { panic!() };
        assert_eq!(addrs[1].as_record().unwrap().get("family"), Some(&"inet6".into()));

        let link = r#"[{"ifindex":5,"ifname":"br-1be49ef13616","flags":["NO-CARRIER","BROADCAST","MULTICAST","UP"],"mtu":1500,"qdisc":"noqueue","operstate":"DOWN","linkmode":"DEFAULT","group":"default","link_type":"ether","address":"66:87:93:ac:6d:dc","broadcast":"ff:ff:ff:ff:ff:ff"}]"#;
        let rows = parse(link).unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.get("state"), Some(&"DOWN".into()));
        assert_eq!(r.get("addresses"), Some(&Value::Nothing));
        assert_eq!(
            r.get("flags"),
            Some(&Value::List(vec!["NO-CARRIER".into(), "BROADCAST".into(), "MULTICAST".into(), "UP".into()]))
        );
    }

    #[test]
    fn routes_and_neighbors() {
        let routes = r#"[{"dst":"default","gateway":"192.168.1.1","dev":"wlp166s0","protocol":"dhcp","prefsrc":"192.168.1.243","metric":600,"flags":[]},{"dst":"10.99.0.0/24","dev":"docker0","protocol":"kernel","scope":"link","prefsrc":"10.99.0.1","flags":["linkdown"]}]"#;
        let rows = parse(routes).unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.type_name(), Some("net.route"));
        assert_eq!(r.get("gateway"), Some(&"192.168.1.1".into()));
        assert_eq!(r.get("metric"), Some(&Value::Int(600)));
        assert_eq!(rows[1].as_record().unwrap().get("gateway"), Some(&Value::Nothing));

        let neigh = r#"[{"dst":"192.168.1.1","dev":"wlp166s0","lladdr":"9c:05:d6:00:00:01","state":["REACHABLE"]},{"dst":"fe80::461:b81a:fdcd:d9ff","dev":"wlp166s0","lladdr":"ac:bc:b5:00:00:02","router":null,"state":["STALE"]},{"dst":"10.0.0.9","dev":"eth0","state":["FAILED"]}]"#;
        let rows = parse(neigh).unwrap();
        assert_eq!(rows.len(), 3);
        let n = rows[1].as_record().unwrap();
        assert_eq!(n.type_name(), Some("net.neighbor"));
        assert_eq!(n.get("state"), Some(&"STALE".into()));
        assert_eq!(n.get("router"), Some(&Value::Bool(true)));
        assert_eq!(rows[2].as_record().unwrap().type_name(), Some("net.neighbor"));
    }

    #[test]
    fn bad_output_is_an_error() {
        assert!(parse("1: lo: <LOOPBACK,UP>").is_err());
        assert_eq!(parse("").unwrap(), vec![]);
    }
}
