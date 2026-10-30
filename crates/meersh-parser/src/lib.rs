//! Lexer and parser for the meersh language.
//!
//! The surface stays close to POSIX shells for the common case (`ls -la src`, `cd ..`,
//! `cmd | cmd`). Arguments of builtins are full expressions, which is what lets
//! `where size > 10mb and name =~ rs` work without quoting. Arguments of external programs
//! are parsed as plain words, so `^grep -E "a|b" file` behaves as in bash.
//!
//! The parser asks the caller which names are builtin commands. That is how it knows that
//! `llm map` is one command and that `git` is an external program.

mod ast;
mod lexer;
mod parser;

pub use ast::{Arg, BinOp, CommandCall, Element, Expr, Pipeline, Redirect, Script, Statement};
pub use parser::{parse, statement_sources};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub msg: String,
    /// Character offset in the source, for a caret under the error.
    pub pos: Option<usize>,
}

impl ParseError {
    pub fn new(msg: impl Into<String>) -> Self {
        ParseError { msg: msg.into(), pos: None }
    }

    pub fn at(msg: impl Into<String>, pos: usize) -> Self {
        ParseError { msg: msg.into(), pos: Some(pos) }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ParseError {}
