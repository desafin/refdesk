//! Tries the configured server first, then the local model on connection/server failures.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{TranslateBackend, TranslateError, TranslateRequest};

/// A server backend with an embedded model as an offline fallback.
pub struct FallbackBackend {
    primary: Arc<dyn TranslateBackend>,
    local: Arc<dyn TranslateBackend>,
    local_active: AtomicBool,
}

impl FallbackBackend {
    /// Creates a server-first backend.
    pub fn new(
        primary: Arc<dyn TranslateBackend>,
        local: Arc<dyn TranslateBackend>,
    ) -> FallbackBackend {
        FallbackBackend {
            primary,
            local,
            local_active: AtomicBool::new(false),
        }
    }

    fn can_fallback(error: &TranslateError) -> bool {
        matches!(error, TranslateError::Unreachable(_))
            || matches!(
                error,
                TranslateError::Http {
                    status: 500..=599,
                    ..
                }
            )
    }

    fn both_failed(server: TranslateError, local: TranslateError) -> TranslateError {
        TranslateError::Fallback {
            server: Box::new(server),
            local: Box::new(local),
        }
    }
}

impl TranslateBackend for FallbackBackend {
    fn translate(
        &self,
        req: &TranslateRequest,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        if self.local_active.load(Ordering::Acquire) {
            return self.local.translate(req, cancel, on_text);
        }

        let emitted = std::cell::Cell::new(false);
        let result = self.primary.translate(req, cancel, &mut |piece| {
            emitted.set(true);
            on_text(piece);
        });
        match result {
            Ok(()) => {
                self.local_active.store(false, Ordering::Release);
                Ok(())
            }
            Err(server_error)
                if !emitted.get()
                    && Self::can_fallback(&server_error)
                    && !cancel.load(Ordering::Relaxed) =>
            {
                match self.local.translate(req, cancel, on_text) {
                    Ok(()) => {
                        self.local_active.store(true, Ordering::Release);
                        Ok(())
                    }
                    Err(local_error) => Err(Self::both_failed(server_error, local_error)),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn warm_up(&self, cancel: &AtomicBool) -> Result<(), TranslateError> {
        match self.primary.warm_up(cancel) {
            Ok(()) => {
                self.local_active.store(false, Ordering::Release);
                Ok(())
            }
            Err(server_error)
                if Self::can_fallback(&server_error) && !cancel.load(Ordering::Relaxed) =>
            {
                match self.local.warm_up(cancel) {
                    Ok(()) => {
                        self.local_active.store(true, Ordering::Release);
                        Ok(())
                    }
                    Err(local_error) => Err(Self::both_failed(server_error, local_error)),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn models(&self, cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
        let models = self.primary.models(cancel)?;
        self.local_active.store(false, Ordering::Release);
        Ok(models)
    }

    fn identity(&self) -> String {
        format!(
            "fallback|{}|{}",
            self.primary.identity(),
            self.local.identity()
        )
    }

    fn local_fallback_active(&self) -> bool {
        self.local_active.load(Ordering::Acquire)
    }

    fn release(&self) {
        self.primary.release();
        self.local.release();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::translate::{TranslateKind, TranslateRequest};

    struct StubBackend {
        name: &'static str,
        warm_up_error: Option<TranslateError>,
        translate_error: Option<TranslateError>,
        piece: Option<&'static str>,
    }

    impl TranslateBackend for StubBackend {
        fn translate(
            &self,
            _req: &TranslateRequest,
            _cancel: &AtomicBool,
            on_text: &mut dyn FnMut(&str),
        ) -> Result<(), TranslateError> {
            if let Some(piece) = self.piece {
                on_text(piece);
            }
            self.translate_error.clone().map_or(Ok(()), Err)
        }

        fn warm_up(&self, _cancel: &AtomicBool) -> Result<(), TranslateError> {
            self.warm_up_error.clone().map_or(Ok(()), Err)
        }

        fn models(&self, _cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
            Ok(vec![self.name.to_string()])
        }

        fn identity(&self) -> String {
            self.name.to_string()
        }
    }

    fn stub(
        name: &'static str,
        warm_up_error: Option<TranslateError>,
        translate_error: Option<TranslateError>,
        piece: Option<&'static str>,
    ) -> Arc<dyn TranslateBackend> {
        Arc::new(StubBackend {
            name,
            warm_up_error,
            translate_error,
            piece,
        })
    }

    #[test]
    fn unreachable_server_uses_local_backend_for_warmup_and_translation() {
        let backend = FallbackBackend::new(
            stub(
                "server",
                Some(TranslateError::Unreachable("offline".into())),
                Some(TranslateError::Unreachable("offline".into())),
                None,
            ),
            stub("local", None, None, Some("local answer")),
        );
        let cancel = AtomicBool::new(false);
        backend.warm_up(&cancel).unwrap();
        assert!(backend.local_fallback_active());
        assert_eq!(backend.identity(), "fallback|server|local");

        let mut output = String::new();
        backend
            .translate(&TranslateRequest::passage("text"), &cancel, &mut |piece| {
                output.push_str(piece);
            })
            .unwrap();
        assert_eq!(output, "local answer");
    }

    #[test]
    fn server_errors_that_are_not_outages_do_not_fall_back() {
        let backend = FallbackBackend::new(
            stub(
                "server",
                Some(TranslateError::Http {
                    status: 401,
                    message: "unauthorized".into(),
                }),
                None,
                None,
            ),
            stub("local", None, None, None),
        );
        let error = backend.warm_up(&AtomicBool::new(false)).unwrap_err();
        assert!(matches!(error, TranslateError::Http { status: 401, .. }));
        assert!(!backend.local_fallback_active());
    }

    #[test]
    fn a_partial_server_answer_is_not_mixed_with_a_local_answer() {
        let backend = FallbackBackend::new(
            stub(
                "server",
                None,
                Some(TranslateError::Unreachable("connection lost".into())),
                Some("partial"),
            ),
            stub("local", None, None, Some("local")),
        );
        let mut output = String::new();
        let error = backend
            .translate(
                &TranslateRequest {
                    text: "word".into(),
                    kind: TranslateKind::Word {
                        context: String::new(),
                    },
                },
                &AtomicBool::new(false),
                &mut |piece| output.push_str(piece),
            )
            .unwrap_err();
        assert_eq!(output, "partial");
        assert!(matches!(error, TranslateError::Unreachable(_)));
        assert!(!backend.local_fallback_active());
    }
}
