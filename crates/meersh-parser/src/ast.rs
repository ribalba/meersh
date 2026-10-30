#[derive(Debug, Clone, PartialEq)]
pub struct Script {
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Let {
        name: String,
        pipeline: Pipeline,
    },
    Pipeline(Pipeline),
    /// `cmd &`, started as a job.
    Background(Pipeline),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Element {
    Command(CommandCall),
    /// A value at the start of a pipeline, as in `"some text" | llm "translate"`.
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommandCall {
    pub name: String,
    pub external: bool,
    pub args: Vec<Arg>,
    pub redirects: Vec<Redirect>,
    /// Character offset of the command name in the source.
    pub pos: usize,
}

/// `o> file`, `e>> file`, `o+e> file`.
#[derive(Debug, Clone, PartialEq)]
pub struct Redirect {
    pub stdout: bool,
    pub stderr: bool,
    pub append: bool,
    pub target: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Positional(Expr),
    /// `--long`, `--long=value` or a group of short flags such as `-la`. External programs
    /// receive `raw` unchanged. Builtins resolve the flag against their signature.
    Flag {
        name: String,
        short: bool,
        value: Option<Expr>,
        raw: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A bare word. Inside a row condition it names a column if the row has one. Otherwise
    /// it is a literal such as `10kb` or `3day`, or else a plain string.
    Word(String),
    /// A quoted string. Never a column and never a literal.
    Str(String),
    Var(String),
    Member(Box<Expr>, String),
    /// `$p.kill()` is sugar for `$p | kill`, resolved through the record's type tag.
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        args: Vec<Arg>,
    },
    List(Vec<Expr>),
    Record(Vec<(String, Expr)>),
    /// `{ ... }` evaluated later, once per row, with the row bound to `$it`.
    Block(Pipeline),
    Subexpr(Pipeline),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Match,
    NotMatch,
    In,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Match => "=~",
            BinOp::NotMatch => "!~",
            BinOp::In => "in",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
        }
    }

    pub(crate) fn from_word(word: &str) -> Option<(BinOp, u8)> {
        Some(match word {
            "or" => (BinOp::Or, 1),
            "and" => (BinOp::And, 2),
            "==" => (BinOp::Eq, 3),
            "!=" => (BinOp::Ne, 3),
            "<" => (BinOp::Lt, 3),
            "<=" => (BinOp::Le, 3),
            ">" => (BinOp::Gt, 3),
            ">=" => (BinOp::Ge, 3),
            "=~" => (BinOp::Match, 3),
            "!~" => (BinOp::NotMatch, 3),
            "in" => (BinOp::In, 3),
            "+" => (BinOp::Add, 4),
            "-" => (BinOp::Sub, 4),
            "*" => (BinOp::Mul, 5),
            "/" => (BinOp::Div, 5),
            _ => return None,
        })
    }
}

impl Pipeline {
    /// Visits every command call, including the ones nested in blocks and subexpressions.
    /// The engine uses this to compute the effects of a pipeline before running it.
    pub fn walk_commands(&self, f: &mut dyn FnMut(&CommandCall)) {
        for element in &self.elements {
            match element {
                Element::Command(call) => {
                    f(call);
                    for arg in &call.args {
                        if let Arg::Positional(e) | Arg::Flag { value: Some(e), .. } = arg {
                            e.walk_commands(f);
                        }
                    }
                }
                Element::Expr(e) => e.walk_commands(f),
            }
        }
    }
}

impl Expr {
    fn walk_commands(&self, f: &mut dyn FnMut(&CommandCall)) {
        match self {
            Expr::Block(p) | Expr::Subexpr(p) => p.walk_commands(f),
            Expr::Member(e, _) | Expr::Not(e) => e.walk_commands(f),
            Expr::Binary(a, _, b) => {
                a.walk_commands(f);
                b.walk_commands(f);
            }
            Expr::List(items) => items.iter().for_each(|e| e.walk_commands(f)),
            Expr::Record(fields) => fields.iter().for_each(|(_, e)| e.walk_commands(f)),
            Expr::MethodCall { receiver, method, args } => {
                receiver.walk_commands(f);
                f(&CommandCall { name: method.clone(), args: args.clone(), ..CommandCall::default() });
            }
            Expr::Word(_) | Expr::Str(_) | Expr::Var(_) => {}
        }
    }
}

/// Prints the AST back as source. Rules, definitions and generated pipelines are shown this way.
impl std::fmt::Display for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self
            .elements
            .iter()
            .map(|e| match e {
                Element::Command(call) => call.to_string(),
                Element::Expr(expr) => expr.to_string(),
            })
            .collect();
        f.write_str(&parts.join(" | "))
    }
}

impl std::fmt::Display for CommandCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.external {
            f.write_str("^")?;
        }
        f.write_str(&self.name)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        for r in &self.redirects {
            let streams = match (r.stdout, r.stderr) {
                (true, true) => "o+e",
                (false, true) => "e",
                _ => "o",
            };
            write!(f, " {streams}{} {}", if r.append { ">>" } else { ">" }, r.target)?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Arg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Arg::Positional(expr) => write!(f, "{expr}"),
            Arg::Flag { raw, value: Some(v), .. } if !raw.contains('=') => write!(f, "{raw} {v}"),
            Arg::Flag { raw, .. } => f.write_str(raw),
        }
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Word(w) => f.write_str(w),
            Expr::Str(s) => write!(f, "\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Expr::Var(v) => write!(f, "${v}"),
            Expr::Member(base, m) => write!(f, "{base}.{m}"),
            Expr::MethodCall { receiver, method, args } => {
                let args: Vec<String> = args.iter().map(ToString::to_string).collect();
                write!(f, "{receiver}.{method}({})", args.join(" "))
            }
            Expr::List(items) => {
                let items: Vec<String> = items.iter().map(ToString::to_string).collect();
                write!(f, "[{}]", items.join(" "))
            }
            Expr::Record(fields) => {
                let fields: Vec<String> = fields.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                write!(f, "{{{}}}", fields.join(", "))
            }
            Expr::Block(p) => write!(f, "{{ {p} }}"),
            Expr::Subexpr(p) => write!(f, "({p})"),
            Expr::Binary(a, op, b) => write!(f, "{a} {} {b}", op.symbol()),
            Expr::Not(e) => write!(f, "not {e}"),
        }
    }
}
