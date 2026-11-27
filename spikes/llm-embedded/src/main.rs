//! WP-82: run a GGUF translation model inside the process with llama.cpp and measure it.
//!
//! Throwaway experiment code (unwrap is fine). See README.md.
//!
//!     llm-embedded <model.gguf> [--gpu-layers N] [--threads N] [--runs N] [--device N]
//!                               [--manual-template] [--no-cancel-test]
//!
//! llama.cpp's own log (device list, offload lines) goes to stderr.

use std::io::Write;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::params::kv_overrides::ParamOverrideValue;
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

const N_CTX: u32 = 2048;
const MAX_NEW_TOKENS: usize = 400;

struct Args {
    model: String,
    gpu_layers: u32,
    threads: i32,
    runs: usize,
    device: Option<usize>,
    manual_template: bool,
    cancel_test: bool,
    rms_eps_override: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        model: String::new(),
        gpu_layers: 999,
        threads: 8,
        runs: 3,
        device: None,
        manual_template: false,
        cancel_test: true,
        rms_eps_override: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--gpu-layers" => a.gpu_layers = it.next().unwrap().parse().unwrap(),
            "--threads" => a.threads = it.next().unwrap().parse().unwrap(),
            "--runs" => a.runs = it.next().unwrap().parse().unwrap(),
            "--device" => a.device = Some(it.next().unwrap().parse().unwrap()),
            "--manual-template" => a.manual_template = true,
            "--no-cancel-test" => a.cancel_test = false,
            "--rms-eps-override" => a.rms_eps_override = true,
            s if s.starts_with("--") => panic!("unknown option {s}"),
            s => a.model = s.to_string(),
        }
    }
    assert!(
        !a.model.is_empty(),
        "usage: llm-embedded <model.gguf> [--gpu-layers N] [--threads N] [--runs N] [--device N]"
    );
    a
}

struct Req {
    kind: &'static str,
    label: &'static str,
    user: String,
}

fn word_req(word: &'static str, sentence: &str) -> Req {
    Req {
        kind: "word",
        label: word,
        user: format!(
            "문장: {sentence}\n\n위 문장에서 '{word}' 가 뜻하는 바를 한국어 낱말 하나나 둘로만 답하라. 설명, 문장, 따옴표 없이 뜻만 쓴다."
        ),
    }
}

fn passage_req(label: &'static str, text: &str) -> Req {
    Req {
        kind: "passage",
        label,
        user: format!(
            "You are a professional English (en) to Korean (ko) translator. Your goal is to accurately convey the meaning and nuances of the original English text while adhering to Korean grammar, vocabulary, and cultural sensitivities.\nProduce only the Korean translation, without any additional explanations or commentary. Please translate the following English text into Korean:\n\n\n{text}"
        ),
    }
}

fn requests() -> Vec<Req> {
    vec![
        word_req(
            "liveness",
            "The watchdog supervises the liveness of each process.",
        ),
        word_req(
            "bus",
            "Each ECU sends its frames on the CAN bus with a fixed identifier.",
        ),
        word_req(
            "idempotent",
            "The Offer method shall be idempotent, so calling it twice has no further effect.",
        ),
        word_req(
            "deadline",
            "The deadline supervision checks that a checkpoint is reached in time.",
        ),
        passage_req(
            "line-1",
            "The Execution Management shall start the process within the configured timeout after the Function Group state transition was requested.",
        ),
        passage_req(
            "line-2",
            "If the liveness supervision fails, [SWS_PHM_00121] PlatformHealthManagement shall report the failure to the State Management via RecoveryAction::RecoveryHandler().",
        ),
        passage_req(
            "three-sentences",
            "A Function Group is a set of coherent Processes, which need to be controlled consistently. Depending on the state of the Function Group, Processes are started or terminated. Processes can belong to more than one Function Group State, but not to different Function Groups.",
        ),
    ]
}

/// Gemma chat format built by hand (fallback / comparison).
fn manual_gemma(user: &str) -> String {
    format!("<start_of_turn>user\n{user}<end_of_turn>\n<start_of_turn>model\n")
}

fn render_prompt(model: &LlamaModel, user: &str, manual: bool) -> (String, &'static str) {
    if !manual {
        if let Ok(tmpl) = model.chat_template(None) {
            let msgs = [LlamaChatMessage::new("user".into(), user.into()).unwrap()];
            if let Ok(s) = model.apply_chat_template(&tmpl, &msgs, true) {
                return (s, "model-template");
            }
        }
    }
    (manual_gemma(user), "manual-gemma")
}

/// Turns token byte pieces into text without splitting a UTF-8 sequence (Korean is 3 bytes per char).
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8], mut emit: impl FnMut(&str)) {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                emit(s);
                self.pending.clear();
            }
            Err(e) => {
                let n = e.valid_up_to();
                if n > 0 {
                    emit(std::str::from_utf8(&self.pending[..n]).unwrap());
                    self.pending.drain(..n);
                }
                // a hard error (error_len is Some) would never resolve; drop it lossy.
                if e.error_len().is_some() {
                    let s = String::from_utf8_lossy(&self.pending).into_owned();
                    emit(&s);
                    self.pending.clear();
                }
            }
        }
    }
}

#[derive(Default, Clone)]
struct Measure {
    prompt_tokens: usize,
    prompt_ms: f64,
    ttft_ms: f64,
    gen_tokens: usize,
    gen_ms: f64,
    total_ms: f64,
    text: String,
    cancelled: bool,
}

impl Measure {
    fn tok_per_s(&self) -> f64 {
        // tokens after the first one over the time after the first one
        if self.gen_tokens > 1 && self.gen_ms > 0.0 {
            (self.gen_tokens - 1) as f64 / (self.gen_ms / 1000.0)
        } else {
            0.0
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// One request. `on_text` receives text pieces as they are produced (streaming).
/// `cancel` is checked between tokens (and the decode of one token is the longest uninterruptible step).
fn translate(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    prompt: &str,
    cancel: &AtomicBool,
    chunk: usize,
    on_text: &mut dyn FnMut(&str),
) -> Measure {
    let t0 = Instant::now();
    ctx.clear_kv_cache();
    let vocab = model.vocab();
    let tokens = vocab.tokenize(prompt.as_bytes(), vocab.should_add_bos(), true);
    let mut m = Measure {
        prompt_tokens: tokens.len(),
        ..Default::default()
    };

    // The prompt is decoded in chunks of `chunk` tokens (0 = all at once) so the cancel flag
    // can be seen between chunks; a single llama_decode call cannot be interrupted.
    let chunk = if chunk == 0 {
        tokens.len().max(1)
    } else {
        chunk
    };
    let mut batch = LlamaBatch::new(chunk, 1);
    let mut sampler = LlamaSampler::greedy();
    let mut done = 0;
    while done < tokens.len() {
        if cancel.load(Ordering::Relaxed) {
            m.cancelled = true;
            m.prompt_ms = ms(t0.elapsed());
            m.total_ms = m.prompt_ms;
            return m;
        }
        let end = (done + chunk).min(tokens.len());
        batch.clear();
        for i in done..end {
            batch
                .add(tokens[i], i as i32, &[0], i == tokens.len() - 1)
                .unwrap();
        }
        ctx.decode(&mut batch).unwrap();
        done = end;
    }
    ctx.synchronize();
    m.prompt_ms = ms(t0.elapsed());

    let mut pos = tokens.len() as i32;
    let mut sample_idx = batch.n_tokens() - 1;
    let mut stream = Utf8Stream::default();
    let mut first_at: Option<Instant> = None;
    let mut out = String::new();
    let mut one = LlamaBatch::new(1, 1);
    loop {
        if cancel.load(Ordering::Relaxed) {
            m.cancelled = true;
            break;
        }
        let tok = sampler.sample(ctx, sample_idx);
        if first_at.is_none() {
            first_at = Some(Instant::now());
            m.ttft_ms = ms(t0.elapsed());
        }
        if vocab.is_eog(tok) || m.gen_tokens >= MAX_NEW_TOKENS || pos as u32 >= N_CTX - 1 {
            break;
        }
        m.gen_tokens += 1;
        let piece = vocab.token_to_piece(tok, false, None);
        stream.push(&piece, |s| {
            out.push_str(s);
            on_text(s);
        });
        one.clear();
        one.add(tok, pos, &[0], true).unwrap();
        pos += 1;
        sample_idx = 0;
        ctx.decode(&mut one).unwrap();
    }
    ctx.synchronize();
    if let Some(f) = first_at {
        m.gen_ms = ms(f.elapsed());
    }
    m.total_ms = ms(t0.elapsed());
    m.text = out.trim().to_string();
    m
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

fn main() {
    let args = parse_args();

    // Log lines of llama.cpp / ggml (device list, "offloaded n/m layers") stay on stderr (default).
    let backend = LlamaBackend::init().unwrap();
    println!("== devices (ggml backend registry)");
    let devices = llama_cpp_2::list_llama_ggml_backend_devices();
    for d in &devices {
        println!(
            "  [{}] {:<10} {:<8} {:?} total {} MiB free {} MiB  {}",
            d.index,
            d.name,
            d.backend,
            d.device_type,
            d.memory_total >> 20,
            d.memory_free >> 20,
            d.description
        );
    }
    println!("supports_gpu_offload = {}", backend.supports_gpu_offload());

    let mut mp = LlamaModelParams::default().with_n_gpu_layers(args.gpu_layers);
    if let Some(dev) = args.device {
        mp = mp.with_devices(&[dev]).unwrap().with_main_gpu(0);
    }
    // The progress callback is also the model-load cancel hook (return false to abort the load).
    mp = mp.with_progress_callback(|_p| true);
    let mut mp = Box::pin(mp);
    if args.rms_eps_override {
        // Ollama's gemma3 GGUF lacks this key; llama.cpp requires it. Try supplying it by override.
        let key = std::ffi::CString::new("gemma3.attention.layer_norm_rms_epsilon").unwrap();
        mp.as_mut()
            .append_kv_override(&key, ParamOverrideValue::Float(1e-6));
    }

    let t = Instant::now();
    let model = LlamaModel::load_from_file(&backend, &args.model, &mp).unwrap_or_else(|e| {
        eprintln!("model load failed: {e}");
        std::process::exit(2)
    });
    let load_ms = ms(t.elapsed());
    println!(
        "== model loaded in {:.0} ms  (params {:.2} B, file {:.2} GiB, n_layer {}, n_ctx_train {})",
        load_ms,
        model.n_params() as f64 / 1e9,
        model.size() as f64 / (1u64 << 30) as f64,
        model.n_layer(),
        model.n_ctx_train()
    );
    println!(
        "   chat template in model: {}",
        model
            .chat_template(None)
            .map(|t| t.to_string().unwrap().len().to_string() + " bytes")
            .unwrap_or("none".into())
    );

    let cp = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(N_CTX))
        .with_n_threads(args.threads)
        .with_n_threads_batch(args.threads);
    let t = Instant::now();
    let ctx = model.new_context(&backend, cp).unwrap();
    let ctx_ms = ms(t.elapsed());
    println!(
        "== context (n_ctx {}) created in {:.0} ms, threads {}",
        ctx.n_ctx(),
        ctx_ms,
        args.threads
    );

    let reqs = requests();
    let (sample_prompt, how) = render_prompt(&model, &reqs[0].user, args.manual_template);
    println!(
        "== prompt format: {how}; first prompt rendered as:\n{}\n--",
        sample_prompt.replace('\n', "\\n\n")
    );
    {
        let vocab = model.vocab();
        let toks = vocab.tokenize(sample_prompt.as_bytes(), vocab.should_add_bos(), true);
        println!(
            "   should_add_bos={} tokens={} first ids {:?}; eog(<end_of_turn>)={}",
            vocab.should_add_bos(),
            toks.len(),
            toks.iter().take(4).map(|t| t.0).collect::<Vec<_>>(),
            vocab.is_eog(vocab.tokenize(b"<end_of_turn>", false, true)[0]),
        );
    }

    // The context is Send: move it to a worker thread (the way a translate job would own it).
    let never = AtomicBool::new(false);
    let results: Vec<Vec<Measure>> = std::thread::scope(|s| {
        let model = &model;
        let never = &never;
        let reqs = &reqs;
        let manual = args.manual_template;
        let runs = args.runs;
        let mut ctx = ctx;
        s.spawn(move || {
            let mut all = Vec::new();
            for run in 0..runs {
                let mut row = Vec::new();
                for r in reqs {
                    let (prompt, _) = render_prompt(model, &r.user, manual);
                    let m = translate(model, &mut ctx, &prompt, never, 0, &mut |_| {});
                    println!(
                        "run {} {:<7} {:<16} prompt {:>3} tok {:>7.1} ms | ttft {:>7.1} ms | gen {:>3} tok {:>7.1} ms {:>6.1} tok/s | total {:>7.1} ms | {}",
                        run + 1, r.kind, r.label, m.prompt_tokens, m.prompt_ms, m.ttft_ms,
                        m.gen_tokens, m.gen_ms, m.tok_per_s(), m.total_ms, m.text.replace('\n', "⏎")
                    );
                    row.push(m);
                }
                all.push(row);
            }
            if args.cancel_test {
                cancel_test(model, &mut ctx, &reqs[6], manual);
            }
            all
        })
        .join()
        .unwrap()
    });

    // summary
    println!(
        "\n== summary (model load {:.0} ms; context {:.0} ms; threads {}; gpu_layers {})",
        load_ms, ctx_ms, args.threads, args.gpu_layers
    );
    println!(
        "| request | prompt tok | gen tok | cold ttft ms | warm ttft ms (median) | cold total ms | warm total ms (median) | warm tok/s (median) |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for (i, r) in reqs.iter().enumerate() {
        let cold = &results[0][i];
        let mut tt: Vec<f64> = results.iter().skip(1).map(|row| row[i].ttft_ms).collect();
        let mut to: Vec<f64> = results.iter().skip(1).map(|row| row[i].total_ms).collect();
        let mut tp: Vec<f64> = results
            .iter()
            .skip(1)
            .map(|row| row[i].tok_per_s())
            .collect();
        let same = results.iter().all(|row| row[i].text == cold.text);
        println!(
            "| {} {} | {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1} |{}",
            r.kind,
            r.label,
            cold.prompt_tokens,
            cold.gen_tokens,
            cold.ttft_ms,
            median(&mut tt),
            cold.total_ms,
            median(&mut to),
            median(&mut tp),
            if same {
                ""
            } else {
                " (output differs between runs)"
            }
        );
    }
    let _ = std::io::stdout().flush();
}

/// Starts the long request, sets the cancel flag from another thread after 150 ms and
/// measures how long it takes until `translate` returns; then proves the context is still usable.
fn cancel_test(model: &LlamaModel, ctx: &mut LlamaContext<'_>, r: &Req, manual: bool) {
    let (prompt, _) = render_prompt(model, &r.user, manual);
    let cancel = AtomicBool::new(false);
    let set_at: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let mut streamed = 0usize;
    let mut m = Measure::default();
    let returned_at = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            *set_at.lock().unwrap() = Some(Instant::now());
            cancel.store(true, Ordering::Relaxed);
        });
        m = translate(model, ctx, &prompt, &cancel, 32, &mut |p| {
            streamed += p.chars().count()
        });
        Instant::now()
    });
    let latency = returned_at.duration_since(set_at.lock().unwrap().unwrap());
    println!(
        "== cancel test: flag set at 150 ms, translate returned {:.1} ms later; cancelled={}, {} tokens produced ({} chars streamed): {:?}",
        ms(latency),
        m.cancelled,
        m.gen_tokens,
        streamed,
        m.text
    );
    let never = AtomicBool::new(false);
    let after = translate(model, ctx, &prompt, &never, 0, &mut |_| {});
    println!(
        "   next request after a cancel: {} tokens, total {:.0} ms",
        after.gen_tokens, after.total_ms
    );
}
