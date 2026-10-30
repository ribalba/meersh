# Claude Code with meersh as its only shell

These files make Claude Code run every command and every file change through meersh. Each
agent then works as its own principal (`agent:claude-<pid>`), may change or delete only what
it created, gets your approval for everything else, and leaves a journal you can undo.

| File | What it is |
| --- | --- |
| [CLAUDE-meersh.md](CLAUDE-meersh.md) | The section to paste into `~/.claude/CLAUDE.md` |
| [CLAUDE-meersh-fallback.md](CLAUDE-meersh-fallback.md) | The same section for a setup that keeps Bash as a fallback |
| [settings.json](settings.json) | The permissions to merge into `~/.claude/settings.json` |
| [init.msh](init.msh) | The meersh policy for agents, for `~/.config/meersh/init.msh` |

## Setup

1. Build and install meersh.

   ```sh
   cd ~/code/meersh
   make install PREFIX=~/.local
   ```

2. Give meersh the agent policy. Read it first. It decides which programs agents may run
   without asking.

   ```sh
   mkdir -p ~/.config/meersh
   cp integrations/claude/init.msh ~/.config/meersh/init.msh
   ```

3. Register the MCP server for all your projects. `--unique` makes every Claude Code instance
   its own principal, so parallel agents keep apart through ownership and leases. Leave it out
   if one agent should keep ownership of its files across restarts.

   ```sh
   claude mcp add --scope user meersh -- ~/.local/bin/meersh mcp --agent claude --unique
   ```

4. Block the built in tools that would bypass meersh. Merge [settings.json](settings.json)
   into `~/.claude/settings.json`. `deny` removes Bash, Edit, Write and NotebookEdit. `allow`
   lets the meersh tools run without a Claude Code prompt, because meersh asks itself.

5. Paste [CLAUDE-meersh.md](CLAUDE-meersh.md) into `~/.claude/CLAUDE.md`.

6. Check it. `claude mcp list` should show meersh as connected. In a new session, ask the
   agent to list the files of the project. It should call `run` with `ls`.

To keep Bash, Edit and Write as a fallback, merge only the `allow` part in step 4 and paste
[CLAUDE-meersh-fallback.md](CLAUDE-meersh-fallback.md) in step 5. The agent then uses meersh
and falls back to Bash only when meersh cannot do the job. [../setup.sh](../setup.sh) sets up
this variant together with Konsole, Ptyxis and VS Code.

## How approvals reach you

When a plan needs you, meersh asks you through Claude Code (MCP elicitation). A dialog shows
the pipeline and every action with its decision and reason, and you choose approve or reject.
Only an approval applies the plan. When you reject or dismiss the dialog, nothing changes and
the agent gets `applied: false` and `declined: true` back.

A client that cannot show such a dialog, for example `claude -p`, gets the plan and a hint
instead and tells you what it wanted. So does a plan whose blocked actions are all refused,
because you could not approve them either. You then run the pipeline yourself in meersh,
see the same plan and approve it, row by row if you like.

```text
meersh
> rm build/old.log
```

Approving rule by rule (`r` at the prompt) adds a narrow policy rule for this session.
Rules you want to keep belong in `~/.config/meersh/init.msh`.

## What to adjust

- **Programs your CLAUDE.md relies on.** Everything an agent runs that is not in the allow
  rules of `init.msh` asks you each time. Add the programs your instructions use, for example
  `psql` or your own scripts, with a rule like
  `policy allow agent:* exec $it.program in [psql]`.
- **Interpreters.** `python3`, `node`, `npm` and `make` run arbitrary code. Landlock keeps
  them from writing outside the project and the temp directory, but they can read your files
  and use the network. The template leaves them commented out.
- **Edits.** `policy allow agent:* fs.write in_project` lets agents change existing files in
  the project without asking. Every change keeps a snapshot, and `journal | where principal
  =~ agent | undo` takes changes back. Remove the rule if you want to approve every edit.

## What this does not cover

- Claude Code's Read, Glob and Grep tools still read files directly. They change nothing.
- Web access and other MCP servers are not affected.
- The policy binds what meersh runs. It is a guardrail for your own agents, not a sandbox
  against hostile code. AUTHORITY.md section 10 discusses the limits.
