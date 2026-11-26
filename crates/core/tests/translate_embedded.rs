//! The embedded translation backend (WP-83, TR-14).
//!
//! Without the `embedded` feature (`scripts/check.sh`): model file choice, the model list, the
//! thread count, the identity, and that the backend answers `NotBuilt`.
//!
//! With it (`scripts/check-embedded.sh`): UTF-8 assembly and the engine. The tests that need a
//! real model run only when `REFDESK_TEST_GGUF` names a GGUF file; they take one lock so that
//! only one model is in GPU memory at a time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use refdesk_core::translate::embedded::{
    MAX_AUTO_THREADS, Utf8Pieces, auto_threads, file_of_identity, identity_for, is_gguf_name,
    list_models, resolve_model, threads_for,
};
use refdesk_core::translate::gemma_chat;
use refdesk_core::{
    EmbeddedBackend, TranslateBackend, TranslateBackendKind, TranslateConfig, TranslateError,
    TranslateRequest, backend_from_config,
};

fn touch(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, b"GGUF").unwrap();
    path
}

fn embedded_config(model: &str) -> TranslateConfig {
    TranslateConfig {
        backend: TranslateBackendKind::Embedded,
        model: model.to_string(),
        ..TranslateConfig::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Choosing the model file
// ---------------------------------------------------------------------------------------------

#[test]
fn the_model_name_is_used_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let a = touch(dir.path(), "a.gguf");
    touch(dir.path(), "b.gguf");
    assert_eq!(resolve_model(dir.path(), "a.gguf"), Ok(a));
    let b = dir.path().join("b.gguf");
    assert_eq!(resolve_model(dir.path(), "  b.gguf "), Ok(b));
}

#[test]
fn the_extension_may_be_left_out() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "a.gguf");
    let b = touch(dir.path(), "translategemma-4b-it.Q4_K_M.gguf");
    assert_eq!(
        resolve_model(dir.path(), "translategemma-4b-it.Q4_K_M"),
        Ok(b)
    );
}

#[test]
fn an_absolute_path_is_used_wherever_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let file = touch(other.path(), "elsewhere.gguf");
    touch(dir.path(), "a.gguf");
    assert_eq!(
        resolve_model(dir.path(), file.to_str().unwrap()),
        Ok(file.clone())
    );
    let missing = other.path().join("missing.gguf");
    assert_eq!(
        resolve_model(dir.path(), missing.to_str().unwrap()),
        Err(TranslateError::ModelMissing(
            missing.to_str().unwrap().to_string()
        ))
    );
}

#[test]
fn an_unknown_name_with_one_file_uses_that_file() {
    let dir = tempfile::tempdir().unwrap();
    let only = touch(dir.path(), "model.gguf");
    assert_eq!(resolve_model(dir.path(), "translategemma:4b"), Ok(only));
}

#[test]
fn an_unknown_name_with_several_files_uses_the_first_by_name() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "qwen.gguf");
    let first = touch(dir.path(), "gemma.gguf");
    touch(dir.path(), "llama.gguf");
    assert_eq!(resolve_model(dir.path(), "translategemma:4b"), Ok(first));
}

#[test]
fn no_model_file_is_model_missing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_model(dir.path(), "translategemma:4b"),
        Err(TranslateError::ModelMissing("translategemma:4b".into()))
    );
    let absent = dir.path().join("does-not-exist");
    assert_eq!(
        resolve_model(&absent, "m"),
        Err(TranslateError::ModelMissing("m".into()))
    );
}

#[test]
fn files_that_are_not_gguf_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "README.md");
    touch(dir.path(), "model.bin");
    fs::create_dir(dir.path().join("folder.gguf")).unwrap();
    assert_eq!(
        resolve_model(dir.path(), "translategemma:4b"),
        Err(TranslateError::ModelMissing("translategemma:4b".into()))
    );
    let upper = touch(dir.path(), "Big.GGUF");
    assert_eq!(resolve_model(dir.path(), "translategemma:4b"), Ok(upper));
    assert!(is_gguf_name("a.gguf"));
    assert!(is_gguf_name("A.GgUf"));
    assert!(!is_gguf_name("a.gguf.part"));
    assert!(!is_gguf_name("gguf"));
}

#[test]
fn the_model_list_is_the_gguf_files_in_name_order() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(list_models(dir.path()), Ok(vec![]));
    touch(dir.path(), "b.gguf");
    touch(dir.path(), "a.gguf");
    touch(dir.path(), "notes.txt");
    fs::create_dir(dir.path().join("c.gguf")).unwrap();
    assert_eq!(
        list_models(dir.path()),
        Ok(vec!["a.gguf".to_string(), "b.gguf".to_string()])
    );
    assert_eq!(list_models(&dir.path().join("absent")), Ok(vec![]));
}

// ---------------------------------------------------------------------------------------------
// Threads, identity, prompt format
// ---------------------------------------------------------------------------------------------

#[test]
fn automatic_threads_are_half_the_cores_at_most_sixteen() {
    assert_eq!(auto_threads(0), 1);
    assert_eq!(auto_threads(1), 1);
    assert_eq!(auto_threads(4), 2);
    assert_eq!(auto_threads(16), 8);
    assert_eq!(auto_threads(32), 16);
    assert_eq!(auto_threads(64), 16);
    assert_eq!(MAX_AUTO_THREADS, 16);
    assert_eq!(threads_for(5), 5);
    let auto = threads_for(0);
    assert!((1..=16).contains(&auto), "{auto}");
}

#[test]
fn the_identity_names_the_file_and_the_target() {
    let id = identity_for("translategemma-4b-it.Q4_K_M.gguf", "ko");
    assert_eq!(id, "embedded|translategemma-4b-it.Q4_K_M.gguf|ko");
    assert_eq!(
        file_of_identity(&id),
        Some("translategemma-4b-it.Q4_K_M.gguf")
    );
    assert_eq!(file_of_identity("translategemma:4b|ko"), None);

    let dir = tempfile::tempdir().unwrap();
    let cfg = embedded_config("translategemma:4b");
    let none = EmbeddedBackend::new(&cfg, dir.path().to_path_buf());
    assert_eq!(none.identity(), "embedded|translategemma:4b|ko");
    touch(dir.path(), "x.gguf");
    let one = EmbeddedBackend::new(&cfg, dir.path().to_path_buf());
    assert_eq!(one.identity(), "embedded|x.gguf|ko");
    assert_ne!(
        one.identity(),
        refdesk_core::OllamaBackend::new(&cfg).identity()
    );
}

#[test]
fn gemma_chat_wraps_one_user_turn() {
    assert_eq!(
        gemma_chat("Hi"),
        "<start_of_turn>user\nHi<end_of_turn>\n<start_of_turn>model\n"
    );
}

// ---------------------------------------------------------------------------------------------
// UTF-8 assembly
// ---------------------------------------------------------------------------------------------

#[test]
fn utf8_pieces_wait_for_whole_characters() {
    let text = "생존 감시";
    let bytes = text.as_bytes();
    // "생" is three bytes: split 1 + 2, then "존" 2 + 1.
    let mut p = Utf8Pieces::default();
    assert_eq!(p.push(&bytes[..1]), None);
    assert_eq!(p.pending(), 1);
    assert_eq!(p.push(&bytes[1..3]).as_deref(), Some("생"));
    assert_eq!(p.push(&bytes[3..5]), None);
    assert_eq!(p.push(&bytes[5..7]).as_deref(), Some("존 "));
    let mut out = String::new();
    for b in &bytes[7..] {
        if let Some(s) = p.push(std::slice::from_ref(b)) {
            out.push_str(&s);
        }
    }
    assert_eq!(out, "감시");
    assert_eq!(p.pending(), 0);
}

#[test]
fn utf8_pieces_drop_an_unfinished_character_at_the_end() {
    let mut p = Utf8Pieces::default();
    let bytes = "a한".as_bytes();
    assert_eq!(p.push(&bytes[..3]).as_deref(), Some("a"));
    assert_eq!(p.pending(), 2);
    p.finish();
    assert_eq!(p.pending(), 0);
    assert_eq!(p.push(b"b").as_deref(), Some("b"));
}

#[test]
fn utf8_pieces_replace_invalid_bytes() {
    let mut p = Utf8Pieces::default();
    assert_eq!(p.push(&[b'x', 0xFF, b'y']).as_deref(), Some("x\u{FFFD}y"));
    assert_eq!(p.pending(), 0);
}

// ---------------------------------------------------------------------------------------------
// Without the engine
// ---------------------------------------------------------------------------------------------

#[cfg(not(feature = "embedded"))]
#[test]
fn without_the_feature_requests_fail_with_not_built_and_the_list_works() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "b.gguf");
    touch(dir.path(), "a.gguf");
    let cfg = TranslateConfig {
        models_dir: Some(dir.path().to_path_buf()),
        ..embedded_config("a")
    };
    let backend = backend_from_config(&cfg);
    let never = AtomicBool::new(false);
    let mut pieces = Vec::new();
    assert_eq!(
        backend.translate(&TranslateRequest::passage("Hello."), &never, &mut |p| {
            pieces.push(p.to_string())
        }),
        Err(TranslateError::NotBuilt)
    );
    assert!(pieces.is_empty());
    assert_eq!(backend.warm_up(&never), Err(TranslateError::NotBuilt));
    assert_eq!(
        backend.models(&never),
        Ok(vec!["a.gguf".to_string(), "b.gguf".to_string()])
    );
    assert_eq!(backend.identity(), "embedded|a.gguf|ko");
    backend.release();
    assert!(!EmbeddedBackend::built());
    assert!(
        TranslateError::NotBuilt
            .to_string()
            .contains("--features embedded-vulkan")
    );
}

// ---------------------------------------------------------------------------------------------
// With the engine
// ---------------------------------------------------------------------------------------------

#[cfg(feature = "embedded")]
mod engine {
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// One real model in memory at a time.
    static MODEL_LOCK: Mutex<()> = Mutex::new(());

    /// The model file of `REFDESK_TEST_GGUF`, or `None` (and a note) to skip.
    fn test_model() -> Option<PathBuf> {
        match std::env::var_os("REFDESK_TEST_GGUF").map(PathBuf::from) {
            Some(p) if p.is_file() => Some(p),
            Some(p) => {
                println!("skipped: REFDESK_TEST_GGUF={} is not a file", p.display());
                None
            }
            None => {
                println!("skipped: REFDESK_TEST_GGUF is not set");
                None
            }
        }
    }

    fn backend_for(file: &Path, cfg: TranslateConfig) -> EmbeddedBackend {
        let dir = file.parent().unwrap().to_path_buf();
        let name = file.file_name().unwrap().to_str().unwrap().to_string();
        EmbeddedBackend::new(&TranslateConfig { model: name, ..cfg }, dir)
    }

    fn run(backend: &dyn TranslateBackend, req: &TranslateRequest) -> (Vec<String>, String) {
        let never = AtomicBool::new(false);
        let mut pieces = Vec::new();
        let result = backend.translate(req, &never, &mut |p| pieces.push(p.to_string()));
        assert_eq!(result, Ok(()), "pieces so far: {pieces:?}");
        let text = pieces.concat();
        (pieces, text)
    }

    fn has_hangul(s: &str) -> bool {
        s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c))
    }

    /// Releases the model and waits until the engine thread dropped it.
    fn unload(backend: &EmbeddedBackend) {
        backend.release();
        let deadline = Instant::now() + Duration::from_secs(20);
        while backend.loaded() {
            assert!(Instant::now() < deadline, "the model was not released");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    const THREE_SENTENCES: &str = "A Function Group is a set of coherent Processes, which need to be controlled consistently. Depending on the state of the Function Group, Processes are started or terminated. Processes can belong to more than one Function Group State, but not to different Function Groups.";

    #[test]
    fn a_missing_model_file_is_model_missing() {
        let dir = tempfile::tempdir().unwrap();
        let backend = backend_from_config(&TranslateConfig {
            models_dir: Some(dir.path().to_path_buf()),
            ..embedded_config("m")
        });
        let never = AtomicBool::new(false);
        assert_eq!(
            backend.warm_up(&never),
            Err(TranslateError::ModelMissing("m".into()))
        );
        let gone = dir.path().join("gone.gguf");
        let backend = EmbeddedBackend::new(
            &embedded_config(gone.to_str().unwrap()),
            dir.path().to_path_buf(),
        );
        assert!(matches!(
            backend.translate(&TranslateRequest::passage("x"), &never, &mut |_| {}),
            Err(TranslateError::ModelMissing(_))
        ));
        assert!(!backend.loaded());
        assert!(EmbeddedBackend::built());
    }

    #[test]
    fn a_file_that_is_not_gguf_is_an_engine_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fake.gguf"), b"this is not a model at all").unwrap();
        fs::write(dir.path().join("tiny.gguf"), b"GG").unwrap();
        let never = AtomicBool::new(false);
        for name in ["fake.gguf", "tiny.gguf"] {
            let backend = EmbeddedBackend::new(&embedded_config(name), dir.path().to_path_buf());
            match backend.warm_up(&never) {
                Err(TranslateError::Engine(why)) => assert!(why.contains(name), "{why}"),
                other => panic!("{name}: {other:?}"),
            }
        }
        // Right magic, broken content: llama.cpp itself refuses it.
        let mut broken = b"GGUF".to_vec();
        broken.extend_from_slice(&[0xFF; 64]);
        fs::write(dir.path().join("broken.gguf"), broken).unwrap();
        let backend =
            EmbeddedBackend::new(&embedded_config("broken.gguf"), dir.path().to_path_buf());
        assert!(
            matches!(backend.warm_up(&never), Err(TranslateError::Engine(_))),
            "a broken file must fail without killing the process"
        );
    }

    #[test]
    fn a_cancelled_call_returns_at_once_even_before_the_engine_started() {
        let dir = tempfile::tempdir().unwrap();
        let backend = EmbeddedBackend::new(&embedded_config("m"), dir.path().to_path_buf());
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            backend.translate(&TranslateRequest::passage("x"), &cancelled, &mut |_| {}),
            Err(TranslateError::Cancelled)
        );
    }

    #[test]
    fn gguf_loads_and_translates_a_word_and_a_passage() {
        let Some(file) = test_model() else { return };
        let _one = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let backend = backend_for(&file, embedded_config(""));
        let never = AtomicBool::new(false);
        let started = Instant::now();
        assert_eq!(backend.warm_up(&never), Ok(()));
        println!("warm-up: {} ms", started.elapsed().as_millis());
        assert!(backend.loaded());
        let name = file.file_name().unwrap().to_str().unwrap();
        assert_eq!(backend.identity(), format!("embedded|{name}|ko"));
        // Loaded already: the second warm-up ends at once.
        let again = Instant::now();
        assert_eq!(backend.warm_up(&never), Ok(()));
        assert!(again.elapsed() < Duration::from_secs(1));

        let word = TranslateRequest::word(
            "liveness",
            "The watchdog supervises the liveness of each process.",
        );
        let (_, text) = run(&backend, &word);
        println!("word: {text:?}");
        assert!(has_hangul(&text), "{text:?}");

        let (pieces, text) = run(&backend, &TranslateRequest::passage(THREE_SENTENCES));
        println!("passage in {} pieces: {text:?}", pieces.len());
        assert!(has_hangul(&text), "{text:?}");
        assert!(pieces.len() > 3, "{pieces:?}");
        assert!(pieces.iter().all(|p| !p.contains('\u{FFFD}')), "{pieces:?}");
        unload(&backend);
    }

    #[test]
    fn gguf_a_cancel_during_generation_ends_quickly_and_the_next_request_works() {
        let Some(file) = test_model() else { return };
        let _one = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let backend = backend_for(&file, embedded_config(""));
        let never = AtomicBool::new(false);
        assert_eq!(backend.warm_up(&never), Ok(()));

        let cancel = AtomicBool::new(false);
        let (first_piece, got_piece) = mpsc::channel();
        let (result, latency) = std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let mut sent = false;
                let r = backend.translate(
                    &TranslateRequest::passage(THREE_SENTENCES),
                    &cancel,
                    &mut |_| {
                        if !sent {
                            sent = true;
                            let _ = first_piece.send(());
                        }
                    },
                );
                (r, Instant::now())
            });
            got_piece
                .recv_timeout(Duration::from_secs(60))
                .expect("a first piece");
            let set_at = Instant::now();
            cancel.store(true, Ordering::Relaxed);
            let (r, returned) = worker.join().unwrap();
            (r, returned.duration_since(set_at))
        });
        println!("cancel latency: {} ms", latency.as_millis());
        assert_eq!(result, Err(TranslateError::Cancelled));
        assert!(latency < Duration::from_millis(500), "{latency:?}");

        let (_, text) = run(
            &backend,
            &TranslateRequest::passage("The process is started."),
        );
        assert!(has_hangul(&text), "{text:?}");
        unload(&backend);
    }

    #[test]
    fn gguf_after_release_the_next_request_loads_the_model_again() {
        let Some(file) = test_model() else { return };
        let _one = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let backend = backend_for(&file, embedded_config(""));
        let never = AtomicBool::new(false);
        assert_eq!(backend.warm_up(&never), Ok(()));
        unload(&backend);
        assert!(!backend.loaded());
        let (_, text) = run(
            &backend,
            &TranslateRequest::word("deadline", "The deadline supervision checks it."),
        );
        assert!(has_hangul(&text), "{text:?}");
        assert!(backend.loaded());
        unload(&backend);
    }

    #[test]
    fn gguf_the_model_is_dropped_after_the_keep_alive_time() {
        let Some(file) = test_model() else { return };
        let _one = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let backend =
            backend_for(&file, embedded_config("")).with_keep_alive(Duration::from_millis(300));
        run(&backend, &TranslateRequest::word("bus", "the CAN bus"));
        assert!(backend.loaded());
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.loaded() {
            assert!(Instant::now() < deadline, "still loaded");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn gguf_input_longer_than_the_context_is_cut_not_fatal() {
        let Some(file) = test_model() else { return };
        let _one = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = TranslateConfig {
            context_tokens: 256,
            ..embedded_config("")
        };
        let backend = backend_for(&file, cfg);
        let long = THREE_SENTENCES.repeat(8);
        assert!(long.len() > 2000);
        let (_, text) = run(&backend, &TranslateRequest::passage(long));
        println!("cut passage: {text:?}");
        assert!(!text.trim().is_empty());
        unload(&backend);
    }

    #[test]
    fn gguf_dropping_the_backend_while_it_generates_stops_it_and_frees_the_model() {
        let Some(file) = test_model() else { return };
        let _one = MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let backend = std::sync::Arc::new(backend_for(&file, embedded_config("")));
        let never = AtomicBool::new(false);
        assert_eq!(backend.warm_up(&never), Ok(()));
        let job = refdesk_core::TranslateJob::start(
            backend.clone(),
            TranslateRequest::passage(THREE_SENTENCES),
            std::sync::Arc::new(|| {}),
        );
        std::thread::sleep(Duration::from_millis(150));
        drop(job);
        // The job thread lets go of its handle within ~20 ms of the cancel.
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::sync::Arc::strong_count(&backend) > 1 {
            assert!(Instant::now() < deadline, "the job kept the backend");
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        drop(backend);
        // The drop cancels the generation and waits for the model to be freed, briefly.
        let took = started.elapsed();
        println!("drop while generating: {} ms", took.as_millis());
        assert!(took < Duration::from_secs(3), "{took:?}");
    }
}
