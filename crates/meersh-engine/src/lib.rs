//! The meersh engine. It binds parsed calls to commands, evaluates expressions, moves
//! [`PipelineData`] between stages and bridges to external programs.
//!
//! Commands live in other crates (`meersh-cmds`, `meersh-coreutils`, `meersh-llm`) and only
//! see the [`Command`] trait and a [`Ctx`]. The split follows Nushell's `EngineState` and
//! `Stack`. The [`Engine`] is immutable and shared behind an `Arc` once the shell runs, so
//! lazy streams can evaluate row conditions on any thread. The [`Stack`] holds variables
//! and is cheap to clone into such a stream.

pub mod authority;
mod command;
mod data;
mod engine;
mod error;
mod external;
pub mod render;
pub mod util;

pub use authority::{Cap, ExecMode, Permit, Plan, PlannedAction, Principal, Verdict};
pub use command::{Call, Command, Ctx, Effect, Flag, Param, Signature};
pub use data::{ByteSource, ByteStream, PipelineData, ValueStream};
pub use engine::{Adapter, Consent, Engine, Job, Stack, TypeInfo};
pub use error::{Result, ShellError};
pub use meersh_parser as parser;
pub use meersh_value as value;
