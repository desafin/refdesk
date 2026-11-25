//! A small translation table (Korean and English). Everything the user reads goes through [`tr`].
//!
//! The language comes from the `LANG` environment variable: a value starting with `ko` selects
//! Korean, anything else English.

use std::sync::OnceLock;

/// A UI language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// Korean.
    Ko,
    /// English.
    En,
}

/// One row of the table: key, Korean text, English text.
type Row = (&'static str, &'static str, &'static str);

/// All UI strings. A text containing `{}` is a template filled in with `str::replace`.
pub const TABLE: &[Row] = &[
    ("menu.file", "파일", "File"),
    ("menu.view", "보기", "View"),
    ("menu.tab", "탭", "Tab"),
    ("cmd.file.open", "파일 열기…", "Open File…"),
    ("cmd.tab.close", "탭 닫기", "Close Tab"),
    ("cmd.tab.next", "다음 탭", "Next Tab"),
    ("cmd.tab.prev", "이전 탭", "Previous Tab"),
    ("cmd.tab.duplicate", "탭 복제", "Duplicate Tab"),
    ("cmd.view.zoom_in", "확대", "Zoom In"),
    ("cmd.view.zoom_out", "축소", "Zoom Out"),
    ("cmd.view.zoom_reset", "100%", "Actual Size"),
    ("cmd.view.fit_width", "폭 맞춤", "Fit Width"),
    ("cmd.view.fit_page", "쪽 맞춤", "Fit Page"),
    ("cmd.app.quit", "종료", "Quit"),
    ("tab.start", "시작", "Start"),
    ("tab.unknown", "(알 수 없음)", "(unknown)"),
    ("start.open_button", "파일 열기", "Open a file"),
    (
        "start.hint",
        "PDF 파일을 열어 읽기 시작하세요.",
        "Open a PDF file to start reading.",
    ),
    ("doc.path", "경로", "Path"),
    ("doc.loading", "여는 중…", "Opening…"),
    ("doc.pages", "{} 쪽", "{} pages"),
    (
        "doc.needs_password",
        "암호가 필요합니다",
        "A password is required",
    ),
    ("doc.failed", "열 수 없습니다: {}", "Could not open: {}"),
    ("dialog.title", "PDF 파일 열기", "Open PDF files"),
    ("dialog.filter", "PDF 문서", "PDF documents"),
    (
        "cli.help",
        "사용법:
  refdesk [옵션] [파일[#page=N]]...
  refdesk --diff 이전.pdf 이후.pdf

옵션:
  --diff A B             두 PDF 를 비교한다
  --no-single-instance   실행 중인 인스턴스를 찾지 않고 새로 띄운다 (개발용)
  -h, --help             이 도움말을 보인다
  -V, --version          버전을 보인다
  --                     이 뒤의 인자는 모두 파일로 본다

이미 실행 중이면 새 창을 띄우지 않고 그 창에 탭으로 연다.",
        "Usage:
  refdesk [options] [file[#page=N]]...
  refdesk --diff old.pdf new.pdf

Options:
  --diff A B             compare two PDF files
  --no-single-instance   start a new instance without looking for a running one (for development)
  -h, --help             show this help
  -V, --version          show the version
  --                     treat everything after it as a file

If refdesk is already running, the files open as tabs in its window instead of a new one.",
    ),
    (
        "cli.unknown_option",
        "알 수 없는 옵션: {}",
        "unknown option: {}",
    ),
    (
        "cli.diff_needs_two",
        "--diff 에는 파일 두 개가 필요합니다",
        "--diff needs two files",
    ),
    (
        "cli.diff_twice",
        "--diff 는 한 번만 쓸 수 있습니다",
        "--diff can be used only once",
    ),
    (
        "cli.diff_with_files",
        "--diff 는 다른 파일과 함께 쓸 수 없습니다",
        "--diff cannot be combined with other files",
    ),
];

/// Picks the language from the value of `LANG`.
pub fn lang_from_env_value(value: Option<&str>) -> Lang {
    match value {
        Some(v) if v.starts_with("ko") => Lang::Ko,
        _ => Lang::En,
    }
}

/// The language of this process, decided once from `LANG`.
pub fn current_lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(|| lang_from_env_value(std::env::var("LANG").ok().as_deref()))
}

/// Looks `key` up for `lang`. `None` when the key is not in the table.
pub fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    TABLE
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, ko, en)| match lang {
            Lang::Ko => *ko,
            Lang::En => *en,
        })
}

/// The text for `key` in the current language. An unknown key shows as `"???"` and is logged.
pub fn tr(key: &str) -> &'static str {
    match lookup(current_lang(), key) {
        Some(text) => text,
        None => {
            log::warn!("missing translation key: {key}");
            "???"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_follows_the_lang_variable() {
        assert_eq!(lang_from_env_value(Some("ko_KR.UTF-8")), Lang::Ko);
        assert_eq!(lang_from_env_value(Some("ko")), Lang::Ko);
        assert_eq!(lang_from_env_value(Some("en_US.UTF-8")), Lang::En);
        assert_eq!(lang_from_env_value(Some("C")), Lang::En);
        assert_eq!(lang_from_env_value(None), Lang::En);
    }

    #[test]
    fn table_has_unique_keys_and_no_empty_text() {
        for (i, (key, ko, en)) in TABLE.iter().enumerate() {
            assert!(!ko.is_empty() && !en.is_empty(), "empty text for {key}");
            assert!(
                TABLE[..i].iter().all(|(k, _, _)| k != key),
                "duplicate key {key}"
            );
        }
    }

    #[test]
    fn lookup_returns_the_requested_language() {
        assert_eq!(lookup(Lang::Ko, "tab.start"), Some("시작"));
        assert_eq!(lookup(Lang::En, "tab.start"), Some("Start"));
        assert_eq!(lookup(Lang::En, "no.such.key"), None);
    }
}
