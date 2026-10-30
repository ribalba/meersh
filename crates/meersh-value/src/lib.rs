//! The data model of meersh.
//!
//! Everything that flows through a meersh pipeline is a [`Value`]. Values are plain data,
//! so they can always be serialized for another process or an LLM. Records can carry a
//! nominal type tag such as `fs.entry` or `proc`. The engine uses that tag to find methods,
//! renderers and type specific commands, which is what makes `ps | where cpu > 50 | kill`
//! work without the commands knowing about each other.

mod format;
mod json;
pub mod protocol;
mod record;
mod types;
mod value;

pub use format::{format_duration, format_filesize, parse_literal};
pub use record::Record;
pub use types::Type;
pub use value::Value;
