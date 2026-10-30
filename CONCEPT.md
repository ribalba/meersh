# meersh concept

meersh is a sketch of an object oriented shell for Linux. It is written in Rust, and a language model is an ordinary pipeline stage in it. This document explains the idea, surveys what already exists, describes how the sketch in this repository is built and lists what is still missing.

## 1. The problem

Unix pipes move bytes. Every program prints text for humans and the next program has to parse that text again. `ps aux | awk '$3 > 50 {print $2}' | xargs kill` works, but it depends on column positions, the locale, the terminal width and the exact version of `ps`. Every pipeline contains a small parser that breaks silently.

PowerShell showed in 2006 that a shell can pass objects instead. `Get-Process | Where-Object CPU -gt 50 | Stop-Process` needs no parsing, because the second command sees typed properties and the third one knows what a process is. PowerShell has run on Linux since 2016 and is MIT licensed, yet it never caught on there. There are three reasons.

1. Its objects are live .NET objects. They do not survive a process boundary, so every native Linux program stays a text island.
2. It brings the .NET runtime, a noticeable startup time and a Verb-Noun vocabulary that feels foreign next to coreutils.
3. Nothing in the Linux ecosystem produces its objects. `ip`, `systemctl` and `git` all print text.

Language models make the case for structured shells stronger. A model is good at exactly the two steps that classic pipelines are bad at. It can turn unstructured text into records, and it can turn a wish into a pipeline. It can only do both reliably when the shell knows the types of its data and the signatures of its commands.

## 2. What already exists

There are several open source shells that pass structured data instead of text. None of them treats a language model as a typed pipeline stage.

### Structured shells

| Project | Language | Data model | What meersh takes from it |
| --- | --- | --- | --- |
| [PowerShell 7](https://github.com/PowerShell/PowerShell) | C# | Live .NET objects with properties and methods | Objects with methods, verbs that accept typed input (`Get-Process \| Stop-Process`), formatting only at the end |
| [Nushell](https://www.nushell.sh) | Rust | Plain structured values and tables, typed command signatures, plugins over a msgpack or JSON protocol | Most of the architecture. Values, lazy streams, the split of engine state and stack, signatures, the [reedline](https://github.com/nushell/reedline) line editor, and wrapping uutils crates for `cp` and `mv` |
| [Elvish](https://elv.sh) | Go | Every pipe carries a byte channel and a value channel side by side | Text and values can travel together |
| [Murex](https://murex.rocks) | Go | Byte streams tagged with a data type such as json or csv | Content types on byte streams |
| [Oils (OSH and YSH)](https://oils.pub) | C++ generated from Python | Bash compatible OSH plus the new YSH language, J8 notation for structured data on pipes | An upgrade path from bash and line oriented JSON conventions |
| [Crush](https://github.com/liljencrantz/crush) | Rust | Streams of typed tables, by Axel Liljencrantz, the original author of fish | Typed table streams in Rust |
| [Marcel](https://github.com/geophile/marcel) | Python | Python values in pipes, the successor of osh | Pipelines as values that can be stored and reused |
| [xonsh](https://xon.sh) | Python | Python and shell syntax in one language | A real expression language next to commands |
| [fish 4](https://fishshell.com) | Rust since 4.0 | Text | The interactive standard of autosuggestions, highlighting and sane defaults |

### Rust rewrites of the standard tools

| Project | What it is | Role for meersh |
| --- | --- | --- |
| [uutils coreutils](https://github.com/uutils/coreutils) | GNU compatible coreutils in Rust, one crate per tool (`uu_cp`, `uu_mv`, ...). Ubuntu 26.10 uses it for all coreutils including `cp`, `mv` and `rm` | The mechanism behind the meersh versions of these tools, the way [Nushell already wraps `uu_cp` and `uu_mv`](https://www.nushell.sh/blog/2023-09-05-why-uu.html) |
| [jc](https://github.com/kellyjonbrazil/jc) | Parsers that convert the output of many legacy tools to JSON | A model for adapters for programs that will never speak objects |

### LLM tools for the terminal

| Project | What it does | Gap |
| --- | --- | --- |
| [llm](https://llm.datasette.io) by Simon Willison | Pipes text into many models, supports schemas, Ollama through a plugin | A text filter outside the shell. It does not know the type of its input |
| [aichat](https://github.com/sigoden/aichat) | Rust CLI for many providers with a shell assistant mode | Generates bash commands as text |
| shell-gpt, [Butterfish](https://github.com/bakks/butterfish), nl-sh | Natural language to shell commands. Butterfish wraps the running shell | Command generation only |
| [ai.nu](https://github.com/Fau57/ai.nu) | Nushell commands around Ollama | Closest in spirit. Scripts on top of Nushell, not typed operators |
| [LOTUS](https://arxiv.org/abs/2407.11418) | Semantic operators (`sem_map`, `sem_filter`, `sem_topk`, `sem_join`) over dataframes with batching and model cascades | A Python library, not a shell. Its operators are the model for `llm map` and `llm where` |

Every LLM tool in this list treats the model either as a text filter or as a command generator. None of them makes it a stage that reads typed records and has to answer with typed records.

### Why not simply use Nushell?

Nushell already covers most of the object part. It is mature, fast and MIT licensed. Any honest plan has to start from it. meersh explores four ideas that Nushell does not have.

1. **Nominal record types.** A record from `ls` is tagged `fs.entry`, one from `ps` is tagged `proc`. The tag allows methods (`$p.kill()`), type specific renderers and commands that accept only certain types. Nushell records are anonymous.
2. **Effects in every signature.** Each command declares whether it is pure, reads the system, sends data to a model, writes or destroys. The shell can then decide whether a pipeline that a model wrote may run without asking.
3. **LLM stages as typed operators.** `llm map`, `llm where` and `llm parse` take a meersh type, turn it into a JSON Schema for constrained decoding and turn the answer back into typed values. They batch rows and cache answers.
4. **An object protocol for external programs.** A Rust rewrite of a tool can serve bash users with text and meersh users with typed objects from the same binary.

There are three ways to get there.

- Write the LLM commands as a Nushell plugin. This is the fastest path and covers idea 3 and part of idea 4.
- Fork Nushell and change its value model.
- Build a new shell that reuses Nushell crates where it can.

The sketch takes the third way because ideas 1 and 2 change the value model and the command contract, which are the core of any shell. Code from Nushell and uutils is MIT licensed and may be reused in this AGPL project. Whether meersh should stay separate is the first open question at the end of this document.

## 3. Principles

- **Data until the terminal.** Values stay values through the whole pipeline. Rendering to text happens once, at the end.
- **Objects are data plus a type tag.** They can always be serialized, so they can go to a model, over a pipe or into a file without losing meaning.
- **Every command can be inspected.** Signatures carry input and output types, flags, effects and examples. `help` returns them as a table. The same data drives completion, error messages and the prompt for the model.
- **The model is a stage, not a chat window.** It gets typed input and must return typed output.
- **Local first.** The default backend runs on the machine. Data leaves the machine only when the user picks a cloud backend.
- **The model never acts on its own.** A pipeline written by the model is shown with its effects. Anything that writes, deletes or runs an external program needs an explicit yes.
- **Bash habits still work.** `ls -la src`, `cd ..`, `^git log --oneline | first 5` and globs behave as expected.

## 4. Architecture

```text
                 ┌──────────────── meersh (binary) ─────────────────┐
   keyboard ───▶ │ reedline REPL, `?` request, confirm, render       │
                 └───────────────────────┬──────────────────────────┘
                                         │ source text
                              meersh-parser (lexer, AST, expressions)
                                         │ Script
                              meersh-engine (bind, eval, streams, externals, render)
            ┌──────────────────┬─────────┴─────────┬─────────────────────┐
       meersh-cmds      meersh-coreutils       meersh-llm          external programs
   where select each    ls rm cp mv ps kill   llm  llm map  ai     ^git ^jq meer-box
            └──────────────────┴──── meersh-value (Value, Record, Type, protocol) ───┘
```

| Crate | Responsibility |
| --- | --- |
| [meersh-value](crates/meersh-value) | `Value`, `Record` with type tag, `Type`, JSON conversion, literal parsing, the object protocol. Small on purpose, so third party tools can depend on it |
| [meersh-parser](crates/meersh-parser) | Lexer, AST and parser. Pratt parsing for row conditions |
| [meersh-engine](crates/meersh-engine) | `Command` trait, `Signature`, `Effect`, argument binding, evaluation, `PipelineData`, external processes, table rendering |
| [meersh-cmds](crates/meersh-cmds) | Data commands that do not care where data comes from, such as `where`, `select`, `sort-by`, `group-by`, `each`, `to json` and `help` |
| [meersh-coreutils](crates/meersh-coreutils) | The standard tools rewritten to emit objects, plus the `meer-box` multi call binary |
| [meersh-sysutils](crates/meersh-sysutils) | More tools with typed output, such as `cat`, `head`, `tail`, `wc`, `du`, `df`, `find`, `stat`, `chmod`, `ln`, `free` and `uptime` |
| [meersh-adapters](crates/meersh-adapters) | Adapters that turn the output of `ip`, `ss`, `systemctl`, `journalctl`, `git`, `lsblk`, `findmnt`, `docker`, `podman`, `who`, `free`, `uptime` and `df` into typed records |
| [meersh-llm](crates/meersh-llm) | Backends, prompt encoding, cache and the LLM commands, including the actor mode |
| [meersh-plugin](crates/meersh-plugin) | The plugin protocol, the host side and a helper for plugin authors. [meersh-plugin-example](crates/meersh-plugin-example) is a small plugin |
| [meersh-mcp](crates/meersh-mcp) | The MCP server that offers meersh to coding agents under an agent principal |
| [meersh](crates/meersh) | The binary with REPL, completion, `-c` mode, script files, the `?` flow, approvals, `meersh mcp` and the `__exec` helper for sandboxes and Landlock |

The authority layer described in [AUTHORITY.md](AUTHORITY.md) lives in `meersh-engine/src/authority`.

The engine is immutable and shared behind an `Arc` once the shell runs. Variables live in a small `Stack` that is cloned into lazy streams. This is the split Nushell uses between `EngineState` and `Stack`. It lets a stage like `where` evaluate its condition on whatever thread pulls the stream, for example the thread that feeds an external program.

## 5. Values and types

```rust
pub enum Value {
    Nothing, Bool(bool), Int(i64), Float(f64),
    Filesize(i64),            // bytes, renders as 4.2 KiB, compares with 10mb
    Duration(i64),            // nanoseconds
    Date(DateTime<FixedOffset>),
    String(String), Binary(Vec<u8>),
    List(Vec<Value>), Record(Record),
}

pub struct Record {
    pub type_name: Option<Arc<str>>,   // "fs.entry", "proc", ...
    pub fields: IndexMap<String, Value>,
}
```

A table is a list of records. Filesizes, durations and dates are separate types so that `where size > 10mb` and `where modified > ((date now) - 1day)` work without conversions.

The type tag is what turns a bag of fields into an object. It is deliberately nominal and light. It is not a class with a vtable. Behaviour hangs off the tag in the engine.

- **Methods.** `$p.kill()` is sugar for `$p | kill`. The engine first looks for a command named `proc.kill` and then for `kill`. So a type can override a generic verb.
- **Typed verbs.** `rm` reads the `name` field of `fs.entry` records and `kill` reads the `pid` of `proc` records. That is why `ls *.tmp | rm` and `ps | where name == sleep | kill` need no `xargs`.
- **Renderers.** The table renderer colours directories in `fs.entry` tables. A full version would let every type register its own renderer.

`Type` describes values structurally (`table<fs.entry>{name: string, size: filesize, ...}`). It shows up in signatures, in `describe`, and in `llm --schema`, where a type written as a meersh value (`[{unit: string, count: int}]`) becomes a JSON Schema.

Between stages flows `PipelineData`.

```rust
pub enum PipelineData {
    Empty,
    Value(Value),
    Stream(Box<dyn Iterator<Item = Result<Value>> + Send>),
    Bytes(ByteStream),   // stdout of an external program, still raw
}
```

Streams are lazy and pull based. `^yes | first 3` ends, because `first` stops pulling and the pipe closes. Items are `Result`s, so a failure in the middle of a stream, such as a failed LLM batch, reaches the user instead of ending the stream silently. Bytes stay bytes until a builtin consumes them, and two external programs in a row are joined by a plain OS pipe.

## 6. The language

```text
ls | where size > 10mb and modified > ((date now) - 7day) | sort-by size -r
ps | where cpu > 50 | kill                       # objects flow into kill
ls ~/Downloads | where name =~ '\.iso$' | rm --dry-run
let big = (ls | where size > 1mb)
$big | each { {file: $it.name, kb: ($it.size / 1kb)} }
ps | where name == sleep | first | $in.kill()    # a method call through the type tag
^git log --oneline | first 20 | llm "write release notes"
help | where effect == destructive               # help is a table too
ls o> listing.json; ^make o+e> build.log         # redirections
^python3 -m http.server &                        # a background job, see jobs and fg
def big {min: filesize} { ls | where size > $min }
alias ll "ls -l"
with {fs.write: ./out, net: none} { ^make }      # less authority for a block
sandbox { ^npm install }                         # try it in an overlay first
```

The rules are few.

- Stages are separated by `|`, statements by `;` or newlines.
- Arguments of builtins are expressions. In a row condition a bare word names a column when the row has one. Otherwise it is a literal (`10mb`, `3day`, `42`, `true`) or a string. Quoted strings are always strings.
- Operators are words and need spaces (`==`, `!=`, `<`, `>=`, `=~`, `in`, `and`, `or`, `not`, `+`, `-`, `*`, `/`).
- `{ ... }` is a block with the current row in `$it`. `{key: value}` is a record. `[a b c]` is a list.
- `$in` is the input of an expression stage, `$_` the result of the previous command line, `$env` the environment as a record.
- Names that are not builtins are external programs. `^name` forces an external. Arguments of externals are parsed as plain words, so `^grep -E "a|b" file` behaves as in bash, including globs and `~`.
- `o>`, `e>`, `o+e>` and their `>>` forms redirect output into files. The file is created or overwritten through the authority layer like any other write. A trailing `&` starts an external program as a job.
- `def` defines commands with typed parameters, `alias` defines aliases that may contain whole pipelines, `env set` changes the environment of the programs meersh starts. `~/.config/meersh/init.msh` runs at startup.
- Statements are parsed one at a time, so an alias or definition applies from the next statement on, also inside scripts.

The parser asks the engine which names are builtins. That is how it knows that `llm map` is one command and `git` is a program. Literal arguments are checked against the declared parameter types before a command runs, and errors point at the source with a caret. Tab completion offers commands, flags, paths and, after `where`, `select`, `sort-by` and similar commands, the columns of the data that flows in. The columns come from the output type of the upstream command and the registered record types. A full type checker that follows every value through the pipeline is still open.

Scripts for POSIX shells are not reimplemented. `meersh --posix`, a `.sh` or `.bash` name, or a shell shebang hand the script to bash, the way Oils keeps OSH for compatibility.

## 7. Commands, signatures and effects

```rust
pub trait Command: Send + Sync {
    fn signature(&self) -> Signature;
    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData>;
}

Signature::build("sort-by")
    .desc("Sort rows by a column, or plain values by themselves.")
    .input(Type::any_table()).output(Type::any_table())
    .optional("column", Type::String, "column to sort by")
    .switch("reverse", Some('r'), "largest first")
    .example("ls | sort-by size -r | first 5", "the five largest files")
```

The engine binds arguments against the signature before the command runs, so an unknown flag is an error that points at `help`. Arguments stay unevaluated expressions in the call, because `where` and `each` evaluate them once per row.

| Effect | Meaning | Examples |
| --- | --- | --- |
| pure | transforms its input only | `where`, `sort-by`, `to json` |
| read | reads the system | `ls`, `ps`, `open` |
| llm | sends data to a model | `llm`, `llm map`, `ai` |
| write | creates or changes things | `cp`, `mv`, `mkdir`, `save` |
| destructive | deletes or kills | `rm`, `kill` |
| unknown | an external program | `^anything` |

The effect of a pipeline is the strongest effect of any command in it, nested blocks included.

## 8. External programs and the object protocol

Most programs will never know about meersh. The design therefore has three levels.

1. **Text.** Output of an external program arrives as lines of text. Input to an external program is written as text, where strings become lines and structured values become JSON lines. `ls | ^jq -r .name` works out of the box.
2. **Adapters.** A registry of parsers for well known programs in the spirit of jc. `^ip addr | where state == UP`, `^git log | first 5` and `^systemctl list-units | where active == failed` come back as typed records. An adapter applies only when a builtin reads the output, so `^ip addr` in the terminal and `^ip addr | ^grep UP` stay text. The model based fallback for everything else is `llm parse`.
3. **meersh aware programs.** When meersh reads the stdout of an external program it sets `MEERSH_OBJECTS=json-seq` in the program's environment. A program that knows the protocol answers with an [RFC 7464](https://www.rfc-editor.org/rfc/rfc7464) JSON text sequence, in which every value starts with the ASCII record separator `0x1E`. meersh sees that first byte and decodes objects instead of lines. Plain JSON cannot tell an int from a filesize or a string from a date, so each record type is declared once before its first use.

```text
<RS>{"@decl":"fs.entry","fields":{"size":"filesize","modified":"date"}}
<RS>{"@type":"fs.entry","name":"Cargo.toml","type":"file","size":812,"modified":"2026-10-29T19:01:00+01:00"}
```

The stream stays readable for `jq --seq` and for humans. Programs that do not know the variable are not affected by it.

`meer-box` demonstrates level 3. It is a multi call binary in the style of busybox or the uutils `coreutils` binary. Started from bash it prints a rendered table. Started from meersh, `^meer-box ls | where size > 1kb` receives real `fs.entry` objects across the process boundary. One Rust implementation serves both worlds.

Protocol v2 moves the objects to their own file descriptor, as Elvish does with its value channel. meersh passes descriptor 3 and sets `MEERSH_OBJECT_FD=3`. An aware program writes its objects there and keeps stdout for humans. It has to write to descriptor 3 before it writes to stdout, and whichever channel has data first decides. The human text of an object program goes to stderr, so it stays visible. `meer-box` uses v2 when it is offered and v1 otherwise. A binary encoding such as CBOR is an option once the format settles.

## 9. Rewriting the standard tools

The standard tools are split into two layers.

- **Interface (meersh-coreutils).** Argument parsing, typed output records, effects, help and error messages.
- **Mechanism (uutils).** The hard part of `cp`, `mv` or `rm` is decades of GNU edge cases (sparse files, extended attributes, cross device moves, permissions). uutils has already rebuilt this for GNU compatibility and ships one crate per tool. Nushell wraps `uu_cp` and `uu_mv` exactly this way, and so does meersh. `cp` and `mv` call `uu_cp` and `uu_mv` 0.12 as libraries, the other tools still use small std based implementations. uutils is not made for library use yet. `uu_cp` prints the reason for a skipped file to stderr and only returns that something was left out, so meersh compares source and target afterwards and names what is missing. The two crates also pull in about 50 crates that meersh does not need, such as clap and indicatif.

The destructive tools behave differently from their GNU ancestors.

- `rm` moves to the freedesktop trash by default. `--permanent` really deletes.
- `rm` and friends return records describing what they did (`{path, action, size}`). `--dry-run` returns the same records without touching anything. This makes them safe to put behind a model.
- `rm` refuses `/`, the home directory, `.` and `..`, and directories without `-r`.

| Command | Output type | Fields |
| --- | --- | --- |
| `ls [-a] [-l]` | `table<fs.entry>` | name, type, size, modified (with `-l` also mode, user, group, target) |
| `ps` | `table<proc>` | pid, ppid, name, user, cpu, mem, status, command |
| `kill [-s SIG]` | `table<proc.signal>` | pid, name, signal, ok, error |
| `rm`, `cp`, `mv`, `mkdir`, `touch`, `save` | `table<fs.change>` | path, action, and size or from where it applies |
| `open` | depends on the file | JSON and TOML become values, everything else text |
| `cd`, `pwd`, `which` | | |

`meersh-sysutils` adds `cat`, `head`, `tail`, `wc`, `du` (`fs.usage`), `df` (`fs.mount`), `find` (a lazy stream of `fs.entry`), `stat` (`fs.stat`), `chmod`, `ln`, `free` (`mem.info`), `uptime`, `whoami`, `hostname` and `uname`. The adapters cover `ip`, `ss`, `systemctl`, `journalctl` and `git`, see section 8. `spawn`, `jobs`, `fg` and `undo` complete the set.

## 10. LLM integration

### Backends

| Kind | How meersh talks to it | Typed output through |
| --- | --- | --- |
| `ollama` | HTTP `POST /api/chat` | `format` with a JSON Schema |
| `openai` | HTTP `POST /v1/chat/completions` of any compatible server (llama.cpp, vLLM, LM Studio or a hosted API) | `response_format` with `json_schema` |
| `claude-code` | `claude -p --output-format json --tools "" --system-prompt ... --json-schema ...` with the prompt on stdin | `structured_output` in the result |

The local Ollama backend is the default. Claude Code reuses the user's login and model choice. `--tools ""` and a replaced system prompt make it a plain model that cannot touch the machine. Backends are configured in `~/.config/meersh/config.toml` (see [config.example.toml](config.example.toml)), and `llm use claude` switches the default for a session. Every LLM command also takes `--backend` and `--model`.

### Commands

| Command | Input | Output | Semantics |
| --- | --- | --- | --- |
| `llm <instruction> [--schema T]` | anything | a string, or a value of type T | One request over the whole input |
| `llm map <instruction> --into col [--schema T]` | table | the same table plus `col` of type T | Like LOTUS `sem_map`. Rows are sent in batches with an index and merged back by index |
| `llm where <condition>` | table | the matching rows | Like `sem_filter`. One request per batch returns a verdict for every row |
| `llm parse <record type>` | text | `table<T>` | Extracts records from unstructured text, chunked by lines |
| `llm rank <criterion> [--top N]` | table | the table sorted by a `score` from 0 to 100 | Like `sem_topk`. Each row gets a reason and a score |
| `llm group <criterion> [--max N]` | table | the table plus a `group` column | Like `sem_group_by`. First a set of categories, then one category per row |
| `llm join <right> <condition>` | table | merged records of matching pairs | Like `sem_join`. Pairs are judged in batches, at most 2500 pairs |
| `llm embed`, `llm search <query>` | table | an `embedding` column, or rows sorted by `similarity` | Vectors from Ollama or an OpenAI compatible server |
| `llm edit <instruction>` | table | the edited table | Patches that are shown as a diff before anything continues |
| `llm "<action>"` on objects | typed records | the results of their methods | Actor mode, see [AUTHORITY.md](AUTHORITY.md) section 7 |
| `llm agent <task>` | | `{result, turns, cost_usd}` | Claude Code with meersh as its only tool, under the principal `agent:claude` |
| `ai <request>` and `? <request>` | | `{pipeline, explanation, effect}` | Writes a meersh pipeline from plain language |

`llm map`, `llm where`, `llm rank`, `llm group` and `llm join` take `--cascade <backend>`. The first model then also rates its confidence, and rows below 0.7 go to the stronger backend. Rows a model skipped are asked again on their own.

```text
^journalctl -b -p err | last 40 | llm "group the problems" --schema [{unit: string, problem: string, count: int}]
ls *.md | llm map "topic in three words" --into topic
ls ~/Downloads | llm where "looks like an invoice" | rm --dry-run
^dmesg | last 30 | llm parse {subsystem: string, message: string} | group-by subsystem
? the five biggest files in my home directory that I have not touched for a month
```

### From a type to a schema and back

`--schema [{unit: string, problem: string, count: int}]` is parsed as a meersh value, turned into a `Type` and then into a JSON Schema. The backend uses the schema for constrained decoding, so a small local model cannot answer with prose. The JSON answer is coerced back into values of the requested type. That step is lenient, because models like to answer `"42"` where `42` was asked for. Filesizes travel as byte counts, durations as seconds and dates as RFC 3339 strings. The schema tells the model so.

### Encoding the input

A table goes into the prompt as one header line and one JSON array per row.

```text
columns: ["i","name","type","size","modified"]
[0,"Cargo.toml","file","812 B","2026-10-29 19:01:00"]
[1,"crates","dir","4.0 KiB","2026-10-29 19:20:13"]
```

This costs far fewer tokens than repeating every key in every row. Filesizes, durations and dates appear the way a human reads them, which is also what the model needs to reason about them. When the input exceeds the budget whole rows are dropped and the model is told how many.

### Batching, laziness and caching

`llm map` and `llm where` send `batch_size` rows per request (25 by default) instead of one request per row. Batches are pulled lazily, so `ls | llm map ... | first 3` costs one request. Answers are cached on disk keyed by backend, model, prompt, input and schema, because people iterate on the end of a pipeline and rerun it many times.

### Pipelines from plain language

`? <request>` sends the model a short description of the syntax, the signature and examples of every command, the working directory and the type of `$_`. The answer has to follow a schema with `pipeline` and `explanation`. The shell parses the pipeline. If it does not parse, the error goes back to the model once. Then the shell shows the pipeline with its computed effect.

```text
meersh〉? the three biggest files here

  ls | sort-by size -r | first 3
  # The request asks for the three biggest files. We use ls to list files, sort-by size in reverse ...
  effect: read
run it? [Y/n/e]
```

Pure, read and llm pipelines default to yes. Anything that writes, deletes or calls an external program defaults to no. `e` puts the pipeline into the line editor for changes. The pipeline is added to the history either way.

### Safety and privacy

- **Local default.** Data goes to a cloud model only when the user picks that backend. `select` before an LLM stage limits what is sent.
- **No tools.** LLM stages are plain text in and data out. The Claude Code backend runs with all tools disabled.
- **Prompt injection.** File contents and logs can contain instructions. The stage prompt frames them as data, but that is no guarantee. The real protection is that model output is data. A stage cannot run commands, and a generated pipeline with any effect stronger than read needs an explicit yes.
- **Effects are declared, not proven.** An external program counts as unknown and is treated like a destructive one.
- **Authority.** [AUTHORITY.md](AUTHORITY.md) takes these ideas further. It covers principals, ownership of what a session created, plans, undo, typed methods and models that act on objects.

### Also in place

- Text answers of a final `llm` stage are streamed to the terminal as Ollama produces them.
- `?` sends the model only the commands that look relevant to the request, plus a fixed core set, in a stable order. The full catalogue took over a minute on a 4B model.
- Sending data to a cloud backend is the capability `llm.send`. Under a model principal it needs a policy rule.

## 11. State of the sketch

The sketch builds and runs. Every crate has unit or integration tests, and opt-in tests run the LLM stages against a real model.

| Area | State |
| --- | --- |
| Values, types, identity, JSON, object protocol v1 and v2 | Implemented and tested |
| Parser (pipelines, `let`, row conditions, blocks, records, lists, members, methods, multi word commands, aliases, redirections, `&`, source positions) | Implemented and tested |
| Engine (binding with literal checks, evaluation, lazy streams, externals with OS pipes, adapters, environment overlay, `def`, jobs, statement by statement execution) | Implemented and tested |
| Authority (principals, capabilities, plans, ledger, leases, policies, journal, undo, sandboxes, Landlock, tasks) | Implemented and tested, see [AUTHORITY.md](AUTHORITY.md) |
| Data commands, session commands and authority commands | Implemented and tested |
| Coreutils with methods and `meer-box`, sysutils, adapters for 13 programs | Implemented and tested |
| LLM (Ollama, OpenAI compatible, Claude Code, all commands in section 10, cache, cascades, streaming) | Implemented. Ollama with `qwen3:4b` and Claude Code with haiku were tested. Embeddings are not verified against a server, because the local Ollama has no embedding endpoint |
| Plugins and the MCP server | Implemented and tested |
| REPL with completion, approvals and sandbox review | Implemented. Driven through a pseudo terminal in tests by hand |
| Full type checking through the pipeline, Ctrl-Z and `bg` | Not started |

### What the end to end runs showed

The pipelines below ran on this machine with Ollama and `qwen3:4b` (thinking switched off) and with Claude Code on haiku.

| Pipeline | Result | Time |
| --- | --- | --- |
| `ls crates/meersh-llm/src \| llm map "purpose of this file from its name" --into purpose` | Sensible labels for all 6 files in one request | 22 s local |
| `ai the three biggest files here` | `ls \| sort-by size -r \| first 3`, effect read | 70 s local |
| `ps \| sort-by mem -r \| first 10 \| llm where "is part of a web browser"` | Only `firefox`. The Firefox content process "Isolated Web Co" is missed | 98 s local |
| the same `llm where` with `--backend claude --model haiku` | Correct | 3.5 s |
| `^meer-box ls crates \| describe` | `table<fs.entry>{name: string, type: string, size: filesize, modified: date}` across the process boundary | instant |

Five lessons came out of these runs.

- **Ask small models for one verdict per row.** The first version of `llm where` asked for the list of matching indices. `qwen3:4b` answered with every index, every time. Asking for `{i, matches}` per row fixed it. `llm map` with `--schema bool` had already worked, which pointed to the cause.
- **Latency is dominated by prompt size on a local model.** `?` sends the whole command catalogue and takes over a minute on a 4B model. A real shell should send only the commands that are relevant to the request (retrieved by name and description) and keep the catalogue prefix stable so the backend can cache it.
- **Reasoning models ignore `think: false`.** `qwen3:4b` on Ollama reasoned in plain text whatever the setting and whatever the `/no_think` switch said. A question took minutes and the answer was buried in the reasoning. Constrained decoding is the fix. meersh asks Ollama for text answers as `{"answer": ...}` and streams the answer field as it arrives. A translation then takes 5 seconds instead of minutes.
- **The assistant must know the shell.** Without piped data, `llm` first answered like a bash assistant. Now `?`, `ai` and `llm` without input share one assistant with a fixed primer (syntax, record types, worked examples) and the commands relevant to the request. Its pipelines are checked before they are shown, including that every program exists. The fixed part of the prompt comes first, so Ollama can reuse its cache, which halves the time of the next question. Because the assistant never sees data, `assistant = "claude"` sends only the question to a fast model.
- **Cascades are worth building.** The small local model is fine for labelling and extraction. Judgements like the browser question are where a larger model pays off, which supports the cascade idea in section 10.

Try it like this.

```sh
cargo build
./target/debug/meersh                                   # REPL
./target/debug/meersh -c 'ls crates | sort-by name | get name'
./target/debug/meersh -c 'ps | sort-by cpu -r | first 5 | select pid name cpu mem'
./target/debug/meer-box ls -l                           # the same ls, from bash
cargo test                                              # offline tests
cargo test -p meersh-llm -- --ignored                   # needs Ollama, last test needs claude
```

## 12. Roadmap

1. **Sketch.** Done. The value model, the object protocol and typed LLM stages work.
2. **A usable shell.** Done in large parts. Literal argument checks with error carets, completion of column names from types, redirections, background jobs with `jobs` and `fg`, environment changes, aliases, `def` with typed parameters, `init.msh` and streaming tables all exist. Open are full type checking through the pipeline, Ctrl-Z and `bg`, and handing the terminal to a foreground job.
3. **The ecosystem.** Done in large parts. Adapters for 13 programs, the object channel on descriptor 3, a plugin protocol with an example plugin, `llm agent`, cascades and embeddings exist. The plugin protocol is meersh's own and not compatible with Nushell's. Many more adapters are needed.
4. **Distribution.** Started. There is a Makefile, an RPM spec and `cargo deb` metadata. POSIX scripts are handed to bash instead of being reimplemented. Packages are not published.
5. **Authority.** Done, see [AUTHORITY.md](AUTHORITY.md) section 11.

## 13. Open questions

1. **Separate shell or Nushell?** The cheapest valuable step is a Nushell plugin with `llm`, `llm map`, `llm where`, `llm parse` and `?`. A separate shell is only worth it if typed records and effects turn out to matter in practice.
2. **Syntax.** Stay close to Nushell, so users and models can transfer knowledge, or closer to POSIX?
3. **Protocol encoding.** JSON text sequences are easy to debug. CBOR or MessagePack would be faster and lossless. Should the object channel move to a separate file descriptor right away?
4. **Default model.** Which local model should be the recommended default, given that `?` needs a model that follows a long command catalogue reliably?
