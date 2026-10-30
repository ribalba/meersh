#!/usr/bin/env python3
"""Fill the terminal windows of public/index.html with real meersh output.

The page shows terminal windows instead of screenshots. This script builds a small demo
project in a temporary directory, runs the commands with an isolated ledger, cache and
trash, and writes the output between the `<!-- capture:<name> -->` markers. So a window
on the page shows what meersh actually printed.

    python3 terminal/capture.py                  # everything that needs no model
    python3 terminal/capture.py --llm            # also the actor run, needs Ollama
    python3 terminal/capture.py --meersh PATH    # a specific binary

The demo files get fixed modification times, so the tables do not change between runs.
"""

import argparse
import html
import os
import re
import subprocess
import tempfile
from pathlib import Path

WEBSITE = Path(__file__).resolve().parent.parent
REPO = WEBSITE.parent
PAGE = WEBSITE / "public" / "index.html"
STAMP = 1793311200  # a fixed modification time for every demo file

CAPTURES = {
    "objects": ["ls | where size > 1kb | sort-by size -r"],
    "authority": [
        "policy explain fs.delete dump.sql --principal actor | select principal decision because",
        "touch draft.txt; rm draft.txt; undo | select action target result",
    ],
    "actor": ['ls | llm "delete all files that are larger than 5mb"'],
    "roles": [
        "[user plan actor agent:claude] | each { policy explain fs.delete dump.sql -p $it } | select principal decision because",
        "[README.md /etc/hostname] | each { policy explain fs.write $it -p agent:claude } | select target decision because",
    ],
}
NEEDS_MODEL = {"actor"}


def demo_project(root: Path) -> None:
    (root / "src").mkdir()
    files = {
        "README.md": "# demo\n" + "A small project for the meersh website.\n" * 30,
        "Cargo.toml": '[package]\nname = "demo"\nversion = "0.1.0"\n',
        "src/main.rs": 'fn main() {\n    println!("hello");\n}\n',
        "dump.sql": "INSERT INTO t VALUES (1);\n" * 80_000,
        "build-old.bin": "\0" * 7_000_000,
    }
    for name, content in files.items():
        path = root / name
        path.write_text(content)
        os.utime(path, (STAMP, STAMP))
    os.utime(root / "src", (STAMP, STAMP))


def meersh_binary(given: str | None) -> str:
    if given:
        return str(Path(given).resolve())
    for profile in ("release", "debug"):
        candidate = REPO / "target" / profile / "meersh"
        if candidate.exists():
            return str(candidate)
    raise SystemExit("build meersh first, or pass --meersh")


def run(meersh: str, command: str, cwd: Path, env: dict) -> str:
    done = subprocess.run([meersh, "-c", command], cwd=cwd, env=env, capture_output=True, text=True, timeout=600)
    # A plan goes to stderr in -c mode, results to stdout. The window shows both, in that order.
    return (done.stderr + done.stdout).rstrip()


def block(meersh: str, commands: list[str], cwd: Path, env: dict) -> str:
    parts = []
    for command in commands:
        output = run(meersh, command, cwd, env)
        line = f'<span class="prompt">〉</span><span class="cmd">{html.escape(command)}</span>'
        parts.append(line + ("\n" + html.escape(output) if output else ""))
    return "<pre>" + "\n".join(parts) + "</pre>"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--llm", action="store_true", help="also capture the runs that need a model")
    parser.add_argument("--meersh", help="the meersh binary to run")
    args = parser.parse_args()
    meersh = meersh_binary(args.meersh)

    page = PAGE.read_text()
    with tempfile.TemporaryDirectory(prefix="meersh-site-") as tmp:
        tmp = Path(tmp)
        project = tmp / "demo"
        project.mkdir()
        demo_project(project)
        env = dict(os.environ)
        env.update({
            "NO_COLOR": "1",
            "COLUMNS": "104",
            "MEERSH_INIT": "/dev/null",
            "XDG_DATA_HOME": str(tmp / "data"),
            "XDG_CACHE_HOME": str(tmp / "cache"),
            "XDG_CONFIG_HOME": str(tmp / "config"),
        })
        for name, commands in CAPTURES.items():
            if name in NEEDS_MODEL and not args.llm:
                print(f"skipped {name}, it needs --llm")
                continue
            fresh = block(meersh, commands, project, env)
            marker = re.compile(rf"(<!-- capture:{name} -->\n).*?(\n<!-- /capture:{name} -->)", re.S)
            if not marker.search(page):
                raise SystemExit(f"no capture:{name} markers in {PAGE}")
            page = marker.sub(lambda m: m.group(1) + fresh + m.group(2), page)
            print(f"captured {name}")
    PAGE.write_text(page)


if __name__ == "__main__":
    main()
