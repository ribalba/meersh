use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::time::Duration;

use meersh_engine::authority::{Inverse, JournalEntry, proc_id};
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Cap, Command, Ctx, Effect, Engine, Job, Permit, PipelineData, Result, ShellError, Signature};

use crate::table;

pub fn register(engine: &mut Engine) {
    engine.register(Spawn);
    engine.register(Jobs);
    engine.register(Fg);
    engine.register(Undo);
}

struct Spawn;

impl Command for Spawn {
    fn signature(&self) -> Signature {
        Signature::build("spawn")
            .desc(
                "Start a program in the background and return its job record. The session owns the process, so \
                 it may stop it later without asking. Quote arguments that start with a dash.",
            )
            .input(Type::Nothing)
            .output(Type::record("job"))
            .required("program", Type::String, "program to start")
            .rest("args", Type::String, "its arguments")
            .effect(Effect::Write)
            .example("spawn python3 \"-m\" http.server", "serve the current directory in the background")
            .example("spawn sleep 30 | get pid", "the pid of the new process")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let program = ctx.req_string(0, "program")?;
        let args = ctx.rest_strings(1)?;
        Ok(PipelineData::Value(ctx.engine.spawn_job(ctx.stack, &program, &args)?))
    }
}

struct Jobs;

impl Command for Jobs {
    fn signature(&self) -> Signature {
        Signature::build("jobs")
            .desc("List the programs started with spawn or &, with their state and the exit code of finished ones.")
            .input(Type::Nothing)
            .output(Type::table("job"))
            .effect(Effect::Read)
            .example("jobs | where state == running", "jobs that are still running")
            .example("jobs | where state == running | kill", "stop every running job")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut jobs = ctx.engine.jobs.lock().expect("jobs lock");
        let mut rows = Vec::new();
        for job in jobs.iter_mut() {
            let status = poll(job)?;
            rows.push(job_record(job, status));
        }
        Ok(table(rows))
    }
}

/// `Some` once the job has ended. A job without a child handle cannot be reaped here, so
/// it counts as running while its pid still names the process it started as.
fn poll(job: &mut Job) -> Result<Option<Option<ExitStatus>>> {
    Ok(match job.child.as_mut() {
        Some(child) => child.try_wait()?.map(Some),
        None if proc_id(job.pid).is_some() => None,
        None => Some(None),
    })
}

/// `status` is `None` while the job runs and `Some(None)` when it ended with an unknown status.
fn job_record(job: &Job, status: Option<Option<ExitStatus>>) -> Record {
    let (state, exit) = match status {
        None => ("running", None),
        Some(None) => ("exited", None),
        Some(Some(s)) => match s.signal() {
            // The shell convention for a process that a signal ended.
            Some(sig) => ("killed", Some(i64::from(128 + sig))),
            None => ("exited", s.code().map(i64::from)),
        },
    };
    let record = Record::typed("job")
        .with("id", job.id as i64)
        .with("pid", i64::from(job.pid))
        .with("command", job.command.as_str())
        .with("task", job.task.clone())
        .with("unit", job.unit.clone())
        .with("state", state)
        .with("exit", exit);
    // Only a running job can still be acted on, and then as the process it started as.
    match proc_id(job.pid).filter(|_| state == "running") {
        Some(id) => record.with_id(id),
        None => record,
    }
}

struct Fg;

impl Command for Fg {
    fn signature(&self) -> Signature {
        Signature::build("fg")
            .desc(
                "Wait for a job to end and return its job record with the exit code. Ctrl-C stops waiting and \
                 leaves the job running.",
            )
            .input(Type::Nothing)
            .output(Type::record("job"))
            .required("id", Type::Int, "job number from jobs or spawn")
            .effect(Effect::Read)
            .example("fg 1", "wait for job 1")
            .example("spawn make | get id", "the job number to wait for later")
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let id = ctx.req(0, "id")?.as_int().ok_or_else(|| ctx.err("the job id must be a number"))?;
        loop {
            {
                let mut jobs = ctx.engine.jobs.lock().expect("jobs lock");
                let job = jobs.iter_mut().find(|j| j.id as i64 == id).ok_or_else(|| ctx.err(format!("no job {id}")))?;
                // The lock is released between polls, so jobs and spawn keep working meanwhile.
                if let Some(status) = poll(job)? {
                    let record = job_record(job, Some(status));
                    if let Some(code) = record.get("exit").and_then(Value::as_int) {
                        ctx.engine.last_exit.store(code as i32, Ordering::Relaxed);
                    }
                    return Ok(PipelineData::Value(Value::Record(record)));
                }
            }
            ctx.engine.check_interrupt()?;
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

struct Undo;

impl Command for Undo {
    fn signature(&self) -> Signature {
        Signature::build("undo")
            .desc(
                "Take back actions from the journal, newest first. Takes piped journal.entry records, entry ids, \
                 or nothing for the newest action that can be undone. Each reversal is authorized like any other \
                 action. Only a human can undo, never a language model.",
            )
            .input(Type::OneOf(vec![Type::Nothing, Type::table("journal.entry"), Type::list(Type::Int)]))
            .output(Type::table("journal.undo"))
            .rest("ids", Type::Int, "journal entry ids")
            .effect(Effect::Write)
            .example("undo", "take back the last action")
            .example("journal | where principal == actor | undo", "take back everything a model did")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let principal = ctx.principal();
        if principal.is_model() {
            return Err(ShellError::Refused(format!(
                "undo by {}. Only a human may take actions back",
                principal.name()
            )));
        }
        let journal = ctx.engine.authority.journal();
        let mut ids = Vec::new();
        for value in input.into_stream() {
            collect_ids(ctx, value?, &mut ids)?;
        }
        for index in 0..ctx.call.positional.len() {
            if let Some(value) = ctx.arg(index)? {
                collect_ids(ctx, value, &mut ids)?;
            }
        }
        if ids.is_empty() {
            let newest = journal.iter().rev().find(|e| undoable(e)).ok_or_else(|| ctx.err("nothing to undo"))?;
            ids.push(newest.id);
        }
        // Later actions may build on earlier ones, so they are taken back first.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.dedup();

        let mut steps = Vec::new();
        for id in ids {
            let entry = journal.iter().find(|e| e.id == id).ok_or_else(|| ctx.err(format!("no journal entry {id}")))?;
            let permit = match inverse_cap(&entry.inverse) {
                _ if entry.undone => Permit::Skip("already undone".into()),
                None => Permit::Skip("this action cannot be undone".into()),
                Some(cap) => ctx.authorize(cap)?,
            };
            steps.push((entry, permit));
        }
        let mut rows = Vec::new();
        for (entry, permit) in steps {
            let row = Record::typed("journal.undo")
                .with("id", entry.id as i64)
                .with("action", entry.action.as_str())
                .with("target", entry.target.as_str());
            // `action` is the action being taken back, so the outcome goes into `result`.
            match permit {
                Permit::Plan => rows.push(row.with("result", "would undo")),
                Permit::Skip(reason) => rows.push(row.with("result", "refused").with("because", reason)),
                // The engine updates the ledger and marks the entry as undone itself.
                Permit::Execute => match ctx.engine.authority.undo(entry.id)? {
                    Value::Record(done) => rows.push(done),
                    other => rows.push(row.with("result", other)),
                },
            }
        }
        Ok(table(rows))
    }
}

fn undoable(entry: &JournalEntry) -> bool {
    !entry.undone && !matches!(entry.inverse, Inverse::None)
}

/// The action that reverses a journal entry, so the policy judges an undo like any other
/// change and a plan shows what it will do.
fn inverse_cap(inverse: &Inverse) -> Option<Cap> {
    Some(match inverse {
        Inverse::None => return None,
        Inverse::RestoreTrash { path, .. } => Cap::FsCreate { path: path.clone() },
        Inverse::MoveBack { original, current } => Cap::FsMove { from: current.clone(), to: original.clone() },
        Inverse::RemoveCreated { path } => Cap::FsDelete { path: path.clone(), permanent: false },
        Inverse::RestoreSnapshot { path, .. } => Cap::FsWrite { path: path.clone() },
        Inverse::Chmod { path, mode } => Cap::FsChmod { path: path.clone(), mode: *mode },
    })
}

fn collect_ids(ctx: &Ctx, value: Value, out: &mut Vec<u64>) -> Result<()> {
    let id = match value {
        Value::List(items) => {
            for item in items {
                collect_ids(ctx, item, out)?;
            }
            return Ok(());
        }
        Value::Record(r) => r.get("id").and_then(Value::as_int),
        Value::String(s) => s.trim().parse().ok(),
        other => other.as_int(),
    };
    let id = id
        .and_then(|id| u64::try_from(id).ok())
        .ok_or_else(|| ctx.err("expected journal entry ids or rows of journal"))?;
    out.push(id);
    Ok(())
}
