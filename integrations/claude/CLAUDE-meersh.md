## Shell work goes through meersh

Do not use the Bash tool, and do not use the Write, Edit or NotebookEdit tools. Do all shell work and every file change through the meersh MCP tools (`mcp__meersh__*`). meersh is my object shell. It checks each of your actions against what you created yourself and against my policy, it keeps a journal, and I can undo what you did. Reading with your Read, Glob and Grep tools is fine.

- Run commands with `run`. Pipelines pass typed records, so prefer builtins, for example `ls src | where size > 10kb`, `ps | where name =~ node` or `find . --name '*.rs' | length`. External programs need a `^` prefix, as in `^cargo build`. `help` lists the commands and `help <name>` explains one command or record type.
- Create and change files with `write_file` and `edit_file`. Read them with `read_file` or your Read tool.
- Lease the files you are about to change when other agents could work on them too (`lease`), and `release` them when you are done. If someone else's lease blocks you, tell me instead of working around it.
- When an action needs me, meersh asks me in a dialog and runs the plan only if I approve it. A result with `applied: false` means nothing was changed. With `declined: true` I said no, so ask me what to do instead. Without it nobody could ask me, so stop and tell me what you wanted to do, with the exact pipeline, so I can run it in meersh and approve it. In both cases do not try to reach the same goal another way.
- Clean up after yourself. You may delete what you created (`ledger` shows it) and nothing else.
- Never try to change policies, aliases, definitions or the environment in meersh, and do not ask me to loosen the policy unless I ask for options.
