//! Back/forward history of long jumps. See `docs/ARCHITECTURE.md` §4.4.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::Location;

/// Two places on the same page closer than this (in points, vertically) count as the same spot.
pub const SAME_SPOT_TOLERANCE_PT: f32 = 24.0;

/// The most entries a history keeps. The oldest ones are dropped first.
pub const MAX_ENTRIES: usize = 200;

/// A location in a specific document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// The document.
    pub doc: PathBuf,
    /// The position in it.
    pub loc: Location,
}

impl Place {
    /// Whether `other` is the same spot: same document, same page, and a vertical distance of at
    /// most [`SAME_SPOT_TOLERANCE_PT`].
    pub fn is_same_spot(&self, other: &Place) -> bool {
        self.doc == other.doc
            && self.loc.page == other.loc.page
            && (self.loc.y - other.loc.y).abs() <= SAME_SPOT_TOLERANCE_PT
    }
}

/// The places visited by long jumps and an index of where the reader is now.
///
/// There is always at least one entry, and the index always points at one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "HistoryRepr")]
pub struct History {
    entries: Vec<Place>,
    index: usize,
}

/// The stored form, validated on the way in so that a damaged file cannot break the invariants.
#[derive(Deserialize)]
struct HistoryRepr {
    entries: Vec<Place>,
    index: usize,
}

impl TryFrom<HistoryRepr> for History {
    type Error = String;

    fn try_from(repr: HistoryRepr) -> Result<History, String> {
        if repr.entries.is_empty() {
            return Err("history has no entries".to_string());
        }
        let mut h = History {
            index: repr.index.min(repr.entries.len() - 1),
            entries: repr.entries,
        };
        h.enforce_limit();
        Ok(h)
    }
}

impl History {
    /// Starts a history at `start`.
    pub fn new(start: Place) -> History {
        History {
            entries: vec![start],
            index: 0,
        }
    }

    /// The place the reader is at, as of the last update.
    pub fn current(&self) -> &Place {
        &self.entries[self.index]
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always false: a history has at least one entry.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Records a long jump from `from` to `to`.
    ///
    /// The current entry becomes `from`, everything after it is dropped, and `to` is added unless
    /// it is the same spot as `from`.
    pub fn jump(&mut self, from: Place, to: Place) {
        self.entries[self.index] = from;
        self.entries.truncate(self.index + 1);
        if !self.entries[self.index].is_same_spot(&to) {
            self.entries.push(to);
            self.index += 1;
            self.enforce_limit();
        }
    }

    /// Goes one entry back. The current entry is first updated to `current`, so that going
    /// forward again returns to where the reader left. `None` (and no change) at the oldest entry.
    pub fn back(&mut self, current: Place) -> Option<Place> {
        if self.index == 0 {
            return None;
        }
        self.entries[self.index] = current;
        self.index -= 1;
        Some(self.entries[self.index].clone())
    }

    /// Goes one entry forward, mirroring [`History::back`].
    pub fn forward(&mut self, current: Place) -> Option<Place> {
        if self.index + 1 >= self.entries.len() {
            return None;
        }
        self.entries[self.index] = current;
        self.index += 1;
        Some(self.entries[self.index].clone())
    }

    /// Updates the position of the current entry without adding one (called when scrolling
    /// stops).
    pub fn touch(&mut self, current: Place) {
        self.entries[self.index] = current;
    }

    /// Whether [`History::back`] would move.
    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Whether [`History::forward`] would move.
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    /// A copy with at most `max` entries (at least one is always kept) taken around the current
    /// index. The current entry is always kept. Meant for saving.
    pub fn trimmed(&self, max: usize) -> History {
        let max = max.max(1);
        let len = self.entries.len();
        if len <= max {
            return self.clone();
        }
        let start = self.index.saturating_sub(max / 2).min(len - max);
        History {
            entries: self.entries[start..start + max].to_vec(),
            index: self.index - start,
        }
    }

    fn enforce_limit(&mut self) {
        if self.entries.len() > MAX_ENTRIES {
            let drop = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..drop);
            self.index = self.index.saturating_sub(drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(page: u32, y: f32) -> Place {
        Place {
            doc: PathBuf::from("/docs/a.pdf"),
            loc: Location::new(page, 0.0, y),
        }
    }

    #[test]
    fn jump_then_back_and_forward() {
        let (a, b) = (place(0, 0.0), place(5, 0.0));
        let mut h = History::new(a.clone());
        h.jump(a.clone(), b.clone());
        assert_eq!(h.back(b.clone()), Some(a.clone()));
        assert_eq!(h.forward(a), Some(b));
    }

    #[test]
    fn jump_after_back_drops_forward_entries() {
        let (a, b, c, d) = (place(0, 0.0), place(1, 0.0), place(2, 0.0), place(3, 0.0));
        let mut h = History::new(a.clone());
        h.jump(a.clone(), b.clone());
        h.jump(b.clone(), c.clone());
        assert_eq!(h.back(c), Some(b.clone()));
        assert_eq!(h.back(b), Some(a.clone()));
        h.jump(a, d.clone());
        assert!(!h.can_forward());
        assert!(h.can_back());
        assert_eq!(h.current(), &d);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn touch_does_not_add_entries() {
        let mut h = History::new(place(0, 0.0));
        for i in 0..50 {
            h.touch(place(0, i as f32 * 100.0));
        }
        assert_eq!(h.len(), 1);
        assert_eq!(h.current(), &place(0, 4900.0));
        assert!(!h.can_back());
        assert!(!h.can_forward());
    }

    #[test]
    fn back_then_forward_returns_to_scrolled_position() {
        let (a, b) = (place(0, 0.0), place(5, 100.0));
        let mut h = History::new(a.clone());
        h.jump(a.clone(), b);
        let b_scrolled = place(5, 600.0);
        assert_eq!(h.back(b_scrolled.clone()), Some(a.clone()));
        assert_eq!(h.forward(a), Some(b_scrolled));
    }

    #[test]
    fn jump_to_same_spot_adds_nothing() {
        let a = place(2, 100.0);
        let mut h = History::new(a.clone());
        h.jump(a.clone(), place(2, 100.0 + SAME_SPOT_TOLERANCE_PT));
        assert_eq!(h.len(), 1);
        assert!(!h.can_back());
        // Just beyond the tolerance is a new spot.
        h.jump(a.clone(), place(2, 100.0 + SAME_SPOT_TOLERANCE_PT + 1.0));
        assert_eq!(h.len(), 2);
        // Another page or another document is never the same spot.
        h.jump(a.clone(), place(3, 100.0));
        assert_eq!(h.len(), 3);
        let mut other = place(2, 100.0);
        other.doc = PathBuf::from("/docs/b.pdf");
        let mut h = History::new(a.clone());
        h.jump(a, other);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn limit_drops_oldest_and_keeps_index_valid() {
        let mut h = History::new(place(0, 0.0));
        let total = MAX_ENTRIES as u32 + 50;
        for i in 0..total {
            h.jump(place(i, 0.0), place(i + 1, 0.0));
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.current(), &place(total, 0.0));
        assert!(!h.can_forward());
        // Walk back to the oldest entry that is left.
        let mut cur = h.current().clone();
        let mut steps = 0;
        while let Some(p) = h.back(cur.clone()) {
            assert_eq!(p.loc.page, cur.loc.page - 1);
            cur = p;
            steps += 1;
        }
        assert_eq!(steps, MAX_ENTRIES - 1);
        assert_eq!(cur, place(total - MAX_ENTRIES as u32 + 1, 0.0));
        // And forward again to the newest.
        while let Some(p) = h.forward(cur.clone()) {
            cur = p;
        }
        assert_eq!(cur, place(total, 0.0));
    }

    #[test]
    fn serde_roundtrip_keeps_index() {
        let (a, b, c) = (place(0, 0.0), place(1, 0.0), place(2, 0.0));
        let mut h = History::new(a.clone());
        h.jump(a, b.clone());
        h.jump(b.clone(), c.clone());
        h.back(c);
        let s = serde_json::to_string(&h).unwrap();
        let back: History = serde_json::from_str(&s).unwrap();
        assert_eq!(back, h);
        assert_eq!(back.current(), &b);
        assert!(back.can_back() && back.can_forward());
    }

    #[test]
    fn deserialize_rejects_empty_and_clamps_index() {
        assert!(serde_json::from_str::<History>(r#"{"entries":[],"index":0}"#).is_err());
        let one = serde_json::to_string(&place(1, 2.0)).unwrap();
        let h: History =
            serde_json::from_str(&format!(r#"{{"entries":[{one}],"index":9}}"#)).unwrap();
        assert_eq!(h.current(), &place(1, 2.0));
    }

    #[test]
    fn trimmed_keeps_current_entry() {
        let mut h = History::new(place(0, 0.0));
        for i in 0..100 {
            h.jump(place(i, 0.0), place(i + 1, 0.0));
        }
        // Current is the newest entry.
        let t = h.trimmed(50);
        assert_eq!(t.len(), 50);
        assert_eq!(t.current(), h.current());
        assert!(!t.can_forward());
        // Go back to the middle: the window is centered on it.
        let mut cur = h.current().clone();
        for _ in 0..50 {
            cur = h.back(cur).unwrap();
        }
        let t = h.trimmed(50);
        assert_eq!(t.len(), 50);
        assert_eq!(t.current(), h.current());
        assert!(t.can_back() && t.can_forward());
        // At the oldest entry.
        while let Some(p) = h.back(cur.clone()) {
            cur = p;
        }
        let t = h.trimmed(10);
        assert_eq!(t.len(), 10);
        assert_eq!(t.current(), &place(0, 0.0));
        assert!(!t.can_back());
        // Degenerate sizes.
        assert_eq!(h.trimmed(0).len(), 1);
        assert_eq!(h.trimmed(0).current(), h.current());
        assert_eq!(h.trimmed(1000), h);
    }
}
