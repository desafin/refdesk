//! Times the document engine on a real PDF: opening, rendering the first 20 pages at scale 2.0
//! and extracting the text of every page.
//!
//! `cargo run --release -p refdesk-core --example engine_timing -- file.pdf`

use std::error::Error;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::{DocStatus, DocumentHandle, RenderKey};

fn main() -> Result<(), Box<dyn Error>> {
    let path: PathBuf = std::env::args_os()
        .nth(1)
        .ok_or("usage: engine_timing FILE.pdf")?
        .into();

    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let started = Instant::now();
    let handle = DocumentHandle::open(
        &path,
        Arc::new(move || {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(());
            }
        }),
    );
    while handle.status() == DocStatus::Loading {
        rx.recv_timeout(Duration::from_secs(120))?;
    }
    let open = started.elapsed();
    let info = match handle.status() {
        DocStatus::Ready => handle.info().ok_or("ready without info")?,
        other => return Err(format!("cannot open {}: {other:?}", path.display()).into()),
    };
    let pages = info.page_sizes.len() as u32;
    println!("{}", path.display());
    println!(
        "  open: {open:?} ({pages} pages, {} outline items, {} page labels)",
        info.outline.items.len(),
        info.page_labels.len()
    );

    let mut total = Duration::ZERO;
    let n = pages.min(20);
    for page in 0..n {
        let t = Instant::now();
        let r = handle.render_wait(RenderKey {
            page,
            scale_milli: 2000,
            generation: info.generation,
        })?;
        let took = t.elapsed();
        total += took;
        println!(
            "  render page {:>3} @2.0: {}x{} in {took:?}",
            page + 1,
            r.width,
            r.height
        );
    }
    if n > 0 {
        println!("  render average: {:?}", total / n);
    }

    let t = Instant::now();
    let mut chars = 0usize;
    for page in 0..pages {
        chars += handle.page_text_wait(page)?.chars.len();
    }
    let took = t.elapsed();
    println!(
        "  text of all {pages} pages: {chars} chars in {took:?} ({:?} per page)",
        took / pages.max(1)
    );
    Ok(())
}
