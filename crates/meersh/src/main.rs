//! The meersh binary. It wires the command crates into one engine and runs the REPL, a
//! script file, a `-c` command line, the MCP server or the sandbox helper.

mod complete;
mod consent;
mod helper;
mod update;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use consent::Asker;
use meersh_engine::authority::Ledger;
use meersh_engine::render::{RenderOptions, render, render_chunk};
use meersh_engine::value::Value;
use meersh_engine::{Engine, PipelineData, Principal, Result, ShellError, Stack};
use meersh_llm::{LlmConfig, LlmRuntime};
use reedline::{
    ColumnarMenu, DefaultPrompt, DefaultPromptSegment, EditCommand, Emacs, FileBackedHistory, HistoryItem, KeyCode,
    KeyModifiers, MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};

fn config_dir() -> PathBuf {
    dirs::config_dir().map_or_else(|| PathBuf::from("."), |d| d.join("meersh"))
}

fn config_path() -> PathBuf {
    std::env::var_os("MEERSH_CONFIG").map(PathBuf::from).unwrap_or_else(|| config_dir().join("config.toml"))
}

fn build_engine() -> std::result::Result<Arc<Engine>, String> {
    let mut engine = Engine::with_ledger(Ledger::shared());
    engine.exec_helper = std::env::current_exe().ok();
    meersh_cmds::register(&mut engine);
    meersh_coreutils::register(&mut engine);
    meersh_sysutils::register(&mut engine);
    meersh_llm::register(&mut engine, LlmRuntime::new(LlmConfig::load(&config_path())?));
    meersh_plugin::register(&mut engine);
    meersh_adapters::register(&mut engine);
    let engine = Arc::new(engine);
    for warning in meersh_plugin::load_registered(&engine) {
        eprintln!("meersh: {warning}");
    }
    Ok(engine)
}

struct Options {
    command: Option<String>,
    script: Option<String>,
    approve: bool,
    /// `--approve-only 0,2`. Approves only these rows of a plan.
    approve_only: Option<Vec<usize>>,
    /// Run the command or script with bash, for POSIX scripts.
    posix: bool,
    /// Arguments after the script name, passed to POSIX scripts.
    rest: Vec<String>,
}

fn parse_args(args: &[String]) -> std::result::Result<Options, String> {
    let mut options =
        Options { command: None, script: None, approve: false, approve_only: None, posix: false, rest: vec![] };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" => options.command = Some(it.next().cloned().ok_or("-c needs a command")?),
            "--approve" => options.approve = true,
            "--approve-only" => {
                let rows = it.next().ok_or("--approve-only needs row numbers such as 0,2")?;
                options.approve_only =
                    Some(consent::parse_selection(rows).ok_or("--approve-only needs row numbers such as 0,2")?);
            }
            "--posix" => options.posix = true,
            other if other.starts_with('-') && options.script.is_none() => {
                return Err(format!("unknown option {other}"));
            }
            path if options.script.is_none() => options.script = Some(path.to_string()),
            extra => options.rest.push(extra.to_string()),
        }
    }
    Ok(options)
}

/// meersh does not reimplement bash. POSIX scripts, recognised by `--posix`, a `.sh` or
/// `.bash` name or a shell shebang, are handed to bash, the way Oils keeps OSH for them.
fn is_posix_script(path: &str) -> bool {
    if path.ends_with(".sh") || path.ends_with(".bash") {
        return true;
    }
    let first =
        std::fs::read_to_string(path).ok().and_then(|t| t.lines().next().map(str::to_string)).unwrap_or_default();
    first.starts_with("#!")
        && !first.contains("meersh")
        && ["/sh", "bash", "dash", "zsh", "ksh"].iter().any(|s| first.contains(s))
}

fn run_posix(options: &Options) -> i32 {
    let mut bash = std::process::Command::new("bash");
    match (&options.command, &options.script) {
        (Some(command), _) => bash.arg("-c").arg(command),
        (None, Some(script)) => bash.arg(script).args(&options.rest),
        (None, None) => &mut bash,
    };
    match bash.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("meersh: cannot start bash ({e})");
            127
        }
    }
}

const USAGE: &str =
    "usage: meersh [--version] [--approve | --approve-only <rows>] [--posix] [-c <command> | <script> [args...]]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "__exec") {
        helper::run(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "--version") {
        println!("meersh {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let engine = match build_engine() {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("meersh: {e}");
            std::process::exit(2);
        }
    };
    let interrupt = engine.interrupt.clone();
    // Ctrl-C while a pipeline runs stops it at the next row instead of killing the shell.
    let _ = ctrlc::set_handler(move || interrupt.store(true, Ordering::Relaxed));

    if args.first().is_some_and(|a| a == "mcp") {
        std::process::exit(serve_mcp(&engine, &args[1..]));
    }
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("meersh: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    if options.posix || options.script.as_deref().is_some_and(is_posix_script) {
        std::process::exit(run_posix(&options));
    }
    let mut stack = Stack::default();
    let interactive = options.command.is_none() && options.script.is_none();
    if interactive {
        update::offer(&config_path());
        // A Ctrl-C that stopped the update build must not stop the startup script as well.
        engine.interrupt.store(false, Ordering::Relaxed);
    }
    let asker = if interactive {
        Asker::Interactive
    } else {
        Asker::Batch { approve: options.approve, only: options.approve_only.clone() }
    };
    run_init(&engine, &mut stack, &asker);
    let code = match (&options.command, &options.script) {
        (Some(command), _) => run_line(&engine, &mut stack, command, &asker),
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(source) => {
                let script = stack.with_principal(Principal::Script(path.clone()));
                let mut script_stack = script;
                run_line(&engine, &mut script_stack, &source, &asker)
            }
            Err(e) => {
                eprintln!("meersh: {path}: {e}");
                2
            }
        },
        (None, None) => repl(&engine, &mut stack),
    };
    std::process::exit(code);
}

/// `meersh mcp [--agent <name>]`. Serves meersh to a coding agent over MCP on stdio. The
/// startup script runs quietly first, so its definitions and policy rules apply, because
/// nothing but the protocol may reach stdout.
fn serve_mcp(engine: &Arc<Engine>, args: &[String]) -> i32 {
    // `--unique` appends the pid of the agent that started the server. Parallel agents then
    // are separate principals, so their leases and ownership keep them apart. Without it an
    // agent keeps ownership of what it created across restarts.
    let mut agent = "agent".to_string();
    let mut unique = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--agent" => match it.next() {
                Some(name) => agent = name.clone(),
                None => return mcp_usage(),
            },
            "--unique" => unique = true,
            _ => return mcp_usage(),
        }
    }
    if unique {
        agent = format!("{agent}-{}", nix::unistd::getppid());
    }
    let path = std::env::var_os("MEERSH_INIT").map(PathBuf::from).unwrap_or_else(|| config_dir().join("init.msh"));
    if let Ok(source) = std::fs::read_to_string(&path)
        && let Err(e) = engine.run_script(&mut Stack::default(), &source, false, &mut |_| Ok(()))
    {
        eprintln!("meersh: {}: {e}", path.display());
    }
    match meersh_mcp::serve(engine.clone(), &agent) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("meersh mcp: {e}");
            1
        }
    }
}

fn mcp_usage() -> i32 {
    eprintln!("usage: meersh mcp [--agent <name>] [--unique]");
    2
}

/// `~/.config/meersh/init.msh` runs at startup. It is the place for definitions, aliases,
/// environment changes and policy rules.
fn run_init(engine: &Arc<Engine>, stack: &mut Stack, asker: &Asker) {
    let path = std::env::var_os("MEERSH_INIT").map(PathBuf::from).unwrap_or_else(|| config_dir().join("init.msh"));
    if let Ok(source) = std::fs::read_to_string(&path)
        && run_line(engine, stack, &source, asker) != 0
    {
        eprintln!("meersh: {} had errors", path.display());
    }
}

const STREAM_AFTER: Duration = Duration::from_millis(300);

/// Prints the output of one pipeline and remembers it as `$_`. A stream that takes longer
/// than a moment is shown in chunks as it arrives, so `^tail -f log | lines` works.
fn show(engine: &Engine, data: PipelineData, last: &mut Option<Value>) -> Result<()> {
    let opts = RenderOptions::detect();
    let value = match data {
        PipelineData::Empty => return Ok(()),
        PipelineData::Bytes(bytes) => {
            std::io::copy(&mut bytes.into_reader(), &mut std::io::stdout())?;
            return Ok(());
        }
        PipelineData::Stream(stream) => {
            let start = Instant::now();
            let (mut pending, mut all, mut offset, mut streaming) = (Vec::new(), Vec::new(), 0, false);
            for item in stream {
                engine.check_interrupt()?;
                pending.push(item?);
                if streaming || start.elapsed() > STREAM_AFTER {
                    streaming = true;
                    println!("{}", render_chunk(&pending, offset, &opts));
                    offset += pending.len();
                    all.append(&mut pending);
                }
            }
            if streaming {
                if !pending.is_empty() {
                    println!("{}", render_chunk(&pending, offset, &opts));
                }
                all.append(&mut pending);
                *last = Some(Value::List(all));
                return Ok(());
            }
            Value::List(pending)
        }
        other => other.into_value()?,
    };
    let text = match &value {
        Value::Record(r) if r.type_name() == Some("ai.pipeline") => proposal_text(r),
        other => render(other, &opts),
    };
    if !text.is_empty() {
        println!("{text}");
    }
    *last = Some(value);
    Ok(())
}

fn run_line(engine: &Arc<Engine>, stack: &mut Stack, source: &str, asker: &Asker) -> i32 {
    let mut last = None;
    let consent_stack = stack.clone();
    let result = engine.run_planned(
        stack,
        source,
        true,
        &mut |plan| consent::decide(engine, &consent_stack, plan, asker),
        &mut |data| show(engine, data, &mut last),
    );
    consent::review_sandboxes(engine, stack, asker);
    // The user may have changed into another project. Models never move the project root.
    if !stack.principal().is_model() {
        engine.authority.set_project(meersh_engine::authority::project_root());
    }
    if let Some(value) = last {
        stack.set("_", value);
    }
    match result {
        Ok(()) => 0,
        Err(e) => {
            report(&e, source);
            1
        }
    }
}

/// Prints an error, and for errors with a position the source line with a caret under it.
fn report(e: &ShellError, source: &str) {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let label = if color { "\x1b[31merror:\x1b[0m" } else { "error:" };
    eprintln!("{label} {}", e.inner());
    let Some(pos) = e.position() else { return };
    let chars: Vec<char> = source.chars().collect();
    let pos = pos.min(chars.len());
    let start = chars[..pos].iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
    let end = chars[pos..].iter().position(|c| *c == '\n').map_or(chars.len(), |i| pos + i);
    let line: String = chars[start..end].iter().collect();
    eprintln!("  {line}\n  {}{}", " ".repeat(pos - start), if color { "\x1b[31m^\x1b[0m" } else { "^" });
}

fn editor(engine: &Arc<Engine>) -> Reedline {
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![ReedlineEvent::Menu("completion_menu".into()), ReedlineEvent::MenuNext]),
    );
    let mut editor = Reedline::create()
        .with_completer(Box::new(complete::MeershCompleter { engine: engine.clone() }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(ColumnarMenu::default().with_name("completion_menu"))))
        .with_edit_mode(Box::new(Emacs::new(keybindings)));
    if let Some(dir) = dirs::data_dir().map(|d| d.join("meersh")) {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(history) = FileBackedHistory::with_file(10_000, dir.join("history.txt")) {
            editor = editor.with_history(Box::new(history));
        }
    }
    editor
}

fn repl(engine: &Arc<Engine>, stack: &mut Stack) -> i32 {
    let mut editor = editor(engine);
    let prompt = DefaultPrompt::new(DefaultPromptSegment::WorkingDirectory, DefaultPromptSegment::Empty);
    println!(
        "meersh {}. Type `help` for commands and `? <request>` to have a pipeline written for you.",
        env!("CARGO_PKG_VERSION")
    );
    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                engine.interrupt.store(false, Ordering::Relaxed);
                match line.trim().strip_prefix('?') {
                    Some(request) => ask(engine, stack, &mut editor, request.trim()),
                    None => {
                        run_line(engine, stack, &line, &Asker::Interactive);
                        // `llm` and `ai` without input may answer with a pipeline. Offer it like `?` does.
                        let first = line.split_whitespace().next().unwrap_or_default();
                        if matches!(first, "llm" | "ai")
                            && let Some(Value::Record(proposal)) = stack.get("_").cloned()
                            && proposal.type_name() == Some("ai.pipeline")
                        {
                            offer(engine, stack, &mut editor, &proposal, false);
                        }
                    }
                }
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => return 0,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("meersh: {e}");
                return 1;
            }
        }
    }
}

/// `? request`. The model proposes a pipeline. It runs under the plan principal, so
/// everything it would change is planned and shown before anything happens.
fn ask(engine: &Arc<Engine>, stack: &mut Stack, editor: &mut Reedline, request: &str) {
    let quoted = format!("\"{}\"", request.replace('\\', "\\\\").replace('"', "\\\""));
    let mut proposal = None;
    let result = engine.run_script(stack, &format!("ai {quoted}"), false, &mut |data| {
        proposal = Some(data.into_value()?);
        Ok(())
    });
    if let Err(e) = result {
        return report(&e, request);
    }
    let Some(Value::Record(proposal)) = proposal else { return };
    println!("{}", proposal_text(&proposal));
    offer(engine, stack, editor, &proposal, true);
}

/// A pipeline the assistant proposed, as the REPL shows it.
fn proposal_text(proposal: &meersh_engine::value::Record) -> String {
    let field = |k: &str| proposal.get(k).map(Value::to_text).unwrap_or_default();
    let terminal = std::io::stdout().is_terminal();
    format!(
        "\n  {}\n  {}\n  effect: {}",
        consent::styled("1", &field("pipeline"), terminal),
        consent::styled("2", &format!("# {}", field("explanation")), terminal),
        field("effect")
    )
}

/// Offers to run a proposed pipeline. It runs under the plan principal, so everything it
/// would change is planned and shown before anything happens.
fn offer(
    engine: &Arc<Engine>,
    stack: &mut Stack,
    editor: &mut Reedline,
    proposal: &meersh_engine::value::Record,
    default_yes: bool,
) {
    let pipeline = proposal.get("pipeline").map(Value::to_text).unwrap_or_default();
    let answer = consent::read_answer(if default_yes { "run it? [Y/n/e]" } else { "run it? [y/N/e]" });
    let _ = editor.history_mut().save(HistoryItem::from_command_line(pipeline.clone()));
    let run = match answer.as_str() {
        "e" => {
            editor.run_edit_commands(&[EditCommand::InsertString(pipeline)]);
            return;
        }
        "y" | "yes" => true,
        "" => default_yes,
        _ => false,
    };
    if run {
        let mut planned = stack.with_principal(Principal::Plan);
        run_line(engine, &mut planned, &pipeline, &Asker::Interactive);
        if let Some(value) = planned.get("_") {
            stack.set("_", value.clone());
        }
    }
}
