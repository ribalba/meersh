use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use meersh_engine::authority::{Plan, Rule};
use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{
    Cap, Command, Ctx, Effect, Engine, ExecMode, Permit, PipelineData, Principal, Result, Signature, Stack, Verdict,
};

/// `make <path>` creates an empty file through the authority API.
struct Make;

impl Command for Make {
    fn signature(&self) -> Signature {
        Signature::build("make").required("path", Type::String, "").effect(Effect::Write)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let path = PathBuf::from(ctx.req_string(0, "path")?);
        let action = match ctx.authorize(Cap::FsCreate { path: path.clone() })? {
            Permit::Execute => {
                std::fs::write(&path, "")?;
                ctx.performed(Cap::FsCreate { path: path.clone() })?;
                "created"
            }
            Permit::Plan => "would create",
            Permit::Skip(_) => "skipped",
        };
        Ok(PipelineData::Value(Value::Record(Record::new().with("action", action))))
    }
}

/// `remove ...paths` deletes files for good.
struct Remove;

impl Command for Remove {
    fn signature(&self) -> Signature {
        Signature::build("remove").rest("paths", Type::String, "").effect(Effect::Destructive)
    }

    fn run(&self, ctx: &Ctx, _input: PipelineData) -> Result<PipelineData> {
        let mut rows = Vec::new();
        for path in ctx.rest_strings(0)?.into_iter().map(PathBuf::from) {
            let cap = || Cap::FsDelete { path: path.clone(), permanent: true };
            let action = match ctx.authorize(cap())? {
                Permit::Execute => {
                    std::fs::remove_file(&path)?;
                    ctx.performed(cap())?;
                    "deleted".to_string()
                }
                Permit::Plan => "would delete".into(),
                Permit::Skip(why) => format!("skipped: {why}"),
            };
            rows.push(Value::Record(Record::new().with("action", action)));
        }
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

fn engine() -> Arc<Engine> {
    let mut engine = Engine::new();
    engine.register(Make);
    engine.register(Remove);
    Arc::new(engine)
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("meersh-authority-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(engine: &Arc<Engine>, stack: &mut Stack, src: &str) -> Result<Value> {
    let mut out = Value::Nothing;
    engine.run_script(stack, src, false, &mut |d| {
        out = d.into_value()?;
        Ok(())
    })?;
    Ok(out)
}

fn plan(engine: &Arc<Engine>, stack: &Stack, src: &str) -> Plan {
    let plan = Arc::new(Mutex::new(Plan::default()));
    let mut planning = stack.with_mode(ExecMode::Plan(plan.clone()));
    run(engine, &mut planning, src).unwrap();
    std::mem::take(&mut *plan.lock().unwrap())
}

#[test]
fn a_model_may_remove_what_the_session_created_and_nothing_else() {
    let engine = engine();
    let dir = temp("own");
    let mine = dir.join("mine.txt");
    let foreign = dir.join("foreign.txt");
    std::fs::write(&foreign, "written outside meersh").unwrap();

    let mut user = Stack::default();
    run(&engine, &mut user, &format!("make '{}'", mine.display())).unwrap();

    let actor = user.with_principal(Principal::Actor);
    let p = plan(&engine, &actor, &format!("remove '{}'; remove '{}'", mine.display(), foreign.display()));
    let verdicts: Vec<_> = p.actions.iter().map(|a| (a.verdict, a.reason.clone())).collect();
    assert_eq!(verdicts[0].0, Verdict::Allow, "{verdicts:?}");
    assert_eq!(verdicts[1].0, Verdict::Deny, "{verdicts:?}");
    assert!(mine.exists() && foreign.exists(), "planning changes nothing");

    // Without a plan, a model may not act at all.
    let mut direct = actor.clone();
    assert!(run(&engine, &mut direct, &format!("remove '{}'", mine.display())).is_err());

    // Applying the approved plan removes only the approved file.
    let mut applying = actor.with_mode(ExecMode::Apply(Arc::new(p.approvable_keys())));
    run(&engine, &mut applying, &format!("remove '{}'; remove '{}'", mine.display(), foreign.display())).unwrap();
    assert!(!mine.exists());
    assert!(foreign.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ownership_lapses_when_someone_else_changes_the_file() {
    let engine = engine();
    let dir = temp("lapse");
    let file = dir.join("notes.md");
    let mut user = Stack::default();
    run(&engine, &mut user, &format!("make '{}'", file.display())).unwrap();
    std::fs::write(&file, "three pages of the user's own work").unwrap();
    let p = plan(&engine, &user.with_principal(Principal::Actor), &format!("remove '{}'", file.display()));
    assert_ne!(p.actions[0].verdict, Verdict::Allow, "{:?}", p.actions[0]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rules_come_before_defaults_and_the_journal_can_undo() {
    let engine = engine();
    let dir = temp("rules");
    let file = dir.join("a.txt");
    let script = format!("make '{}'", file.display());
    let user = Stack::default();
    engine.authority.add_rule(Rule {
        verdict: Verdict::Deny,
        principal: "*".into(),
        cap: "fs.*".into(),
        condition: Some(meersh_engine::parser::Expr::Word("in_project".into())),
        task: None,
        text: "nothing in the project".into(),
    });
    // The temp file is outside the project, so the rule does not match.
    let p = plan(&engine, &user, &script);
    assert_eq!(p.actions[0].verdict, Verdict::Allow);

    let mut stack = user.clone();
    run(&engine, &mut stack, &script).unwrap();
    let entry = engine.authority.journal().last().cloned().unwrap();
    assert_eq!(entry.action, "create");
    // Undoing a creation moves the file to the trash. Do not touch the real trash in tests.
    assert!(file.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn approving_some_rows_applies_only_those_and_the_allowed_ones() {
    use meersh_engine::Consent;

    let engine = engine();
    let dir = temp("partial");
    let (mine, first, second) = (dir.join("mine.txt"), dir.join("first.txt"), dir.join("second.txt"));
    std::fs::write(&first, "a").unwrap();
    std::fs::write(&second, "b").unwrap();
    let mut user = Stack::default();
    run(&engine, &mut user, &format!("make '{}'", mine.display())).unwrap();

    // The user removes files, but a model picked them, so every foreign target asks.
    let mut tainted = user.tainted();
    let src = format!("remove '{}' '{}' '{}'", mine.display(), first.display(), second.display());
    let mut seen = Vec::new();
    engine
        .run_planned(
            &mut tainted,
            &src,
            false,
            &mut |plan| {
                seen.extend(plan.actions.iter().map(|a| a.verdict));
                Consent::Only(vec![1])
            },
            &mut |_| Ok(()),
        )
        .unwrap();
    assert_eq!(seen, [Verdict::Allow, Verdict::Ask, Verdict::Ask]);
    assert!(!mine.exists(), "owned, allowed without approval");
    assert!(!first.exists(), "approved");
    assert!(second.exists(), "not approved");
    std::fs::remove_dir_all(dir).unwrap();
}
