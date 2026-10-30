# meersh

<img src="website/public/img/logo-square.png" width="160" alt="The meersh meerkat, peeking over a terminal" />

The meer shell

An object oriented shell for Linux, written in Rust. Commands pass typed records instead of text, language models are ordinary pipeline stages, and every action has an author, a target and a right to act. [CONCEPT.md](CONCEPT.md) explains the design and the projects it builds on. [AUTHORITY.md](AUTHORITY.md) explains principals, ownership, plans, undo and models that act on objects.

```text
ps | where cpu > 50 | sort-by cpu -r | first 5 | kill
ls | llm "delete all files that are larger than 5mb"       # the model plans, you approve
ls *.md | llm map "topic in three words" --into topic
^journalctl -b -p err | last 40 | llm "group the problems" --schema [{unit: string, problem: string, count: int}]
^ip addr | where state == UP | select name addresses      # adapters turn legacy output into records
sandbox { ^npm install }                                  # see what a program would change first
with {fs.write: ./out, net: none} { ^make }               # Landlock confined block
journal | where principal == actor | undo
? the three biggest files here
```

The static build runs on every Linux distribution on x86_64 and ARM64. The script installs it to `~/.local/bin`, or to `/usr/local/bin` as root, and updates it when run again. The [releases](https://github.com/ribalba/meersh/releases) also hold .deb and .rpm packages and tarballs.

```sh
curl -fsSL https://github.com/ribalba/meersh/releases/latest/download/install.sh | sh
```

To build from source:

```sh
cargo build
./target/debug/meersh                     # REPL with completion and approvals
./target/debug/meersh -c 'ls | sort-by size -r | first 3'
./target/debug/meersh mcp --agent claude  # meersh as the tool layer of a coding agent
make install PREFIX=~/.local
```

To make Claude Code use meersh instead of bash, with every security feature in force, follow [integrations/claude](integrations/claude/README.md).

Language models run locally through Ollama by default. Claude Code and OpenAI compatible servers can be configured, see [config.example.toml](config.example.toml). Startup definitions, aliases and policy rules go into `~/.config/meersh/init.msh`.

An interactive meersh checks once a day whether the version on the main branch is newer than itself. If it is, meersh offers to build it from a clone in `~/.cache/meersh/source` and install it into the prefix it runs from, then restarts. `s` skips that version. A meersh it cannot write to, such as one from a package, only says that an update exists. Set `check = false` under `[update]` in the config or `MEERSH_NO_UPDATE_CHECK=1` to turn the check off. Raising the version in `Cargo.toml` on main is what releases it.
