//! Links on a page and the merging of per-line annotations. See `docs/ARCHITECTURE.md` §4.6.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::geom::{Point, Rect};
use crate::nav::dest::Destination;

/// Where a link leads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LinkTarget {
    /// A place in the same document.
    Internal(Destination),
    /// An external address (http, https, mailto, ...).
    External(String),
    /// Another document, optionally at a destination in it.
    OtherDocument {
        /// Path of the other document.
        path: PathBuf,
        /// Place in the other document.
        dest: Option<Destination>,
    },
}

/// A link on a page, possibly spanning several lines.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// Active areas in page space, one per line of text.
    pub rects: Vec<Rect>,
    /// Where the link leads.
    pub target: LinkTarget,
}

/// A link annotation as the engine reads it: one rectangle and one target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawLink {
    /// Active area in page space.
    pub rect: Rect,
    /// Where the link leads.
    pub target: LinkTarget,
}

/// The index of the first link that has a rectangle containing `p` (edges included).
pub fn hit_test(links: &[Link], p: Point) -> Option<usize> {
    links
        .iter()
        .position(|l| l.rects.iter().any(|r| r.contains(p)))
}

/// Merges annotations that belong to one link broken over several lines.
///
/// Two neighbouring annotations (in the given order) are merged when they have the same target
/// and the second one is on the same line as the first (their vertical ranges overlap) or on the
/// line right below it (it starts within 1.5 times the height of the first below its bottom).
pub fn merge(raw: Vec<RawLink>) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut last: Option<Rect> = None;
    for r in raw {
        if let (Some(prev), Some(link)) = (last, out.last_mut())
            && link.target == r.target
            && continues(&prev, &r.rect)
        {
            link.rects.push(r.rect);
        } else {
            out.push(Link {
                rects: vec![r.rect],
                target: r.target,
            });
        }
        last = Some(r.rect);
    }
    out
}

/// Whether `next` continues the link that ended with `prev`.
fn continues(prev: &Rect, next: &Rect) -> bool {
    let same_line = next.y0 < prev.y1 && prev.y0 < next.y1;
    let next_line = next.y0 >= prev.y1 && next.y0 <= prev.y1 + 1.5 * prev.height();
    same_line || next_line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::dest::DestKind;

    fn dest(page: u32) -> LinkTarget {
        LinkTarget::Internal(Destination {
            page,
            kind: DestKind::Fit,
        })
    }

    fn raw(x0: f32, y0: f32, x1: f32, y1: f32, target: LinkTarget) -> RawLink {
        RawLink {
            rect: Rect::new(x0, y0, x1, y1),
            target,
        }
    }

    #[test]
    fn two_line_link_becomes_one() {
        // "see section 4.2 on the next" / "page for details": line height 12, 2pt leading.
        let merged = merge(vec![
            raw(400.0, 100.0, 540.0, 112.0, dest(7)),
            raw(72.0, 114.0, 150.0, 126.0, dest(7)),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].rects.len(), 2);
        assert_eq!(merged[0].target, dest(7));
    }

    #[test]
    fn same_line_pieces_merge() {
        let merged = merge(vec![
            raw(72.0, 100.0, 120.0, 112.0, dest(1)),
            raw(121.0, 100.5, 180.0, 112.5, dest(1)),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].rects.len(), 2);
    }

    #[test]
    fn three_lines_chain() {
        let merged = merge(vec![
            raw(300.0, 100.0, 500.0, 112.0, dest(1)),
            raw(72.0, 114.0, 500.0, 126.0, dest(1)),
            raw(72.0, 128.0, 200.0, 140.0, dest(1)),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].rects.len(), 3);
    }

    #[test]
    fn different_targets_do_not_merge() {
        let merged = merge(vec![
            raw(400.0, 100.0, 540.0, 112.0, dest(7)),
            raw(72.0, 114.0, 150.0, 126.0, dest(8)),
        ]);
        assert_eq!(merged.len(), 2);
        let ext = |s: &str| LinkTarget::External(s.to_string());
        let merged = merge(vec![
            raw(0.0, 0.0, 10.0, 10.0, ext("https://a.example")),
            raw(0.0, 11.0, 10.0, 21.0, ext("https://b.example")),
        ]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn distant_links_with_same_target_do_not_merge() {
        // Next rectangle starts 18pt below the first; allowed is 1.5 * 12 = 18, so 18.5 is out.
        let far = merge(vec![
            raw(72.0, 100.0, 150.0, 112.0, dest(2)),
            raw(72.0, 130.5, 150.0, 142.5, dest(2)),
        ]);
        assert_eq!(far.len(), 2);
        let very_far = merge(vec![
            raw(72.0, 100.0, 150.0, 112.0, dest(2)),
            raw(72.0, 600.0, 150.0, 612.0, dest(2)),
        ]);
        assert_eq!(very_far.len(), 2);
        // A rectangle above the previous one is not a continuation either.
        let above = merge(vec![
            raw(72.0, 200.0, 150.0, 212.0, dest(2)),
            raw(72.0, 100.0, 150.0, 112.0, dest(2)),
        ]);
        assert_eq!(above.len(), 2);
    }

    #[test]
    fn next_line_limit_is_inclusive() {
        let merged = merge(vec![
            raw(72.0, 100.0, 150.0, 112.0, dest(2)),
            raw(72.0, 130.0, 150.0, 142.0, dest(2)),
        ]);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn only_consecutive_annotations_merge() {
        // A link to another target sits between two pieces of the same target.
        let merged = merge(vec![
            raw(72.0, 100.0, 150.0, 112.0, dest(1)),
            raw(72.0, 114.0, 150.0, 126.0, dest(2)),
            raw(72.0, 128.0, 150.0, 140.0, dest(1)),
        ]);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn merge_of_nothing_is_empty() {
        assert!(merge(Vec::new()).is_empty());
    }

    #[test]
    fn hit_test_on_edges_and_misses() {
        let links = merge(vec![
            raw(400.0, 100.0, 540.0, 112.0, dest(7)),
            raw(72.0, 114.0, 150.0, 126.0, dest(7)),
            raw(200.0, 300.0, 260.0, 312.0, dest(3)),
        ]);
        assert_eq!(links.len(), 2);
        // Inside either line of the first link.
        assert_eq!(hit_test(&links, Point::new(450.0, 105.0)), Some(0));
        assert_eq!(hit_test(&links, Point::new(100.0, 120.0)), Some(0));
        // On the boundary counts as inside.
        assert_eq!(hit_test(&links, Point::new(400.0, 100.0)), Some(0));
        assert_eq!(hit_test(&links, Point::new(540.0, 112.0)), Some(0));
        assert_eq!(hit_test(&links, Point::new(260.0, 312.0)), Some(1));
        // Just outside.
        assert_eq!(hit_test(&links, Point::new(540.1, 105.0)), None);
        assert_eq!(hit_test(&links, Point::new(399.9, 105.0)), None);
        // Between the lines of the first link there is a gap.
        assert_eq!(hit_test(&links, Point::new(450.0, 113.0)), None);
        assert_eq!(hit_test(&[], Point::new(0.0, 0.0)), None);
    }
}
