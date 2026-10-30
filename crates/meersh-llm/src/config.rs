use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// The `[llm]` section of `~/.config/meersh/config.toml`. See `config.example.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Name of the backend used when a command gets no `--backend`.
    pub default: String,
    /// Backend for `?`, `ai` and `llm` without input. They never see your data, only the
    /// request, the working directory and types, so a fast cloud model is safe here.
    pub assistant: Option<String>,
    pub cache: bool,
    /// Upper bound for the serialized pipeline input in one request.
    pub max_input_chars: usize,
    /// Rows per request for `llm map`, `llm where` and `llm parse`.
    pub batch_size: usize,
    pub backends: BTreeMap<String, BackendConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackendConfig {
    pub kind: BackendKind,
    #[serde(default)]
    pub url: Option<String>,
    /// For Ollama, no model means the first installed one.
    #[serde(default)]
    pub model: Option<String>,
    /// Name of the environment variable that holds an API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Ollama only. `false` switches off the thinking phase of reasoning models.
    #[serde(default)]
    pub think: Option<bool>,
    /// Ollama only. Its default context window silently truncates long prompts.
    #[serde(default)]
    pub num_ctx: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// The model for `llm embed` and `llm search`, when it differs from the chat model.
    #[serde(default)]
    pub embed_model: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Ollama,
    Openai,
    ClaudeCode,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Ollama => "ollama",
            BackendKind::Openai => "openai",
            BackendKind::ClaudeCode => "claude-code",
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        let local = BackendConfig {
            kind: BackendKind::Ollama,
            url: Some("http://localhost:11434".into()),
            model: None,
            api_key_env: None,
            think: Some(false),
            num_ctx: Some(16_384),
            timeout_secs: None,
            embed_model: None,
        };
        let claude = BackendConfig {
            kind: BackendKind::ClaudeCode,
            url: None,
            model: None,
            api_key_env: None,
            think: None,
            num_ctx: None,
            timeout_secs: None,
            embed_model: None,
        };
        LlmConfig {
            default: "local".into(),
            assistant: None,
            cache: true,
            max_input_chars: 40_000,
            batch_size: 25,
            backends: BTreeMap::from([("local".into(), local), ("claude".into(), claude)]),
        }
    }
}

impl LlmConfig {
    /// Reads the `[llm]` table of a meersh config file. A missing file gives the defaults.
    pub fn load(path: &Path) -> Result<LlmConfig, String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(LlmConfig::default());
        };
        #[derive(Deserialize, Default)]
        struct File {
            #[serde(default)]
            llm: Option<LlmConfig>,
        }
        let file: File = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(file.llm.unwrap_or_default())
    }
}
