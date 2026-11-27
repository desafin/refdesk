//! The HTTP plumbing both backends share: one request whose answer is read line by line, with
//! cancellation and the mapping of failures to [`TranslateError`].
//!
//! `ureq` blocks while it waits for the server, and a server that is loading a model sends
//! nothing for many seconds. To give up quickly on cancel, the request runs on a helper thread
//! that hands each line over a channel; the calling thread waits on the channel in short steps
//! and checks `cancel` between them. A cancelled call returns at once; the helper stops at the
//! next line it reads (dropping the connection, which also tells the server to stop).

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::TranslateError;

/// Connecting to a local server takes milliseconds; a server that does not answer within this
/// is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the calling thread looks at `cancel` while it waits for the next line.
const CANCEL_POLL: Duration = Duration::from_millis(20);
/// The most bytes of an error answer that are read.
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// The most bytes of a whole answer (a model list) that are read.
const MAX_LIST_BODY: u64 = 16 * 1024 * 1024;

/// What a line handler wants next.
pub(crate) enum Flow {
    /// Keep reading.
    More,
    /// The answer is complete.
    Done,
}

/// How a non-success status is reported.
#[derive(Clone, Copy)]
pub(crate) enum On404 {
    /// 404, or an error text with "not found", means the model is missing.
    ModelMissing,
    /// Every status is an `Http` error (listing models: a 404 means a wrong address or API).
    Http,
}

/// One request.
pub(crate) struct Call<'a> {
    /// `None` for GET.
    pub body: Option<serde_json::Value>,
    /// The path after the base address, starting with `/`.
    pub path: &'a str,
    /// Limit for the whole exchange, body included.
    pub timeout: Duration,
    /// How 404 is reported.
    pub on_404: On404,
    /// Hand the whole answer to the handler as one piece instead of line by line (a JSON
    /// document that may be pretty-printed over several lines).
    pub whole: bool,
}

/// A server address and the HTTP client to reach it.
#[derive(Clone)]
pub(crate) struct Client {
    agent: ureq::Agent,
    base: String,
    model: String,
    bearer_token: Option<String>,
}

impl Client {
    /// `base` is an `http://` or `https://` address without a trailing `/`; `model` names the
    /// model in `ModelMissing` errors.
    pub fn new(base: String, model: String, bearer_token: Option<String>) -> Client {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            // Contact the configured endpoint directly; do not send document text through an
            // environment-configured proxy.
            .proxy(None)
            .build();
        Client {
            agent: ureq::Agent::new_with_config(config),
            base,
            model,
            bearer_token,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Sends `call` and feeds every non-empty line of the answer to `on_line` until it says
    /// [`Flow::Done`]. An answer that ends before that is a `Protocol` error.
    pub fn run(
        &self,
        call: Call<'_>,
        cancel: &AtomicBool,
        on_line: &mut dyn FnMut(&str) -> Result<Flow, TranslateError>,
    ) -> Result<(), TranslateError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(TranslateError::Cancelled);
        }
        let (tx, rx) = mpsc::channel::<Event>();
        let stop = Arc::new(AtomicBool::new(false));
        let helper_stop = Arc::clone(&stop);
        let client = self.clone();
        let url = format!("{}{}", self.base, call.path);
        let (body, timeout, on_404, whole) = (call.body, call.timeout, call.on_404, call.whole);
        let spawned = std::thread::Builder::new()
            .name("refdesk-translate-http".to_string())
            .spawn(move || {
                let end = client.exchange(&url, body, timeout, (on_404, whole), &helper_stop, &tx);
                let _ = tx.send(Event::End(end));
            });
        if let Err(e) = spawned {
            return Err(TranslateError::Protocol(format!(
                "cannot start a thread: {e}"
            )));
        }
        let result = loop {
            if cancel.load(Ordering::Relaxed) {
                break Err(TranslateError::Cancelled);
            }
            match rx.recv_timeout(CANCEL_POLL) {
                Ok(Event::Line(line)) => match on_line(&line) {
                    Ok(Flow::More) => {}
                    Ok(Flow::Done) => break Ok(()),
                    Err(e) => break Err(e),
                },
                Ok(Event::End(Ok(()))) => {
                    break Err(TranslateError::Protocol(
                        "the answer ended before it was complete".to_string(),
                    ));
                }
                Ok(Event::End(Err(e))) => break Err(e),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(TranslateError::Protocol(
                        "the request thread ended".to_string(),
                    ));
                }
            }
        };
        stop.store(true, Ordering::Relaxed);
        result
    }

    /// The helper thread's part: send the request and pass the lines on.
    fn exchange(
        &self,
        url: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
        (on_404, whole): (On404, bool),
        stop: &AtomicBool,
        tx: &mpsc::Sender<Event>,
    ) -> Result<(), TranslateError> {
        let sent = match body {
            Some(body) => {
                let bytes = serde_json::to_vec(&body)
                    .map_err(|e| TranslateError::Protocol(e.to_string()))?;
                let request = self
                    .agent
                    .post(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .header("content-type", "application/json");
                match &self.bearer_token {
                    Some(token) => request
                        .header("authorization", format!("Bearer {token}"))
                        .send(&bytes[..]),
                    None => request.send(&bytes[..]),
                }
            }
            None => {
                let request = self
                    .agent
                    .get(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build();
                match &self.bearer_token {
                    Some(token) => request
                        .header("authorization", format!("Bearer {token}"))
                        .call(),
                    None => request.call(),
                }
            }
        };
        let response = sent.map_err(|e| self.request_error(&e))?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        if !(200..300).contains(&status) {
            let text = body
                .with_config()
                .limit(MAX_ERROR_BODY)
                .lossy_utf8(true)
                .read_to_string()
                .unwrap_or_default();
            return Err(self.status_error(status, &text, on_404));
        }

        if whole {
            let text = body
                .with_config()
                .limit(MAX_LIST_BODY)
                .read_to_string()
                .map_err(|e| TranslateError::Protocol(format!("connection lost: {e}")))?;
            let _ = tx.send(Event::Line(text));
            return Ok(());
        }
        let mut reader = BufReader::new(body.into_reader());
        let mut line = String::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return Ok(()),
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() && tx.send(Event::Line(trimmed.to_string())).is_err() {
                        // The caller has given up.
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(TranslateError::Protocol(
                        "the answer is not UTF-8 text".to_string(),
                    ));
                }
                Err(e) => {
                    let e = ureq::Error::from(e);
                    return Err(match e {
                        ureq::Error::Timeout(_) => TranslateError::Protocol(format!(
                            "no complete answer within {} s",
                            timeout.as_secs()
                        )),
                        other => TranslateError::Protocol(format!("connection lost: {other}")),
                    });
                }
            }
        }
    }

    /// A failure before any answer came.
    fn request_error(&self, e: &ureq::Error) -> TranslateError {
        match e {
            ureq::Error::Timeout(ureq::Timeout::Connect | ureq::Timeout::Resolve) => {
                TranslateError::Unreachable(format!("{}: no answer to connect", self.base))
            }
            ureq::Error::Timeout(_) => {
                TranslateError::Protocol("the server did not answer in time".to_string())
            }
            // Something accepted the connection and then hung up: a server is there, but it
            // did not answer as the API says.
            ureq::Error::Io(io)
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                TranslateError::Protocol(format!("the server closed the connection: {io}"))
            }
            other => TranslateError::Unreachable(format!("{}: {other}", self.base)),
        }
    }

    /// A non-success status, with the error text of the body when there is one.
    fn status_error(&self, status: u16, body: &str, on_404: On404) -> TranslateError {
        let message =
            error_message(body).unwrap_or_else(|| body.trim().chars().take(300).collect());
        let missing = status == 404 || message.to_ascii_lowercase().contains("not found");
        match on_404 {
            On404::ModelMissing if missing => TranslateError::ModelMissing(self.model.clone()),
            _ => TranslateError::Http { status, message },
        }
    }
}

/// What the helper thread reports.
enum Event {
    Line(String),
    End(Result<(), TranslateError>),
}

/// The error text of a JSON answer: `{"error": "..."}` (Ollama) or
/// `{"error": {"message": "..."}}` (OpenAI).
pub(crate) fn error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    error_of(&value)
}

/// The error text in a parsed JSON value, in either of the two shapes.
pub(crate) fn error_of(value: &serde_json::Value) -> Option<String> {
    match value.get("error")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => Some(
            o.get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        ),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// An error line in the middle of a stream: "not found" means the model is missing.
pub(crate) fn stream_error(message: String, model: &str) -> TranslateError {
    if message.to_ascii_lowercase().contains("not found") {
        TranslateError::ModelMissing(model.to_string())
    } else {
        TranslateError::Http {
            status: 200,
            message,
        }
    }
}

/// Parses one line of JSON.
pub(crate) fn parse_line(line: &str) -> Result<serde_json::Value, TranslateError> {
    serde_json::from_str(line).map_err(|_| {
        let shown: String = line.chars().take(120).collect();
        TranslateError::Protocol(format!("not a JSON line: {shown}"))
    })
}
