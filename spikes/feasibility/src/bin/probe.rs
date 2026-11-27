//! 헤드리스 타당성 검증: mupdf 크레이트로 열기/목차/링크/텍스트/렌더링이 되는지와 대략의 소요 시간.
use mupdf::{Colorspace, Document, Matrix, Outline, TextPageFlags};
use std::time::Instant;

fn outline_stats(items: &[Outline], depth: usize, count: &mut usize, max_depth: &mut usize, with_dest: &mut usize) {
    for o in items {
        *count += 1;
        *max_depth = (*max_depth).max(depth);
        if o.dest.is_some() {
            *with_dest += 1;
        }
        outline_stats(&o.down, depth + 1, count, max_depth, with_dest);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for path in std::env::args().skip(1) {
        let t = Instant::now();
        let doc = Document::open(path.as_str())?;
        let n = doc.page_count()?;
        let open = t.elapsed();

        let t = Instant::now();
        let outlines = doc.outlines()?;
        let (mut oc, mut od, mut ow) = (0, 0, 0);
        outline_stats(&outlines, 1, &mut oc, &mut od, &mut ow);
        let outline_t = t.elapsed();

        let t = Instant::now();
        let (mut links, mut internal, mut chars, mut lines) = (0usize, 0usize, 0usize, 0usize);
        for i in 0..n {
            let page = doc.load_page(i)?;
            for l in page.links()? {
                links += 1;
                if l.dest.is_some() {
                    internal += 1;
                }
            }
            let tp = page.to_text_page(TextPageFlags::empty())?;
            for b in tp.blocks() {
                for line in b.lines() {
                    lines += 1;
                    chars += line.chars().count();
                }
            }
        }
        let scan = t.elapsed();

        let t = Instant::now();
        let page = doc.load_page(0)?;
        let b = page.bounds()?;
        let pm = page.to_pixmap(&Matrix::new_scale(2.0, 2.0), &Colorspace::device_rgb(), false, true)?;
        let render = t.elapsed();

        let t = Instant::now();
        let dl = page.to_display_list(true)?;
        let pm2 = std::thread::spawn(move || {
            dl.to_pixmap(&Matrix::new_scale(2.0, 2.0), &Colorspace::device_rgb(), false).map(|p| (p.width(), p.height()))
        })
        .join()
        .unwrap()?;
        let dl_render = t.elapsed();

        println!(
            "{}\n  pages={n} open={open:?}\n  outline: entries={oc} depth={od} with_dest={ow} ({outline_t:?})\n  all pages: links={links} (internal={internal}) lines={lines} chars={chars} scan={scan:?}\n  page0 {:.0}x{:.0}pt -> {}x{} px n={} render@2x={render:?}; display-list on worker thread -> {}x{} ({dl_render:?})",
            path.rsplit('/').next().unwrap_or(&path),
            b.x1 - b.x0, b.y1 - b.y0, pm.width(), pm.height(), pm.n(), pm2.0, pm2.1,
        );
    }
    Ok(())
}
