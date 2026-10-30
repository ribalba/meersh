//! Language models as pipeline stages.
//!
//! An LLM stage is an ordinary command. It consumes the values of the previous stage and
//! produces values for the next one. With `--schema` the model has to answer in a JSON
//! Schema derived from a meersh type, so the output is typed data and not prose.
//!
//! The row-wise commands follow the semantic operators of LOTUS (`sem_map`, `sem_filter`,
//! `sem_extract`). They batch rows into one request instead of calling the model per row.
//!
//! Backends are local first. Ollama and every OpenAI compatible server (llama.cpp, vLLM,
//! LM Studio) are spoken to over HTTP. Claude Code is driven through `claude -p`, which
//! reuses the user's existing login and model choice.

mod actor;
mod agent;
mod assistant;
mod backend;
mod cache;
mod commands;
mod config;
mod embed;
mod encode;
mod semantic;

use std::sync::Arc;

pub use backend::{Request, Response};
pub use config::{BackendConfig, BackendKind, LlmConfig};
use meersh_engine::Engine;

/// Session wide LLM state, shared by all LLM commands.
pub struct LlmRuntime {
    config: std::sync::RwLock<LlmConfig>,
}

impl LlmRuntime {
    pub fn new(config: LlmConfig) -> Arc<Self> {
        Arc::new(LlmRuntime { config: std::sync::RwLock::new(config) })
    }

    pub fn config(&self) -> LlmConfig {
        self.config.read().expect("config lock").clone()
    }

    pub fn set_default(&self, name: &str) -> Result<(), String> {
        let mut config = self.config.write().expect("config lock");
        if !config.backends.contains_key(name) {
            return Err(format!("no backend named '{name}', see `llm backends`"));
        }
        config.default = name.to_string();
        Ok(())
    }

    /// The backend a request would use and whether it runs on this machine.
    pub fn locality(&self, backend: Option<&str>) -> (String, bool) {
        let config = self.config();
        let name = backend.unwrap_or(&config.default).to_string();
        let local = config.backends.get(&name).is_some_and(|b| match b.kind {
            BackendKind::ClaudeCode => false,
            _ => b.url.as_deref().is_none_or(|u| ["localhost", "127.0.0.1", "[::1]"].iter().any(|h| u.contains(h))),
        });
        (name, local)
    }

    /// Like [`LlmRuntime::complete`] for plain text, calling `on_token` as text arrives.
    /// Ollama streams. Other backends deliver the whole answer at once.
    pub fn complete_streaming(
        &self,
        request: &Request,
        backend: Option<&str>,
        model: Option<&str>,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Response, String> {
        let config = self.config();
        let name = backend.unwrap_or(&config.default);
        let mut backend_config = config
            .backends
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no backend named '{name}', see `llm backends`"))?;
        if let Some(model) = model {
            backend_config.model = Some(model.to_string());
        }
        let key = cache::key(&[
            name,
            backend_config.model.as_deref().unwrap_or("default"),
            &request.system,
            &request.prompt,
            "",
        ]);
        if config.cache
            && let Some(hit) = cache::get(&key)
        {
            on_token(&hit.text);
            return Ok(hit);
        }
        let response = if backend_config.kind == BackendKind::Ollama {
            backend::ollama_stream(&backend_config, request, on_token)?
        } else {
            let response = backend::complete(&backend_config, request)?;
            on_token(&response.text);
            response
        };
        if config.cache {
            cache::put(&key, &response);
        }
        Ok(response)
    }

    /// Sends one request, answering from the cache when the same request was seen before.
    /// Caching matters because users iterate on the end of a pipeline and rerun it often.
    pub fn complete(&self, request: &Request, backend: Option<&str>, model: Option<&str>) -> Result<Response, String> {
        let config = self.config();
        let name = backend.unwrap_or(&config.default);
        let mut backend_config = config
            .backends
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no backend named '{name}', see `llm backends`"))?;
        if let Some(model) = model {
            backend_config.model = Some(model.to_string());
        }
        let key = cache::key(&[
            name,
            backend_config.model.as_deref().unwrap_or("default"),
            &request.system,
            &request.prompt,
            &request.schema.as_ref().map(ToString::to_string).unwrap_or_default(),
        ]);
        if config.cache
            && let Some(hit) = cache::get(&key)
        {
            return Ok(hit);
        }
        let response = backend::complete(&backend_config, request)?;
        if config.cache {
            cache::put(&key, &response);
        }
        Ok(response)
    }
}

pub fn register(engine: &mut Engine, runtime: Arc<LlmRuntime>) {
    commands::register(engine, runtime.clone());
    semantic::register(engine, runtime);
}
