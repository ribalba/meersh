//! Embeddings turn text into vectors, so rows can be compared by meaning with cosine
//! similarity. Ollama answers on `/api/embed` and every OpenAI compatible server on
//! `/embeddings`. Claude Code has no such endpoint.

use std::time::Duration;

use serde_json::{Value as Json, json};

use crate::backend::Response;
use crate::cache;
use crate::config::{BackendConfig, BackendKind};

/// Embeds every text and answers from the disk cache for texts seen before. Only the
/// misses go to the backend, all in one request. Embeddings depend on nothing but the
/// model and the text, so a cached vector is always the right one.
pub fn embed(config: &BackendConfig, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    supported(config)?;
    let keys: Vec<String> = texts.iter().map(|text| key(config, text)).collect();
    let mut vectors: Vec<Option<Vec<f32>>> =
        keys.iter().map(|key| cache::get(key).and_then(|hit| vector(hit.json.as_ref()?))).collect();
    let missing: Vec<usize> = (0..texts.len()).filter(|i| vectors[*i].is_none()).collect();
    if !missing.is_empty() {
        let request: Vec<String> = missing.iter().map(|i| texts[*i].clone()).collect();
        let (fresh, model) = request_vectors(config, &request)?;
        for (i, v) in missing.into_iter().zip(fresh) {
            cache::put(&keys[i], &Response { text: String::new(), json: Some(json!(v)), model: model.clone() });
            vectors[i] = Some(v);
        }
    }
    Ok(vectors.into_iter().map(Option::unwrap_or_default).collect())
}

/// Embeds every text without touching the cache, for `cache = false` in the config.
pub fn fetch(config: &BackendConfig, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    supported(config)?;
    Ok(request_vectors(config, texts)?.0)
}

const NO_ENDPOINT: &str = "Claude Code has no embedding endpoint. Use an Ollama or OpenAI compatible backend for \
embeddings, for example --backend local";

fn supported(config: &BackendConfig) -> Result<(), String> {
    match config.kind {
        BackendKind::ClaudeCode => Err(NO_ENDPOINT.into()),
        BackendKind::Ollama | BackendKind::Openai => Ok(()),
    }
}

/// Like the key of a completion it covers backend, model and input. The marker keeps an
/// embedding apart from a chat answer.
fn key(config: &BackendConfig, text: &str) -> String {
    cache::key(&[
        "embed",
        config.kind.as_str(),
        config.url.as_deref().unwrap_or_default(),
        config.model.as_deref().unwrap_or("default"),
        text,
    ])
}

/// One request for all texts. Returns the vectors in the order of the texts and the model
/// that made them.
fn request_vectors(config: &BackendConfig, texts: &[String]) -> Result<(Vec<Vec<f32>>, String), String> {
    if texts.is_empty() {
        return Ok((vec![], config.model.clone().unwrap_or_default()));
    }
    let (vectors, model) = match config.kind {
        BackendKind::Ollama => ollama(config, texts)?,
        BackendKind::Openai => openai(config, texts)?,
        BackendKind::ClaudeCode => return Err(NO_ENDPOINT.into()),
    };
    if vectors.len() != texts.len() {
        return Err(format!("the backend returned {} embeddings for {} texts", vectors.len(), texts.len()));
    }
    Ok((vectors, model))
}

fn ollama(config: &BackendConfig, texts: &[String]) -> Result<(Vec<Vec<f32>>, String), String> {
    let url = config.url.as_deref().unwrap_or("http://localhost:11434");
    let model = match &config.model {
        Some(model) => model.clone(),
        None => embedding_model(config, url)?,
    };
    let answer = post(config, &format!("{url}/api/embed"), &json!({"model": model, "input": texts})).map_err(|e| {
        if e.contains("does not support embeddings") {
            format!(
                "{e}\nThe model {model} cannot embed. Pass --model with an embedding model such as nomic-embed-text."
            )
        } else {
            e
        }
    })?;
    let vectors = answer["embeddings"]
        .as_array()
        .ok_or_else(|| format!("{url}/api/embed returned no embeddings"))?
        .iter()
        .map(|v| {
            vector(v).ok_or_else(|| format!("{url}/api/embed returned an embedding that is not a list of numbers"))
        })
        .collect::<Result<_, _>>()?;
    Ok((vectors, model))
}

/// Without a configured model, the first installed model that Ollama lists as able to embed.
/// The first model overall is usually a chat model, which cannot.
fn embedding_model(config: &BackendConfig, url: &str) -> Result<String, String> {
    let mut response = agent(config).get(&format!("{url}/api/tags")).call().map_err(|e| format!("{url}: {e}"))?;
    let tags: Json = response.body_mut().read_json().map_err(|e| e.to_string())?;
    tags["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["capabilities"].as_array().is_some_and(|c| c.iter().any(|c| c == "embedding")))
        .and_then(|m| m["name"].as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "no embedding model installed in Ollama at {url}. Run `ollama pull nomic-embed-text` or pass --model."
            )
        })
}

fn openai(config: &BackendConfig, texts: &[String]) -> Result<(Vec<Vec<f32>>, String), String> {
    let url = config.url.as_deref().unwrap_or("http://localhost:8080/v1");
    let model = config.model.clone().unwrap_or_else(|| "default".into());
    let answer = post(config, &format!("{url}/embeddings"), &json!({"model": model, "input": texts}))?;
    let mut data: Vec<&Json> =
        answer["data"].as_array().ok_or_else(|| format!("{url}/embeddings returned no data"))?.iter().collect();
    // The order of `data` is not guaranteed. Every item carries the index of its input.
    data.sort_by_key(|item| item["index"].as_u64().unwrap_or(0));
    let vectors = data
        .into_iter()
        .map(|item| {
            vector(&item["embedding"])
                .ok_or_else(|| format!("{url}/embeddings returned an embedding that is not a list of numbers"))
        })
        .collect::<Result<_, _>>()?;
    Ok((vectors, model))
}

fn vector(json: &Json) -> Option<Vec<f32>> {
    json.as_array()?.iter().map(|n| n.as_f64().map(|f| f as f32)).collect()
}

fn agent(config: &BackendConfig) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(config.timeout_secs.unwrap_or(600))))
        .http_status_as_error(false)
        .build()
        .into()
}

fn post(config: &BackendConfig, url: &str, body: &Json) -> Result<Json, String> {
    let mut request = agent(config).post(url);
    if let Some(var) = &config.api_key_env {
        let key = std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))?;
        request = request.header("Authorization", &format!("Bearer {key}"));
    }
    let mut response = request.send_json(body).map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("{url} answered {status}: {detail}"));
    }
    response.body_mut().read_json().map_err(|e| format!("{url}: invalid response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(kind: BackendKind) -> BackendConfig {
        BackendConfig {
            kind,
            url: Some("http://127.0.0.1:9".into()),
            model: Some("m".into()),
            api_key_env: None,
            think: None,
            num_ctx: None,
            timeout_secs: Some(1),
            embed_model: None,
        }
    }

    #[test]
    fn claude_code_cannot_embed() {
        let err = fetch(&backend(BackendKind::ClaudeCode), &["a".into()]).unwrap_err();
        assert!(err.contains("no embedding endpoint"), "{err}");
    }

    #[test]
    fn no_texts_make_no_request() {
        // The url points at the discard port, so a request would fail.
        assert_eq!(fetch(&backend(BackendKind::Ollama), &[]), Ok(vec![]));
    }

    #[test]
    fn keys_depend_on_model_and_text() {
        let a = backend(BackendKind::Ollama);
        let b = BackendConfig { model: Some("other".into()), ..a.clone() };
        assert_ne!(key(&a, "x"), key(&b, "x"));
        assert_ne!(key(&a, "x"), key(&a, "y"));
        assert_eq!(key(&a, "x"), key(&a.clone(), "x"));
    }

    #[test]
    fn vectors_are_lists_of_numbers() {
        assert_eq!(vector(&json!([1, 0.5, -2])), Some(vec![1.0, 0.5, -2.0]));
        assert_eq!(vector(&json!([1, "x"])), None);
        assert_eq!(vector(&json!({"a": 1})), None);
    }
}
