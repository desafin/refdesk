//! Checks that `testutil::PdfBuilder` writes PDFs that MuPDF opens and reads back as intended.
//! (Rule R-2 applies to `src/` only, so this file uses `mupdf` directly.)

use std::error::Error;
use std::time::Instant;

use mupdf::{DestinationKind, Document, Outline, TextPageFlags};
use refdesk_core::testutil::{FlowOptions, PdfBuilder, flow_document};

type R = Result<(), Box<dyn Error>>;

fn open(pdf: &[u8]) -> Result<Document, Box<dyn Error>> {
    Ok(Document::from_bytes(pdf, "application/pdf")?)
}

/// Text lines of a page: (text, left, top, right, bottom, baseline y) in MuPDF's coordinates.
struct Line {
    text: String,
    bbox: (f32, f32, f32, f32),
    baseline: f32,
}

fn lines(doc: &Document, page: i32) -> Result<Vec<Line>, Box<dyn Error>> {
    let page = doc.load_page(page)?;
    let tp = page.to_text_page(TextPageFlags::empty())?;
    let mut out = Vec::new();
    for b in tp.blocks() {
        for l in b.lines() {
            let text: String = l.chars().filter_map(|c| c.char()).collect();
            let baseline = l.chars().next().map_or(0.0, |c| c.origin().y);
            let r = l.bounds();
            out.push(Line {
                text,
                bbox: (r.x0, r.y0, r.x1, r.y1),
                baseline,
            });
        }
    }
    Ok(out)
}

fn flat_outline<'a>(items: &'a [Outline], depth: usize, out: &mut Vec<(usize, &'a str)>) {
    for o in items {
        out.push((depth, o.title.as_str()));
        flat_outline(&o.down, depth + 1, out);
    }
}

#[test]
fn opens_with_page_count_and_sizes() -> R {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |_| {})
        .page(300.5, 400.0, |p| {
            p.text(10.0, 20.0, 10.0, "x");
        })
        .page(842.0, 595.0, |_| {})
        .build();
    let doc = open(&pdf)?;
    assert_eq!(doc.page_count()?, 3);
    let sizes: Vec<(f32, f32)> = (0..3)
        .map(|i| {
            let b = doc.load_page(i)?.bounds()?;
            Ok((b.x1 - b.x0, b.y1 - b.y0))
        })
        .collect::<Result<_, Box<dyn Error>>>()?;
    assert_eq!(sizes, vec![(612.0, 792.0), (300.5, 400.0), (842.0, 595.0)]);
    Ok(())
}

#[test]
fn xref_offsets_are_exact() {
    let pdf = PdfBuilder::new()
        .page(100.0, 100.0, |p| {
            p.text(10.0, 20.0, 10.0, "x");
        })
        .outline_leaf("t", 1, 10.0)
        .build();
    let text = String::from_utf8(pdf).expect("ASCII output");
    let startxref: usize = text
        .rsplit("startxref\n")
        .next()
        .and_then(|s| s.lines().next())
        .and_then(|s| s.parse().ok())
        .expect("startxref");
    assert!(text[startxref..].starts_with("xref\n"));
    let table: Vec<&str> = text[startxref..]
        .lines()
        .skip(2)
        .take_while(|l| l.len() == 19)
        .collect();
    assert!(table.len() > 3);
    for (n, entry) in table.iter().enumerate().skip(1) {
        let off: usize = entry[..10].parse().expect("offset");
        assert!(
            text[off..].starts_with(&format!("{n} 0 obj")),
            "object {n} is not at offset {off}"
        );
    }
}

#[test]
fn extracted_text_matches_including_escapes() -> R {
    let strings = [
        "Hello, world",
        "f(x) = (a + b) \\ c",
        "unbalanced ( paren",
        "close ) first",
        "100% sure #1 [ok] {yes}",
    ];
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            for (i, s) in strings.iter().enumerate() {
                p.text(72.0, 100.0 + 30.0 * i as f32, 12.0, s);
            }
        })
        .build();
    let doc = open(&pdf)?;
    let got: Vec<String> = lines(&doc, 0)?.into_iter().map(|l| l.text).collect();
    assert_eq!(got, strings);
    Ok(())
}

#[test]
fn non_ascii_becomes_question_mark() -> R {
    let pdf = PdfBuilder::new()
        .page(200.0, 200.0, |p| {
            p.text(10.0, 50.0, 12.0, "caf\u{e9} \u{d55c}");
        })
        .build();
    let doc = open(&pdf)?;
    let got: Vec<String> = lines(&doc, 0)?.into_iter().map(|l| l.text).collect();
    assert_eq!(got, ["caf? ?"]);
    Ok(())
}

#[test]
fn text_position_is_page_space_baseline() -> R {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "1 Introduction");
            p.text(300.0, 700.0, 20.0, "low");
        })
        .build();
    let doc = open(&pdf)?;
    let ls = lines(&doc, 0)?;
    assert_eq!(ls.len(), 2);

    let first = &ls[0];
    assert_eq!(first.text, "1 Introduction");
    assert!(
        (first.baseline - 100.0).abs() < 0.5,
        "baseline {}",
        first.baseline
    );
    let (l, t, r, b) = first.bbox;
    assert!((l - 72.0).abs() < 1.0, "left {l}");
    assert!(
        t < 100.0 && b > 100.0,
        "bbox must straddle the baseline: {t}..{b}"
    );
    assert!(t > 100.0 - 15.0 && b < 100.0 + 15.0, "bbox {t}..{b}");
    assert!(r > l);

    // Lower on the page means a larger y (page space points downwards).
    assert!((ls[1].baseline - 700.0).abs() < 0.5);
    assert!(ls[1].baseline > first.baseline);
    Ok(())
}

#[test]
fn outline_titles_depth_and_count() -> R {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |_| {})
        .page(612.0, 792.0, |_| {})
        .page(612.0, 792.0, |_| {})
        .outline("1 Introduction", 1, 100.0, |o| {
            o.child_with("1.1 Scope", 1, 300.0, |o| {
                o.child("1.1.1 Audience", 2, 120.0);
                o.child("1.1.2 Terms", 2, 220.0);
            });
            o.child("1.2 Overview", 2, 400.0);
        })
        .outline("2 Details", 3, 50.0, |o| {
            o.child_with("2.1 Format", 3, 200.0, |o| {
                o.child("2.1.1 Header", 3, 300.0);
            });
        })
        .outline_leaf("3 Appendix", 3, 600.0)
        .build();
    let doc = open(&pdf)?;
    let outline = doc.outlines()?;
    let mut flat = Vec::new();
    flat_outline(&outline, 0, &mut flat);
    assert_eq!(
        flat,
        vec![
            (0, "1 Introduction"),
            (1, "1.1 Scope"),
            (2, "1.1.1 Audience"),
            (2, "1.1.2 Terms"),
            (1, "1.2 Overview"),
            (0, "2 Details"),
            (1, "2.1 Format"),
            (2, "2.1.1 Header"),
            (0, "3 Appendix"),
        ]
    );
    assert_eq!(outline.len(), 3);

    // Every entry resolves to its page.
    fn pages(items: &[Outline], out: &mut Vec<u32>) {
        for o in items {
            out.push(o.dest.map_or(u32::MAX, |d| d.loc.page_number));
            pages(&o.down, out);
        }
    }
    let mut got = Vec::new();
    pages(&outline, &mut got);
    assert_eq!(got, vec![0, 0, 1, 1, 1, 2, 2, 2, 2]);
    Ok(())
}

#[test]
fn links_count_destination_page_and_uri() -> R {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.link_to((72.0, 90.0, 200.0, 104.0), 2, Some((72.0, 100.0)));
            p.link_to((72.0, 120.0, 200.0, 134.0), 3, None);
            p.link_uri((72.0, 150.0, 300.0, 164.0), "https://example.com/a(b)?q=1");
        })
        .page(612.0, 792.0, |_| {})
        .page(612.0, 792.0, |p| {
            p.link_to((10.0, 10.0, 50.0, 30.0), 1, Some((0.0, 0.0)));
        })
        .build();
    let doc = open(&pdf)?;

    let page1: Vec<_> = doc.load_page(0)?.links()?.collect();
    assert_eq!(page1.len(), 3);
    assert_eq!(doc.load_page(1)?.links()?.count(), 0);
    assert_eq!(doc.load_page(2)?.links()?.count(), 1);

    let internal = page1[0].dest.expect("internal link");
    assert_eq!(internal.loc.page_number, 1);
    let fit = page1[1].dest.expect("fit link");
    assert_eq!(fit.loc.page_number, 2);
    assert_eq!(fit.kind, DestinationKind::Fit);
    assert!(page1[2].dest.is_none());
    assert_eq!(page1[2].uri, "https://example.com/a(b)?q=1");

    // Link rectangles come back in MuPDF's page coordinates (top left origin).
    let b = page1[0].bounds;
    assert!((b.x0 - 72.0).abs() < 0.5 && (b.x1 - 200.0).abs() < 0.5);
    assert!(
        (b.y0 - 90.0).abs() < 0.5 && (b.y1 - 104.0).abs() < 0.5,
        "{b:?}"
    );
    Ok(())
}

/// Records what MuPDF reports as the destination point, which the engine has to normalise (§3).
#[test]
fn link_destination_coordinates_as_reported_by_mupdf() -> R {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.link_to((72.0, 90.0, 200.0, 104.0), 2, Some((72.0, 100.0)));
        })
        .page(612.0, 792.0, |_| {})
        .outline_leaf("Target", 2, 100.0)
        .build();
    let doc = open(&pdf)?;
    let link = doc.load_page(0)?.links()?.next().expect("a link");
    let dest = link.dest.expect("destination");
    println!(
        "link dest (page space (72, 100) on page 2): {:?}",
        dest.kind
    );
    let DestinationKind::XYZ { left, top, .. } = dest.kind else {
        panic!("expected XYZ, got {:?}", dest.kind);
    };
    assert_eq!(dest.loc.page_number, 1);
    // Pins the observed MuPDF behaviour: top is measured from the top of the page, in points.
    assert!((left.expect("left") - 72.0).abs() < 0.5, "left {left:?}");
    assert!((top.expect("top") - 100.0).abs() < 0.5, "top {top:?}");

    let outline = doc.outlines()?;
    let od = outline[0].dest.expect("outline dest");
    println!("outline dest (page space y 100 on page 2): {:?}", od.kind);
    Ok(())
}

fn paragraphs(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let words = 8 + (i * 7) % 40;
            let body: Vec<String> = (0..words).map(|w| format!("w{}x{}", i, w)).collect();
            format!("P{i}: {}", body.join(" "))
        })
        .collect()
}

fn flow_opts() -> FlowOptions {
    FlowOptions::default()
        .header(|n| format!("Header page {n}"))
        .footer(|n| format!("Footer page {n}"))
}

/// First line of a page that is neither header nor footer.
fn first_body_line(doc: &Document, page: i32) -> Result<String, Box<dyn Error>> {
    Ok(lines(doc, page)?
        .into_iter()
        .map(|l| l.text)
        .find(|t| !t.starts_with("Header page") && !t.starts_with("Footer page"))
        .unwrap_or_default())
}

#[test]
fn flow_layout_pages_text_inside_header_footer_everywhere() -> R {
    let opts = flow_opts();
    let pdf = flow_document(&paragraphs(200), &opts).build();
    let doc = open(&pdf)?;
    let n = doc.page_count()?;
    assert!(n > 5, "200 paragraphs should span many pages, got {n}");

    let mut body_words = 0;
    for i in 0..n {
        let page = doc.load_page(i)?;
        let b = page.bounds()?;
        let ls = lines(&doc, i)?;
        for l in &ls {
            let (x0, y0, x1, y1) = l.bbox;
            assert!(
                x0 >= b.x0 && y0 >= b.y0 && x1 <= b.x1 && y1 <= b.y1,
                "page {i}: {:?} {:?} outside {b:?}",
                l.text,
                l.bbox
            );
            // The body stays inside the margins.
            if !l.text.starts_with("Header") && !l.text.starts_with("Footer") {
                assert!(
                    x1 <= b.x1 - opts.margin + 0.5,
                    "page {i}: line too wide: {:?}",
                    l.text
                );
                assert!(
                    l.baseline <= b.y1 - opts.margin + 0.5,
                    "page {i}: below bottom margin"
                );
                body_words += l.text.split(' ').count();
            }
        }
        let texts: Vec<&str> = ls.iter().map(|l| l.text.as_str()).collect();
        assert!(
            texts.contains(&format!("Header page {}", i + 1).as_str()),
            "page {i} header"
        );
        assert!(
            texts.contains(&format!("Footer page {}", i + 1).as_str()),
            "page {i} footer"
        );
    }
    let expected: usize = paragraphs(200).iter().map(|p| p.split(' ').count()).sum();
    assert_eq!(body_words, expected, "no word may be lost or duplicated");
    Ok(())
}

#[test]
fn flow_layout_insertion_shifts_later_pages() -> R {
    let base = paragraphs(200);
    let mut with_extra = base.clone();
    with_extra.insert(3, "Inserted paragraph near the front. ".repeat(6));

    let opts = flow_opts();
    let a = open(&flow_document(&base, &opts).build())?;
    let b = open(&flow_document(&with_extra, &opts).build())?;
    let (na, nb) = (a.page_count()?, b.page_count()?);
    assert!(nb == na || nb == na + 1, "pages {na} -> {nb}");

    // The first page differs too, but the point is that pages further back moved.
    for page in [1, na / 2, na - 1] {
        assert_ne!(
            first_body_line(&a, page)?,
            first_body_line(&b, page)?,
            "page {page} should start with different text"
        );
    }
    Ok(())
}

#[test]
fn flow_layout_is_deterministic_and_handles_long_words() -> R {
    let long = "z".repeat(400);
    let paras = vec![
        "short".to_owned(),
        long.clone(),
        String::new(),
        "end".to_owned(),
    ];
    let pdf = flow_document(&paras, &FlowOptions::default()).build();
    assert_eq!(pdf, flow_document(&paras, &FlowOptions::default()).build());
    let doc = open(&pdf)?;
    let page = doc.load_page(0)?;
    let b = page.bounds()?;
    let ls = lines(&doc, 0)?;
    let joined: String = ls.iter().map(|l| l.text.as_str()).collect();
    assert!(
        joined.contains(&long),
        "all letters of the long word are kept"
    );
    for l in ls {
        assert!(l.bbox.2 <= b.x1 - 72.0 + 0.5, "{:?} too wide", l.text);
    }
    Ok(())
}

#[test]
fn hundred_pages_build_in_under_a_second() {
    let start = Instant::now();
    let mut doc = PdfBuilder::new();
    for i in 0..100 {
        doc = doc.page(612.0, 792.0, |p| {
            for line in 0..45 {
                p.text(
                    72.0,
                    100.0 + 14.0 * line as f32,
                    12.0,
                    &format!("page {i} line {line} lorem ipsum"),
                );
            }
            p.link_to(
                (72.0, 90.0, 200.0, 104.0),
                (i + 1) % 100 + 1,
                Some((72.0, 100.0)),
            );
        });
    }
    let pdf = doc.outline_leaf("start", 1, 0.0).build();
    let elapsed = start.elapsed();
    assert!(elapsed.as_secs_f32() < 1.0, "took {elapsed:?}");
    assert!(pdf.len() > 100_000);
}

#[test]
fn hundred_pages_open_with_correct_count() -> R {
    let mut doc = PdfBuilder::new();
    for i in 0..100 {
        doc = doc.page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, &format!("page {i}"));
        });
    }
    let doc = open(&doc.build())?;
    assert_eq!(doc.page_count()?, 100);
    assert_eq!(lines(&doc, 99)?[0].text, "page 99");
    Ok(())
}

#[test]
fn write_temp_creates_readable_file_and_cleans_up() -> R {
    let tmp = PdfBuilder::new()
        .page(100.0, 100.0, |_| {})
        .write_temp("my doc");
    let path = tmp.path().to_path_buf();
    assert!(path.exists());
    let doc = Document::open(path.to_str().expect("utf-8 temp path"))?;
    assert_eq!(doc.page_count()?, 1);
    drop(tmp);
    assert!(!path.exists());
    Ok(())
}

#[test]
#[should_panic(expected = "missing page")]
fn link_to_missing_page_panics() {
    let _ = PdfBuilder::new()
        .page(100.0, 100.0, |p| {
            p.link_to((0.0, 0.0, 10.0, 10.0), 2, None);
        })
        .build();
}
