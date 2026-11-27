//! Translation (WP-80): backends against a fake HTTP server, prompts, the job, the cache.
//! Nothing here talks to a real Ollama; the fake server listens on `127.0.0.1:0`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::geom::{Point, Quad, Rect};
use refdesk_core::store::{TranslateBackendKind, TranslateConfig, atomic::bad_path};
use refdesk_core::text::{PageText, TextBlock, TextChar, TextLine};
use refdesk_core::translate::{
    MAX_CACHE_ENTRIES, MAX_PASSAGE_CHARS, Prompts, clean_output, fill_passage_template,
    fill_word_template, language_name, max_tokens, passage_prompt, prepare_passage,
    sentence_around, word_prompt,
};
use refdesk_core::{
    Notifier, OllamaBackend, OpenAiBackend, Paths, TranslateBackend, TranslateError, TranslateJob,
    TranslateKind, TranslateRequest, TranslateState, TranslationCache, backend_from_config,
};

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// The fake server
// ---------------------------------------------------------------------------------------------

/// One request the fake server received.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    body: serde_json::Value,
}

/// What the fake server answers to one connection.
enum Reply {
    /// The status, content type and body pieces, each sent as its own chunk and flushed.
    Chunks {
        status: u16,
        content_type: &'static str,
        parts: Vec<String>,
        /// Close the connection without the final empty chunk.
        cut: bool,
    },
    /// Send the pieces with status 200, then wait until the test drops the sender (or sends).
    Stall {
        parts: Vec<String>,
        release: mpsc::Receiver<()>,
    },
    /// Read the request and close the connection without answering.
    Close,
}

fn ok_ndjson(lines: &[&str]) -> Reply {
    Reply::Chunks {
        status: 200,
        content_type: "application/x-ndjson",
        parts: lines.iter().map(|l| format!("{l}\n")).collect(),
        cut: false,
    }
}

fn sse(events: &[&str]) -> Reply {
    Reply::Chunks {
        status: 200,
        content_type: "text/event-stream",
        parts: events.iter().map(|e| format!("{e}\n\n")).collect(),
        cut: false,
    }
}

fn json_reply(status: u16, body: &str) -> Reply {
    Reply::Chunks {
        status,
        content_type: "application/json",
        parts: vec![body.to_string()],
        cut: false,
    }
}

struct Server {
    addr: SocketAddr,
    log: Arc<(Mutex<Vec<Recorded>>, Condvar)>,
}

impl Server {
    /// Starts a server that answers the connections in order with `replies`.
    fn start(replies: Vec<Reply>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let log: Arc<(Mutex<Vec<Recorded>>, Condvar)> = Arc::default();
        let thread_log = Arc::clone(&log);
        std::thread::spawn(move || {
            let mut replies: VecDeque<Reply> = replies.into();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let Some(reply) = replies.pop_front() else {
                    return;
                };
                let log = Arc::clone(&thread_log);
                std::thread::spawn(move || serve(stream, reply, &log));
            }
        });
        Server { addr, log }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Waits until `n` requests have come and returns them.
    fn requests(&self, n: usize) -> Vec<Recorded> {
        let (lock, cv) = &*self.log;
        let guard = lock.lock().unwrap();
        let (guard, _) = cv.wait_timeout_while(guard, WAIT, |r| r.len() < n).unwrap();
        assert!(guard.len() >= n, "only {} requests came", guard.len());
        guard.clone()
    }
}

fn serve(stream: TcpStream, reply: Reply, log: &(Mutex<Vec<Recorded>>, Condvar)) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    let body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    {
        let (lock, cv) = log;
        lock.lock().unwrap().push(Recorded { method, path, body });
        cv.notify_all();
    }
    let mut out = stream;
    let head = |status: u16, ct: &str| {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
    };
    let chunk = |out: &mut TcpStream, part: &str| {
        let _ = write!(out, "{:x}\r\n{part}\r\n", part.len());
        let _ = out.flush();
    };
    match reply {
        Reply::Chunks {
            status,
            content_type,
            parts,
            cut,
        } => {
            let _ = out.write_all(head(status, content_type).as_bytes());
            for p in &parts {
                chunk(&mut out, p);
            }
            if !cut {
                let _ = out.write_all(b"0\r\n\r\n");
            }
            let _ = out.flush();
        }
        Reply::Stall { parts, release } => {
            let _ = out.write_all(head(200, "application/x-ndjson").as_bytes());
            for p in &parts {
                chunk(&mut out, p);
            }
            let _ = release.recv();
        }
        Reply::Close => {}
    }
}

/// An address where nothing listens.
///
/// Not a port that was bound and let go: the tests run side by side, and the fake server of
/// another test can be given that very port a moment later. The ports tried here are below the
/// range the system hands out, so no test's server can take them.
fn closed_endpoint() -> String {
    for port in [9u16, 7, 13, 19, 37, 79, 113] {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let refused = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
        if refused {
            return format!("http://{addr}");
        }
    }
    panic!("no closed port among the ones tried");
}

fn config(endpoint: &str, model: &str) -> TranslateConfig {
    TranslateConfig {
        endpoint: Some(endpoint.to_string()),
        model: model.to_string(),
        ..TranslateConfig::default()
    }
}

fn openai_config(endpoint: &str, model: &str) -> TranslateConfig {
    TranslateConfig {
        backend: TranslateBackendKind::OpenAi,
        ..config(endpoint, model)
    }
}

/// Runs `translate` and gives the pieces and the result.
fn run(
    backend: &dyn TranslateBackend,
    req: &TranslateRequest,
) -> (Vec<String>, Result<(), TranslateError>) {
    let cancel = AtomicBool::new(false);
    let mut pieces = Vec::new();
    let result = backend.translate(req, &cancel, &mut |p| pieces.push(p.to_string()));
    (pieces, result)
}

fn chat_line(content: &str) -> String {
    serde_json::json!({"model": "m", "message": {"role": "assistant", "content": content}, "done": false})
        .to_string()
}

const DONE_LINE: &str =
    r#"{"model":"m","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#;

// ---------------------------------------------------------------------------------------------
// Ollama backend
// ---------------------------------------------------------------------------------------------

#[test]
fn ollama_pieces_arrive_in_order() {
    let (a, b, c) = (
        chat_line("감시"),
        chat_line("자는"),
        chat_line(" 감독한다."),
    );
    let server = Server::start(vec![ok_ndjson(&[&a, &b, &c, DONE_LINE])]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "translategemma:4b"));
    let (pieces, result) = run(
        &backend,
        &TranslateRequest::passage("The watchdog supervises."),
    );
    assert_eq!(result, Ok(()));
    assert_eq!(pieces, ["감시", "자는", " 감독한다."]);
}

#[test]
fn ollama_sends_the_documented_request() {
    let server = Server::start(vec![ok_ndjson(&[DONE_LINE]), ok_ndjson(&[DONE_LINE])]);
    let cfg = TranslateConfig {
        keep_alive_minutes: 7,
        ..config(&format!("{}/", server.endpoint()), "translategemma:4b")
    };
    let backend = OllamaBackend::new(&cfg);
    let text = "The watchdog supervises the liveness of each process.";
    assert_eq!(run(&backend, &TranslateRequest::passage(text)).1, Ok(()));
    let word = TranslateRequest::word("liveness", text);
    assert_eq!(run(&backend, &word).1, Ok(()));

    let reqs = server.requests(2);
    let passage = &reqs[0];
    assert_eq!(passage.method, "POST");
    assert_eq!(passage.path, "/api/chat");
    let b = &passage.body;
    assert_eq!(b["model"], "translategemma:4b");
    assert_eq!(b["stream"], true);
    assert_eq!(b["keep_alive"], "7m");
    assert_eq!(b["options"]["temperature"], 0);
    let chars = text.chars().count() as u64;
    assert_eq!(b["options"]["num_predict"], chars / 2 + 64);
    let messages = b["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(
        messages[0]["content"],
        passage_prompt("translategemma:4b", "ko", text)
    );

    let w = &reqs[1].body;
    assert_eq!(reqs[1].path, "/api/chat");
    assert_eq!(w["options"]["num_predict"], 32);
    assert_eq!(
        w["messages"][0]["content"],
        word_prompt("ko", "liveness", text)
    );
}

#[test]
fn ollama_404_is_a_missing_model() {
    let server = Server::start(vec![json_reply(
        404,
        r#"{"error":"model 'nosuch:1b' not found"}"#,
    )]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "nosuch:1b"));
    let (_, result) = run(&backend, &TranslateRequest::passage("x"));
    assert_eq!(
        result,
        Err(TranslateError::ModelMissing("nosuch:1b".to_string()))
    );
}

#[test]
fn ollama_500_is_an_http_error_with_the_server_text() {
    let server = Server::start(vec![json_reply(500, r#"{"error":"out of memory"}"#)]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "m"));
    let (_, result) = run(&backend, &TranslateRequest::passage("x"));
    assert_eq!(
        result,
        Err(TranslateError::Http {
            status: 500,
            message: "out of memory".to_string()
        })
    );
}

#[test]
fn ollama_closed_port_is_unreachable_quickly() {
    let backend = OllamaBackend::new(&config(&closed_endpoint(), "m"));
    let started = Instant::now();
    let (_, result) = run(&backend, &TranslateRequest::passage("x"));
    assert!(
        matches!(result, Err(TranslateError::Unreachable(_))),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn ollama_broken_line_or_missing_done_is_a_protocol_error() {
    let a = chat_line("a");
    let server = Server::start(vec![
        ok_ndjson(&[&a, "this is not json"]),
        ok_ndjson(&[&a]),
        Reply::Chunks {
            status: 200,
            content_type: "application/x-ndjson",
            parts: vec![format!("{a}\n")],
            cut: true,
        },
        Reply::Close,
    ]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "m"));
    for case in ["broken line", "no done", "cut connection", "closed at once"] {
        let (_, result) = run(&backend, &TranslateRequest::passage("x"));
        assert!(
            matches!(result, Err(TranslateError::Protocol(_))),
            "{case}: {result:?}"
        );
    }
}

#[test]
fn ollama_error_line_in_the_stream_is_an_error() {
    let a = chat_line("a");
    let server = Server::start(vec![
        ok_ndjson(&[&a, r#"{"error":"the runner crashed"}"#]),
        ok_ndjson(&[r#"{"error":"model 'm' not found, try pulling it first"}"#]),
    ]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "m"));
    let (pieces, result) = run(&backend, &TranslateRequest::passage("x"));
    assert_eq!(pieces, ["a"]);
    assert!(
        matches!(&result, Err(TranslateError::Http { message, .. }) if message == "the runner crashed"),
        "{result:?}"
    );
    let (_, result) = run(&backend, &TranslateRequest::passage("x"));
    assert_eq!(result, Err(TranslateError::ModelMissing("m".to_string())));
}

/// Translates on a thread with a cancel flag; the first piece is reported on `first`.
fn cancel_while_stalled(backend: Arc<dyn TranslateBackend>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (first_tx, first_rx) = mpsc::channel::<String>();
    let thread_cancel = Arc::clone(&cancel);
    let worker = std::thread::spawn(move || {
        backend.translate(&TranslateRequest::passage("x"), &thread_cancel, &mut |p| {
            let _ = first_tx.send(p.to_string());
        })
    });
    assert_eq!(first_rx.recv_timeout(WAIT).unwrap(), "a");
    let cancelled_at = Instant::now();
    cancel.store(true, Ordering::Relaxed);
    let result = worker.join().unwrap();
    assert_eq!(result, Err(TranslateError::Cancelled));
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
}

#[test]
fn ollama_cancel_ends_a_stalled_answer() {
    let (_release, rx) = mpsc::channel();
    let server = Server::start(vec![Reply::Stall {
        parts: vec![format!("{}\n", chat_line("a"))],
        release: rx,
    }]);
    cancel_while_stalled(Arc::new(OllamaBackend::new(&config(
        &server.endpoint(),
        "m",
    ))));
}

#[test]
fn ollama_warm_up_loads_the_model() {
    let server = Server::start(vec![json_reply(
        200,
        r#"{"model":"m","created_at":"x","response":"","done":true,"done_reason":"load"}"#,
    )]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "m"));
    assert_eq!(backend.warm_up(&AtomicBool::new(false)), Ok(()));
    let req = &server.requests(1)[0];
    assert_eq!(req.path, "/api/generate");
    assert_eq!(
        req.body,
        serde_json::json!({"model": "m", "keep_alive": "30m"})
    );
}

#[test]
fn ollama_warm_up_tells_a_missing_model_from_a_missing_server() {
    let server = Server::start(vec![json_reply(404, r#"{"error":"model 'm' not found"}"#)]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "m"));
    assert_eq!(
        backend.warm_up(&AtomicBool::new(false)),
        Err(TranslateError::ModelMissing("m".to_string()))
    );
    let backend = OllamaBackend::new(&config(&closed_endpoint(), "m"));
    assert!(matches!(
        backend.warm_up(&AtomicBool::new(false)),
        Err(TranslateError::Unreachable(_))
    ));
}

#[test]
fn ollama_lists_its_models() {
    let server = Server::start(vec![json_reply(
        200,
        "{\n  \"models\": [\n    {\"name\": \"translategemma:4b\", \"size\": 1},\n    {\"name\": \"qwen2.5:7b\"}\n  ]\n}",
    )]);
    let backend = OllamaBackend::new(&config(&server.endpoint(), "m"));
    assert_eq!(
        backend.models(&AtomicBool::new(false)),
        Ok(vec![
            "translategemma:4b".to_string(),
            "qwen2.5:7b".to_string()
        ])
    );
    let req = &server.requests(1)[0];
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("GET", "/api/tags")
    );
    let backend = OllamaBackend::new(&config(&closed_endpoint(), "m"));
    assert!(matches!(
        backend.models(&AtomicBool::new(false)),
        Err(TranslateError::Unreachable(_))
    ));
}

#[test]
fn identity_is_model_and_target() {
    let cfg = TranslateConfig {
        target: "ja".to_string(),
        ..config("http://127.0.0.1:1", "translategemma:4b")
    };
    assert_eq!(OllamaBackend::new(&cfg).identity(), "translategemma:4b|ja");
    assert_eq!(OpenAiBackend::new(&cfg).identity(), "translategemma:4b|ja");
}

// ---------------------------------------------------------------------------------------------
// OpenAI compatible backend
// ---------------------------------------------------------------------------------------------

fn delta(content: &str) -> String {
    format!(
        "data: {}",
        serde_json::json!({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]})
    )
}

#[test]
fn openai_pieces_arrive_until_done_and_comments_are_skipped() {
    let (a, b, c) = (delta("감시"), delta("자는"), delta(" 감독한다."));
    let server = Server::start(vec![sse(&[
        ": keep-alive comment",
        &a,
        "event: ignored",
        &b,
        &c,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ])]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "m"));
    let text = "The watchdog supervises.";
    let (pieces, result) = run(&backend, &TranslateRequest::passage(text));
    assert_eq!(result, Ok(()));
    assert_eq!(pieces, ["감시", "자는", " 감독한다."]);

    let req = &server.requests(1)[0];
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(req.body["model"], "m");
    assert_eq!(req.body["stream"], true);
    assert_eq!(req.body["temperature"], 0);
    assert_eq!(
        req.body["max_tokens"],
        max_tokens(&TranslateRequest::passage(text))
    );
    assert_eq!(req.body["messages"][0]["role"], "user");
    assert_eq!(
        req.body["messages"][0]["content"],
        passage_prompt("m", "ko", text)
    );
}

#[test]
fn openai_stream_without_done_is_a_protocol_error() {
    let a = delta("a");
    let server = Server::start(vec![sse(&[&a])]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "m"));
    assert!(matches!(
        run(&backend, &TranslateRequest::passage("x")).1,
        Err(TranslateError::Protocol(_))
    ));
}

#[test]
fn openai_reads_both_error_body_shapes() {
    let server = Server::start(vec![
        json_reply(
            400,
            r#"{"error":{"message":"bad things","type":"invalid_request_error"}}"#,
        ),
        json_reply(503, r#"{"error":"loading model"}"#),
        sse(&[r#"data: {"error":{"message":"context too long"}}"#]),
    ]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "m"));
    let expect = [
        (400, "bad things"),
        (503, "loading model"),
        (200, "context too long"),
    ];
    for (status, message) in expect {
        assert_eq!(
            run(&backend, &TranslateRequest::passage("x")).1,
            Err(TranslateError::Http {
                status,
                message: message.to_string()
            })
        );
    }
}

#[test]
fn openai_404_is_a_missing_model_and_a_closed_port_unreachable() {
    let server = Server::start(vec![json_reply(
        404,
        r#"{"error":{"message":"model 'm' not found","type":"not_found_error"}}"#,
    )]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "m"));
    assert_eq!(
        run(&backend, &TranslateRequest::passage("x")).1,
        Err(TranslateError::ModelMissing("m".to_string()))
    );
    let backend = OpenAiBackend::new(&openai_config(&closed_endpoint(), "m"));
    let started = Instant::now();
    assert!(matches!(
        run(&backend, &TranslateRequest::passage("x")).1,
        Err(TranslateError::Unreachable(_))
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn openai_cancel_ends_a_stalled_answer() {
    let (_release, rx) = mpsc::channel();
    let server = Server::start(vec![Reply::Stall {
        parts: vec![format!("{}\n\n", delta("a"))],
        release: rx,
    }]);
    cancel_while_stalled(Arc::new(OpenAiBackend::new(&openai_config(
        &server.endpoint(),
        "m",
    ))));
}

const MODELS_BODY: &str =
    r#"{"object":"list","data":[{"id":"translategemma:4b","object":"model"},{"id":"other"}]}"#;

#[test]
fn openai_lists_models() {
    let server = Server::start(vec![json_reply(200, MODELS_BODY)]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "m"));
    assert_eq!(
        backend.models(&AtomicBool::new(false)),
        Ok(vec!["translategemma:4b".to_string(), "other".to_string()])
    );
    let req = &server.requests(1)[0];
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("GET", "/v1/models")
    );
}

#[test]
fn openai_warm_up_checks_the_list_then_asks_for_one_token() {
    let server = Server::start(vec![
        json_reply(200, MODELS_BODY),
        sse(&[&delta("Hi"), "data: [DONE]"]),
    ]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "translategemma:4b"));
    assert_eq!(backend.warm_up(&AtomicBool::new(false)), Ok(()));
    let reqs = server.requests(2);
    assert_eq!(reqs[0].path, "/v1/models");
    assert_eq!(reqs[1].path, "/v1/chat/completions");
    assert_eq!(reqs[1].body["max_tokens"], 1);
    assert_eq!(reqs[1].body["model"], "translategemma:4b");
}

#[test]
fn openai_warm_up_reports_a_model_not_in_the_list_and_a_missing_server() {
    let server = Server::start(vec![json_reply(200, MODELS_BODY)]);
    let backend = OpenAiBackend::new(&openai_config(&server.endpoint(), "absent"));
    assert_eq!(
        backend.warm_up(&AtomicBool::new(false)),
        Err(TranslateError::ModelMissing("absent".to_string()))
    );
    let backend = OpenAiBackend::new(&openai_config(&closed_endpoint(), "absent"));
    assert!(matches!(
        backend.warm_up(&AtomicBool::new(false)),
        Err(TranslateError::Unreachable(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// Picking a backend
// ---------------------------------------------------------------------------------------------

#[test]
fn backend_from_config_picks_by_kind_and_uses_the_default_address() {
    // The answer of each backend's model list tells which API was used.
    let server = Server::start(vec![
        json_reply(200, r#"{"models":[{"name":"from-ollama"}]}"#),
        json_reply(200, r#"{"data":[{"id":"from-openai"}]}"#),
    ]);
    let ollama = backend_from_config(&config(&server.endpoint(), "m"));
    assert_eq!(
        ollama.models(&AtomicBool::new(false)),
        Ok(vec!["from-ollama".to_string()])
    );
    let openai = backend_from_config(&openai_config(&server.endpoint(), "m"));
    assert_eq!(
        openai.models(&AtomicBool::new(false)),
        Ok(vec!["from-openai".to_string()])
    );

    let plain = TranslateConfig::default();
    assert_eq!(plain.endpoint(), "http://127.0.0.1:11434");
    let openai_default = TranslateConfig {
        backend: TranslateBackendKind::OpenAi,
        ..TranslateConfig::default()
    };
    assert_eq!(openai_default.endpoint(), "http://127.0.0.1:8080");
    let slash = TranslateConfig {
        endpoint: Some("http://localhost:9/".to_string()),
        ..TranslateConfig::default()
    };
    assert_eq!(slash.endpoint(), "http://localhost:9");
}

// ---------------------------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------------------------

#[test]
fn translategemma_template_is_exact() {
    let expected = "You are a professional English (en) to Korean (ko) translator. Your goal is to accurately convey the meaning and nuances of the original English text while adhering to Korean grammar, vocabulary, and cultural sensitivities.\nProduce only the Korean translation, without any additional explanations or commentary. Please translate the following English text into Korean:\n\n\nHello [SWS_EM_01004].";
    assert_eq!(
        passage_prompt("translategemma:4b", "ko", "Hello [SWS_EM_01004]."),
        expected
    );
    assert_eq!(
        passage_prompt("translategemma:12b", "ko", "Hello [SWS_EM_01004]."),
        expected
    );
    let ja = passage_prompt("translategemma:4b", "ja", "x");
    assert!(
        ja.contains("English (en) to Japanese (ja) translator"),
        "{ja}"
    );
    assert!(ja.ends_with("into Japanese:\n\n\nx"), "{ja}");
}

#[test]
fn other_models_get_the_general_instruction() {
    let p = passage_prompt("qwen2.5:7b", "ko", "The watchdog.");
    assert!(p.contains("Korean"), "{p}");
    assert!(p.contains("identifiers"), "{p}");
    assert!(p.contains("requirement IDs"), "{p}");
    assert!(p.contains("Output only the translation"), "{p}");
    assert!(p.ends_with("\n\nThe watchdog."), "{p}");
    assert!(!p.contains("professional English (en)"));
}

#[test]
fn language_names() {
    for (code, name) in [
        ("ko", "Korean"),
        ("en", "English"),
        ("ja", "Japanese"),
        ("zh", "Chinese"),
        ("de", "German"),
        ("fr", "French"),
        ("es", "Spanish"),
        ("xx", "xx"),
    ] {
        assert_eq!(language_name(code), name);
    }
}

#[test]
fn word_prompt_with_and_without_context() {
    assert_eq!(
        word_prompt("ko", "idempotent", "The call is idempotent."),
        "문장: The call is idempotent.\n\n위 문장에서 'idempotent' 가 뜻하는 바를 한국어 낱말 하나나 둘로만 답하라. 설명, 문장, 따옴표 없이 뜻만 쓴다."
    );
    let alone = word_prompt("ko", "idempotent", "  ");
    assert!(!alone.contains("문장:"), "{alone}");
    assert!(alone.contains("'idempotent'"), "{alone}");
    assert!(alone.contains("한국어 낱말 하나나 둘로만"), "{alone}");

    let en = word_prompt("ja", "idempotent", "The call is idempotent.");
    assert!(
        en.starts_with("Sentence: The call is idempotent.\n\n"),
        "{en}"
    );
    assert!(en.contains("one or two Japanese words"), "{en}");
    let en_alone = word_prompt("de", "idempotent", "");
    assert!(!en_alone.contains("Sentence:"), "{en_alone}");
    assert!(en_alone.contains("German"), "{en_alone}");
}

#[test]
fn own_templates_fill_their_placeholders() {
    assert_eq!(
        fill_passage_template("Into {target}: {text}", "ko", "a {word} b").as_deref(),
        Some("Into Korean: a {word} b")
    );
    assert_eq!(
        fill_word_template("{word} in \"{sentence}\" -> {target}", "ja", "x", " s ").as_deref(),
        Some("x in \"s\" -> Japanese")
    );
    // Unknown braces stay.
    assert_eq!(
        fill_passage_template("{a} {text} {", "ko", "t").as_deref(),
        Some("{a} t {")
    );
    // A template without its required placeholder is not used.
    assert_eq!(fill_passage_template("Translate: {word}", "ko", "t"), None);
    assert_eq!(
        fill_word_template("Meaning of {text}", "ko", "w", "s"),
        None
    );
}

#[test]
fn own_templates_win_over_the_model_and_bad_ones_fall_back() {
    let mut prompts = Prompts {
        model: "translategemma:4b".to_string(),
        target: "ko".to_string(),
        passage_template: Some("P[{text}]".to_string()),
        word_template: Some("W[{word}|{sentence}]".to_string()),
    };
    assert_eq!(prompts.message(&TranslateRequest::passage("abc")), "P[abc]");
    assert_eq!(
        prompts.message(&TranslateRequest::word("w", "the w here")),
        "W[w|the w here]"
    );
    prompts.passage_template = Some("no placeholder".to_string());
    prompts.word_template = Some("none here".to_string());
    assert_eq!(
        prompts.message(&TranslateRequest::passage("abc")),
        passage_prompt("translategemma:4b", "ko", "abc")
    );
    assert_eq!(
        prompts.message(&TranslateRequest::word("w", "s")),
        word_prompt("ko", "w", "s")
    );
}

#[test]
fn token_budgets() {
    assert_eq!(max_tokens(&TranslateRequest::word("w", "s")), 32);
    assert_eq!(max_tokens(&TranslateRequest::passage("abcd")), 66);
    assert_eq!(
        max_tokens(&TranslateRequest::passage("x".repeat(10_000))),
        2048
    );
}

#[test]
fn prepare_passage_collapses_space_and_cuts_at_a_word_border() {
    assert_eq!(
        prepare_passage("  a\n b\t\tc  "),
        ("a b c".to_string(), false)
    );
    // 400 words of 4 letters and a space: 2000 characters with the last space.
    let long = "abcd ".repeat(450);
    let (text, cut) = prepare_passage(&long);
    assert!(cut);
    assert!(text.chars().count() <= MAX_PASSAGE_CHARS);
    assert!(text.ends_with("abcd"), "{}", &text[text.len() - 10..]);
    // Cutting in the middle of a word goes back to the word before.
    let odd = format!("{}{}", "a".repeat(1998), " bcdef ghi");
    let (text, cut) = prepare_passage(&odd);
    assert!(cut);
    assert_eq!(text, "a".repeat(1998));
    // Exactly the limit is not cut.
    let exact = "é".repeat(MAX_PASSAGE_CHARS);
    assert_eq!(prepare_passage(&exact), (exact.clone(), false));
    // One giant word is cut hard.
    let (text, cut) = prepare_passage(&"z".repeat(2500));
    assert!(cut);
    assert_eq!(text.chars().count(), MAX_PASSAGE_CHARS);
}

fn word_kind() -> TranslateKind {
    TranslateKind::Word {
        context: String::new(),
    }
}

#[test]
fn clean_output_of_passages() {
    let p = TranslateKind::Passage;
    assert_eq!(clean_output(&p, "  감시자.\n"), "감시자.");
    assert_eq!(
        clean_output(&p, "\"감시자가 감독한다.\""),
        "감시자가 감독한다."
    );
    assert_eq!(clean_output(&p, "“감시자”"), "감시자");
    // Quotes inside are not wrapping quotes.
    assert_eq!(clean_output(&p, "\"A\" 와 \"B\""), "\"A\" 와 \"B\"");
    // Several lines stay for a passage.
    assert_eq!(clean_output(&p, "첫 줄\n둘째 줄"), "첫 줄\n둘째 줄");
}

#[test]
fn clean_output_of_words() {
    let w = word_kind();
    assert_eq!(clean_output(&w, " 멱등성 "), "멱등성");
    assert_eq!(clean_output(&w, "\"멱등성\""), "멱등성");
    assert_eq!(clean_output(&w, "'멱등'."), "멱등");
    assert_eq!(clean_output(&w, "멱등성.\n설명: 같은 결과"), "멱등성");
    assert_eq!(clean_output(&w, "\n\n감시\n"), "감시");
    assert_eq!(clean_output(&w, "생존성。"), "생존성");
    let long = clean_output(&w, &"가".repeat(60));
    assert_eq!(long.chars().count(), 40);
}

#[test]
fn clean_output_does_not_waver_on_partial_text() {
    // Every prefix of a streamed answer shows a prefix of the final display.
    for (kind, answer) in [
        (word_kind(), "\"멱등성\".\n설명"),
        (word_kind(), "감시 장치"),
        (TranslateKind::Passage, "감시자는 각 프로세스를 감독한다."),
    ] {
        let full = clean_output(&kind, answer);
        let chars: Vec<char> = answer.chars().collect();
        for n in 0..=chars.len() {
            let part: String = chars[..n].iter().collect();
            let shown = clean_output(&kind, &part);
            assert!(
                full.starts_with(&shown),
                "{answer:?}: prefix {part:?} shows {shown:?}, final {full:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// sentence_around
// ---------------------------------------------------------------------------------------------

/// A page whose blocks are lists of lines (characters 10 pt wide, lines 16 pt apart).
fn page(blocks: &[&[&str]]) -> PageText {
    let mut pt = PageText::default();
    let mut y = 72.0;
    for lines in blocks {
        let first_line = pt.lines.len() as u32;
        for text in *lines {
            let start = pt.chars.len() as u32;
            for (i, ch) in text.chars().enumerate() {
                let x = 72.0 + i as f32 * 10.0;
                pt.chars.push(TextChar {
                    ch,
                    quad: Quad {
                        ul: Point::new(x, y),
                        ur: Point::new(x + 10.0, y),
                        ll: Point::new(x, y + 12.0),
                        lr: Point::new(x + 10.0, y + 12.0),
                    },
                    size: 12.0,
                });
            }
            let end = pt.chars.len() as u32;
            pt.lines.push(TextLine {
                chars: start..end,
                bounds: Rect::new(72.0, y, 72.0 + text.len() as f32 * 10.0, y + 12.0),
            });
            y += 16.0;
        }
        pt.blocks.push(TextBlock {
            lines: first_line..pt.lines.len() as u32,
            bounds: Rect::new(72.0, 72.0, 600.0, y),
        });
        y += 8.0;
    }
    pt
}

/// The character range of the first occurrence of `word` in the page text.
fn find(pt: &PageText, word: &str) -> Range<u32> {
    let all: String = pt.chars.iter().map(|c| c.ch).collect();
    let byte = all.find(word).unwrap();
    let start = all[..byte].chars().count() as u32;
    start..start + word.chars().count() as u32
}

#[test]
fn sentence_inside_one_line() {
    let pt = page(&[&["First one. The watchdog runs. Last one."]]);
    assert_eq!(
        sentence_around(&pt, find(&pt, "watchdog")),
        "The watchdog runs."
    );
    assert_eq!(sentence_around(&pt, find(&pt, "First")), "First one.");
    assert_eq!(sentence_around(&pt, find(&pt, "Last")), "Last one.");
}

#[test]
fn sentence_across_lines_and_a_hyphenated_word() {
    let pt = page(&[&[
        "Intro text here. The watchdog super-",
        "vises the liveness of each",
        "process! Next sentence.",
    ]]);
    assert_eq!(
        sentence_around(&pt, find(&pt, "liveness")),
        "The watchdog supervises the liveness of each process!"
    );
    assert_eq!(
        sentence_around(&pt, find(&pt, "super")),
        "The watchdog supervises the liveness of each process!"
    );
}

#[test]
fn sentence_stops_at_a_block_border_and_the_page_edges() {
    let pt = page(&[
        &["7.3 Watchdog supervision"],
        &["The watchdog checks", "each process"],
    ]);
    assert_eq!(
        sentence_around(&pt, find(&pt, "Watchdog")),
        "7.3 Watchdog supervision"
    );
    assert_eq!(
        sentence_around(&pt, find(&pt, "each")),
        "The watchdog checks each process"
    );
}

#[test]
fn sentence_does_not_end_at_abbreviations_or_decimals() {
    let pt =
        page(&[&["Old one; Use a timer, e.g. the watchdog of section 3.2 in v1.0 here. New one."]]);
    assert_eq!(
        sentence_around(&pt, find(&pt, "watchdog")),
        "Use a timer, e.g. the watchdog of section 3.2 in v1.0 here."
    );
    let pt = page(&[&["See section 3. The 4 rules apply. Then (a) holds."]]);
    assert_eq!(
        sentence_around(&pt, find(&pt, "rules")),
        "The 4 rules apply."
    );
}

#[test]
fn sentence_is_limited_to_three_hundred_characters_each_side() {
    let before = "word ".repeat(100);
    let after = " more".repeat(100);
    let line = format!("{before}target{after}");
    let pt = page(&[&[line.as_str()]]);
    let s = sentence_around(&pt, find(&pt, "target"));
    assert!(s.contains("target"));
    let (left, right) = s.split_once("target").unwrap();
    assert!(left.chars().count() <= 300, "{}", left.len());
    assert!(right.chars().count() <= 300, "{}", right.len());
    assert!(left.starts_with("word"), "no cut word: {left:?}");
    assert!(right.ends_with("more"), "no cut word: {right:?}");
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

/// A backend that answers from a script.
#[derive(Default)]
struct FakeBackend {
    pieces: Vec<String>,
    error: Option<TranslateError>,
    /// When set, `translate` waits after the pieces until cancelled.
    hang: bool,
    /// Set when `translate` returns.
    ended: Arc<AtomicBool>,
}

impl TranslateBackend for FakeBackend {
    fn translate(
        &self,
        _req: &TranslateRequest,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        let result = (|| {
            for p in &self.pieces {
                on_text(p);
            }
            if self.hang {
                while !cancel.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                    std::thread::sleep(Duration::from_millis(1));
                }
                return Err(TranslateError::Cancelled);
            }
            match &self.error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        })();
        self.ended.store(true, Ordering::SeqCst);
        result
    }

    fn warm_up(&self, _cancel: &AtomicBool) -> Result<(), TranslateError> {
        match &self.error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn models(&self, _cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
        Ok(Vec::new())
    }

    fn identity(&self) -> String {
        "fake|ko".to_string()
    }
}

/// A notifier that counts its calls, and a way to wait for a condition after a call.
struct Notes {
    count: Arc<(Mutex<usize>, Condvar)>,
}

impl Notes {
    fn new() -> (Notes, Notifier) {
        let count: Arc<(Mutex<usize>, Condvar)> = Arc::default();
        let c = Arc::clone(&count);
        let notifier: Notifier = Arc::new(move || {
            *c.0.lock().unwrap() += 1;
            c.1.notify_all();
        });
        (Notes { count }, notifier)
    }

    fn calls(&self) -> usize {
        *self.count.0.lock().unwrap()
    }

    /// Waits (re-checking at every notification) until `done` holds.
    fn wait_until(&self, done: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "condition not met in time");
            // `done` may read the count, so the lock is not held while it runs. A notification
            // between the check and the wait is caught by the short timeout.
            let guard = self.count.0.lock().unwrap();
            let _ = self
                .count
                .1
                .wait_timeout(guard, left.min(Duration::from_millis(20)))
                .unwrap();
        }
    }
}

#[test]
fn job_snapshot_grows_and_ends_done() {
    let backend = Arc::new(FakeBackend {
        pieces: vec!["\"감시".into(), "자".into(), "\".".into()],
        ..FakeBackend::default()
    });
    let (notes, notifier) = Notes::new();
    let job = TranslateJob::start(backend, TranslateRequest::word("watchdog", ""), notifier);
    notes.wait_until(|| job.snapshot().finished());
    let snap = job.snapshot();
    assert_eq!(snap.state, TranslateState::Done);
    assert_eq!(snap.text, "감시자");
    assert!(notes.calls() >= 1);
}

#[test]
fn job_failure_is_in_the_snapshot() {
    let backend = Arc::new(FakeBackend {
        pieces: vec!["partial".into()],
        error: Some(TranslateError::ModelMissing("m".into())),
        ..FakeBackend::default()
    });
    let (notes, notifier) = Notes::new();
    let job = TranslateJob::start(backend, TranslateRequest::passage("x"), notifier);
    notes.wait_until(|| job.snapshot().finished());
    let snap = job.snapshot();
    assert_eq!(
        snap.state,
        TranslateState::Failed(TranslateError::ModelMissing("m".into()))
    );
    assert_eq!(snap.text, "partial");
}

#[test]
fn job_notifies_while_streaming() {
    let ended = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(FakeBackend {
        pieces: vec!["첫 조각".into()],
        hang: true,
        ended: Arc::clone(&ended),
        ..FakeBackend::default()
    });
    let (notes, notifier) = Notes::new();
    let job = TranslateJob::start(backend, TranslateRequest::passage("x"), notifier);
    // The first piece notifies before the job ends.
    notes.wait_until(|| notes.calls() >= 1);
    let snap = job.snapshot();
    assert_eq!(snap.text, "첫 조각");
    assert_eq!(snap.state, TranslateState::Running);
    drop(job);
    let deadline = Instant::now() + WAIT;
    while !ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the thread did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn dropping_a_job_cancels_its_thread_without_waiting() {
    let ended = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(FakeBackend {
        hang: true,
        ended: Arc::clone(&ended),
        ..FakeBackend::default()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let notifier: Notifier = Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    });
    let job = TranslateJob::start(backend, TranslateRequest::passage("x"), notifier);
    let started = Instant::now();
    drop(job);
    assert!(started.elapsed() < Duration::from_millis(100));
    let deadline = Instant::now() + WAIT;
    while !ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the thread did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A dropped job does not notify at its end. Give the thread a moment past `ended`.
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn warm_up_job_changes_only_the_state() {
    let (notes, notifier) = Notes::new();
    let job = TranslateJob::warm_up(Arc::new(FakeBackend::default()), notifier);
    notes.wait_until(|| job.snapshot().finished());
    assert_eq!(job.snapshot().state, TranslateState::Done);
    assert_eq!(job.snapshot().text, "");

    let (notes, notifier) = Notes::new();
    let failing = FakeBackend {
        error: Some(TranslateError::Unreachable("x".into())),
        ..FakeBackend::default()
    };
    let job = TranslateJob::warm_up(Arc::new(failing), notifier);
    notes.wait_until(|| job.snapshot().finished());
    assert_eq!(
        job.snapshot().state,
        TranslateState::Failed(TranslateError::Unreachable("x".into()))
    );
}

// ---------------------------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------------------------

#[test]
fn cache_insert_and_get() {
    let mut cache = TranslationCache::default();
    let req = TranslateRequest::passage("The watchdog.");
    assert_eq!(cache.get("m|ko", &req), None);
    assert!(!cache.dirty());
    cache.insert("m|ko", &req, "감시자.".into());
    assert!(cache.dirty());
    assert_eq!(cache.get("m|ko", &req), Some("감시자."));
    assert!(cache.contains("m|ko", &req));
    // An empty translation is not remembered.
    cache.insert("m|ko", &TranslateRequest::passage("y"), "  ".into());
    assert_eq!(cache.len(), 1);
}

#[test]
fn cache_keys_include_context_kind_and_identity() {
    let mut cache = TranslationCache::default();
    let a = TranslateRequest::word("bank", "The river bank.");
    let b = TranslateRequest::word("bank", "The bank account.");
    cache.insert("m|ko", &a, "강둑".into());
    cache.insert("m|ko", &b, "은행".into());
    assert_eq!(cache.get("m|ko", &a), Some("강둑"));
    assert_eq!(cache.get("m|ko", &b), Some("은행"));
    assert_eq!(cache.get("other|ko", &a), None);
    assert_eq!(cache.get("m|ja", &a), None);
    assert_eq!(cache.get("m|ko", &TranslateRequest::passage("bank")), None);
}

#[test]
fn cache_drops_the_least_recently_used() {
    let mut cache = TranslationCache::with_capacity(3);
    let r = |s: &str| TranslateRequest::passage(s);
    cache.insert("m", &r("a"), "A".into());
    cache.insert("m", &r("b"), "B".into());
    cache.insert("m", &r("c"), "C".into());
    // Using "a" makes "b" the oldest.
    assert_eq!(cache.get("m", &r("a")), Some("A"));
    cache.insert("m", &r("d"), "D".into());
    assert_eq!(cache.len(), 3);
    assert!(cache.contains("m", &r("a")));
    assert!(!cache.contains("m", &r("b")));
    assert!(cache.contains("m", &r("d")));
    assert_eq!(TranslationCache::default().len(), 0);
    assert_eq!(MAX_CACHE_ENTRIES, 5000);
}

#[test]
fn cache_file_roundtrip_keeps_entries_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = Paths::under(dir.path()).translations_file();
    assert!(path.ends_with("state/translations.json"));
    let mut cache = TranslationCache::with_capacity(3);
    let r = |s: &str| TranslateRequest::passage(s);
    cache.insert("m", &r("a"), "A".into());
    cache.insert("m", &TranslateRequest::word("w", "ctx"), "W".into());
    cache.insert("m", &r("c"), "C".into());
    cache.get("m", &r("a"));
    cache.save(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"version\""), "{text}");

    let mut back = TranslationCache::load_with_capacity(&path, 3);
    assert!(!back.dirty());
    assert_eq!(back.len(), 3);
    assert_eq!(
        back.get("m", &TranslateRequest::word("w", "ctx")),
        Some("W")
    );
    // The file keeps the use order (w, c, a); after the get of w above, c is the oldest.
    back.insert("m", &r("d"), "D".into());
    assert!(!back.contains("m", &r("c")));
    assert!(back.contains("m", &r("a")));
}

#[test]
fn cache_missing_file_is_empty_and_a_damaged_one_is_kept_as_bad() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("translations.json");
    assert!(TranslationCache::load(&path).is_empty());
    std::fs::write(&path, b"{ not json").unwrap();
    assert!(TranslationCache::load(&path).is_empty());
    assert!(bad_path(&path).exists());
    assert!(!path.exists());
}

#[test]
fn cache_dirty_is_cleared_by_mark_saved() {
    let mut cache = TranslationCache::default();
    cache.insert("m", &TranslateRequest::passage("a"), "A".into());
    assert!(cache.dirty());
    cache.mark_saved();
    assert!(!cache.dirty());
    // Reading does not make it dirty.
    cache.get("m", &TranslateRequest::passage("a"));
    assert!(!cache.dirty());
}
