use meersh_parser::ParseError;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("command not found: {0}")]
    CommandNotFound(String),
    #[error("{cmd}: {msg}")]
    Command { cmd: String, msg: String },
    #[error("expected {expected}, found {found}")]
    Type { expected: String, found: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("interrupted")]
    Interrupted,
    /// The action needs a human. The REPL turns this into a plan to approve.
    #[error("needs approval, {0}")]
    NeedsApproval(String),
    #[error("refused, {0}")]
    Refused(String),
    #[error("{0}")]
    Other(String),
    /// An error together with the source offset of the command that caused it.
    #[error("{inner}")]
    At { pos: usize, inner: Box<ShellError> },
}

impl ShellError {
    /// Attaches a source position to errors that point at one command.
    pub fn at(self, pos: usize) -> ShellError {
        match self {
            ShellError::Command { .. }
            | ShellError::CommandNotFound(_)
            | ShellError::Type { .. }
            | ShellError::Other(_) => ShellError::At { pos, inner: Box::new(self) },
            other => other,
        }
    }

    /// The error without its position.
    pub fn inner(&self) -> &ShellError {
        match self {
            ShellError::At { inner, .. } => inner.inner(),
            other => other,
        }
    }

    pub fn position(&self) -> Option<usize> {
        match self {
            ShellError::At { pos, .. } => Some(*pos),
            ShellError::Parse(e) => e.pos,
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, ShellError>;
