//! Plugins for meersh. A plugin is an executable that adds commands to the shell.
//!
//! meersh starts the plugin with the single argument `--meersh-plugin` and talks to it over
//! standard input and output. [`protocol`] describes the wire format. The host asks for the
//! signatures once at `plugin add` and at every start, and then starts the plugin again for
//! every call. Values cross the process boundary as an RFC 7464 JSON text sequence, the same
//! object protocol that meersh aware programs speak, so types survive both ways.
//!
//! A plugin author implements [`Plugin`] and calls [`serve`] from `main`. The crate
//! `meersh-plugin-example` shows a whole plugin.
//!
//! Commands that declare the effect write or destructive run only after the authority allows
//! running the plugin for that command (`Cap::Exec` with the plugin path and the command
//! name). In plan mode they report nothing and do not run. Pure and read commands just run.
//!
//! One process per call keeps plugins simple and stateless. The next step is a persistent
//! plugin process that serves many requests, which saves the start up cost and lets a plugin
//! keep state such as an open connection.

mod commands;
mod host;
pub mod protocol;
mod registry;
mod serve;

pub use commands::register;
pub use host::{PluginCommand, load_registered, query_signatures};
pub use meersh_engine::{Effect, Signature};
pub use meersh_value::{Record, Type, Value};
pub use registry::{ENV_VAR, Entry, Registry, plugin_name, registry_path};
pub use serve::{Plugin, serve, serve_io};
