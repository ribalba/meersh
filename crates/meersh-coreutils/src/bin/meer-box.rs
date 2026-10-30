//! A multi-call binary in the style of busybox and the uutils `coreutils` binary. It runs
//! one meersh coreutils command outside the shell, as `meer-box ls -l src` or through a
//! symlink named after the command.
//!
//! In a terminal or a bash pipe it prints the same tables as the shell. When meersh starts
//! it, it writes typed objects instead, on descriptor 3 when meersh offers it (protocol v2)
//! and on standard output when only `MEERSH_OBJECTS=json-seq` is set (v1). So one Rust
//! implementation serves bash users with text and meersh users with objects that cross the
//! external program boundary intact.

use std::io::{self, BufWriter, ErrorKind, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use meersh_engine::render::{RenderOptions, render};
use meersh_engine::value::protocol::{Encoder, object_writer};
use meersh_engine::{Engine, PipelineData, Result, ShellError, Stack};

const NAME: &str = "meer-box";

fn main() -> ExitCode {
    let mut engine = Engine::new();
    meersh_coreutils::register(&mut engine);
    let engine = Arc::new(engine);

    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let mut args: Vec<String> = args.collect();
    let invoked = Path::new(&argv0).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let command = if engine.is_command(&invoked) {
        invoked
    } else if args.is_empty() {
        eprintln!("{}", usage(&engine));
        return ExitCode::FAILURE;
    } else if matches!(args[0].as_str(), "-h" | "--help") {
        println!("{}", usage(&engine));
        return ExitCode::SUCCESS;
    } else {
        args.remove(0)
    };
    // Without this check an unknown name would reach the parser and run as an external program.
    if !engine.is_command(&command) {
        eprintln!("'{command}' is not a {NAME} command\n{}", usage(&engine));
        return ExitCode::FAILURE;
    }

    let src = script(&command, &args);
    let mut encoder = object_writer().map(|out| Encoder::new(BufWriter::new(out)));
    let mut sink = |data: PipelineData| -> Result<()> {
        match encoder.as_mut() {
            Some(encoder) => {
                for value in data.into_stream() {
                    encoder.write(&value?)?;
                }
                encoder.flush()?;
            }
            None => {
                let text = render(&data.into_value()?, &RenderOptions::detect());
                if !text.is_empty() {
                    writeln!(io::stdout(), "{text}")?;
                }
            }
        }
        Ok(())
    };
    match engine.run_script(&mut Stack::default(), &src, true, &mut sink) {
        Ok(()) => ExitCode::SUCCESS,
        // A reader like `head` that stops early is not a failure of the command.
        Err(ShellError::Io(e)) if e.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(engine: &Engine) -> String {
    let names: Vec<String> = engine.signatures().iter().map(|s| s.name.clone()).collect();
    format!("{NAME} <command> [args...]\ncommands: {}", names.join(", "))
}

/// Flags stay bare so the parser binds them. Everything else becomes a string literal, so
/// a file named `10kb` or `a|b` arrives as written and not as a filesize or a pipe. After
/// `--` every argument is a literal, even one that starts with a dash.
fn script(command: &str, args: &[String]) -> String {
    let mut words = vec![command.to_string()];
    let mut literal_only = false;
    for arg in args {
        if !literal_only && arg == "--" {
            literal_only = true;
        } else if !literal_only && arg.starts_with('-') {
            words.push(arg.clone());
        } else {
            words.push(format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")));
        }
    }
    words.join(" ")
}
