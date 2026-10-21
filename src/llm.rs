//! Talking to a local language-model server, for file name suggestions.
//!
//! Three kinds of server are supported, each through its own API so we get
//! what it can offer:
//!
//! | Server | List models | Loaded state | Load on demand | Chat |
//! |---|---|---|---|---|
//! | Ollama | `/api/tags` | `/api/ps` | `/api/generate` with no prompt | `/api/chat` |
//! | LM Studio | `/api/v1/models` (→ `/api/v0`, `/v1`) | `loaded_instances` | `/api/v1/models/load` | `/v1/chat/completions` |
//! | OpenAI-compatible | `/v1/models` | — | — (server's own JIT, if any) | `/v1/chat/completions` |
//!
//! Replies are requested as JSON matching a schema (Ollama's `format`, the
//! OpenAI-style `response_format`); servers or models that reject that get
//! a plain request instead, and the reply is parsed leniently either way.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::config::LlmApi;

/// One model the server offers.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// What requests name it by.
    pub id: String,
    /// Human-friendly name, when the server has one.
    pub name: Option<String>,
    /// `None` when the server can't tell.
    pub loaded: Option<bool>,
    /// Accepts images. `None` when the server can't tell.
    pub vision: Option<bool>,
    pub size_bytes: Option<u64>,
    /// Model "thinks" before answering (needs a bigger token budget).
    pub reasoning: bool,
}

impl ModelInfo {
    /// "● Llama 3.2 Vision 11B · vision · 7.9 GB": loaded state first and size last,
    /// because the dropdown is narrow and cuts the end off.
    pub fn label(&self) -> String {
        let dot = match self.loaded {
            Some(true) => "● ",
            Some(false) => "○ ",
            None => "",
        };
        let mut parts = vec![format!("{dot}{}", self.name.clone().unwrap_or_else(|| self.id.clone()))];
        if self.vision == Some(true) {
            parts.push("vision".into());
        }
        if let Some(b) = self.size_bytes.filter(|b| *b > 0) {
            parts.push(format!("{:.1} GB", b as f64 / 1e9));
        }
        parts.join(" · ")
    }
}

pub struct Client {
    api: LlmApi,
    base: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(api: LlmApi, endpoint: &str) -> Client {
        // Loading a model or a first answer from a big one can take minutes;
        // a dead server fails at connect time regardless.
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .into();
        let base = endpoint.trim().trim_end_matches('/').trim_end_matches("/v1").to_string();
        Client { api, base, agent }
    }

    fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut resp = self.agent.get(&url).call().with_context(|| format!("can't reach {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("{url} answered {status}");
        }
        Ok(resp.body_mut().read_json()?)
    }

    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        let url = format!("{}{path}", self.base);
        let mut resp = self.agent.post(&url).send_json(body).with_context(|| format!("can't reach {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string()?;
        let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, value))
    }

    // ─── Models ───────────────────────────────────────────────────────────

    /// The chat-capable models the server offers, loaded ones first.
    pub fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let mut models = match self.api {
            LlmApi::Ollama => self.list_ollama()?,
            LlmApi::LmStudio => self.list_lmstudio()?,
            LlmApi::OpenAi => parse_openai_models(&self.get("/v1/models")?),
        };
        models.sort_by_key(|m| (m.loaded != Some(true), m.name.clone().unwrap_or_else(|| m.id.clone()).to_lowercase()));
        Ok(models)
    }

    fn list_ollama(&self) -> Result<Vec<ModelInfo>> {
        let tags = self.get("/api/tags")?;
        let running = self.get("/api/ps").ok();
        Ok(parse_ollama_models(&tags, running.as_ref()))
    }

    fn list_lmstudio(&self) -> Result<Vec<ModelInfo>> {
        // Newest API first; older LM Studio versions only have v0 (and
        // every version has the OpenAI-style list, without states).
        if let Ok(v) = self.get("/api/v1/models") {
            return Ok(parse_lmstudio_v1(&v));
        }
        if let Ok(v) = self.get("/api/v0/models") {
            return Ok(parse_lmstudio_v0(&v));
        }
        Ok(parse_openai_models(&self.get("/v1/models")?))
    }

    /// The server answers LM Studio's own API (for suggesting the LM Studio
    /// server type to someone who set it up as plain OpenAI-compatible).
    pub fn looks_like_lmstudio(&self) -> bool {
        self.get("/api/v1/models").is_ok_and(|v| v["models"].is_array())
            || self.get("/api/v0/models").is_ok_and(|v| v["data"][0]["state"].is_string())
    }

    /// Make sure `model` is loaded, loading it if the server supports that.
    /// Returns the seconds spent loading, `0.0` if it was already loaded.
    pub fn ensure_loaded(&self, model: &str) -> Result<f32> {
        let models = self.list_models().unwrap_or_default();
        let known = find_model(&models, model);
        if known.is_some_and(|m| m.loaded == Some(true)) {
            return Ok(0.0);
        }
        // Load APIs want the exact id.
        let model = known.map(|m| m.id.as_str()).unwrap_or(model);
        let started = std::time::Instant::now();
        match self.api {
            LlmApi::Ollama => {
                // An empty prompt loads the model and returns.
                let (status, v) = self.post("/api/generate", &json!({ "model": model, "keep_alive": "15m" }))?;
                if status >= 400 {
                    bail!("Ollama couldn't load {model}: {}", error_text(&v));
                }
            }
            LlmApi::LmStudio => {
                let (status, v) = self.post("/api/v1/models/load", &json!({ "model": model }))?;
                if status >= 400 {
                    bail!("LM Studio couldn't load {model}: {}", error_text(&v));
                }
            }
            // No portable load API; servers like this load on first use, if at all.
            LlmApi::OpenAi => return Ok(0.0),
        }
        Ok(started.elapsed().as_secs_f32())
    }

    // ─── Chat ─────────────────────────────────────────────────────────────

    /// Ask for a JSON reply matching `schema`. `image` is a JPEG to show
    /// the model alongside the text. Returns the raw reply text.
    pub fn chat_json(
        &self,
        model: &str,
        system: &str,
        user: &str,
        image: Option<&[u8]>,
        schema: &Value,
        reasoning: bool,
    ) -> Result<String> {
        let image_b64 = image.map(|b| base64::engine::general_purpose::STANDARD.encode(b));
        // Thinking models spend tokens before answering; give them room.
        let max_tokens = if reasoning { 4096 } else { 400 };
        match self.api {
            LlmApi::Ollama => {
                let mut msg = json!({ "role": "user", "content": user });
                if let Some(b) = &image_b64 {
                    msg["images"] = json!([b]);
                }
                let mut body = json!({
                    "model": model,
                    "stream": false,
                    "keep_alive": "15m",
                    "options": { "temperature": 0, "num_predict": max_tokens },
                    "messages": [{ "role": "system", "content": system }, msg],
                    "format": schema,
                });
                let (mut status, mut v) = self.post("/api/chat", &body)?;
                if status >= 400 && body.get("format").is_some() {
                    body.as_object_mut().unwrap().remove("format");
                    (status, v) = self.post("/api/chat", &body)?;
                }
                if status >= 400 {
                    bail!("{}", error_text(&v));
                }
                v["message"]["content"].as_str().map(String::from).context("no message in the reply")
            }
            LlmApi::LmStudio | LlmApi::OpenAi => {
                let content = match &image_b64 {
                    Some(b) => json!([
                        { "type": "text", "text": user },
                        { "type": "image_url", "image_url": { "url": format!("data:image/jpeg;base64,{b}") } },
                    ]),
                    None => json!(user),
                };
                let mut body = json!({
                    "model": model,
                    "temperature": 0,
                    "max_tokens": max_tokens,
                    "messages": [{ "role": "system", "content": system }, { "role": "user", "content": content }],
                    "response_format": {
                        "type": "json_schema",
                        "json_schema": { "name": "file_name", "strict": true, "schema": schema },
                    },
                });
                let (mut status, mut v) = self.post("/v1/chat/completions", &body)?;
                // Some servers/models reject structured output; ask plainly.
                if status >= 400 {
                    body.as_object_mut().unwrap().remove("response_format");
                    (status, v) = self.post("/v1/chat/completions", &body)?;
                }
                if status >= 400 {
                    bail!("{}", error_text(&v));
                }
                v["choices"][0]["message"]["content"].as_str().map(String::from).context("no message in the reply")
            }
        }
    }
}

fn error_text(v: &Value) -> String {
    v["error"]["message"]
        .as_str()
        .or_else(|| v["error"].as_str())
        .or_else(|| v.as_str())
        .map(|s| s.chars().take(300).collect())
        .unwrap_or_else(|| v.to_string().chars().take(300).collect())
}

/// Drop a reasoning model's `<think>…</think>` preamble.
pub fn strip_thinking(reply: &str) -> &str {
    match reply.rfind("</think>") {
        Some(i) => &reply[i + "</think>".len()..],
        None => reply,
    }
}

fn is_chat_model(id: &str, kind: Option<&str>) -> bool {
    let id = id.to_lowercase();
    !matches!(kind, Some("embedding" | "embeddings" | "decision")) && !id.contains("embed") && !id.contains("rerank")
}

fn parse_openai_models(v: &Value) -> Vec<ModelInfo> {
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .filter(|id| is_chat_model(id, None))
        .map(|id| ModelInfo {
            id: id.to_string(),
            name: None,
            loaded: None,
            vision: None,
            size_bytes: None,
            reasoning: false,
        })
        .collect()
}

fn parse_ollama_models(tags: &Value, running: Option<&Value>) -> Vec<ModelInfo> {
    let running: Vec<&str> =
        running.and_then(|r| r["models"].as_array()).into_iter().flatten().filter_map(|m| m["name"].as_str()).collect();
    tags["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["name"].as_str()?;
            is_chat_model(id, None).then(|| ModelInfo {
                id: id.to_string(),
                name: None,
                loaded: Some(running.contains(&id)),
                // /api/tags doesn't say; "llava", "-vl", "vision" in the
                // family/name are the common tells.
                vision: {
                    let fams = m["details"]["families"].to_string().to_lowercase();
                    let id_l = id.to_lowercase();
                    (fams.contains("clip") || id_l.contains("llava") || id_l.contains("-vl") || id_l.contains("vision"))
                        .then_some(true)
                },
                size_bytes: m["size"].as_u64(),
                reasoning: false,
            })
        })
        .collect()
}

fn parse_lmstudio_v1(v: &Value) -> Vec<ModelInfo> {
    v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["key"].as_str()?;
            is_chat_model(id, m["type"].as_str()).then(|| ModelInfo {
                id: id.to_string(),
                name: m["display_name"].as_str().map(String::from),
                loaded: Some(m["loaded_instances"].as_array().is_some_and(|a| !a.is_empty())),
                vision: m["capabilities"]["vision"].as_bool(),
                size_bytes: m["size_bytes"].as_u64(),
                reasoning: !m["capabilities"]["reasoning"].is_null() || id.contains("thinking"),
            })
        })
        .collect()
}

fn parse_lmstudio_v0(v: &Value) -> Vec<ModelInfo> {
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let kind = m["type"].as_str();
            is_chat_model(id, kind).then(|| ModelInfo {
                id: id.to_string(),
                name: None,
                loaded: m["state"].as_str().map(|s| s == "loaded"),
                vision: kind.map(|k| k == "vlm"),
                size_bytes: None,
                reasoning: id.contains("thinking"),
            })
        })
        .collect()
}

/// Find `id` in a model list, accepting the short names servers also take:
/// "tiny-4b" for LM Studio's "acme/tiny-4b", "llama3.2" for
/// Ollama's "llama3.2:latest".
pub fn find_model<'a>(models: &'a [ModelInfo], id: &str) -> Option<&'a ModelInfo> {
    let id = id.trim();
    let lower = id.to_lowercase();
    models
        .iter()
        .find(|m| m.id == id)
        .or_else(|| models.iter().find(|m| m.id.to_lowercase() == lower))
        .or_else(|| models.iter().find(|m| m.id.to_lowercase().ends_with(&format!("/{lower}"))))
        .or_else(|| models.iter().find(|m| m.id.to_lowercase() == format!("{lower}:latest")))
}

/// The server type's usual address, for filling the field in when the
/// user switches type.
pub fn default_endpoint(api: LlmApi) -> &'static str {
    match api {
        LlmApi::Ollama => "http://localhost:11434",
        LlmApi::LmStudio => "http://localhost:1234",
        LlmApi::OpenAi => "http://localhost:8080",
    }
}

/// Encode a page as a modest JPEG for a vision model: big enough to read a
/// letterhead, small enough not to swamp the model's context.
pub fn page_jpeg(image: &image::DynamicImage) -> Result<Vec<u8>> {
    let small = image.thumbnail(1280, 1280);
    crate::pipeline::encode_jpeg(&small, 80).map_err(|e| anyhow!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lmstudio_v1_list() {
        let v = json!({ "models": [
            { "type": "llm", "key": "acme/tiny-4b", "display_name": "Tiny 4B", "size_bytes": 2_500_000_000u64,
              "loaded_instances": [{ "id": "acme/tiny-4b", "config": {} }], "capabilities": { "vision": true } },
            { "type": "llm", "key": "acme/big-thinking", "display_name": "Big", "loaded_instances": [],
              "capabilities": { "vision": false, "reasoning": { "default": "on" } } },
            { "type": "embedding", "key": "nomic-embed", "loaded_instances": [] },
        ]});
        let m = parse_lmstudio_v1(&v);
        assert_eq!(m.len(), 2, "embedding models are dropped");
        assert_eq!(m[0].loaded, Some(true));
        assert_eq!(m[0].vision, Some(true));
        assert!(m[1].reasoning);
        assert_eq!(m[0].label(), "● Tiny 4B · vision · 2.5 GB");
    }

    #[test]
    fn parses_lmstudio_v0_and_openai_lists() {
        let v0 = json!({ "data": [
            { "id": "a", "type": "vlm", "state": "loaded" },
            { "id": "b", "type": "llm", "state": "not-loaded" },
            { "id": "text-embedding-x", "type": "embeddings", "state": "not-loaded" },
        ]});
        let m = parse_lmstudio_v0(&v0);
        assert_eq!(
            m.iter().map(|m| (m.id.as_str(), m.loaded, m.vision)).collect::<Vec<_>>(),
            vec![("a", Some(true), Some(true)), ("b", Some(false), Some(false)),]
        );
        let oa = json!({ "data": [{ "id": "x" }, { "id": "text-embedding-y" }] });
        assert_eq!(parse_openai_models(&oa).len(), 1);
    }

    #[test]
    fn parses_ollama_list_with_running_models() {
        let tags = json!({ "models": [
            { "name": "llama3.2:latest", "size": 2_000_000_000u64, "details": { "families": ["llama"] } },
            { "name": "llava:7b", "size": 4_700_000_000u64, "details": { "families": ["llama", "clip"] } },
            { "name": "nomic-embed-text:latest", "size": 1 },
        ]});
        let ps = json!({ "models": [{ "name": "llava:7b" }] });
        let m = parse_ollama_models(&tags, Some(&ps));
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].loaded, Some(false));
        assert_eq!(m[1].loaded, Some(true));
        assert_eq!(m[1].vision, Some(true));
    }

    #[test]
    fn finds_models_by_short_name() {
        let m = |id: &str| ModelInfo {
            id: id.into(),
            name: None,
            loaded: None,
            vision: None,
            size_bytes: None,
            reasoning: false,
        };
        let list = vec![m("acme/tiny-4b"), m("llama3.2:latest"), m("Other")];
        assert_eq!(find_model(&list, "acme/tiny-4b").unwrap().id, "acme/tiny-4b");
        assert_eq!(find_model(&list, "tiny-4b").unwrap().id, "acme/tiny-4b");
        assert_eq!(find_model(&list, "llama3.2").unwrap().id, "llama3.2:latest");
        assert_eq!(find_model(&list, "other").unwrap().id, "Other");
        assert!(find_model(&list, "4b").is_none());
    }

    #[test]
    fn strips_thinking_preamble() {
        assert_eq!(strip_thinking("<think>hmm</think>\n{\"a\":1}"), "\n{\"a\":1}");
        assert_eq!(strip_thinking("{\"a\":1}"), "{\"a\":1}");
    }
}
