//! First pass of a comparison: aligning the paragraph flows of two documents
//! (`docs/SPEC.md` §8.5 step 4).
//!
//! The paragraph keys of both sides are compared with the histogram algorithm of `similar`,
//! which anchors on keys that are rare on both sides (section headings, paragraphs that occur
//! once) and recurses around them. The result is a list of runs of equal paragraphs and of
//! misaligned regions in between; the regions go to [`refine`](super::refine).

use std::hash::Hash;
use std::ops::Range;
use std::time::Instant;

use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

/// A piece of the paragraph alignment. Ranges index the paragraph lists of the two sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Paragraphs with equal keys, pairwise: `left.len() == right.len()`.
    Same {
        left: Range<usize>,
        right: Range<usize>,
    },
    /// Paragraphs that did not align. One side may be empty, never both.
    Differ {
        left: Range<usize>,
        right: Range<usize>,
    },
}

/// Compares two key sequences with the histogram algorithm. The second value is `true` when the
/// deadline passed during the comparison; the operations are then a valid but rough script.
pub(crate) fn diff_keys<T: Hash + Eq>(
    left: &[T],
    right: &[T],
    deadline: Option<Instant>,
) -> (Vec<DiffOp>, bool) {
    let ops = capture_diff_slices_deadline(Algorithm::Histogram, left, right, deadline);
    let late = deadline.is_some_and(|d| Instant::now() > d);
    (ops, late)
}

/// Aligns two paragraph key sequences. The segments cover both sequences completely, in order;
/// two `Differ` segments never follow each other. The second value is `true` when the deadline
/// passed (the alignment is then rougher than it could be, but still complete).
pub(crate) fn align_paragraphs(
    left: &[u64],
    right: &[u64],
    deadline: Option<Instant>,
) -> (Vec<Segment>, bool) {
    let (ops, late) = diff_keys(left, right, deadline);
    let mut out: Vec<Segment> = Vec::new();
    for op in ops {
        let (l, r, equal) = match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => (old_index..old_index + len, new_index..new_index + len, true),
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => (old_index..old_index + old_len, new_index..new_index, false),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (old_index..old_index, new_index..new_index + new_len, false),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (
                old_index..old_index + old_len,
                new_index..new_index + new_len,
                false,
            ),
        };
        if l.is_empty() && r.is_empty() {
            continue;
        }
        match (out.last_mut(), equal) {
            (Some(Segment::Same { left, right }), true) => {
                left.end = l.end;
                right.end = r.end;
            }
            (Some(Segment::Differ { left, right }), false) => {
                left.end = l.end;
                right.end = r.end;
            }
            (_, true) => out.push(Segment::Same { left: l, right: r }),
            (_, false) => out.push(Segment::Differ { left: l, right: r }),
        }
    }
    (out, late)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(l: Range<usize>, r: Range<usize>) -> Segment {
        Segment::Same { left: l, right: r }
    }

    fn differ(l: Range<usize>, r: Range<usize>) -> Segment {
        Segment::Differ { left: l, right: r }
    }

    #[test]
    fn equal_sequences_give_one_same_segment() {
        let (s, late) = align_paragraphs(&[1, 2, 3], &[1, 2, 3], None);
        assert_eq!(s, [same(0..3, 0..3)]);
        assert!(!late);
    }

    #[test]
    fn insert_delete_and_replace_become_differ_segments() {
        let (s, _) = align_paragraphs(&[1, 2, 3, 4, 5], &[1, 9, 2, 3, 5, 6], None);
        assert_eq!(
            s,
            [
                same(0..1, 0..1),
                differ(1..1, 1..2),
                same(1..3, 2..4),
                differ(3..4, 4..4),
                same(4..5, 4..5),
                differ(5..5, 5..6),
            ]
        );
        let (s, _) = align_paragraphs(&[1, 2, 3], &[1, 7, 8, 3], None);
        assert_eq!(s, [same(0..1, 0..1), differ(1..2, 1..3), same(2..3, 3..4)]);
    }

    #[test]
    fn empty_sides() {
        assert_eq!(align_paragraphs(&[], &[], None).0, []);
        assert_eq!(align_paragraphs(&[1, 2], &[], None).0, [differ(0..2, 0..0)]);
        assert_eq!(align_paragraphs(&[], &[1], None).0, [differ(0..0, 0..1)]);
    }

    #[test]
    fn segments_cover_both_sides_in_order() {
        let left: Vec<u64> = (0..200).map(|i| (i * 7919) % 101).collect();
        let right: Vec<u64> = (0..230).map(|i| (i * 104_729) % 97).collect();
        let (s, _) = align_paragraphs(&left, &right, None);
        let (mut l, mut r) = (0, 0);
        let mut prev_differ = false;
        for seg in &s {
            let (sl, sr, is_differ) = match seg {
                Segment::Same {
                    left: sl,
                    right: sr,
                } => {
                    assert_eq!(sl.len(), sr.len());
                    for (a, b) in sl.clone().zip(sr.clone()) {
                        assert_eq!(left[a], right[b]);
                    }
                    (sl, sr, false)
                }
                Segment::Differ {
                    left: sl,
                    right: sr,
                } => (sl, sr, true),
            };
            assert_eq!(sl.start, l);
            assert_eq!(sr.start, r);
            assert!(!(prev_differ && is_differ));
            prev_differ = is_differ;
            l = sl.end;
            r = sr.end;
        }
        assert_eq!((l, r), (left.len(), right.len()));
    }
}
