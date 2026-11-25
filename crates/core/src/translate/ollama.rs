//! Ollama's own API: `POST /api/chat` (streamed, one JSON object per line), `POST /api/generate`
//! with only `model` and `keep_alive` to load a model, `GET /api/tags` to list models.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::json;

use super::http::{Call, Client, Flow, On404, error_of, parse_line, stream_error};
use super::prompt::{Prompts, max_tokens};
use super::{TranslateBackend, TranslateError, TranslateRequest, identity_of};
use crate::store::TranslateConfig;

/// Limit for a word answer, model loading included.
const WORD_TIMEOUT: Duration = Duration::from_secs(60);
/// Limit for a passage answer (the first piece may take 20 s while the model loads).
const PASSAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Limit for loading a model.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(120);
/// Limit for listing models.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// A backend that talks to Ollama.
pub struct OllamaBackend {
    client: Client,
    prompts: Prompts,
    keep_alive: String,
}

impl OllamaBackend {
    /// A backend for `config` (`endpoint`, `model`, `target`, `keep_alive_minutes`, prompts).
    /// Nothing is contacted until a call.
    pub fn new(config: &TranslateConfig) -> OllamaBackend {
        OllamaBackend {
            client: Client::new(config.endpoint(), config.model.clone()),
            prompts: Prompts::from_config(config),
            keep_alive: format!("{}m", config.keep_alive_minutes),
        }
    }

    /// The server address.
    pub fn endpoint(&self) -> &str {
        self.client.base()
    }

    fn model(&self) -> &str {
        &self.prompts.model
    }
}

impl TranslateBackend for OllamaBackend {
    fn translate(
        &self,
        req: &TranslateRequest,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        let body = json!({
            "model": self.model(),
            "messages": [{"role": "user", "content": self.prompts.message(req)}],
            "stream": true,
            "keep_alive": self.keep_alive,
            "options": {"temperature": 0, "num_predict": max_tokens(req)},
        });
        let call = Call {
            body: Some(body),
            path: "/api/chat",
            timeout: if req.is_word() {
                WORD_TIMEOUT
            } else {
                PASSAGE_TIMEOUT
            },
            on_404: On404::ModelMissing,
            whole: false,
        };
        self.client.run(call, cancel, &mut |line| {
            let value = parse_line(line)?;
            if let Some(message) = error_of(&value) {
                return Err(stream_error(message, self.model()));
            }
            if let Some(piece) = value
                .pointer("/message/content")
                .and_then(serde_json::Value::as_str)
                .filter(|p| !p.is_empty())
            {
                on_text(piece);
            }
            Ok(done_flow(&value))
        })
    }

    fn warm_up(&self, cancel: &AtomicBool) -> Result<(), TranslateError> {
        let call = Call {
            body: Some(json!({"model": self.model(), "keep_alive": self.keep_alive})),
            path: "/api/generate",
            timeout: WARM_UP_TIMEOUT,
            on_404: On404::ModelMissing,
            whole: false,
        };
        self.client.run(call, cancel, &mut |line| {
            let value = parse_line(line)?;
            if let Some(message) = error_of(&value) {
                return Err(stream_error(message, self.model()));
            }
            Ok(done_flow(&value))
        })
    }

    fn models(&self, cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
        let call = Call {
            body: None,
            path: "/api/tags",
            timeout: LIST_TIMEOUT,
            on_404: On404::Http,
            whole: true,
        };
        let mut names = None;
        self.client.run(call, cancel, &mut |line| {
            let value = parse_line(line)?;
            let list = value
                .get("models")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| TranslateError::Protocol("no \"models\" list".to_string()))?;
            names = Some(
                list.iter()
                    .filter_map(|m| m.get("name").and_then(serde_json::Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            );
            Ok(Flow::Done)
        })?;
        Ok(names.unwrap_or_default())
    }

    fn identity(&self) -> String {
        identity_of(self.model(), &self.prompts.target)
    }
}

/// `"done": true` ends the answer.
fn done_flow(value: &serde_json::Value) -> Flow {
    if value.get("done").and_then(serde_json::Value::as_bool) == Some(true) {
        Flow::Done
    } else {
        Flow::More
    }
}
