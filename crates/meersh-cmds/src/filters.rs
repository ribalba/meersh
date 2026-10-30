use std::cmp::Ordering;
use std::sync::Arc;

use meersh_engine::value::{Record, Type, Value};
use meersh_engine::{Command, Ctx, Engine, PipelineData, Result, ShellError, Signature};

pub fn register(engine: &mut Engine) {
    engine.register(Where);
    engine.register(Each);
    engine.register(Insert);
    engine.register(Select);
    engine.register(Reject);
    engine.register(Get);
    engine.register(SortBy);
    engine.register(First);
    engine.register(Last);
    engine.register(Length);
    engine.register(Reverse);
    engine.register(Uniq);
    engine.register(GroupBy);
    engine.register(Sum);
}

fn table_in_out(sig: Signature) -> Signature {
    sig.input(Type::any_table()).output(Type::any_table())
}

struct Where;

impl Command for Where {
    fn signature(&self) -> Signature {
        table_in_out(Signature::build("where"))
            .desc("Keep the rows for which the condition is true. Bare words name columns of the row.")
            .required("condition", Type::Block, "a row condition such as `size > 1mb` or a block `{ $it.size > 1mb }`")
            .example("ls | where size > 1mb and name =~ '\\.rs$'", "large Rust files")
            .example("ps | where cpu > 10", "busy processes")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let condition = ctx.positional(0).cloned().ok_or_else(|| ctx.err("missing condition"))?;
        let engine = Arc::clone(ctx.engine);
        let stack = ctx.stack.clone();
        Ok(PipelineData::stream(input.into_stream().filter_map(move |row| {
            let row = match row {
                Ok(row) => row,
                Err(e) => return Some(Err(e)),
            };
            match engine.eval_row(&stack, &condition, &row) {
                Ok(keep) if keep.is_truthy() => Some(Ok(row)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }
        })))
    }
}

struct Each;

impl Command for Each {
    fn signature(&self) -> Signature {
        Signature::build("each")
            .desc("Run a block for every item. The item is `$it` and also the block's input.")
            .input(Type::list(Type::Any))
            .output(Type::list(Type::Any))
            .required("block", Type::Block, "the block to run")
            .example("ls | each { {file: $it.name, kb: ($it.size / 1kb)} }", "build new records")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let block = ctx.positional(0).cloned().ok_or_else(|| ctx.err("missing block"))?;
        let engine = Arc::clone(ctx.engine);
        let stack = ctx.stack.clone();
        Ok(PipelineData::stream(input.into_stream().map(move |item| engine.eval_row(&stack, &block, &item?))))
    }
}

struct Insert;

impl Command for Insert {
    fn signature(&self) -> Signature {
        table_in_out(Signature::build("insert"))
            .desc("Add a column. The value is evaluated per row, so it can use the row's columns.")
            .required("column", Type::String, "name of the new column")
            .required("value", Type::Any, "a value or a block")
            .example("ls | insert kb { $it.size / 1kb }", "add a computed column")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let column = ctx.req_string(0, "column")?;
        let value = ctx.positional(1).cloned().ok_or_else(|| ctx.err("missing value"))?;
        let engine = Arc::clone(ctx.engine);
        let stack = ctx.stack.clone();
        Ok(PipelineData::stream(input.into_stream().map(move |row| {
            let row = row?;
            let new = engine.eval_row(&stack, &value, &row)?;
            let mut record = match row {
                Value::Record(r) => r,
                other => Record::new().with("value", other),
            };
            record.insert(column.clone(), new);
            Ok(Value::Record(record))
        })))
    }
}

struct Select;

impl Command for Select {
    fn signature(&self) -> Signature {
        table_in_out(Signature::build("select"))
            .desc("Keep only the given columns, in the given order.")
            .rest("columns", Type::String, "columns to keep")
            .example("ps | select pid name cpu", "three columns of the process table")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let columns = ctx.rest_strings(0)?;
        Ok(map_records(input, move |r| {
            let mut out = Record { type_name: r.type_name.clone(), id: r.id.clone(), ..Record::default() };
            for c in &columns {
                out.insert(c.clone(), r.get(c).cloned().unwrap_or(Value::Nothing));
            }
            out
        }))
    }
}

struct Reject;

impl Command for Reject {
    fn signature(&self) -> Signature {
        table_in_out(Signature::build("reject")).desc("Drop the given columns.").rest(
            "columns",
            Type::String,
            "columns to drop",
        )
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let columns = ctx.rest_strings(0)?;
        Ok(map_records(input, move |mut r| {
            r.fields.retain(|k, _| !columns.contains(k));
            r
        }))
    }
}

/// Applies `f` to every record and passes other values through unchanged.
fn map_records(input: PipelineData, f: impl Fn(Record) -> Record + Send + 'static) -> PipelineData {
    match input {
        PipelineData::Value(Value::Record(r)) => PipelineData::Value(Value::Record(f(r))),
        other => PipelineData::stream(other.into_stream().map(move |v| {
            Ok(match v? {
                Value::Record(r) => Value::Record(f(r)),
                other => other,
            })
        })),
    }
}

struct Get;

impl Command for Get {
    fn signature(&self) -> Signature {
        Signature::build("get")
            .desc("Extract a column of a table or a field of a record. Dots walk into nested data.")
            .input(Type::Any)
            .output(Type::Any)
            .required("path", Type::String, "a cell path such as `name` or `meta.size`")
            .example("ls | get name", "a list of file names")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let path = ctx.req_string(0, "path")?;
        let follow = move |value: Value| -> Result<Value> {
            path.split('.').try_fold(value, |v, member| {
                v.follow(member).ok_or_else(|| ShellError::Other(format!("no member '{member}' in {}", v.type_name())))
            })
        };
        match input {
            PipelineData::Value(v @ Value::Record(_)) => Ok(PipelineData::Value(follow(v)?)),
            other => Ok(PipelineData::stream(other.into_stream().map(move |v| follow(v?)))),
        }
    }
}

struct SortBy;

impl Command for SortBy {
    fn signature(&self) -> Signature {
        table_in_out(Signature::build("sort-by"))
            .desc("Sort rows by a column, or plain values by themselves.")
            .optional("column", Type::String, "column to sort by")
            .switch("reverse", Some('r'), "largest first")
            .example("ls | sort-by size -r | first 5", "the five largest files")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let column = ctx.opt_string(0)?;
        let mut rows = collect(input)?;
        let key = |v: &Value| match &column {
            Some(c) => v.follow(c).unwrap_or(Value::Nothing),
            None => v.clone(),
        };
        rows.sort_by(|a, b| key(a).compare(&key(b)).unwrap_or(Ordering::Equal));
        if ctx.switch("reverse") {
            rows.reverse();
        }
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

struct First;

impl Command for First {
    fn signature(&self) -> Signature {
        Signature::build("first")
            .desc("The first n items. Without n, the first item itself.")
            .input(Type::list(Type::Any))
            .output(Type::Any)
            .optional("n", Type::Int, "how many")
            .example("ps | sort-by cpu -r | first 3", "top three by CPU")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        match ctx.arg(0)? {
            None => Ok(PipelineData::Value(input.into_stream().next().transpose()?.unwrap_or(Value::Nothing))),
            Some(n) => {
                let n = n.as_int().ok_or_else(|| ctx.err("n must be an int"))?.max(0) as usize;
                // `take` stops pulling, so `^yes | first 3` ends instead of reading forever.
                Ok(PipelineData::stream(input.into_stream().take(n)))
            }
        }
    }
}

struct Last;

impl Command for Last {
    fn signature(&self) -> Signature {
        Signature::build("last")
            .desc("The last n items. Without n, the last item itself.")
            .input(Type::list(Type::Any))
            .output(Type::Any)
            .optional("n", Type::Int, "how many")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let mut rows = collect(input)?;
        match ctx.arg(0)? {
            None => Ok(PipelineData::Value(rows.pop().unwrap_or(Value::Nothing))),
            Some(n) => {
                let n = n.as_int().ok_or_else(|| ctx.err("n must be an int"))?.max(0) as usize;
                let tail = rows.split_off(rows.len().saturating_sub(n));
                Ok(PipelineData::Value(Value::List(tail)))
            }
        }
    }
}

struct Length;

impl Command for Length {
    fn signature(&self) -> Signature {
        Signature::build("length").desc("Count the items.").input(Type::list(Type::Any)).output(Type::Int)
    }

    fn run(&self, _ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let mut count = 0i64;
        for item in input.into_stream() {
            item?;
            count += 1;
        }
        Ok(PipelineData::Value(Value::Int(count)))
    }
}

struct Reverse;

impl Command for Reverse {
    fn signature(&self) -> Signature {
        Signature::build("reverse")
            .desc("Reverse the order of items.")
            .input(Type::list(Type::Any))
            .output(Type::list(Type::Any))
    }

    fn run(&self, _ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let mut rows = collect(input)?;
        rows.reverse();
        Ok(PipelineData::Value(Value::List(rows)))
    }
}

struct Uniq;

impl Command for Uniq {
    fn signature(&self) -> Signature {
        Signature::build("uniq")
            .desc("Drop duplicate items, keeping the first.")
            .input(Type::list(Type::Any))
            .output(Type::list(Type::Any))
    }

    fn run(&self, _ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let mut seen: Vec<Value> = Vec::new();
        for item in input.into_stream() {
            let item = item?;
            if !seen.contains(&item) {
                seen.push(item);
            }
        }
        Ok(PipelineData::Value(Value::List(seen)))
    }
}

struct GroupBy;

impl Command for GroupBy {
    fn signature(&self) -> Signature {
        Signature::build("group-by")
            .desc("Group rows by the value of a column. Every group has the key, a count and its rows.")
            .input(Type::any_table())
            .output(Type::Table {
                name: None,
                fields: vec![("count".into(), Type::Int), ("items".into(), Type::any_table())],
            })
            .required("column", Type::String, "column to group by")
            .example("ls | group-by type", "files and directories apart")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let column = ctx.req_string(0, "column")?;
        let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
        for row in input.into_stream() {
            let row = row?;
            let key = row.follow(&column).unwrap_or(Value::Nothing);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((key, vec![row])),
            }
        }
        let out = groups
            .into_iter()
            .map(|(key, rows)| {
                Value::Record(
                    Record::new().with(&column, key).with("count", rows.len() as i64).with("items", Value::List(rows)),
                )
            })
            .collect();
        Ok(PipelineData::Value(Value::List(out)))
    }
}

struct Sum;

impl Command for Sum {
    fn signature(&self) -> Signature {
        Signature::build("sum")
            .desc("Add up numbers, filesizes or durations.")
            .input(Type::list(Type::Number))
            .output(Type::Number)
            .example("ls | get size | sum", "total size of the directory listing")
    }

    fn run(&self, ctx: &Ctx, input: PipelineData) -> Result<PipelineData> {
        let mut total = Value::Int(0);
        for item in input.into_stream() {
            total = match (total, item?) {
                // The start value adopts the unit of the first item.
                (Value::Int(0), first @ (Value::Filesize(_) | Value::Duration(_) | Value::Float(_))) => first,
                (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
                (Value::Filesize(a), Value::Filesize(b)) => Value::Filesize(a + b),
                (Value::Duration(a), Value::Duration(b)) => Value::Duration(a + b),
                (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => Value::Float(a as f64 + b),
                (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
                (total, Value::Nothing) => total,
                (a, b) => return Err(ctx.err(format!("cannot add {} and {}", a.type_name(), b.type_name()))),
            };
        }
        Ok(PipelineData::Value(total))
    }
}

pub(crate) fn collect(input: PipelineData) -> Result<Vec<Value>> {
    input.into_stream().collect()
}
