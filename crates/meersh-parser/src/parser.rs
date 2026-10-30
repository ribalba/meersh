use meersh_value::parse_literal;

use crate::ParseError;
use crate::ast::*;
use crate::lexer::{Tok, lex, lex_with_positions};

/// Splits a script into the source text of its statements, at `;` and newlines outside of
/// brackets. The engine parses each statement right before it runs, so an alias or `def`
/// applies to the statements after it.
pub fn statement_sources(src: &str) -> std::result::Result<Vec<String>, ParseError> {
    let (toks, positions) = lex_with_positions(src)?;
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (tok, pos) in toks.iter().zip(&positions) {
        match tok {
            Tok::LParen | Tok::LBracket | Tok::LBrace | Tok::CallOpen => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
            Tok::Semi | Tok::Newline if depth <= 0 => {
                out.push(chars[start..*pos].iter().collect::<String>());
                start = pos + 1;
            }
            _ => {}
        }
    }
    out.push(chars[start.min(chars.len())..].iter().collect());
    out.retain(|s: &String| !s.trim().is_empty() && !s.trim_start().starts_with('#'));
    Ok(out)
}

/// Parses a script. `is_command` tells the parser which names are builtins, so that
/// multi word commands such as `llm map` are recognised and unknown names become externals.
/// `alias` expands aliases at the head of a command. An alias may contain a whole pipeline.
pub fn parse(
    src: &str,
    is_command: &dyn Fn(&str) -> bool,
    alias: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Script, ParseError> {
    let (toks, positions) = lex_with_positions(src)?;
    let mut parser = Parser { toks, positions, pos: 0, is_command, alias };
    parser.script()
}

struct Parser<'a> {
    toks: Vec<Tok>,
    /// Source offset of every token, for error carets.
    positions: Vec<usize>,
    pos: usize,
    is_command: &'a dyn Fn(&str) -> bool,
    alias: &'a dyn Fn(&str) -> Option<String>,
}

type Result<T> = std::result::Result<T, ParseError>;

const PREC_COMPARE: u8 = 3;

impl Parser<'_> {
    /// Source offset of the current token, or of the end of the input.
    fn here(&self) -> usize {
        self.positions.get(self.pos).or(self.positions.last()).copied().unwrap_or(0)
    }

    fn fail(&self, msg: impl Into<String>) -> ParseError {
        ParseError::at(msg, self.here())
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&Tok> {
        self.toks.get(self.pos + offset)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.toks.get(self.pos).cloned();
        self.pos += 1;
        tok
    }

    fn expect(&mut self, want: Tok) -> Result<()> {
        match self.next() {
            Some(tok) if tok == want => Ok(()),
            Some(tok) => Err(self.fail(format!("expected {want:?}, found {tok:?}"))),
            None => Err(self.fail(format!("expected {want:?}, found end of input"))),
        }
    }

    fn skip_newlines(&mut self) {
        while self.peek() == Some(&Tok::Newline) {
            self.pos += 1;
        }
    }

    fn at_pipeline_end(&self) -> bool {
        matches!(self.peek(), None | Some(Tok::Pipe | Tok::Semi | Tok::Newline | Tok::RParen | Tok::RBrace | Tok::Amp))
    }

    fn script(&mut self) -> Result<Script> {
        let mut statements = Vec::new();
        loop {
            while matches!(self.peek(), Some(Tok::Newline | Tok::Semi)) {
                self.pos += 1;
            }
            if self.peek().is_none() {
                return Ok(Script { statements });
            }
            statements.push(self.statement()?);
            match self.peek() {
                None | Some(Tok::Newline | Tok::Semi) => {}
                Some(tok) => return Err(self.fail(format!("unexpected {tok:?}"))),
            }
        }
    }

    fn statement(&mut self) -> Result<Statement> {
        if self.peek() == Some(&Tok::Word("let".into()))
            && let Some(Tok::Word(name)) = self.peek_at(1).cloned()
            && self.peek_at(2) == Some(&Tok::Word("=".into()))
        {
            self.pos += 3;
            let name = name.trim_start_matches('$').to_string();
            return Ok(Statement::Let { name, pipeline: self.pipeline()? });
        }
        let pipeline = self.pipeline()?;
        if self.peek() == Some(&Tok::Amp) {
            self.pos += 1;
            return Ok(Statement::Background(pipeline));
        }
        Ok(Statement::Pipeline(pipeline))
    }

    fn pipeline(&mut self) -> Result<Pipeline> {
        let mut elements = vec![self.element()?];
        while self.peek() == Some(&Tok::Pipe) {
            self.pos += 1;
            self.skip_newlines();
            elements.push(self.element()?);
        }
        Ok(Pipeline { elements })
    }

    fn element(&mut self) -> Result<Element> {
        // An alias that starts with its own name (`alias ls "ls -a"`) expands only once.
        let mut expanded = Vec::new();
        while let Some(Tok::Word(word)) = self.peek().cloned()
            && !expanded.contains(&word)
            && let Some(expansion) = (self.alias)(&word)
        {
            let toks = lex(&expansion)?;
            let here = self.here();
            let count = toks.len();
            self.toks.splice(self.pos..self.pos + 1, toks);
            self.positions.splice(self.pos..(self.pos + 1).min(self.positions.len()), std::iter::repeat_n(here, count));
            expanded.push(word);
        }
        match self.peek() {
            Some(Tok::Word(w)) if parse_literal(w).is_none() && BinOp::from_word(w).is_none() => {
                Ok(Element::Command(self.command()?))
            }
            None => Err(self.fail("expected a command")),
            _ => Ok(Element::Expr(self.expr(0)?)),
        }
    }

    fn command(&mut self) -> Result<CommandCall> {
        let pos = self.here();
        let Some(Tok::Word(head)) = self.next() else { unreachable!() };
        let (mut name, mut external) = match head.strip_prefix('^') {
            Some(stripped) => (stripped.to_string(), true),
            None => (head, false),
        };
        if !external {
            while let Some(Tok::Word(sub)) = self.peek() {
                let joined = format!("{name} {sub}");
                if !(self.is_command)(&joined) {
                    break;
                }
                name = joined;
                self.pos += 1;
            }
            external = !(self.is_command)(&name);
        }
        let mut args = Vec::new();
        let mut redirects = Vec::new();
        while !self.at_pipeline_end() {
            if let Some(Tok::Redirect { stdout, stderr, append }) = self.peek().cloned() {
                self.pos += 1;
                if self.at_pipeline_end() {
                    return Err(self.fail("a redirection needs a file name"));
                }
                redirects.push(Redirect { stdout, stderr, append, target: self.primary()? });
                continue;
            }
            args.push(self.arg(external)?);
        }
        Ok(CommandCall { name, external, args, redirects, pos })
    }

    fn arg(&mut self, external: bool) -> Result<Arg> {
        if let Some(Tok::Word(w)) = self.peek()
            && w.starts_with('-')
            && w.len() > 1
            && parse_literal(w).is_none()
            && BinOp::from_word(w).is_none()
        {
            let raw = w.clone();
            self.pos += 1;
            return Ok(match raw.strip_prefix("--") {
                Some(long) => match long.split_once('=') {
                    Some((name, value)) => Arg::Flag {
                        name: name.into(),
                        short: false,
                        value: Some(Expr::Word(value.into())),
                        raw: raw.clone(),
                    },
                    None => Arg::Flag { name: long.into(), short: false, value: None, raw: raw.clone() },
                },
                None => Arg::Flag { name: raw[1..].into(), short: true, value: None, raw: raw.clone() },
            });
        }
        Ok(Arg::Positional(if external { self.primary()? } else { self.expr(0)? }))
    }

    /// Precedence climbing over binary operators. Operators are words, so `>` needs spaces.
    fn expr(&mut self, min_prec: u8) -> Result<Expr> {
        let mut lhs = self.unary()?;
        while let Some(Tok::Word(w)) = self.peek()
            && let Some((op, prec)) = BinOp::from_word(w)
            && prec >= min_prec
        {
            self.pos += 1;
            self.skip_newlines();
            let rhs = self.expr(prec + 1)?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.peek() == Some(&Tok::Word("not".into())) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.expr(PREC_COMPARE)?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        let expr = match self.next() {
            Some(Tok::Word(w)) => Expr::Word(w),
            Some(Tok::Str(s)) => Expr::Str(s),
            Some(Tok::Var(v)) => Expr::Var(v),
            Some(Tok::LParen) => {
                self.skip_newlines();
                let pipeline = self.pipeline()?;
                self.skip_newlines();
                self.expect(Tok::RParen)?;
                Expr::Subexpr(pipeline)
            }
            Some(Tok::LBracket) => {
                let mut items = Vec::new();
                loop {
                    while matches!(self.peek(), Some(Tok::Newline | Tok::Comma)) {
                        self.pos += 1;
                    }
                    if self.peek() == Some(&Tok::RBracket) {
                        self.pos += 1;
                        break;
                    }
                    items.push(self.primary()?);
                }
                Expr::List(items)
            }
            Some(Tok::LBrace) => self.brace()?,
            Some(tok) => return Err(self.fail(format!("unexpected {tok:?}"))),
            None => return Err(self.fail("unexpected end of input")),
        };
        self.postfix(expr)
    }

    /// `{a: 1}` is a record, `{ $it.name }` is a block. A key followed by a colon decides.
    fn brace(&mut self) -> Result<Expr> {
        self.skip_newlines();
        let is_record = self.peek() == Some(&Tok::RBrace)
            || (matches!(self.peek(), Some(Tok::Word(_) | Tok::Str(_))) && self.peek_at(1) == Some(&Tok::Colon));
        if !is_record {
            let pipeline = self.pipeline()?;
            self.skip_newlines();
            self.expect(Tok::RBrace)?;
            return Ok(Expr::Block(pipeline));
        }
        let mut fields = Vec::new();
        loop {
            while matches!(self.peek(), Some(Tok::Newline | Tok::Comma)) {
                self.pos += 1;
            }
            match self.next() {
                Some(Tok::RBrace) => return Ok(Expr::Record(fields)),
                Some(Tok::Word(key) | Tok::Str(key)) => {
                    self.expect(Tok::Colon)?;
                    fields.push((key, self.expr(0)?));
                }
                other => return Err(self.fail(format!("expected a record key, found {other:?}"))),
            }
        }
    }

    fn postfix(&mut self, mut expr: Expr) -> Result<Expr> {
        while let Some(Tok::Member(member)) = self.peek().cloned() {
            self.pos += 1;
            if self.peek() == Some(&Tok::CallOpen) {
                self.pos += 1;
                let mut args = Vec::new();
                while self.peek() != Some(&Tok::RParen) {
                    if self.peek().is_none() {
                        return Err(self.fail(format!("unclosed call of .{member}()")));
                    }
                    args.push(self.arg(false)?);
                }
                self.pos += 1;
                expr = Expr::MethodCall { receiver: Box::new(expr), method: member, args };
            } else {
                expr = Expr::Member(Box::new(expr), member);
            }
        }
        Ok(expr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_alias(_: &str) -> Option<String> {
        None
    }

    fn builtins(name: &str) -> bool {
        matches!(name, "ls" | "where" | "llm" | "llm map" | "sort-by" | "each")
    }

    fn pipeline(src: &str) -> Pipeline {
        match parse(src, &builtins, &no_alias).unwrap().statements.remove(0) {
            Statement::Pipeline(p) => p,
            other => panic!("not a pipeline: {other:?}"),
        }
    }

    #[test]
    fn row_condition() {
        let p = pipeline("ls | where size > 10kb and name =~ rs");
        let Element::Command(call) = &p.elements[1] else { panic!() };
        assert_eq!(call.name, "where");
        assert!(matches!(&call.args[0], Arg::Positional(Expr::Binary(_, BinOp::And, _))));
    }

    #[test]
    fn multi_word_and_external() {
        let p = pipeline("ls | llm map \"summary\" --into s | git log -n 5");
        let Element::Command(map) = &p.elements[1] else { panic!() };
        assert_eq!(map.name, "llm map");
        let Element::Command(git) = &p.elements[2] else { panic!() };
        assert!(git.external);
        assert_eq!(git.args.len(), 3);
    }

    #[test]
    fn blocks_records_and_methods() {
        let p = pipeline("each { {name: $it.name, kind: $it.type} } | $_.0.kill()");
        let Element::Command(each) = &p.elements[0] else { panic!() };
        assert!(matches!(&each.args[0], Arg::Positional(Expr::Block(_))));
        assert!(matches!(&p.elements[1], Element::Expr(Expr::MethodCall { .. })));
    }

    #[test]
    fn aliases_expand_to_pipelines() {
        let alias = |name: &str| match name {
            "big" => Some("ls | sort-by size -r".to_string()),
            "ls" => Some("ls -a".to_string()),
            _ => None,
        };
        let p = match parse("big | first 3", &builtins, &alias).unwrap().statements.remove(0) {
            Statement::Pipeline(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(p.elements.len(), 3);
        let Element::Command(ls) = &p.elements[0] else { panic!() };
        assert_eq!(ls.args.len(), 1, "ls expanded once to ls -a");
    }

    #[test]
    fn redirects_background_and_positions() {
        let script = parse("^make o> build.log e>> err.log; ^sleep 10 &", &builtins, &no_alias).unwrap();
        let Statement::Pipeline(p) = &script.statements[0] else { panic!() };
        let Element::Command(make) = &p.elements[0] else { panic!() };
        assert_eq!(make.redirects.len(), 2);
        assert!(make.redirects[1].append && make.redirects[1].stderr);
        assert!(matches!(&script.statements[1], Statement::Background(_)));
        let Statement::Background(bg) = &script.statements[1] else { panic!() };
        let Element::Command(sleep) = &bg.elements[0] else { panic!() };
        assert_eq!(sleep.pos, 32);

        let err = parse("ls | where (size > ", &builtins, &no_alias).unwrap_err();
        assert!(err.pos.is_some(), "{err:?}");
    }

    #[test]
    fn let_statement() {
        let script = parse("let big = ls | where size > 1mb; $big", &builtins, &no_alias).unwrap();
        assert!(matches!(&script.statements[0], Statement::Let { name, .. } if name == "big"));
        assert_eq!(script.statements.len(), 2);
    }
}
