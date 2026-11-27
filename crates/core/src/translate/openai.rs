//! An OpenAI compatible API: `POST /v1/chat/completions` streamed as server-sent events
//! (`data: {json}` lines, ending with `data: [DONE]`) and `GET /v1/models`. llama.cpp's
//! `llama-server`, LM Studio, vLLM and Ollama itself speak it.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::json;

use super::http::{Call, Client, Flow, On404, error_of, parse_line, stream_error};
use super::prompt::{Prompts, max_tokens};
use super::{TranslateBackend, TranslateError, TranslateRequest, identity_of};
use crate::store::TranslateConfig;

/// Limit for a word answer, model loading included.
const WORD_TIMEOUT: Duration = Duration::from_secs(60);
/// Limit for a passage answer.
const PASSAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Limit for listing models.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// A backend that talks to an OpenAI compatible server.
pub struct OpenAiBackend {
    client: Client,
    prompts: Prompts,
    api_key_error: Option<TranslateError>,
}

impl OpenAiBackend {
    /// A backend for `config` (`endpoint`, `model`, `target`, prompts). Nothing is contacted
    /// until a call.
    pub fn new(config: &TranslateConfig) -> OpenAiBackend {
        let (bearer_token, api_key_error) = match &config.api_key_env {
            Some(name) => match std::env::var(name) {
                Ok(token) if !token.trim().is_empty() => (Some(token.trim().to_string()), None),
                _ => (None, Some(TranslateError::MissingApiKey(name.clone()))),
            },
            None => (None, None),
        };
        OpenAiBackend {
            client: Client::new(config.endpoint(), config.model.clone(), bearer_token),
            prompts: Prompts::from_config(config),
            api_key_error,
        }
    }

    /// The server address.
    pub fn endpoint(&self) -> &str {
        self.client.base()
    }

    fn model(&self) -> &str {
        &self.prompts.model
    }

    fn check_api_key(&self) -> Result<(), TranslateError> {
        self.api_key_error.clone().map_or(Ok(()), Err)
    }

    /// One streamed chat completion of `message` with at most `tokens` tokens.
    fn complete(
        &self,
        message: &str,
        tokens: u32,
        timeout: Duration,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        self.check_api_key()?;
        let body = json!({
            "model": self.model(),
            "messages": [{"role": "user", "content": message}],
            "stream": true,
            "temperature": 0,
            "max_tokens": tokens,
        });
        let call = Call {
            body: Some(body),
            path: "/v1/chat/completions",
            timeout,
            on_404: On404::ModelMissing,
            whole: false,
        };
        self.client.run(call, cancel, &mut |line| {
            // Comments and other SSE fields (`event:`, `id:`) carry no text.
            let Some(data) = line.strip_prefix("data:") else {
                return Ok(Flow::More);
            };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(Flow::Done);
            }
            let value = parse_line(data)?;
            if let Some(message) = error_of(&value) {
                return Err(stream_error(message, self.model()));
            }
            if let Some(piece) = value
                .pointer("/choices/0/delta/content")
                .and_then(serde_json::Value::as_str)
                .filter(|p| !p.is_empty())
            {
                on_text(piece);
            }
            Ok(Flow::More)
        })
    }
}

impl TranslateBackend for OpenAiBackend {
    fn translate(
        &self,
        req: &TranslateRequest,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        let timeout = if req.is_word() {
            WORD_TIMEOUT
        } else {
            PASSAGE_TIMEOUT
        };
        self.complete(
            &self.prompts.message(req),
            max_tokens(req),
            timeout,
            cancel,
            on_text,
        )
    }

    /// Checks that the server is there and has the model, then asks for one token so that a
    /// server that loads models on demand loads it now.
    fn warm_up(&self, cancel: &AtomicBool) -> Result<(), TranslateError> {
        let models = self.models(cancel)?;
        if !models.iter().any(|m| m == self.model()) {
            return Err(TranslateError::ModelMissing(self.model().to_string()));
        }
        self.complete("Hi", 1, PASSAGE_TIMEOUT, cancel, &mut |_| {})
    }

    fn models(&self, cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
        self.check_api_key()?;
        let call = Call {
            body: None,
            path: "/v1/models",
            timeout: LIST_TIMEOUT,
            on_404: On404::Http,
            whole: true,
        };
        let mut names = None;
        self.client.run(call, cancel, &mut |line| {
            let value = parse_line(line)?;
            let list = value
                .get("data")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| TranslateError::Protocol("no \"data\" list".to_string()))?;
            names = Some(
                list.iter()
                    .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
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
