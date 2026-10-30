# Authority, objects and agents in meersh

This document extends [CONCEPT.md](CONCEPT.md). It takes three ideas from the sketch and thinks them further. Everything described here is implemented. Section 11 lists what exists, what is simplified and what is still open.

1. Commands declare what they do (effects), so the shell knows when an LLM wants to delete something.
2. Objects carry their type, so `$p.kill()` works and objects can be passed around.
3. A language model can be used in two ways, to write commands and to work on the objects themselves.

Together they lead to something that does not exist yet. It is a shell in which every action has an author, a target and a right to act, and in which a language model can act on objects without being able to touch anything it was not handed.

## 1. Why effects per command are not enough

The sketch marks `rm` as destructive. That is true and useless at the same time.

- `rm out/demo.o`, where the file was created by the same session a minute ago, is harmless.
- `rm ~/thesis.tex` is a disaster, although it is the same command with the same effect.
- `ls | llm where "looks like junk" | rm` was typed by the user, but the model picked the targets.

The danger is not in the command. It lies in three other things.

- **The target.** Which object is affected, and can the change be undone?
- **The author.** Who decided that this should happen? The user, a script, a model that wrote the pipeline, or a model that picked the rows?
- **The relationship between the two.** Did the author create the target? Did someone else change it since?

Unix knows none of this. It knows one user id, and every process of that user may delete every file of that user. Claude Code and other agents add permission rules that match command strings such as `Bash(git *)`. A string pattern cannot express "delete what you created" or "stop the server you started".

## 2. The model

Every action that changes the world goes through one call right before it happens.

```text
authorize(principal, capability, target, chosen_by) -> allow | ask(reason) | deny(reason)
```

### Principals

A principal is whoever is responsible for a pipeline. Every pipeline runs under exactly one.

| Principal | Who it is | Typical default |
| --- | --- | --- |
| `user` | A line the user typed | Everything Unix allows, with a few protected paths |
| `script` | A script file | Like the user. Declared needs for scripts, as in SHILL, are open |
| `plan` | A pipeline that a model wrote through `?` | Read freely, write and delete only after a plan is approved |
| `actor` | A model acting on piped objects, as in `ls \| llm "delete ..."` | Only the objects it was handed, only reversible actions without asking |
| `agent:<name>` | An external agent such as Claude Code using meersh as its tool | Only what it created itself, within the project |

### Capabilities

A capability is a right to do one kind of thing to one concrete target. The effects in today's signatures stay. They describe what a command may ask for, which is what plans and explanations need. The check itself happens at run time with the real target.

| Capability | Target | Reversible? |
| --- | --- | --- |
| `fs.read` | a path | not applicable |
| `fs.create` | a new path | yes, by deleting it |
| `fs.write` | a file object | only if a snapshot or git has the old content |
| `fs.delete` | a file object | yes when it goes to the trash or git has it, no when it is unlinked |
| `fs.move` | a file object and a new path | yes |
| `proc.spawn` | a program | not applicable |
| `proc.signal` | a process object and a signal | no |
| `net.connect` | host and port | no |
| `llm.send` | a backend, local or not | no, data that has left cannot be called back |

Reversibility is computed per target, not per command. Deleting a clean file that git tracks is reversible with `git checkout`. Deleting an untracked file with `--permanent` is not. A policy can therefore say "a model may do anything reversible inside the project" and mean it precisely.

### Where the check sits

```rust
pub enum Principal {
    User,
    Script { path: PathBuf },
    Plan { model: String, request: String },
    Actor { model: String, instruction: String },
    Agent { name: String },
}

pub enum Cap {
    FsRead(PathBuf),
    FsCreate(PathBuf),
    FsWrite(ObjectId),
    FsDelete { target: ObjectId, permanent: bool },
    FsMove { target: ObjectId, to: PathBuf },
    ProcSpawn { program: String },
    ProcSignal { target: ObjectId, signal: Signal },
    NetConnect { host: String, port: u16 },
    LlmSend { backend: String, local: bool, columns: Vec<String> },
}

pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

impl Ctx<'_> {
    /// Every command calls this right before it acts, with the concrete target.
    /// In plan mode it records the request and returns a placeholder grant instead.
    pub fn authorize(&self, cap: Cap) -> Result<Grant>;
}
```

`rm` changes from "delete these paths" to "for every target, ask for `FsDelete`, then delete". Commands that do not act need no change at all.

## 3. Ownership. A session may remove what it created

The shell keeps a ledger of everything a session creates. That turns "a session can remove files it has created but not /" into a precise rule.

### What is recorded

Every builtin that creates something writes an entry. That covers `touch`, `mkdir`, `save`, the targets of `cp` and `mv`, and every process started from meersh.

```text
╭───┬──────────────────────┬─────────┬──────────────┬───────┬─────────────────────┬────────────────╮
│ # │ object               │ kind    │ principal    │ task  │ created             │ fingerprint    │
├───┼──────────────────────┼─────────┼──────────────┼───────┼─────────────────────┼────────────────┤
│ 0 │ out/                 │ dir     │ user         │ build │ 2026-10-30 14:01:07 │ dev 41 ino 912 │
│ 1 │ out/demo             │ file    │ user         │ build │ 2026-10-30 14:01:09 │ 18.2 KiB 9f3a… │
│ 2 │ pid 41231 node       │ process │ agent:claude │ dev   │ 2026-10-30 14:03:30 │ start 8812345  │
╰───┴──────────────────────┴─────────┴──────────────┴───────┴─────────────────────┴────────────────╯
```

Identity is not the path. A file is identified by device, inode and a fingerprint of size, modification time and optionally a hash. A process is identified by pid and start time, so a reused pid is never mistaken for the old process.

### The rule, and why ownership lapses

> A principal may delete or change an object that it created, as long as nobody else has changed it since.

The second half matters. When the agent creates `notes.md` and the user then writes three pages into it, the file is no longer the agent's to delete. The ledger notices because the fingerprint changed through a write that did not come from the agent. Ownership lapses and the default falls back to asking.

### Scopes of ownership

| Scope | Lifetime | Use |
| --- | --- | --- |
| command line | one line | `touch x; rm x` in one line needs no confirmation |
| task | a named block, `task build { ... }` | everything an experiment produced can be listed, kept or cleaned up together |
| session | until the shell exits | the default for interactive work |
| persistent | the shared ledger in `~/.local/share/meersh/ledger.json` | the user and named agents keep ownership across sessions, so an agent may clean up its own build artefacts tomorrow |

### Processes

Every task runs in its own cgroup (v2). "Kill what you started" then includes the grandchildren, and `task stop dev` ends exactly the processes of that task. The rule for processes mirrors the one for files. An actor may stop processes of its own task. Stopping anything else needs consent.

### External programs

Builtins report exactly what they create. External programs do not. There are two options.

1. Under the `user` principal, externals run as they do today and their files are not owned by anyone. This is honest and costs nothing.
2. Under `plan`, `actor` and `agent`, externals run in a private mount namespace with an overlay over the project directory, the way the [`try`](https://github.com/binpash/try) tool does it. The overlay works for unprivileged users since Linux 5.11. When the program ends, the shell knows every file it created or changed. It shows the diff, and on commit the new files enter the ledger as owned by that principal.

Option 2 also turns any program into something with a dry run. `sandbox { ^npm install }` shows what `npm install` would change without changing anything.

=> do 2

## 4. Policies are data

Policies are records in the shell language, so they can be listed, filtered and explained like everything else.

```text
policy allow actor fs.delete owned                          # what it created and nobody changed
policy allow actor fs.delete reversible and in_project      # trash inside the project
policy ask   plan  fs.*
policy deny  *     fs.* $it.path =~ '/secrets/'
policy deny  actor llm.send not local                       # no data to cloud models
policy budget actor 10

policy | where verdict == deny
policy explain fs.delete thesis.tex --principal actor
```

A condition is a row condition like in `where`. The row is the request, with the fields `owned`, `changed`, `reversible`, `in_project`, `protected`, `chosen_by_llm`, `leased_by`, `size`, `path`, `to`, `pid`, `program`, `local` and `task`. Rules are checked in order and the first match decides. Protected paths come before every rule. `policy explain` answers why an action would be allowed, asked or denied, with the rule or default that decided. Only the user may change policies. Every `policy` command refuses model principals, also when a model reaches it through `source`.

### Consent becomes a narrow rule

Asking the same question ten times trains people to say yes without reading. A plan can be approved as a whole, row by row, or together with the narrowest rule that would have allowed it.

```text
approve? [y]es all / rows such as 0 2-3 / [r]ule [rows] / [N]o> 1 3
approve? [y]es all / rows such as 0 2-3 / [r]ule [rows] / [N]o> r 1
  new rule: policy allow actor fs.delete target in ["dump.sql"]
```

Row numbers approve only those actions. Actions the policy allows are always part of the plan and refused ones can never be picked. The skipped ones show up as `refused, not approved`. `r` alone makes rules for every action that asked, `r 1 3` only for those rows. Inside a task the rule gets `--task <name>` and only applies to that task. Protected paths get no rule, because they are checked before every rule. Without a terminal, `meersh --approve-only 1,3 -c '...'` does the same as answering `1 3`.

### Authority only narrows

A block can run with less authority than its caller, never with more.

```text
with {fs.write: ./out, net: none} { ^make }       # make may write ./out only and has no network
```

Inside meersh this is enforced by `authorize`. For external programs it maps onto [Landlock](https://docs.kernel.org/userspace-api/landlock.html), which lets an unprivileged process restrict its own file access and, since ABI version 4, its TCP ports. The children of that block inherit the restriction. So the policy is not only a guardrail for builtins. It binds every program a model starts.

## 5. Plans, reversibility and undo

### Plan first

Every command already returns records of what it did (`fs.change`, `proc.signal`). With `authorize` in place, any pipeline can run in plan mode. Each `authorize` call records the request instead of acting, and the pipeline returns the list of actions it would take.

```text
meersh〉plan { ls | where size > 5mb | rm }
╭───┬───────────┬────────┬─────────────────┬────────┬─────────┬────────────┬──────────┬─────────╮
│ # │ principal │ action │ target          │ detail │ size    │ reversible │ decision │ because │
├───┼───────────┼────────┼─────────────────┼────────┼─────────┼────────────┼──────────┼─────────┤
│ 0 │ user      │ trash  │ big-foreign.bin │        │ 5.7 MiB │ true       │ allow    │         │
│ 1 │ user      │ trash  │ big-mine.bin    │        │ 5.7 MiB │ true       │ allow    │         │
╰───┴───────────┴────────┴─────────────────┴────────┴─────────┴────────────┴──────────┴─────────╯
```

The shell plans by itself. Every statement that contains a builtin which writes, deletes or calls a model runs once in plan mode first. A plan with nothing to approve is applied right away. Anything else is shown and needs a yes. The applying run accepts only requests that were in the approved plan, and the key of a request contains the identity and fingerprint of its target. A file that changed between looking and acting is therefore skipped.

### Undo

Every applied action goes into a journal together with its inverse.

| Action | Inverse |
| --- | --- |
| create | delete, if still unchanged |
| trash | restore from the trash |
| move | move back |
| chmod | old mode |
| write | old content from a snapshot, from git or from the overlay |
| kill, permanent delete, network send | none. These are irreversible and say so in every plan |

```text
journal | where task == cleanup | undo
journal | where principal == actor and at > ((date now) - 1hr)
```

The leverage is that every new object type gets plans and undo for free, as soon as its methods declare their capability and their inverse.

## 6. Objects with methods

`$p.kill()` is the visible part of a larger idea. A type declares fields, an identity and methods, and every method declares the capability it needs and its inverse.

```text
help fs.entry
  fields   name: string, type: string, size: filesize, modified: date
  identity device, inode, fingerprint
  methods
    delete()          fs.delete   inverse restore
    trash()           fs.delete   inverse restore
    move(to: path)    fs.move     inverse move back
    chmod(mode)       fs.write    inverse chmod old
    open()            fs.read
    parent()          fs.read     returns fs.entry
    children()        fs.read     returns table<fs.entry>
    hash()            fs.read     returns string

help proc
  methods
    kill(signal = TERM)   proc.signal   irreversible
    children()            read          returns table<proc>
    files()               read          returns table<fs.entry>, the open files
    ports()               read          returns table<net.socket>
```

### Snapshots with identity, not live objects

PowerShell objects are live .NET objects. meersh objects are snapshots, plain data that can go to a model, to a file or to another program. What makes them safe to act on later is their identity. `kill` checks the start time before it sends a signal, `delete` checks inode and fingerprint before it deletes. An object that came back from a model, a file or yesterday's journal can only hit the thing it originally described.

### The model can point, but not name

Every object carries its identity (`file:<dev>:<inode>:<size>:<mtime>` or `proc:<pid>:<start>`). It travels through JSON and the object protocol as `@id` but is not shown in tables. When a model acts on objects it writes a pipeline over the piped objects, and the shell accepts only pipelines that filter those objects and call their methods. No external programs are allowed, no commands that read from elsewhere and no values made up in the pipeline. The model cannot name a path that was not piped in. A prompt injection can at worst pick another object of the same input, and that object still goes through ownership, policy and the plan.

### Objects form a graph

Methods that return objects make the system navigable without text parsing.

```text
ps | where name == node | first | $in.ports()                 # what is this node listening on?
ls *.log | where size > 1gib | each { $it.parent() } | uniq   # where do the big logs live?
ps | where cpu > 80 | each { $it.files() } | where name =~ '\.db$'
```

### Methods are tools

A method with a typed signature is already a tool definition. The actor gets the methods of the piped type as its catalogue, and `meersh mcp` turns every command that reads or computes into a typed MCP tool with a JSON Schema generated from its signature. Its `help` tool shows types with their fields and methods. No one has to write tool definitions by hand. A new type brings its tools along.

## 7. Three ways to use a language model

The two uses named so far grow into three, and they map onto the [CaMeL](https://arxiv.org/abs/2503.18813) architecture from Google DeepMind. CaMeL separates a privileged model that writes the program but never sees untrusted data from a quarantined model that reads the data but can call no tools.

| Mode | Command | Sees | Returns | Authority | CaMeL role |
| --- | --- | --- | --- | --- | --- |
| Planner | `? request` | the request, command signatures, the *types* of the data | a pipeline | none until the plan is approved | privileged LLM |
| Transformer | `llm`, `llm map`, `llm where`, `llm parse` | the data | data | none at all | quarantined LLM |
| Actor | `ls \| llm "delete ..."` | the input type and its methods, never the rows | a pipeline over the input | the `actor` principal, bounded by policy | planner plus quarantined calls |

### The user's example, step by step

```text
meersh〉ls | llm "delete all files that are larger than 5mb"
```

1. **Recognise an action.** The input has methods (`fs.entry`) and the instruction asks for one of them. `llm` switches to actor mode. `--act` and `--no-act` force the choice.
2. **Compile what can be compiled.** "Larger than 5mb" can be written over the fields. So the model does not judge files one by one. It writes code over the schema, and it sees only the schema.

   ```text
   where size > 5mb | each { $it.delete() }
   ```

   This is deterministic, it costs one small request whether there are 10 files or 100 000, and no file name ever reaches the model. A criterion that cannot be written over the fields ("the ones that look like duplicates") becomes a quarantined `llm where` inside that pipeline. The model then judges rows but still cannot act.
3. **Plan.** The pipeline runs in plan mode under the `actor` principal. Every `delete` asks `authorize`.
4. **Show and ask.**

   This is a real run with `qwen3:4b` on Ollama. The model wrote the pipeline from the type alone.

   ```text
     where size > 5mb and type == file | each { $it.delete() }
     # actor wrote this. The instruction asks to delete all files larger than 5mb. ...

   ╭───┬───────────┬────────┬─────────────────┬─────────┬────────────┬──────────┬───────────────────────────────╮
   │ # │ principal │ action │ target          │ size    │ reversible │ decision │ because                       │
   ├───┼───────────┼────────┼─────────────────┼─────────┼────────────┼──────────┼───────────────────────────────┤
   │ 0 │ actor     │ trash  │ big-foreign.bin │ 5.7 MiB │ true       │ ask      │ not created by this session…  │
   │ 1 │ actor     │ trash  │ big-mine.bin    │ 5.7 MiB │ true       │ ask      │ not created by this session…  │
   │ 2 │ actor     │ trash  │ owned-big.bin   │ 6.7 MiB │ true       │ ask      │ created by user, can be rest… │
   ╰───┴───────────┴────────┴─────────────────┴─────────┴────────────┴──────────┴───────────────────────────────╯
     3 actions, 18.1 MiB, all reversible. 3 need your approval, 0 refused.
   approve? [y]es / [r]ule / [N]o>
   ```

   `delete` is `trash` because `$it.delete()` moves to the trash unless `--permanent` is given, and an actor may not unlink what it did not create. A file created earlier in the same session would show `allow, created by this session`. `owned-big.bin` was created by the user in an earlier session, and only persistent principals (the user and named agents) keep ownership across sessions.
5. **Apply and journal.** The result is a table of `fs.change` records. `journal | last 1 | undo` brings the files back.

### Editing data

Piped objects can also be changed without any authority, because a change to data is not yet a change to the world.

```text
open contacts.json | llm edit "normalise all phone numbers to E.164" | save -f contacts.json
```

`llm edit` returns patches (row, field, old value, new value), not a new file. The shell shows them as a diff as part of the plan, and nothing continues before the user accepts. The model becomes a structured editor whose every change can be reviewed. Writing the file back is a separate `save`, with its own capability check.

### Taint. Remembering who chose the target

`ls | llm where "looks like junk" | rm` is typed by the user, but the model chose the rows. The shell knows that, because an LLM stage is upstream of `rm` in the same pipeline. The request reaches `authorize` with `chosen_by_llm` set, and the default asks the user even under the `user` principal. External programs downstream of an LLM stage need approval for the same reason.

This works per pipeline. Each value could later carry its own provenance, the way CaMeL tags every variable. Then `each { if ($it | llm "is junk") { $it.delete() } }` is caught as well. So is a model that read a web page and then proposes a path from it.

### Why this resists prompt injection

- The planner sees the request and the types, never the data. Text inside files cannot change the plan.
- The transformer sees the data but cannot act. At worst it returns wrong data, and that data stays typed.
- The actor can only point at objects it was given. Every action passes ownership, policy and the plan.
- Sending data to a cloud model is a capability (`llm.send`). Under a model principal it needs a rule.
- Irreversible actions always need a human, whatever the model says.

## 8. meersh as the execution layer for agents

Coding agents work through a bash tool today. Their output is text they have to parse, and their permissions are patterns over command strings. meersh can offer agents something better through MCP.

- **Typed tools.** Every command and every method becomes a tool with a JSON Schema. Results come back as records, which costs fewer tokens and removes parsing errors.
- **A principal per agent.** Claude Code gets `agent:claude`, a second instance gets `agent:claude-2`. Each has its own ledger. One agent cannot delete what another agent or the user created without asking. `journal | where principal != agent:claude` shows what others changed in the meantime.
- **Leases.** An agent can lease the files it is editing. Another agent that wants to write the same file sees the lease and its holder instead of silently overwriting the work. This is exactly the situation of several LLM instances working in one project.
- **Clean-up by construction.** At the end of a task the agent can remove everything it created and stop everything it started, and nothing else.
- **File tools.** `read_file`, `write_file` and `edit_file` replace the agent's own file tools. Every edit is an `fs.write` with a snapshot, so it can be undone.
- **Rules that mean something.** "May delete files it created", "may kill the dev server it started" and "no network except localhost" replace `Bash(rm *)` patterns that are either too broad or too narrow.

=> I love this

[integrations/claude](integrations/claude/README.md) has the CLAUDE.md section, the Claude Code settings and the policy that make an agent use meersh instead of bash. Programs a rule allows run directly under Landlock, confined to the project. Programs that need a human come back to the agent as a plan.

## 9. What is new

Each ingredient has a precedent.

| Precedent | What it did | What meersh adds |
| --- | --- | --- |
| [SHILL](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/moore) (OSDI 2014) | Capability safe shell scripts on FreeBSD. Scripts receive capabilities from their caller and declare contracts | Authority derived from ownership and provenance, and principals for code that a model wrote |
| Capsicum, Landlock | Processes give up their own authority | The shell language decides the restriction per block and per principal |
| [CaMeL](https://arxiv.org/abs/2503.18813) (2025) | Privileged and quarantined LLMs, capability tags on values, policies before tool calls | The program the planner writes is a shell pipeline, and the tools are typed methods of system objects |
| [try](https://github.com/binpash/try) | Runs one command in an overlay and shows its changes | Overlays as the default for model principals, and their results feed the ownership ledger |
| PowerShell | Objects with methods | Snapshots with identity, so objects can leave the process and still be acted on safely. Methods carry capabilities and inverses |

The combination is what no shell or agent framework has today.

1. **Ownership based authority.** "You may undo what you did" is a policy primitive.
2. **Principals in the shell.** The same command is judged differently depending on whether a human or a model wrote it, and on who chose the targets.
3. **Reversibility per target.** Trash, git, overlays and snapshots decide whether an action can be undone, and the policy uses that.
4. **Plans and undo for every type.** They come from capability and inverse declarations, not from code in each command.
5. **Bounded LLM actors.** The model points at handles, compiles criteria into code where it can, and every action passes the same checks as a human's.
6. **A safe substrate for agents.** Typed tools, per agent ledgers and leases for several agents in one project.

## 10. Hard problems

- **External programs without a sandbox** cannot be tracked precisely. fanotify and eBPF need privileges, inotify does not scale and LD_PRELOAD is easy to bypass. The overlay is the honest answer, and it costs a copy step on commit.
- **Consent fatigue.** Too many questions and users approve everything. Narrow rules from approvals, budgets and good defaults for owned and reversible actions are the countermeasures. Whether they suffice is an open question.
- **Small models as actors.** Compiling criteria into code helps a lot. Per row judgements by a 4B model are unreliable (the sketch measured this, see CONCEPT.md section 11). Cascades to a larger model are likely necessary.
- **Races.** Identity checks at apply time close most gaps, but not every one. A file can change between the check and the unlink. `openat` with directory file descriptors narrows the window further.
- **Not a security boundary against the user.** meersh protects users from their own tools and agents. Against hostile code it is one layer, together with Landlock, namespaces and seccomp.
- **How much may the planner see?** Types only is the safe choice. Column statistics or a few sample rows make plans better but open a path for injection. This should be a policy setting, not a hard rule.

## 11. What exists

| Part | Where | State |
| --- | --- | --- |
| Principals (user, script, plan, actor, agent) and capabilities with concrete targets | `meersh-engine/src/authority` | implemented |
| Plan first execution, approval, apply with identity keyed approvals | `Engine::run_planned`, the REPL and `-c --approve` | implemented |
| Ownership ledger shared by all meersh processes (flock), with tree ownership and lapsing | `authority/ledger.rs` | implemented |
| Leases between principals | `lease`, `release`, `leases` | implemented |
| Policy rules as data, budgets, `policy explain`, narrow rules from approvals | `meersh-cmds/src/authority.rs` | implemented |
| Journal with inverses and `undo` (trash restore, move back, snapshots, chmod) | `authority/journal.rs`, `undo` | implemented |
| Reversibility per target, including clean git tracked files | `authority/identity.rs` | implemented |
| Object identity and verification before acting | `Record::id`, `Ctx::verify` | implemented |
| Methods with capabilities (`fs.entry.delete`, `trash`, `move`, `copy`, `parent`, `children`, `open`, `proc.kill`, `parent`, `children`, `files`, `ports`), `help <type>` | `meersh-coreutils/src/methods.rs` | implemented |
| Actor mode with validated pipelines, `llm edit` with review, taint, `llm.send` checks | `meersh-llm/src/actor.rs` | implemented |
| Overlay sandboxes for externals (`sandbox { }`, automatic for model principals) | `authority/sandbox.rs`, `meersh __exec` | implemented |
| `with` blocks enforced by Landlock (write roots, no TCP) | `meersh/src/helper.rs` | implemented |
| Tasks with systemd scopes, `spawn`, `jobs`, `fg`, `task stop`, `cmd &` | `meersh-coreutils/src/jobs.rs`, `external.rs` | implemented |
| MCP server with an agent principal, plans for agents and typed tools | `meersh-mcp`, `meersh mcp --agent <name>` | implemented |
| `llm agent`, Claude Code with meersh as its only tool | `meersh-llm/src/agent.rs` | implemented |

What is simplified or still open.

- A statement typed by the user that mixes external programs with builtins that act runs the externals twice, once while planning and once while applying. Statements with only externals run once, as in any shell.
- `with {fs.write: ...}` always allows writes to /dev and the temp directory, because most programs need them.
- Value level provenance is not there. Taint is tracked per pipeline.
- Ownership across sessions is kept only for the user and named agents. Plans and actors own what their session created.
- External programs typed by the user are not tracked in the ledger. Only sandboxed ones are.
