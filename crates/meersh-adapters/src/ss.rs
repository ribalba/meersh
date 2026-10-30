//! `ss` from iproute2. Its text output becomes `net.socket` records. The rewrite adds `-H`
//! to drop the header and `-n` for numeric ports unless the user asked for `-r`.
//!
//! ss leaves out the Netid column when only one socket table is selected (`ss -tl`) and the
//! State column when only one state is (`ss -t state established`). The rewrite works out
//! which value the missing column would have had and leaves it as a hint for `parse`, which
//! runs right after it on the same thread.

use std::cell::RefCell;

use meersh_value::{Record, Value};

use crate::util::{Opt, Spec, has, int_or_text, scan};

const SPEC: Spec =
    Spec { short: "fADFN", short_optional: "", long: &["family", "query", "socket", "diag", "filter", "net"] };

/// What the columns that ss left out would have said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hint {
    pub proto: Option<String>,
    pub state: Option<String>,
}

thread_local! {
    static HINT: RefCell<Hint> = RefCell::default();
}

pub fn rewrite(args: &[String]) -> Option<Vec<String>> {
    let opts = scan(args, &SPEC);
    let unsupported =
        [(Some('s'), "summary"), (Some('E'), "events"), (Some('h'), "help"), (Some('V'), "version"), (None, "json")];
    if unsupported.iter().any(|(short, long)| has(&opts, *short, &[long])) {
        return None;
    }
    HINT.with(|h| *h.borrow_mut() = hint(&opts));
    let mut out = Vec::new();
    if !has(&opts, Some('H'), &["no-header"]) {
        out.push("-H".to_string());
    }
    if !has(&opts, Some('n'), &["numeric"]) && !has(&opts, Some('r'), &["resolve"]) {
        out.push("-n".to_string());
    }
    out.extend_from_slice(args);
    Some(out)
}

pub fn parse(stdout: &str) -> Result<Vec<Value>, String> {
    let hint = HINT.with(|h| std::mem::take(&mut *h.borrow_mut()));
    parse_with(stdout, &hint)
}

/// The socket tables that the selection options name, as ss calls them in its Netid column.
fn tables(name: &str) -> &'static [&'static str] {
    match name {
        "tcp" => &["tcp"],
        "udp" => &["udp"],
        "raw" => &["raw"],
        "dccp" => &["dccp"],
        "sctp" => &["sctp"],
        "mptcp" => &["mptcp"],
        "tipc" => &["tipc"],
        "xdp" => &["xdp"],
        "netlink" => &["nl"],
        "unix_stream" => &["u_str"],
        "unix_dgram" => &["u_dgr"],
        "unix_seqpacket" => &["u_seq"],
        "packet_raw" => &["p_raw"],
        "packet_dgram" => &["p_dgr"],
        "vsock_stream" => &["v_str"],
        "vsock_dgram" => &["v_dgr"],
        "unix" => &["u_str", "u_dgr", "u_seq"],
        "packet" => &["p_raw", "p_dgr"],
        "vsock" => &["v_str", "v_dgr"],
        _ => &["tcp", "udp"],
    }
}

fn add(selected: &mut Vec<&'static str>, name: &str) {
    for t in tables(name) {
        if !selected.contains(t) {
            selected.push(t);
        }
    }
}

fn hint(opts: &[(usize, Opt)]) -> Hint {
    let mut selected = Vec::new();
    let mut family = None;
    let mut words = Vec::new();
    for (_, opt) in opts {
        match *opt {
            Opt::Short('t', _) | Opt::Long("tcp", _) => add(&mut selected, "tcp"),
            Opt::Short('u', _) | Opt::Long("udp", _) => add(&mut selected, "udp"),
            Opt::Short('w', _) | Opt::Long("raw", _) => add(&mut selected, "raw"),
            Opt::Short('x', _) | Opt::Long("unix", _) => add(&mut selected, "unix"),
            Opt::Short('d', _) | Opt::Long("dccp", _) => add(&mut selected, "dccp"),
            Opt::Short('S', _) | Opt::Long("sctp", _) => add(&mut selected, "sctp"),
            Opt::Short('M', _) | Opt::Long("mptcp", _) => add(&mut selected, "mptcp"),
            Opt::Short('0', _) | Opt::Long("packet", _) => add(&mut selected, "packet"),
            Opt::Long(name @ ("vsock" | "tipc" | "xdp"), _) => add(&mut selected, name),
            Opt::Short('A', Some(list)) | Opt::Long("query" | "socket", Some(list)) => {
                list.split(',').for_each(|name| add(&mut selected, name))
            }
            Opt::Short('f', Some(f)) | Opt::Long("family", Some(f)) => family = Some(f),
            Opt::Operand(word) => words.push(word),
            _ => {}
        }
    }
    if selected.is_empty()
        && let Some(f) = family
    {
        let name = match f {
            "unix" => "unix",
            "link" => "packet",
            "netlink" => "netlink",
            "vsock" => "vsock",
            "tipc" => "tipc",
            "xdp" => "xdp",
            _ => "inet",
        };
        add(&mut selected, name);
    }
    let proto = (selected.len() == 1).then(|| selected[0].to_string());

    // A filter of exactly one concrete state, such as `state established`.
    let mut states = Vec::new();
    let mut iter = words.iter();
    while let Some(word) = iter.next() {
        match *word {
            "state" => states.extend(iter.next().map(|s| state_name(s))),
            "exclude" | "excl" => states.push(None),
            _ => {}
        }
    }
    let state = match states.as_slice() {
        [Some(name)] => Some(name.to_string()),
        _ => None,
    };
    Hint { proto, state }
}

/// The column text ss prints for a state given in a filter expression.
fn state_name(filter: &str) -> Option<&'static str> {
    Some(match filter {
        "established" => "ESTAB",
        "syn-sent" => "SYN-SENT",
        "syn-recv" => "SYN-RECV",
        "fin-wait-1" => "FIN-WAIT-1",
        "fin-wait-2" => "FIN-WAIT-2",
        "time-wait" => "TIME-WAIT",
        "closed" => "UNCONN",
        "close-wait" => "CLOSE-WAIT",
        "last-ack" => "LAST-ACK",
        "listening" | "listen" => "LISTEN",
        "closing" => "CLOSING",
        _ => return None,
    })
}

const NETIDS: &[&str] = &[
    "tcp", "udp", "raw", "dccp", "sctp", "mptcp", "tipc", "xdp", "nl", "u_str", "u_dgr", "u_seq", "p_raw", "p_dgr",
    "v_str", "v_dgr", "icmp6",
];

const STATES: &[&str] = &[
    "ESTAB",
    "SYN-SENT",
    "SYN-RECV",
    "FIN-WAIT-1",
    "FIN-WAIT-2",
    "TIME-WAIT",
    "UNCONN",
    "CLOSE-WAIT",
    "LAST-ACK",
    "LISTEN",
    "CLOSING",
    "UNKNOWN",
];

fn is_number(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

/// Parses the text of ss. Columns that are missing because of the selection come from `hint`.
pub fn parse_with(stdout: &str, hint: &Hint) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        // Lines that start with blanks continue the previous socket (`-i`, `-m`).
        if line.trim().is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if matches!(tokens[0], "Netid" | "State" | "Recv-Q") {
            continue;
        }
        rows.push(Value::Record(socket(&tokens, hint).ok_or_else(|| format!("unexpected line '{}'", line.trim()))?));
    }
    Ok(rows)
}

fn socket(tokens: &[&str], hint: &Hint) -> Option<Record> {
    let mut i = 0;
    let first = *tokens.first()?;
    let second = tokens.get(1).copied().unwrap_or_default();
    // The Netid column, when present, is followed by a state or by the receive queue. A
    // netid this list does not know yet is recognized by the state that follows it.
    let netid = NETIDS.contains(&first)
        || (!is_number(first) && !STATES.contains(&first) && (!is_number(second) || hint.state.is_some()));
    let proto = if netid {
        i += 1;
        Some(first.to_string())
    } else {
        hint.proto.clone()
    };
    let state = match tokens.get(i) {
        Some(t) if !is_number(t) => {
            i += 1;
            Some(t.to_string())
        }
        _ => hint.state.clone(),
    };
    let recv_q = tokens.get(i)?.parse::<i64>().ok()?;
    let send_q = tokens.get(i + 1)?.parse::<i64>().ok()?;
    i += 2;
    // Unix sockets print the path and the inode as two words, the others `address:port`.
    let unix = proto.as_deref().is_some_and(|p| p.starts_with("u_"));
    let (local, port, remote, remote_port) = if unix {
        let words = tokens.get(i..i + 4)?;
        i += 4;
        (words[0].to_string(), int_or_text(words[1]), words[2].to_string(), int_or_text(words[3]))
    } else {
        let (local, port) = split_endpoint(tokens.get(i)?);
        let (remote, remote_port) = split_endpoint(tokens.get(i + 1)?);
        i += 2;
        (local, port, remote, remote_port)
    };
    let rest = tokens.get(i..).unwrap_or_default().join(" ");
    let (process, pid) = users(&rest);
    let text = |s: Option<String>| s.map_or(Value::Nothing, Value::String);
    Some(
        Record::typed("net.socket")
            .with("proto", text(proto))
            .with("state", text(state))
            .with("recv_q", recv_q)
            .with("send_q", send_q)
            .with("local", local)
            .with("port", port)
            .with("remote", remote)
            .with("remote_port", remote_port)
            .with("process", process)
            .with("pid", pid),
    )
}

/// `192.168.1.5:22`, `[::1]:631`, `*:*` or `127.0.0.53%lo:53` as address and port.
fn split_endpoint(endpoint: &str) -> (String, Value) {
    let Some((address, port)) = endpoint.rsplit_once(':') else {
        return (endpoint.to_string(), Value::Nothing);
    };
    let address = address.strip_prefix('[').and_then(|a| a.strip_suffix(']')).unwrap_or(address);
    (address.to_string(), int_or_text(port))
}

/// The first process in `users:(("sshd",pid=812,fd=3),...)`.
fn users(rest: &str) -> (Value, Value) {
    let Some(start) = rest.find("users:((\"") else {
        return (Value::Nothing, Value::Nothing);
    };
    let entry = &rest[start + 9..];
    let name = entry.split('"').next().unwrap_or_default();
    let pid = entry
        .split_once("pid=")
        .and_then(|(_, after)| after.split([',', ')']).next())
        .and_then(|p| p.parse::<i64>().ok())
        .map_or(Value::Nothing, Value::Int);
    (Value::String(name.to_string()), pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn hint_for(s: &str) -> Hint {
        hint(&scan(&args(s), &SPEC))
    }

    #[test]
    fn rewrite_drops_the_header_and_resolves_nothing() {
        assert_eq!(rewrite(&args("-tunap")).unwrap(), args("-H -tunap"));
        assert_eq!(rewrite(&args("-tuap")).unwrap(), args("-H -n -tuap"));
        assert_eq!(rewrite(&args("-tlnH")).unwrap(), args("-tlnH"));
        assert_eq!(rewrite(&args("-tr")).unwrap(), args("-H -tr"));
        assert_eq!(rewrite(&args("-s")), None);
        assert_eq!(rewrite(&args("--events")), None);
    }

    #[test]
    fn hints_name_the_columns_ss_leaves_out() {
        assert_eq!(hint_for("-tlnp").proto.as_deref(), Some("tcp"));
        assert_eq!(hint_for("-tunap").proto, None);
        assert_eq!(hint_for("-x").proto, None);
        assert_eq!(hint_for("-A unix_stream").proto.as_deref(), Some("u_str"));
        assert_eq!(hint_for("-f netlink").proto.as_deref(), Some("nl"));
        assert_eq!(hint_for("-t state established").state.as_deref(), Some("ESTAB"));
        assert_eq!(hint_for("-t state established state syn-sent").state, None);
        assert_eq!(hint_for("-t state connected").state, None);
    }

    // `ss -tunapH`, trimmed.
    const TUNAP: &str = r#"udp UNCONN     0      0                     0.0.0.0:41715         0.0.0.0:*     users:(("spotify",pid=1394764,fd=458))
udp UNCONN     0      0               127.0.0.53%lo:53            0.0.0.0:*
udp ESTAB      0      0      192.168.1.243%wlp166s0:68        192.168.1.1:67
tcp LISTEN     0      4096                  0.0.0.0:9573          0.0.0.0:*
tcp ESTAB      0      0               192.168.1.243:52404    89.167.57.13:443   users:(("firefox",pid=5911,fd=11),("firefox",pid=5912,fd=11))
tcp LISTEN     0      4096                     [::]:9573             [::]:*
tcp LISTEN     0      50                          *:1716                *:*     users:(("kdeconnectd",pid=2301,fd=24))
"#;

    #[test]
    fn inet_sockets() {
        let rows = parse_with(TUNAP, &Hint::default()).unwrap();
        assert_eq!(rows.len(), 7);
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.type_name(), Some("net.socket"));
        assert_eq!(r.get("proto"), Some(&"udp".into()));
        assert_eq!(r.get("state"), Some(&"UNCONN".into()));
        assert_eq!(r.get("local"), Some(&"0.0.0.0".into()));
        assert_eq!(r.get("port"), Some(&Value::Int(41715)));
        assert_eq!(r.get("remote_port"), Some(&Value::Nothing));
        assert_eq!(r.get("process"), Some(&"spotify".into()));
        assert_eq!(r.get("pid"), Some(&Value::Int(1394764)));
        let r = rows[1].as_record().unwrap();
        assert_eq!(r.get("local"), Some(&"127.0.0.53%lo".into()));
        assert_eq!(r.get("process"), Some(&Value::Nothing));
        let r = rows[4].as_record().unwrap();
        assert_eq!(r.get("remote"), Some(&"89.167.57.13".into()));
        assert_eq!(r.get("remote_port"), Some(&Value::Int(443)));
        assert_eq!(r.get("pid"), Some(&Value::Int(5911)));
        let r = rows[5].as_record().unwrap();
        assert_eq!(r.get("local"), Some(&"::".into()));
        assert_eq!(r.get("port"), Some(&Value::Int(9573)));
        assert_eq!(rows[6].as_record().unwrap().get("local"), Some(&"*".into()));
    }

    #[test]
    fn missing_columns_come_from_the_hint() {
        // `ss -tlnH`, one table and therefore no Netid column.
        let listen =
            "LISTEN 0      511        127.0.0.1:33617 0.0.0.0:*\nLISTEN 0      4096         0.0.0.0:9573  0.0.0.0:*\n";
        let rows = parse_with(listen, &Hint { proto: Some("tcp".into()), state: None }).unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.get("proto"), Some(&"tcp".into()));
        assert_eq!(r.get("state"), Some(&"LISTEN".into()));
        assert_eq!(r.get("port"), Some(&Value::Int(33617)));

        // `ss -tnH state established`, without Netid and State.
        let estab = "0      0      192.168.1.243:52404    89.167.57.13:443\n";
        let hint = Hint { proto: Some("tcp".into()), state: Some("ESTAB".into()) };
        let r = parse_with(estab, &hint).unwrap().remove(0);
        let r = r.as_record().unwrap();
        assert_eq!(r.get("state"), Some(&"ESTAB".into()));
        assert_eq!(r.get("remote_port"), Some(&Value::Int(443)));

        // `ss -tunH state established`, with Netid but without State.
        let both = "tcp   0      0      192.168.1.243:52404    89.167.57.13:443\n";
        let r = parse_with(both, &Hint { proto: None, state: Some("ESTAB".into()) }).unwrap().remove(0);
        assert_eq!(r.as_record().unwrap().get("proto"), Some(&"tcp".into()));
    }

    #[test]
    fn unix_and_extended_output() {
        let unix = "u_str ESTAB 0      0                                                                                       * 7445670 * 7445669\nu_str LISTEN 0      4096   /run/systemd/private 18234 * 0     users:((\"systemd\",pid=1,fd=17))\n";
        let rows = parse_with(unix, &Hint::default()).unwrap();
        let r = rows[1].as_record().unwrap();
        assert_eq!(r.get("local"), Some(&"/run/systemd/private".into()));
        assert_eq!(r.get("port"), Some(&Value::Int(18234)));
        assert_eq!(r.get("process"), Some(&"systemd".into()));

        // `ss -tieH` continues each socket on an indented line.
        let info = "CLOSE-WAIT 25     0      192.168.1.243:45610       89.167.57.13:443        uid:1000 ino:2093639 sk:8005 -->\n\t ts sack cubic wscale:7,10 rto:249\n";
        let rows = parse_with(info, &Hint { proto: Some("tcp".into()), state: None }).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].as_record().unwrap().get("recv_q"), Some(&Value::Int(25)));
    }

    #[test]
    fn headers_are_skipped_and_garbage_is_an_error() {
        let with_header = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\ntcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n";
        assert_eq!(parse_with(with_header, &Hint::default()).unwrap().len(), 1);
        assert!(parse_with("Total: 1234\n", &Hint::default()).unwrap_err().contains("Total: 1234"));
    }
}
