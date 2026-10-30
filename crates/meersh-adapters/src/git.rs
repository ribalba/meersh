//! `git status`, `git log` and `git branch`, rewritten to machine readable formats and mapped
//! to `git.status`, `git.commit` and `git.branch`. Other subcommands stay text.
//!
//! The three formats are told apart by their separators. `log` ends each commit with the
//! ASCII record separator, `branch` splits fields with the unit separator, and `status`
//! uses neither, because git quotes control characters in paths.

use chrono::DateTime;
use meersh_value::{Record, Value};

use crate::util::{Opt, Spec, insert_at, insert_last, scan};

const LOG_FORMAT: &str = "--format=%H%x1f%an%x1f%ae%x1f%aI%x1f%s%x1e";
const BRANCH_FORMAT: &str = "--format=%(HEAD)%1f%(refname:short)%1f%(upstream:short)";

/// Global options of git that take the next word as their value.
const GLOBAL_VALUES: &[&str] =
    &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix", "--config-env", "--list-cmds"];

/// Options of `git log` that add diffs, stats or graph lines to the output.
const LOG_NOISE: &[&str] = &[
    "-p",
    "-u",
    "--patch",
    "--graph",
    "--numstat",
    "--shortstat",
    "--name-only",
    "--name-status",
    "--raw",
    "--summary",
    "--compact-summary",
    "--cc",
    "-c",
    "-m",
    "--remerge-diff",
    "--check",
    "--patch-with-stat",
    "--patch-with-raw",
    "-W",
    "--function-context",
    "--show-signature",
    "--log-size",
    "--show-linear-break",
];
const LOG_NOISE_PREFIXES: &[&str] = &["--stat", "--dirstat", "-L", "-U", "--unified", "--diff-merges", "--line-prefix"];

/// Options of `git branch` that change branches instead of listing them.
const BRANCH_CHANGES: &[&str] = &[
    "delete",
    "move",
    "copy",
    "edit-description",
    "set-upstream-to",
    "unset-upstream",
    "show-current",
    "force",
    "track",
    "no-track",
    "create-reflog",
    "recurse-submodules",
    "format",
];
const BRANCH_LISTS: &[&str] = &["list", "merged", "no-merged", "contains", "no-contains", "points-at"];
const BRANCH_SPEC: Spec = Spec { short: "u", short_optional: "", long: &["sort", "points-at", "set-upstream-to"] };

pub fn rewrite(args: &[String]) -> Option<Vec<String>> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if GLOBAL_VALUES.contains(&arg.as_str()) {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let sub = args.get(i)?;
    let rest = &args[i + 1..];
    let before_dashdash = || rest.iter().take_while(|a| *a != "--");
    match sub.as_str() {
        "status" => {
            let custom_format = before_dashdash().any(|a| {
                matches!(a.as_str(), "--short" | "--long" | "--null" | "-z" | "--column")
                    || a.starts_with("--porcelain")
                    || a.starts_with("--column=")
                    || short_cluster_has(a, &['s', 'z'], 'u')
            });
            if custom_format {
                return None;
            }
            Some(insert_at(args, i + 1, &["--porcelain=v2", "--branch"]))
        }
        "log" => {
            let noisy = before_dashdash()
                .any(|a| LOG_NOISE.contains(&a.as_str()) || LOG_NOISE_PREFIXES.iter().any(|p| a.starts_with(p)));
            if noisy {
                return None;
            }
            // Placed last so it wins over `--oneline` or `--pretty` of the user. Limits such
            // as `-n 5`, `-5` or `--max-count=5` pass through unchanged.
            let mut out = args[..=i].to_vec();
            out.extend(insert_last(rest, &[LOG_FORMAT]));
            Some(out)
        }
        "branch" => {
            let opts = scan(rest, &BRANCH_SPEC);
            let mut listing = false;
            let mut operands = false;
            for (_, opt) in &opts {
                match *opt {
                    Opt::Short(c, _) if "dDmMcCuftl".contains(c) => {
                        if c == 'l' {
                            listing = true;
                        } else {
                            return None;
                        }
                    }
                    Opt::Long(name, _) if BRANCH_CHANGES.contains(&name) => return None,
                    Opt::Long(name, _) if BRANCH_LISTS.contains(&name) => listing = true,
                    Opt::Operand(_) => operands = true,
                    _ => {}
                }
            }
            // `git branch name` creates a branch unless one of the options asks for a list.
            if operands && !listing {
                return None;
            }
            Some(insert_at(args, i + 1, &[BRANCH_FORMAT]))
        }
        _ => None,
    }
}

/// True when `arg` is a cluster of short options such as `-sb` that contains one of `chars`
/// before the option `value_from`, whose value is the rest of the cluster.
fn short_cluster_has(arg: &str, chars: &[char], value_from: char) -> bool {
    let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.starts_with('-')) else { return false };
    cluster.chars().take_while(|c| *c != value_from).any(|c| chars.contains(&c))
}

pub fn parse(stdout: &str) -> Result<Vec<Value>, String> {
    if stdout.contains('\u{1e}') {
        parse_log(stdout)
    } else if stdout.contains('\u{1f}') {
        parse_branches(stdout)
    } else {
        parse_status(stdout)
    }
}

pub fn parse_log(stdout: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for chunk in stdout.split('\u{1e}') {
        let chunk = chunk.trim_matches(['\n', '\r', '\0']);
        if chunk.is_empty() {
            continue;
        }
        let fields: Vec<&str> = chunk.split('\u{1f}').collect();
        let [hash, author, email, date, subject] = fields[..] else {
            return Err(format!(
                "expected 5 fields per commit, found {} in '{}'",
                fields.len(),
                chunk.replace('\u{1f}', " ")
            ));
        };
        let date = DateTime::parse_from_rfc3339(date).map_err(|e| format!("bad commit date '{date}' ({e})"))?;
        rows.push(Value::Record(
            Record::typed("git.commit")
                .with("hash", hash)
                .with("author", author)
                .with("email", email)
                .with("date", Value::Date(date))
                .with("subject", subject),
        ));
    }
    Ok(rows)
}

pub fn parse_branches(stdout: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\u{1f}').collect();
        let [head, name, upstream] = fields[..] else {
            return Err(format!(
                "expected 3 fields per branch, found {} in '{}'",
                fields.len(),
                line.replace('\u{1f}', " ")
            ));
        };
        rows.push(Value::Record(
            Record::typed("git.branch")
                .with("name", name)
                .with("current", head.trim() == "*")
                .with("upstream", if upstream.is_empty() { Value::Nothing } else { upstream.into() }),
        ));
    }
    Ok(rows)
}

/// Parses `git status --porcelain=v2`. Header lines (`# branch.head main`) are skipped.
pub fn parse_status(stdout: &str) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let bad = || format!("unexpected line in git status '{line}'");
        let (tag, rest) = line.split_once(' ').ok_or_else(bad)?;
        let (kind, xy, path, orig) = match tag {
            "#" => continue,
            "1" => {
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                let [xy, .., path] = parts[..] else { return Err(bad()) };
                if parts.len() != 8 {
                    return Err(bad());
                }
                ("changed", xy, path, None)
            }
            "2" => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                if parts.len() != 9 {
                    return Err(bad());
                }
                let (path, orig) = parts[8].split_once('\t').ok_or_else(bad)?;
                let kind = if parts[7].starts_with('C') { "copied" } else { "renamed" };
                (kind, parts[0], path, Some(orig))
            }
            "u" => {
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                if parts.len() != 10 {
                    return Err(bad());
                }
                ("unmerged", parts[0], parts[9], None)
            }
            "?" => ("untracked", "..", rest, None),
            "!" => ("ignored", "..", rest, None),
            _ => return Err(bad()),
        };
        let mut codes = xy.chars();
        let (staged, unstaged) = (codes.next().ok_or_else(bad)?, codes.next().ok_or_else(bad)?);
        rows.push(Value::Record(
            Record::typed("git.status")
                .with("path", unquote(path))
                .with("staged", change(staged))
                .with("unstaged", change(unstaged))
                .with("kind", kind)
                .with("orig", orig.map_or(Value::Nothing, |o| Value::String(unquote(o)))),
        ));
    }
    Ok(rows)
}

fn change(code: char) -> &'static str {
    match code {
        'M' => "modified",
        'T' => "type-changed",
        'A' => "added",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'U' => "unmerged",
        _ => "",
    }
}

/// Undoes the C style quoting git uses for unusual paths, such as `"\303\274ber.txt"`.
fn unquote(path: &str) -> String {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(7),
            Some('b') => bytes.push(8),
            Some('f') => bytes.push(12),
            Some('v') => bytes.push(11),
            Some(d @ '0'..='7') => {
                let mut value = d as u32 - '0' as u32;
                for _ in 0..2 {
                    if let Some(next) = chars.peek().and_then(|c| c.to_digit(8)) {
                        value = value * 8 + next;
                        chars.next();
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => {
                let mut buf = [0; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
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
    fn rewrite_picks_formats_per_subcommand() {
        assert_eq!(rw("status").as_deref(), Some("status --porcelain=v2 --branch"));
        assert_eq!(rw("-C /tmp/repo status -uno").as_deref(), Some("-C /tmp/repo status --porcelain=v2 --branch -uno"));
        assert_eq!(rw("status -- src").as_deref(), Some("status --porcelain=v2 --branch -- src"));
        assert_eq!(rw("status -s"), None);
        assert_eq!(rw("status -sb"), None);
        assert_eq!(rw("status --porcelain"), None);

        assert_eq!(rw("log").as_deref(), Some(format!("log {LOG_FORMAT}").as_str()));
        assert_eq!(rw("log -n 5 --oneline").as_deref(), Some(format!("log -n 5 --oneline {LOG_FORMAT}").as_str()));
        assert_eq!(
            rw("log -3 main..dev -- src").as_deref(),
            Some(format!("log -3 main..dev {LOG_FORMAT} -- src").as_str())
        );
        assert_eq!(rw("log --max-count=2").as_deref(), Some(format!("log --max-count=2 {LOG_FORMAT}").as_str()));
        assert_eq!(rw("log -p"), None);
        assert_eq!(rw("log --stat=80"), None);
        assert_eq!(rw("log --graph"), None);

        assert_eq!(rw("branch").as_deref(), Some(format!("branch {BRANCH_FORMAT}").as_str()));
        assert_eq!(rw("branch -a -v").as_deref(), Some(format!("branch {BRANCH_FORMAT} -a -v").as_str()));
        assert_eq!(
            rw("branch --list 'feat*'").as_deref(),
            Some(format!("branch {BRANCH_FORMAT} --list 'feat*'").as_str())
        );
        assert_eq!(
            rw("branch --merged main").as_deref(),
            Some(format!("branch {BRANCH_FORMAT} --merged main").as_str())
        );
        assert_eq!(
            rw("branch --sort -committerdate").as_deref(),
            Some(format!("branch {BRANCH_FORMAT} --sort -committerdate").as_str())
        );
        assert_eq!(rw("branch new-feature"), None);
        assert_eq!(rw("branch -D old"), None);
        assert_eq!(rw("branch --format=%(refname)"), None);

        assert_eq!(rw("push"), None);
        assert_eq!(rw("--version"), None);
    }

    // `git status --porcelain=v2 --branch` in a repository with a conflict, a rename, a new
    // file, a change, an untracked file and a quoted path.
    const STATUS: &str = "# branch.oid 7ed54bf7691bb4ce2415414a833fa78aab46256a
# branch.head main
# branch.upstream origin/main
# branch.ab +1 -0
2 R. N... 100644 100644 100644 61780798228d17af2d34fce4cfbdf35556832472 61780798228d17af2d34fce4cfbdf35556832472 R100 b2.txt\tb.txt
1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 3e757656cf36eca53338e520d134963a44f793f8 n.txt
1 .M N... 100644 100644 100644 f2ad6c76f0115a6ba5b00456a849810e7ec0af20 f2ad6c76f0115a6ba5b00456a849810e7ec0af20 with space.txt
u UU N... 100644 100644 100644 100644 78981922613b2afb6025042ff6bd878ac1994e85 08ae698cfcd98c54e130571e2d6bb818d7c30438 93829c7b4af9dfbeea3b31395b042a614d7a190d a.txt
? untracked.txt
? \"\\303\\274mlaut.txt\"
";

    #[test]
    fn status_lines() {
        let rows = parse(STATUS).unwrap();
        assert_eq!(rows.len(), 6);
        let get = |i: usize, k: &str| rows[i].as_record().unwrap().get(k).cloned().unwrap();
        assert_eq!(rows[0].as_record().unwrap().type_name(), Some("git.status"));
        assert_eq!(get(0, "path"), "b2.txt".into());
        assert_eq!(get(0, "orig"), "b.txt".into());
        assert_eq!(get(0, "kind"), "renamed".into());
        assert_eq!(get(0, "staged"), "renamed".into());
        assert_eq!(get(1, "staged"), "added".into());
        assert_eq!(get(1, "unstaged"), "".into());
        assert_eq!(get(2, "path"), "with space.txt".into());
        assert_eq!(get(2, "unstaged"), "modified".into());
        assert_eq!(get(3, "kind"), "unmerged".into());
        assert_eq!(get(3, "path"), "a.txt".into());
        assert_eq!(get(4, "kind"), "untracked".into());
        assert_eq!(get(5, "path"), "ümlaut.txt".into());
        assert_eq!(parse("# branch.oid (initial)\n# branch.head main\n").unwrap(), vec![]);
        assert!(parse(" M README.md\n").is_err());
    }

    #[test]
    fn log_and_branches() {
        let log = "7ed54bf7691bb4ce2415414a833fa78aab46256a\u{1f}Ada Lovelace\u{1f}ada@example.org\u{1f}2026-10-29T20:47:01+01:00\u{1f}Main change\u{1e}\ndfcd453f16e470e3ca2a929ce9ce2d41996ee48d\u{1f}Ada Lovelace\u{1f}ada@example.org\u{1f}2026-10-29T20:47:01+01:00\u{1f}Feature: a | b ; c\u{1e}\n";
        let rows = parse(log).unwrap();
        assert_eq!(rows.len(), 2);
        let c = rows[1].as_record().unwrap();
        assert_eq!(c.type_name(), Some("git.commit"));
        assert_eq!(c.get("subject"), Some(&"Feature: a | b ; c".into()));
        assert_eq!(c.get("email"), Some(&"ada@example.org".into()));
        let Some(Value::Date(d)) = c.get("date") else { panic!("date is not a date") };
        assert_eq!(d.to_rfc3339(), "2026-10-29T20:47:01+01:00");
        assert!(parse("abc\u{1f}only two\u{1e}").is_err());

        let branches = " \u{1f}feature\u{1f}\n*\u{1f}main\u{1f}origin/main\n \u{1f}(HEAD detached at dfcd453)\u{1f}\n";
        let rows = parse(branches).unwrap();
        assert_eq!(rows.len(), 3);
        let b = rows[1].as_record().unwrap();
        assert_eq!(b.type_name(), Some("git.branch"));
        assert_eq!(b.get("current"), Some(&Value::Bool(true)));
        assert_eq!(b.get("upstream"), Some(&"origin/main".into()));
        assert_eq!(rows[0].as_record().unwrap().get("upstream"), Some(&Value::Nothing));
    }

    #[test]
    fn unquote_handles_escapes() {
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote(r#""tab\there""#), "tab\there");
        assert_eq!(unquote(r#""q\"uote""#), "q\"uote");
        assert_eq!(unquote(r#""\303\274""#), "ü");
    }
}
