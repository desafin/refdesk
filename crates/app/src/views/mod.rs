//! Tab contents: the PDF view, the texture cache it draws from, the page preloader, the
//! comparison view and the start page.

pub mod diff_view;
pub mod pdf_view;
pub mod preload;
pub mod selection;
pub mod start_page;
pub mod textures;

/// Helpers for tests that wait on document workers without sleeping.
#[cfg(test)]
pub mod test_support {
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::{Duration, Instant};

    /// Deadline for anything a worker does. Far above what a slow machine needs.
    pub const TIMEOUT: Duration = Duration::from_secs(60);

    /// Repaint requests of an egui context, as a channel. Documents opened through `dispatch`
    /// request a repaint whenever they have something new.
    pub struct Repaints(Receiver<()>);

    /// Starts listening to the repaint requests of `ctx`.
    pub fn watch_repaints(ctx: &egui::Context) -> Repaints {
        let (tx, rx) = mpsc::channel();
        // `Sender` is not `Sync`; the callback must be.
        let tx = Mutex::new(tx);
        ctx.set_request_repaint_callback(move |_| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(());
            }
        });
        Repaints(rx)
    }

    impl Repaints {
        /// Calls `check` until it returns true, re-checking after every repaint request.
        /// Panics after [`TIMEOUT`].
        pub fn wait_until(&self, what: &str, mut check: impl FnMut() -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            while !check() {
                self.wait_one(what, deadline);
            }
        }

        /// Blocks until the next repaint request. Panics at `deadline`.
        pub fn wait_one(&self, what: &str, deadline: Instant) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.0.recv_timeout(left) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
                Err(RecvTimeoutError::Disconnected) => panic!("repaint channel closed"),
            }
        }
    }
}
