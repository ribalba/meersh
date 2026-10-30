//! Sockets of a process, read from procfs. The descriptors in `/proc/<pid>/fd` name their
//! socket inode as `socket:[<inode>]`, and the socket tables of the process's network
//! namespace list the same inodes with addresses and state.

use std::collections::HashSet;
use std::fs;
use std::net::{IpAddr, SocketAddr};

use meersh_engine::value::{Record, Value};
use meersh_engine::{Ctx, Result};

use crate::reason;

/// Listening and connected TCP and UDP sockets of a process, as net.socket records.
pub(crate) fn sockets(ctx: &Ctx, pid: u32) -> Result<Vec<Record>> {
    let fds = fs::read_dir(format!("/proc/{pid}/fd"))
        .map_err(|e| ctx.err(format!("cannot read the sockets of pid {pid} ({})", reason(&e))))?;
    let inodes: HashSet<u64> = fds
        .flatten()
        .filter_map(|fd| fs::read_link(fd.path()).ok())
        .filter_map(|link| link.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok())
        .collect();
    let mut rows = Vec::new();
    if inodes.is_empty() {
        return Ok(rows);
    }
    for proto in ["tcp", "tcp6", "udp", "udp6"] {
        // The tables under the pid belong to its network namespace, which may not be ours.
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/net/{proto}")) else { continue };
        rows.extend(text.lines().skip(1).filter_map(|line| socket(proto, line, &inodes)));
    }
    rows.sort_by_key(|r| (r.get("proto").map(Value::to_text), r.get("port").and_then(Value::as_int)));
    Ok(rows)
}

/// One line of `/proc/net/tcp` and its siblings, if it is one of `inodes` and listens or is
/// connected. The columns are slot, local address, remote address, state, queues, timers,
/// retransmits, uid, timeout and inode.
fn socket(proto: &str, line: &str, inodes: &HashSet<u64>) -> Option<Record> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let inode: u64 = fields.get(9)?.parse().ok()?;
    if !inodes.contains(&inode) {
        return None;
    }
    let (local, port) = address(fields.get(1)?)?;
    let (remote, remote_port) = address(fields.get(2)?)?;
    let code = u8::from_str_radix(fields.get(3)?, 16).ok()?;
    let connected = remote_port != 0;
    let state = match (proto.starts_with("udp"), code) {
        // An unconnected UDP socket reports TCP_CLOSE. Bound to a port, it is what listening means for UDP.
        (true, 0x07) if port != 0 => "listen",
        (true, 0x01) => "established",
        (true, _) => return None,
        (false, 0x07) => return None,
        (false, code) => tcp_state(code),
    };
    let remote = connected.then(|| SocketAddr::new(remote, remote_port).to_string());
    Some(
        Record::typed("net.socket")
            .with("proto", proto)
            .with("local", local.to_string())
            .with("port", i64::from(port))
            .with("remote", remote)
            .with("state", state),
    )
}

/// `0100007F:1F90` is 127.0.0.1 port 8080. The kernel prints the address as 32 bit words
/// in host byte order, so the bytes of each word come back with `to_ne_bytes`. The port is
/// already in host order.
fn address(text: &str) -> Option<(IpAddr, u16)> {
    let (addr, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if addr.len() % 8 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(16);
    for i in (0..addr.len()).step_by(8) {
        bytes.extend(u32::from_str_radix(addr.get(i..i + 8)?, 16).ok()?.to_ne_bytes());
    }
    let ip = match bytes.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?),
        16 => IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?),
        _ => return None,
    };
    Some((ip, port))
}

/// Names from `include/net/tcp_states.h`, in the style of `ss`.
fn tcp_state(code: u8) -> &'static str {
    match code {
        0x01 => "established",
        0x02 => "syn-sent",
        0x03 => "syn-recv",
        0x04 => "fin-wait-1",
        0x05 => "fin-wait-2",
        0x06 => "time-wait",
        0x08 => "close-wait",
        0x09 => "last-ack",
        0x0A => "listen",
        0x0B => "closing",
        0x0C => "new-syn-recv",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_from_procfs() {
        if cfg!(target_endian = "little") {
            assert_eq!(address("0100007F:1F90"), Some(("127.0.0.1".parse().unwrap(), 8080)));
            assert_eq!(address("00000000000000000000000001000000:0016"), Some(("::1".parse().unwrap(), 22)));
        }
        assert_eq!(address("zz:0016"), None);
    }
}
