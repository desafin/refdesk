//! `translations.json`: finished translations, so that the same text is not asked twice (TR-6).
//!
//! The key is the backend identity (model and target language), the kind, the text and, for a
//! word, the sentence it was in (the same word can mean different things in different
//! sentences). The least recently used entries go beyond [`MAX_CACHE_ENTRIES`].

use std::collections::HashMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{TranslateKind, TranslateRequest};
use crate::store::atomic::{Loaded, atomic_write, load_with};
use crate::store::{FORMAT_VERSION, json_version, lenient_items, warn_if_newer};

/// The most translations the cache keeps.
pub const MAX_CACHE_ENTRIES: usize = 5000;

/// One stored translation, as written in the file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct StoredEntry {
    identity: String,
    /// `"passage"` or `"word"`.
    kind: String,
    text: String,
    /// The sentence of a word (empty for a passage).
    #[serde(skip_serializing_if = "String::is_empty")]
    context: String,
    translation: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    stored: StoredEntry,
    /// Larger is more recently used.
    used: u64,
}

/// Remembered translations, least recently used dropped first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationCache {
    entries: HashMap<String, Entry>,
    clock: u64,
    capacity: usize,
    dirty: bool,
}

impl Default for TranslationCache {
    fn default() -> TranslationCache {
        TranslationCache::with_capacity(MAX_CACHE_ENTRIES)
    }
}

/// The parts of a request that make a key: kind name and context.
fn kind_parts(kind: &TranslateKind) -> (&'static str, &str) {
    match kind {
        TranslateKind::Passage => ("passage", ""),
        TranslateKind::Word { context } => ("word", context.as_str()),
    }
}

fn key_of(identity: &str, kind: &str, context: &str, text: &str) -> String {
    // The unit separator does not appear in model names or document text in practice; even if
    // it did, the lengths make the key unambiguous.
    format!(
        "{}:{identity}\u{1f}{kind}\u{1f}{}:{context}\u{1f}{text}",
        identity.len(),
        context.len()
    )
}

impl TranslationCache {
    /// An empty cache that keeps at most `capacity` entries (tests use small ones).
    pub fn with_capacity(capacity: usize) -> TranslationCache {
        TranslationCache {
            entries: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
            dirty: false,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether entries were added since the cache was loaded or last marked saved.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Records that the current content is on disk.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// The translation of `req` by `identity`, if remembered. Counts as a use.
    pub fn get(&mut self, identity: &str, req: &TranslateRequest) -> Option<&str> {
        let (kind, context) = kind_parts(&req.kind);
        let key = key_of(identity, kind, context, &req.text);
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(&key)?;
        entry.used = clock;
        Some(entry.stored.translation.as_str())
    }

    /// Whether `req` by `identity` is remembered, without counting as a use.
    pub fn contains(&self, identity: &str, req: &TranslateRequest) -> bool {
        let (kind, context) = kind_parts(&req.kind);
        self.entries
            .contains_key(&key_of(identity, kind, context, &req.text))
    }

    /// Remembers `text` as the translation of `req` by `identity`. An empty (or blank)
    /// translation is not remembered. Beyond the capacity the least recently used entry goes.
    pub fn insert(&mut self, identity: &str, req: &TranslateRequest, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let (kind, context) = kind_parts(&req.kind);
        self.put(StoredEntry {
            identity: identity.to_string(),
            kind: kind.to_string(),
            text: req.text.clone(),
            context: context.to_string(),
            translation: text,
        });
        self.dirty = true;
    }

    fn put(&mut self, stored: StoredEntry) {
        let key = key_of(
            &stored.identity,
            &stored.kind,
            &stored.context,
            &stored.text,
        );
        self.clock += 1;
        self.entries.insert(
            key,
            Entry {
                stored,
                used: self.clock,
            },
        );
        while self.entries.len() > self.capacity {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Reads the cache. Never fails: a missing or empty file gives an empty cache; a file that
    /// is not a JSON object gives an empty cache and is moved to `.bad` (R-8). Items that do not
    /// parse are dropped.
    pub fn load(path: &Path) -> TranslationCache {
        TranslationCache::load_with_capacity(path, MAX_CACHE_ENTRIES)
    }

    /// [`TranslationCache::load`] with another capacity (used by tests).
    pub fn load_with_capacity(path: &Path, capacity: usize) -> TranslationCache {
        load_with(path, "translations", |bytes| parse(bytes, capacity))
            .unwrap_or_else(|| TranslationCache::with_capacity(capacity))
    }

    /// The file content: entries from least to most recently used.
    pub fn to_bytes(&self) -> io::Result<Vec<u8>> {
        let mut entries: Vec<&Entry> = self.entries.values().collect();
        entries.sort_by_key(|e| e.used);
        let file = FileRef {
            version: FORMAT_VERSION,
            entries: entries.into_iter().map(|e| &e.stored).collect(),
        };
        serde_json::to_vec(&file).map_err(io::Error::other)
    }

    /// Writes the cache with [`atomic_write`]. Does not clear [`TranslationCache::dirty`]; the
    /// caller does that with [`TranslationCache::mark_saved`] (it may write on another thread
    /// from the bytes of [`TranslationCache::to_bytes`]).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        atomic_write(path, &self.to_bytes()?)
    }
}

#[derive(Serialize)]
struct FileRef<'a> {
    version: u32,
    entries: Vec<&'a StoredEntry>,
}

/// `Option` so that the default of `load_with` (a missing file) can be told apart and given the
/// right capacity.
fn parse(bytes: &[u8], capacity: usize) -> Result<Loaded<Option<TranslationCache>>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "top level is not an object".to_string())?;
    warn_if_newer("translations", json_version(obj));
    let mut dropped = Vec::new();
    let items: Vec<StoredEntry> = lenient_items(obj.get("entries"), "entries", &mut dropped);
    let mut cache = TranslationCache::with_capacity(capacity);
    for item in items {
        let known_kind = item.kind == "passage" || item.kind == "word";
        if known_kind && !item.translation.trim().is_empty() {
            cache.put(item);
        }
    }
    Ok(Loaded {
        value: Some(cache),
        dropped: (!dropped.is_empty()).then(|| dropped.join("; ")),
    })
}
