//! Small system tools with column output: `who`, `free`, `uptime` and `df`.

use chrono::{Datelike, Local, NaiveDate, NaiveTime};
use meersh_value::{Record, Value};

use crate::util::{Opt, Spec, column_name, has, insert_last, local_date, scan};

/// Plain `who`, `who -u`, `who -m` and `who am i`. Options that add or change columns stay text.
pub fn who_rewrite(args: &[String]) -> Option<Vec<String>> {
    let supported = scan(args, &Spec::NONE).iter().all(|(_, opt)| match opt {
        Opt::Short(c, _) => "ums".contains(*c),
        Opt::Long(name, _) => *name == "users",
        Opt::Operand(_) => true,
    });
    supported.then(|| args.to_vec())
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Parses lines such as `ada  pts/0  2026-10-28 08:38  00:05  4242 (192.0.2.7)` into
/// `session.login` records. Idle time and pid only appear with `-u`.
pub fn who_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let bad = || format!("unexpected line '{}'", line.trim());
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let (user, tty) = (*tokens.first().ok_or_else(bad)?, *tokens.get(1).ok_or_else(bad)?);
        let (login, used) = login_time(&tokens[2..]).ok_or_else(bad)?;
        let rest = &tokens[2 + used..];
        let comment_at = rest.iter().position(|t| t.starts_with('(')).unwrap_or(rest.len());
        let host = rest[comment_at..].join(" ");
        let host = host.trim_start_matches('(').trim_end_matches(')');
        let mut idle = Value::Nothing;
        let mut pid = Value::Nothing;
        for token in &rest[..comment_at] {
            match token.parse::<i64>() {
                Ok(n) => pid = Value::Int(n),
                Err(_) => idle = Value::String(token.to_string()),
            }
        }
        rows.push(Value::Record(
            Record::typed("session.login")
                .with("user", user)
                .with("tty", tty)
                .with("login", login)
                .with("idle", idle)
                .with("pid", pid)
                .with("host", if host.is_empty() { Value::Nothing } else { host.into() }),
        ));
    }
    Ok(rows)
}

/// The login time at the start of `tokens` and how many tokens it took. GNU who writes
/// `2026-10-28 08:38`, other locales and older versions `Oct 28 08:38` without a year.
fn login_time(tokens: &[&str]) -> Option<(Value, usize)> {
    let time = |s: &str| NaiveTime::parse_from_str(s, "%H:%M").ok();
    if let (Some(day), Some(clock)) = (tokens.first(), tokens.get(1))
        && let (Ok(day), Some(clock)) = (NaiveDate::parse_from_str(day, "%Y-%m-%d"), time(clock))
    {
        return Some((local_date(day.and_time(clock)), 2));
    }
    let month = MONTHS.iter().position(|m| Some(m) == tokens.first())? as u32 + 1;
    let day: u32 = tokens.get(1)?.parse().ok()?;
    let clock = time(tokens.get(2)?)?;
    let now = Local::now().naive_local();
    let mut date = NaiveDate::from_ymd_opt(now.year(), month, day)?.and_time(clock);
    if date > now {
        date = NaiveDate::from_ymd_opt(now.year() - 1, month, day)?.and_time(clock);
    }
    Some((local_date(date), 3))
}

/// Appends `-b` so it wins over `-m` or `-g`. Human sizes, repeats and the one line format
/// stay text.
pub fn free_rewrite(args: &[String]) -> Option<Vec<String>> {
    let opts = scan(args, &Spec { short: "sc", short_optional: "", long: &["seconds", "count"] });
    let unsupported = [
        (Some('h'), "human"),
        (Some('s'), "seconds"),
        (Some('c'), "count"),
        (Some('L'), "line"),
        (Some('V'), "version"),
        (None, "help"),
    ];
    if unsupported.iter().any(|(short, long)| has(&opts, *short, &[long])) {
        return None;
    }
    let mut out = args.to_vec();
    if !matches!(opts.last(), Some((_, Opt::Short('b', _) | Opt::Long("bytes", _)))) {
        out.push("-b".into());
    }
    Some(out)
}

/// Parses `free -b` into one `mem.info` record per row (`mem`, `swap`, and `total` with `-t`).
/// Column names come from the header, so `free -w` gives `buffers` and `cache`.
pub fn free_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else { return Ok(vec![]) };
    let columns: Vec<String> = header.split_whitespace().map(column_name).collect();
    if columns.is_empty() || header.contains(':') {
        return Err(format!("expected a header line, found '{}'", header.trim()));
    }
    let mut rows = Vec::new();
    for line in lines {
        let mut tokens = line.split_whitespace();
        let label = tokens.next().unwrap_or_default();
        let Some(kind) = label.strip_suffix(':') else {
            return Err(format!("unexpected line '{}'", line.trim()));
        };
        let mut record = Record::typed("mem.info").with("kind", kind.to_lowercase());
        let mut values = tokens;
        for column in &columns {
            let value = match values.next() {
                Some(v) => {
                    Value::Filesize(v.parse().map_err(|_| format!("'{v}' is not a byte count in '{}'", line.trim()))?)
                }
                None => Value::Nothing,
            };
            record.insert(column.clone(), value);
        }
        rows.push(Value::Record(record));
    }
    Ok(rows)
}

/// Only plain `uptime` is supported. `-p` and `-s` print other formats.
pub fn uptime_rewrite(args: &[String]) -> Option<Vec<String>> {
    args.is_empty().then(Vec::new)
}

const MINUTE: i64 = 60_000_000_000;

/// Parses ` 20:46:00 up 1 day, 12:09,  2 users,  load average: 2.04, 1.89, 1.83` into a
/// `sys.uptime` record. Decimal commas in the load averages are accepted.
pub fn uptime_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let line = stdout.trim();
    if line.is_empty() {
        return Ok(vec![]);
    }
    let bad = || format!("unexpected uptime output '{line}'");
    let (time, rest) = line.split_once(" up ").ok_or_else(bad)?;
    let (span, load) = match rest.find("load average") {
        Some(at) => (&rest[..at], Some(&rest[at..])),
        None => (rest, None),
    };
    let mut uptime = 0;
    let mut users = Value::Nothing;
    for part in span.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let mut words = part.split_whitespace();
        let first = words.next().unwrap_or_default();
        let unit = words.next().unwrap_or_default();
        if let Some((h, m)) = first.split_once(':') {
            let (h, m): (i64, i64) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
            uptime += (h * 60 + m) * MINUTE;
            continue;
        }
        let n: i64 = first.parse().map_err(|_| bad())?;
        if unit.starts_with("user") {
            users = Value::Int(n);
        } else if unit.starts_with("day") {
            uptime += n * 24 * 60 * MINUTE;
        } else if unit.starts_with("hour") || unit.starts_with("hr") {
            uptime += n * 60 * MINUTE;
        } else if unit.starts_with("min") {
            uptime += n * MINUTE;
        } else if unit.starts_with("sec") {
            uptime += n * 1_000_000_000;
        } else {
            return Err(bad());
        }
    }
    let loads = match load {
        Some(load) => load_averages(load).ok_or_else(bad)?,
        None => vec![],
    };
    let load = |i: usize| loads.get(i).copied().map_or(Value::Nothing, Value::Float);
    Ok(vec![Value::Record(
        Record::typed("sys.uptime")
            .with("time", time.trim())
            .with("uptime", Value::Duration(uptime))
            .with("users", users)
            .with("load1", load(0))
            .with("load5", load(1))
            .with("load15", load(2)),
    )])
}

/// `load average: 2.04, 1.89, 1.83`, also with decimal commas or without the commas.
fn load_averages(text: &str) -> Option<Vec<f64>> {
    let (_, numbers) = text.split_once(':')?;
    let spaced = numbers.replace(", ", " ");
    let words: Vec<String> = if spaced.split_whitespace().count() == 1 {
        spaced.split(',').map(str::to_string).collect()
    } else {
        spaced.split_whitespace().map(|w| w.replace(',', ".")).collect()
    };
    words.iter().map(|w| w.trim().parse().ok()).collect()
}

const DF_OUTPUT: &str = "--output=source,fstype,size,used,avail,pcent,target";

/// Adds byte sizes and a fixed column list at the end, where they win over `-h` or `-k`.
/// Options that cannot be combined with `--output` stay text.
pub fn df_rewrite(args: &[String]) -> Option<Vec<String>> {
    let opts = scan(args, &Spec { short: "BtxF", short_optional: "", long: &["block-size", "type", "exclude-type"] });
    let unsupported = [
        (None, "output"),
        (Some('i'), "inodes"),
        (Some('P'), "portability"),
        (Some('T'), "print-type"),
        (None, "help"),
        (None, "version"),
    ];
    if unsupported.iter().any(|(short, long)| has(&opts, *short, &[long])) {
        return None;
    }
    Some(insert_last(args, &["-B1", DF_OUTPUT]))
}

/// Parses `df -B1 --output=source,fstype,size,used,avail,pcent,target` into `fs.mount`
/// records. Sources and mount points may contain blanks.
pub fn df_parse(stdout: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for (i, line) in stdout.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        match df_row(line) {
            Some(row) => rows.push(Value::Record(row)),
            // The first line is the header, in the language of the user.
            None if i == 0 => {}
            None => return Err(format!("unexpected line '{}'", line.trim())),
        }
    }
    Ok(rows)
}

fn df_row(line: &str) -> Option<Record> {
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, line.len()));
    }
    let word = |i: usize| &line[spans[i].0..spans[i].1];
    let number = |w: &str| w == "-" || w.bytes().all(|b| b.is_ascii_digit());
    let percent = |w: &str| {
        w == "-" || w.strip_suffix('%').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    // The percentage follows three numbers and needs a source and an fstype before them.
    let pct = (5..spans.len()).find(|&j| percent(word(j)) && (j - 3..j).all(|k| number(word(k))))?;
    let bytes = |w: &str| w.parse::<i64>().map_or(Value::Nothing, Value::Filesize);
    let source = line[..spans[pct - 4].0].trim();
    let fstype = word(pct - 4);
    let target = line[spans[pct].1..].trim();
    Some(
        Record::typed("fs.mount")
            .with("source", source)
            .with("fstype", fstype)
            .with("size", bytes(word(pct - 3)))
            .with("used", bytes(word(pct - 2)))
            .with("avail", bytes(word(pct - 1)))
            .with("use_pct", word(pct).trim_end_matches('%').parse::<i64>().map_or(Value::Nothing, Value::Int))
            .with("target", if target.is_empty() { Value::Nothing } else { target.into() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn who_sessions() {
        assert_eq!(who_rewrite(&args("")).unwrap(), args(""));
        assert_eq!(who_rewrite(&args("-u")).unwrap(), args("-u"));
        assert_eq!(who_rewrite(&args("am i")).unwrap(), args("am i"));
        assert_eq!(who_rewrite(&args("-a")), None);
        assert_eq!(who_rewrite(&args("-H")), None);

        let plain = "ada      seat0        2026-10-28 08:38\nada      tty2         2026-10-28 08:38\nbob      pts/3        2026-10-29 07:15 (192.0.2.7)\n";
        let rows = who_parse(plain).unwrap();
        assert_eq!(rows.len(), 3);
        let r = rows[2].as_record().unwrap();
        assert_eq!(r.type_name(), Some("session.login"));
        assert_eq!(r.get("tty"), Some(&"pts/3".into()));
        assert_eq!(r.get("host"), Some(&"192.0.2.7".into()));
        let Some(Value::Date(login)) = r.get("login") else { panic!("login is not a date") };
        assert_eq!(login.format("%Y-%m-%d %H:%M").to_string(), "2026-10-29 07:15");

        // `who -u`
        let users = "ada      seat0        2026-10-28 08:38   ?          5827 (login screen)\nada      tty2         2026-10-28 08:38  old         5827\n";
        let rows = who_parse(users).unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.get("idle"), Some(&"?".into()));
        assert_eq!(r.get("pid"), Some(&Value::Int(5827)));
        assert_eq!(r.get("host"), Some(&"login screen".into()));
        assert_eq!(rows[1].as_record().unwrap().get("idle"), Some(&"old".into()));

        let old = "ada      pts/0        Oct 28 08:38 (:0)\n";
        assert!(matches!(who_parse(old).unwrap()[0].as_record().unwrap().get("login"), Some(Value::Date(_))));
        assert!(who_parse("garbage\n").is_err());
    }

    // `free -b` and `free -b -w -t`.
    const FREE: &str = "               total        used        free      shared  buff/cache   available
Mem:     33331798016 26719113216  1927290880  4015251456  9543725056  6612684800
Swap:    25769795584 21966442496  3803353088
";
    const FREE_WIDE: &str =
        "               total        used        free      shared     buffers       cache   available
Mem:     33331798016 26719113216  1927290880  4015251456      585728  9543139328  6612684800
Swap:    25769795584 21966442496  3803353088
Total:   59101593600 48685555712  5730643968
";

    #[test]
    fn free_rows() {
        assert_eq!(free_rewrite(&args("")).unwrap(), args("-b"));
        assert_eq!(free_rewrite(&args("-m")).unwrap(), args("-m -b"));
        assert_eq!(free_rewrite(&args("-b")).unwrap(), args("-b"));
        assert_eq!(free_rewrite(&args("-h")), None);
        assert_eq!(free_rewrite(&args("-s 2")), None);

        let rows = free_parse(FREE).unwrap();
        let mem = rows[0].as_record().unwrap();
        assert_eq!(mem.type_name(), Some("mem.info"));
        assert_eq!(mem.get("kind"), Some(&"mem".into()));
        assert_eq!(mem.get("total"), Some(&Value::Filesize(33331798016)));
        assert_eq!(mem.get("buff_cache"), Some(&Value::Filesize(9543725056)));
        let swap = rows[1].as_record().unwrap();
        assert_eq!(swap.get("free"), Some(&Value::Filesize(3803353088)));
        assert_eq!(swap.get("available"), Some(&Value::Nothing));

        let rows = free_parse(FREE_WIDE).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].as_record().unwrap().get("buffers"), Some(&Value::Filesize(585728)));
        assert_eq!(rows[2].as_record().unwrap().get("kind"), Some(&"total".into()));

        assert!(free_parse("Mem: 1 2 3\n").is_err());
        assert!(free_parse("  total used\nMem: 31Gi 20Gi\n").unwrap_err().contains("31Gi"));
    }

    #[test]
    fn uptime_variants() {
        assert_eq!(uptime_rewrite(&args("")).unwrap(), args(""));
        assert_eq!(uptime_rewrite(&args("-p")), None);

        let rows = uptime_parse(" 20:46:00 up 1 day, 12:09,  2 users,  load average: 2.04, 1.89, 1.83\n").unwrap();
        let r = rows[0].as_record().unwrap();
        assert_eq!(r.type_name(), Some("sys.uptime"));
        assert_eq!(r.get("time"), Some(&"20:46:00".into()));
        assert_eq!(r.get("uptime"), Some(&Value::Duration((36 * 60 + 9) * MINUTE)));
        assert_eq!(r.get("users"), Some(&Value::Int(2)));
        assert_eq!(r.get("load1"), Some(&Value::Float(2.04)));
        assert_eq!(r.get("load15"), Some(&Value::Float(1.83)));

        let r = uptime_parse(" 08:01:00 up 5 min,  1 user,  load average: 0,52, 0,31, 0,12").unwrap().remove(0);
        let r = r.as_record().unwrap();
        assert_eq!(r.get("uptime"), Some(&Value::Duration(5 * MINUTE)));
        assert_eq!(r.get("load5"), Some(&Value::Float(0.31)));

        // BusyBox leaves out the users.
        let r = uptime_parse("10:00:00 up 3 days,  4 min, load average: 0.00, 0.01, 0.05").unwrap().remove(0);
        let r = r.as_record().unwrap();
        assert_eq!(r.get("uptime"), Some(&Value::Duration((3 * 24 * 60 + 4) * MINUTE)));
        assert_eq!(r.get("users"), Some(&Value::Nothing));

        assert!(uptime_parse("up 1 day, 12 hours, 9 minutes").is_err());
    }

    // `df -B1 --output=source,fstype,size,used,avail,pcent,target`, trimmed.
    const DF: &str = "Filesystem             Type         1B-blocks          Used         Avail Use% Mounted on
/dev/dm-0              btrfs     498387124224  354356781056  136632836096  73% /
devtmpfs               devtmpfs   16600363008             0   16600363008   0% /dev
efivarfs               efivarfs        274344        234676         34548  88% /sys/firmware/efi/efivars
//nas/my share         cifs      1000000000000 500000000000  500000000000  50% /mnt/my share
total                  -        531253831680 354591691988  170182929172  68% -
";

    #[test]
    fn df_rows() {
        assert_eq!(df_rewrite(&args("")).unwrap(), vec!["-B1".to_string(), DF_OUTPUT.to_string()]);
        assert_eq!(df_rewrite(&args("-h /home")).unwrap(), args(&format!("-h /home -B1 {DF_OUTPUT}")));
        assert_eq!(df_rewrite(&args("-x tmpfs -- /")).unwrap(), args(&format!("-x tmpfs -B1 {DF_OUTPUT} -- /")));
        assert_eq!(df_rewrite(&args("-i")), None);
        assert_eq!(df_rewrite(&args("-hT")), None);
        assert_eq!(df_rewrite(&args("--output=target")), None);

        let rows = df_parse(DF).unwrap();
        assert_eq!(rows.len(), 5);
        let root = rows[0].as_record().unwrap();
        assert_eq!(root.type_name(), Some("fs.mount"));
        assert_eq!(root.get("source"), Some(&"/dev/dm-0".into()));
        assert_eq!(root.get("fstype"), Some(&"btrfs".into()));
        assert_eq!(root.get("size"), Some(&Value::Filesize(498387124224)));
        assert_eq!(root.get("use_pct"), Some(&Value::Int(73)));
        assert_eq!(root.get("target"), Some(&"/".into()));
        let share = rows[3].as_record().unwrap();
        assert_eq!(share.get("source"), Some(&"//nas/my share".into()));
        assert_eq!(share.get("target"), Some(&"/mnt/my share".into()));
        assert_eq!(rows[4].as_record().unwrap().get("target"), Some(&"-".into()));
        assert!(df_parse("Filesystem Size\n/dev/sda1 10G 5G 5G 50% /\n").is_err());
    }
}
