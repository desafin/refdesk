//! Prompts, input preparation and output clean-up: pure functions (R-10).

use std::ops::Range;

use super::{TranslateKind, TranslateRequest};
use crate::store::TranslateConfig;
use crate::text::PageText;

/// Passages longer than this many characters are cut (TR-8).
pub const MAX_PASSAGE_CHARS: usize = 2000;
/// A word translation longer than this many characters is cut.
pub const MAX_WORD_OUTPUT_CHARS: usize = 40;
/// How far `sentence_around` looks on each side of the target, in characters.
const SENTENCE_REACH: usize = 300;
/// Token budget of a word answer.
const WORD_TOKENS: u32 = 32;
/// Largest token budget of a passage answer.
const MAX_PASSAGE_TOKENS: u32 = 2048;

/// The English name of a language code; an unknown code is its own name.
pub fn language_name(code: &str) -> String {
    match code {
        "ko" => "Korean",
        "en" => "English",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        other => return other.to_string(),
    }
    .to_string()
}

/// The instruction for translating `text` into `target` with `model`.
///
/// Models whose name starts with `translategemma` get the fixed template that model was trained
/// on (one user message, two blank lines before the text). Others get a general instruction that
/// keeps identifiers, code, numbers, units and requirement IDs as they are (TR-8).
pub fn passage_prompt(model: &str, target: &str, text: &str) -> String {
    let lang = language_name(target);
    if model.starts_with("translategemma") {
        format!(
            "You are a professional English (en) to {lang} ({target}) translator. Your goal is to \
             accurately convey the meaning and nuances of the original English text while \
             adhering to {lang} grammar, vocabulary, and cultural sensitivities.\n\
             Produce only the {lang} translation, without any additional explanations or \
             commentary. Please translate the following English text into {lang}:\n\n\n{text}"
        )
    } else {
        format!(
            "Translate the following text from a software technical document into {lang}. Keep \
             identifiers, code, numbers, units and requirement IDs (such as [SWS_EM_01004]) \
             exactly as they are. Output only the translation, without explanations.\n\n{text}"
        )
    }
}

/// The instruction asking what `word` means in `sentence`, in one or two words of `target`.
/// An empty sentence asks about the word alone.
pub fn word_prompt(target: &str, word: &str, sentence: &str) -> String {
    let sentence = sentence.trim();
    if target == "ko" {
        if sentence.is_empty() {
            format!(
                "영어 낱말 '{word}' 가 뜻하는 바를 한국어 낱말 하나나 둘로만 답하라. 설명, 문장, \
                 따옴표 없이 뜻만 쓴다."
            )
        } else {
            format!(
                "문장: {sentence}\n\n위 문장에서 '{word}' 가 뜻하는 바를 한국어 낱말 하나나 둘로만 \
                 답하라. 설명, 문장, 따옴표 없이 뜻만 쓴다."
            )
        }
    } else {
        let lang = language_name(target);
        if sentence.is_empty() {
            format!(
                "What does the English word '{word}' mean? Answer with one or two {lang} words \
                 only, without explanation, sentences or quotes."
            )
        } else {
            format!(
                "Sentence: {sentence}\n\nWhat does '{word}' mean in the sentence above? Answer \
                 with one or two {lang} words only, without explanation, sentences or quotes."
            )
        }
    }
}

/// Fills an own passage template (TR-13): `{text}` and `{target}` (the language name). `None`
/// when the template has no `{text}`. Placeholders inside the inserted text are left alone.
pub fn fill_passage_template(template: &str, target: &str, text: &str) -> Option<String> {
    if !template.contains("{text}") {
        return None;
    }
    let lang = language_name(target);
    Some(fill(template, &[("text", text), ("target", &lang)]))
}

/// Fills an own word template (TR-13): `{word}`, `{sentence}` and `{target}`. `None` when the
/// template has no `{word}`.
pub fn fill_word_template(
    template: &str,
    target: &str,
    word: &str,
    sentence: &str,
) -> Option<String> {
    if !template.contains("{word}") {
        return None;
    }
    let lang = language_name(target);
    Some(fill(
        template,
        &[
            ("word", word),
            ("sentence", sentence.trim()),
            ("target", &lang),
        ],
    ))
}

/// Replaces `{name}` placeholders in one pass over the template.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let hit = values.iter().find_map(|(name, value)| {
            after
                .strip_prefix(name)
                .and_then(|r| r.strip_prefix('}'))
                .map(|r| (*value, r))
        });
        match hit {
            Some((value, r)) => {
                out.push_str(value);
                rest = r;
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// How a backend words its requests: model, target and the optional own templates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompts {
    /// Model name (picks the built-in template).
    pub model: String,
    /// Target language code.
    pub target: String,
    /// Own passage template (`{text}`), if any.
    pub passage_template: Option<String>,
    /// Own word template (`{word}`), if any.
    pub word_template: Option<String>,
}

impl Prompts {
    /// The prompts of `config` (its `model`, `target` and templates).
    pub fn from_config(config: &TranslateConfig) -> Prompts {
        Prompts {
            model: config.model.clone(),
            target: config.target.clone(),
            passage_template: config.passage_prompt.clone(),
            word_template: config.word_prompt.clone(),
        }
    }

    /// The user message for `req`. An own template wins over the built-in one whatever the
    /// model; a template without its required placeholder is ignored.
    pub fn message(&self, req: &TranslateRequest) -> String {
        match &req.kind {
            TranslateKind::Passage => self
                .passage_template
                .as_deref()
                .and_then(|t| fill_passage_template(t, &self.target, &req.text))
                .unwrap_or_else(|| passage_prompt(&self.model, &self.target, &req.text)),
            TranslateKind::Word { context } => self
                .word_template
                .as_deref()
                .and_then(|t| fill_word_template(t, &self.target, &req.text, context))
                .unwrap_or_else(|| word_prompt(&self.target, &req.text, context)),
        }
    }
}

/// The token budget of the answer: 32 for a word; for a passage half its length in characters
/// plus 64, at most 2048.
pub fn max_tokens(req: &TranslateRequest) -> u32 {
    match req.kind {
        TranslateKind::Word { .. } => WORD_TOKENS,
        TranslateKind::Passage => {
            let chars = u32::try_from(req.text.chars().count()).unwrap_or(u32::MAX);
            (chars / 2).saturating_add(64).min(MAX_PASSAGE_TOKENS)
        }
    }
}

/// Collapses white space and cuts the text to [`MAX_PASSAGE_CHARS`] characters, at a word
/// border when there is one. The flag says whether anything was cut (TR-8).
pub fn prepare_passage(raw: &str) -> (String, bool) {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let joined = words.join(" ");
    if joined.chars().count() <= MAX_PASSAGE_CHARS {
        return (joined, false);
    }
    let cut_at = joined
        .char_indices()
        .nth(MAX_PASSAGE_CHARS)
        .map_or(joined.len(), |(i, _)| i);
    let head = &joined[..cut_at];
    // The next character starts a word or is a space: a space there means the cut is at a border.
    let at_border = joined[cut_at..].starts_with(' ');
    let kept = if at_border {
        head
    } else {
        match head.rfind(' ') {
            Some(space) if space > 0 => &head[..space],
            // One giant word: cut it.
            _ => head,
        }
    };
    (kept.trim_end().to_string(), true)
}

/// Opening quote characters that may wrap a model's answer.
fn is_open_quote(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | '\u{201C}' | '\u{2018}' | '\u{300C}' | '\u{300E}' | '\u{00AB}'
    )
}

/// Closing quote characters that may wrap a model's answer.
fn is_close_quote(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | '\u{201D}' | '\u{2019}' | '\u{300D}' | '\u{300F}' | '\u{00BB}'
    )
}

/// Tidies a model's answer: outer white space and wrapping quotes go. A word answer keeps its
/// first line only, loses a final full stop and is cut to [`MAX_WORD_OUTPUT_CHARS`].
///
/// It is also applied to the text of an answer that is still streaming, and a longer prefix of
/// the same answer never shows less of it: for a word an opening quote goes at once (the closing
/// one may not have come yet).
pub fn clean_output(kind: &TranslateKind, text: &str) -> String {
    match kind {
        TranslateKind::Passage => {
            let t = text.trim();
            let mut chars = t.chars();
            match (chars.next(), chars.next_back()) {
                (Some(a), Some(b))
                    if is_open_quote(a)
                        && is_close_quote(b)
                        && !chars
                            .as_str()
                            .contains(|c| is_open_quote(c) || is_close_quote(c)) =>
                {
                    chars.as_str().trim().to_string()
                }
                _ => t.to_string(),
            }
        }
        TranslateKind::Word { .. } => {
            let first = text.trim().lines().next().unwrap_or("").trim();
            let full_stop = ['.', '\u{3002}'];
            let mut s = first;
            if let Some(c) = s.chars().next().filter(|c| is_open_quote(*c)) {
                s = s[c.len_utf8()..].trim_start();
                // `"word".` and `"word."` both lose the stop and the quote.
                s = s.trim_end_matches(full_stop).trim_end();
                if let Some(e) = s.chars().next_back().filter(|e| is_close_quote(*e)) {
                    s = s[..s.len() - e.len_utf8()].trim_end();
                }
            }
            let s = s.trim_end_matches(full_stop).trim_end();
            let cut: String = s.chars().take(MAX_WORD_OUTPUT_CHARS).collect();
            cut.trim_end().to_string()
        }
    }
}

/// The sentence the characters `chars` of `page` are in: the text from the end of the previous
/// sentence to the end of this one, at most 300 characters on each side of the target.
///
/// Lines are joined with a space; a word broken by a hyphen at a line end (a letter, a hyphen,
/// and a lowercase letter on the next line) is joined without it, as search does. A sentence ends
/// after `!`, `?` or `;` followed by a space, after `.` followed by a space and an uppercase
/// letter, a digit or an opening bracket (so `e.g. this` and `3.2` do not end one), and at the
/// end of a block (paragraph) or of the page.
pub fn sentence_around(page: &PageText, chars: Range<u32>) -> String {
    let flat = Flat::new(page);
    let (Some(t0), Some(t1)) = (
        flat.position_of(chars.start),
        flat.position_of(chars.end.saturating_sub(1).max(chars.start)),
    ) else {
        return String::new();
    };
    let s = &flat.chars;
    let t1 = t1 + 1;

    // Back from the target to the end of the previous sentence.
    let low = t0.saturating_sub(SENTENCE_REACH);
    let mut start = None;
    let mut k = t0;
    while k > low {
        k -= 1;
        if s[k] == BREAK || ends_sentence(s, k) {
            start = Some(k + 1);
            break;
        }
    }
    let start = start.unwrap_or_else(|| {
        if low == 0 {
            0
        } else {
            // Cut at the first space after the limit so that no word is cut.
            (low..t0).find(|&i| s[i] == ' ').map_or(low, |i| i + 1)
        }
    });

    // Forward from the target to the end of this sentence.
    let high = (t1 + SENTENCE_REACH).min(s.len());
    let mut end = None;
    for k in (t1 - 1).max(t0)..high {
        if s[k] == BREAK {
            end = Some(k);
            break;
        }
        if ends_sentence(s, k) || k + 1 == s.len() {
            end = Some(k + 1);
            break;
        }
    }
    let end = end.unwrap_or_else(|| {
        if high == s.len() {
            high
        } else {
            (t1..high).rev().find(|&i| s[i] == ' ').unwrap_or(high)
        }
    });
    let text: String = s[start.min(end)..end].iter().collect();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Marks a block border in [`Flat`].
const BREAK: char = '\n';

/// Whether the character at `k` ends a sentence.
fn ends_sentence(s: &[char], k: usize) -> bool {
    let c = s[k];
    if !matches!(c, '.' | '!' | '?' | ';') {
        return false;
    }
    match s.get(k + 1) {
        None | Some(&BREAK) => true,
        Some(n) if n.is_whitespace() => {
            if c != '.' {
                return true;
            }
            let next = s[k + 1..]
                .iter()
                .find(|c| !c.is_whitespace() || **c == BREAK);
            match next {
                None | Some(&BREAK) => true,
                Some(n) => n.is_uppercase() || n.is_ascii_digit() || matches!(n, '(' | '['),
            }
        }
        Some(_) => false,
    }
}

/// The text of a page as one character sequence: lines joined, blocks separated by [`BREAK`],
/// with the position of every page character in it.
struct Flat {
    chars: Vec<char>,
    /// `pos[i]`: where page character `i` is in `chars` (`None`: dropped, or in no line).
    pos: Vec<Option<usize>>,
}

impl Flat {
    fn new(page: &PageText) -> Flat {
        let n = page.chars.len();
        let mut line_block: Vec<Option<usize>> = vec![None; page.lines.len()];
        for (bi, b) in page.blocks.iter().enumerate() {
            let first = (b.lines.start as usize).min(line_block.len());
            let last = (b.lines.end as usize).min(line_block.len());
            if let Some(slots) = line_block.get_mut(first..last) {
                slots.fill(Some(bi));
            }
        }
        let mut chars: Vec<char> = Vec::with_capacity(n + page.lines.len());
        let mut pos: Vec<Option<usize>> = vec![None; n];
        let mut prev_block: Option<Option<usize>> = None;
        for (li, line) in page.lines.iter().enumerate() {
            let end = (line.chars.end as usize).min(n);
            let start = (line.chars.start as usize).min(end);
            if start == end {
                continue;
            }
            let block = line_block.get(li).copied().flatten();
            if let Some(pb) = prev_block {
                let same = block.is_some() && pb == block;
                let first = page.chars[start].ch;
                if !same {
                    chars.push(BREAK);
                } else if hyphen_break(&chars, first) {
                    chars.pop();
                } else {
                    chars.push(' ');
                }
            }
            for (slot, c) in pos[start..end].iter_mut().zip(&page.chars[start..end]) {
                *slot = Some(chars.len());
                chars.push(c.ch);
            }
            prev_block = Some(block);
        }
        Flat { chars, pos }
    }

    fn position_of(&self, ch: u32) -> Option<usize> {
        self.pos.get(ch as usize).copied().flatten()
    }
}

/// Whether the text so far ends in "letter-" and the next line starts with a lowercase letter.
fn hyphen_break(before: &[char], next: char) -> bool {
    let n = before.len();
    n >= 2
        && matches!(before[n - 1], '-' | '\u{2010}' | '\u{00AD}')
        && before[n - 2].is_alphabetic()
        && next.is_lowercase()
}
