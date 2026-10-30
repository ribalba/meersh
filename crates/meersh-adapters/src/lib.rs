//! Level 2 of the external program interop. Adapters turn the output of well known programs
//! into typed records, in the spirit of jc, so that `^ip addr | where state == UP` works.
//!
//! The engine applies an adapter only when the stdout of the program feeds a builtin. It asks
//! the adapter for the arguments to run (`ip addr` becomes `ip -j addr`) and parses the whole
//! output afterwards. An invocation the adapter does not know, such as `ip addr add`, keeps
//! its plain text lines. Parsing never runs processes, so every parser is tested with
//! fixture strings.

mod blk;
mod container;
mod git;
mod ip;
mod ss;
mod sys;
mod systemd;
mod util;

use meersh_engine::value::{Type, Value};
use meersh_engine::{Adapter, Engine, TypeInfo};

type Rewrite = fn(&[String]) -> Option<Vec<String>>;
type Parse = fn(&str) -> Result<Vec<Value>, String>;

/// An adapter made of a rewrite and a parse function.
struct Program {
    name: &'static str,
    rewrite: Rewrite,
    parse: Parse,
}

impl Adapter for Program {
    fn program(&self) -> &str {
        self.name
    }

    fn rewrite(&self, args: &[String]) -> Option<Vec<String>> {
        (self.rewrite)(args)
    }

    fn parse(&self, stdout: &str) -> Result<Vec<Value>, String> {
        (self.parse)(stdout)
    }
}

/// The programs with an adapter, with their rewrite and parse functions.
const PROGRAMS: &[(&str, Rewrite, Parse)] = &[
    ("ip", ip::rewrite, ip::parse),
    ("ss", ss::rewrite, ss::parse),
    ("systemctl", systemd::systemctl_rewrite, systemd::systemctl_parse),
    ("journalctl", systemd::journalctl_rewrite, systemd::journalctl_parse),
    ("git", git::rewrite, git::parse),
    ("lsblk", blk::lsblk_rewrite, blk::lsblk_parse),
    ("findmnt", blk::findmnt_rewrite, blk::findmnt_parse),
    ("docker", container::docker_rewrite, container::parse),
    ("podman", container::podman_rewrite, container::parse),
    ("who", sys::who_rewrite, sys::who_parse),
    ("free", sys::free_rewrite, sys::free_parse),
    ("uptime", sys::uptime_rewrite, sys::uptime_parse),
    ("df", sys::df_rewrite, sys::df_parse),
];

/// Registers every adapter and the record types they produce.
pub fn register(engine: &mut Engine) {
    for &(name, rewrite, parse) in PROGRAMS {
        engine.register_adapter(Program { name, rewrite, parse });
    }
    for info in types() {
        engine.register_type(info);
    }
}

fn ty(name: &str, description: &str, fields: &[(&str, Type, &str)]) -> TypeInfo {
    TypeInfo {
        name: name.into(),
        description: description.into(),
        fields: fields.iter().map(|(n, t, d)| (n.to_string(), t.clone(), d.to_string())).collect(),
    }
}

/// The record types of all adapters.
pub fn types() -> Vec<TypeInfo> {
    use Type::{Bool, Date, Duration, Filesize, Float, Int, String};
    let strings = || Type::list(String);
    vec![
        ty(
            "net.interface",
            "A network interface, from `^ip addr` or `^ip link`.",
            &[
                ("name", String, "interface name"),
                ("index", Int, "interface index"),
                ("state", String, "operational state such as UP, DOWN or UNKNOWN"),
                ("mtu", Int, "maximum transmission unit"),
                ("mac", String, "link layer address"),
                ("type", String, "link type such as ether, loopback or none"),
                ("flags", strings(), "interface flags such as UP and LOWER_UP"),
                ("addresses", Type::table("net.address"), "assigned addresses, empty for `ip link`"),
            ],
        ),
        ty(
            "net.address",
            "An address of a network interface, in the addresses of `net.interface`.",
            &[
                ("family", String, "inet or inet6"),
                ("address", String, "the address without prefix length"),
                ("prefix", Int, "prefix length"),
                ("scope", String, "host, link or global"),
                ("broadcast", String, "broadcast address"),
            ],
        ),
        ty(
            "net.route",
            "A routing table entry, from `^ip route`.",
            &[
                ("dst", String, "destination prefix or default"),
                ("gateway", String, "next hop"),
                ("dev", String, "outgoing interface"),
                ("protocol", String, "origin such as kernel, dhcp or static"),
                ("scope", String, "scope such as link or host"),
                ("src", String, "preferred source address"),
                ("metric", Int, "route metric"),
                ("flags", strings(), "flags such as linkdown"),
            ],
        ),
        ty(
            "net.neighbor",
            "An ARP or NDP cache entry, from `^ip neigh`.",
            &[
                ("address", String, "IP address of the neighbor"),
                ("dev", String, "interface"),
                ("mac", String, "link layer address"),
                ("state", String, "REACHABLE, STALE, FAILED and so on"),
                ("router", Bool, "whether the neighbor is a router"),
            ],
        ),
        ty(
            "net.socket",
            "A socket, from `^ss`.",
            &[
                ("proto", String, "tcp, udp, u_str and so on"),
                ("state", String, "LISTEN, ESTAB, UNCONN and so on"),
                ("recv_q", Int, "bytes in the receive queue"),
                ("send_q", Int, "bytes in the send queue"),
                ("local", String, "local address, or the path of a unix socket"),
                ("port", Int, "local port, or the inode of a unix socket"),
                ("remote", String, "peer address"),
                ("remote_port", Int, "peer port"),
                ("process", String, "name of the first process using the socket (with -p)"),
                ("pid", Int, "pid of that process (with -p)"),
            ],
        ),
        ty(
            "systemd.unit",
            "A systemd unit, from `^systemctl list-units` or `^systemctl list-unit-files`.",
            &[
                ("unit", String, "unit name"),
                ("load", String, "load state (list-units)"),
                ("active", String, "active state such as active or failed (list-units)"),
                ("sub", String, "sub state such as running or exited (list-units)"),
                ("description", String, "description (list-units)"),
                ("state", String, "enablement such as enabled or static (list-unit-files)"),
                ("preset", String, "vendor preset (list-unit-files)"),
            ],
        ),
        ty(
            "systemd.timer",
            "A systemd timer, from `^systemctl list-timers`.",
            &[
                ("unit", String, "timer unit"),
                ("activates", String, "unit the timer starts"),
                ("next", Date, "next elapse"),
                ("last", Date, "last trigger"),
            ],
        ),
        ty(
            "log.entry",
            "A journal entry, from `^journalctl`.",
            &[
                ("time", Date, "time of the entry"),
                ("unit", String, "systemd unit, the user unit for user services"),
                ("identifier", String, "syslog identifier such as kernel or sshd"),
                ("priority", Int, "0 (emerg) to 7 (debug)"),
                ("pid", Int, "process id"),
                ("message", String, "the message"),
            ],
        ),
        ty(
            "git.status",
            "A changed path, from `^git status`.",
            &[
                ("path", String, "path relative to the current directory, as git prints it"),
                ("staged", String, "change in the index such as modified or added, empty when none"),
                ("unstaged", String, "change in the work tree, empty when none"),
                ("kind", String, "changed, renamed, copied, unmerged, untracked or ignored"),
                ("orig", String, "original path of a rename or copy"),
            ],
        ),
        ty(
            "git.commit",
            "A commit, from `^git log`.",
            &[
                ("hash", String, "full commit hash"),
                ("author", String, "author name"),
                ("email", String, "author email"),
                ("date", Date, "author date"),
                ("subject", String, "first line of the message"),
            ],
        ),
        ty(
            "git.branch",
            "A branch, from `^git branch`.",
            &[
                ("name", String, "short branch name"),
                ("current", Bool, "whether HEAD points at it"),
                ("upstream", String, "upstream branch"),
            ],
        ),
        ty(
            "block.device",
            "A block device, from `^lsblk`. Columns chosen with -o are added under their names.",
            &[
                ("name", String, "device name"),
                ("maj_min", String, "major and minor number"),
                ("rm", Bool, "removable"),
                ("size", Filesize, "size"),
                ("ro", Bool, "read only"),
                ("type", String, "disk, part, crypt, lvm, rom and so on"),
                ("mountpoints", strings(), "where the device is mounted"),
                ("parent", String, "name of the device this one belongs to"),
            ],
        ),
        ty(
            "fs.mount",
            "A mounted file system, from `^df` or `^findmnt`.",
            &[
                ("source", String, "device or remote share"),
                ("fstype", String, "file system type"),
                ("size", Filesize, "total size (df, findmnt -D)"),
                ("used", Filesize, "used space (df, findmnt -D)"),
                ("avail", Filesize, "available space (df, findmnt -D)"),
                ("use_pct", Int, "used space in percent (df, findmnt -D)"),
                ("target", String, "mount point"),
                ("options", String, "mount options (findmnt)"),
            ],
        ),
        ty(
            "container",
            "A container, from `^docker ps` or `^podman ps`.",
            &[
                ("id", String, "container id"),
                ("name", String, "container names, comma separated"),
                ("image", String, "image"),
                ("command", String, "command, shortened by docker"),
                ("state", String, "running, exited, created and so on"),
                ("status", String, "status text such as Up 3 hours"),
                ("created", Date, "creation time"),
                ("ports", String, "published ports"),
            ],
        ),
        ty(
            "session.login",
            "A logged in user, from `^who`.",
            &[
                ("user", String, "user name"),
                ("tty", String, "terminal or seat"),
                ("login", Date, "login time"),
                ("idle", String, "idle time (with -u)"),
                ("pid", Int, "pid of the session leader (with -u)"),
                ("host", String, "remote host or comment"),
            ],
        ),
        ty(
            "mem.info",
            "A row of `^free` for memory, swap or the total.",
            &[
                ("kind", String, "mem, swap or total"),
                ("total", Filesize, "total"),
                ("used", Filesize, "used"),
                ("free", Filesize, "unused"),
                ("shared", Filesize, "shared memory (mem only)"),
                ("buff_cache", Filesize, "buffers and page cache (mem only)"),
                ("available", Filesize, "available for new programs (mem only)"),
            ],
        ),
        ty(
            "sys.uptime",
            "System uptime and load, from `^uptime`.",
            &[
                ("time", String, "current time of day"),
                ("uptime", Duration, "time since boot, to the minute"),
                ("users", Int, "logged in users"),
                ("load1", Float, "load average over 1 minute"),
                ("load5", Float, "load average over 5 minutes"),
                ("load15", Float, "load average over 15 minutes"),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_adapter_and_type_is_registered() {
        let mut engine = Engine::new();
        register(&mut engine);
        assert_eq!(engine.adapters().len(), PROGRAMS.len());
        for name in ["net.interface", "net.socket", "log.entry", "git.commit", "fs.mount", "sys.uptime"] {
            assert!(engine.type_info(name).is_some(), "{name} is not registered");
        }
        assert!(engine.adapter("ip").unwrap().rewrite(&["addr".into()]).is_some());
    }
}
