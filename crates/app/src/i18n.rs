//! A small translation table (Korean and English). Everything the user reads goes through [`tr`].
//!
//! The language comes from the `language` setting ([`force_lang`]) or else from the `LANG`
//! environment variable: a value starting with `ko` selects Korean, anything else English.

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
    ("menu.edit", "편집", "Edit"),
    ("menu.view", "보기", "View"),
    ("menu.go", "이동", "Go"),
    ("menu.tab", "탭", "Tab"),
    ("menu.window", "창", "Window"),
    ("cmd.window.new", "새 창", "New Window"),
    (
        "cmd.window.detach_tab",
        "새 창으로 분리",
        "Move Tab to New Window",
    ),
    (
        "cmd.window.merge_all",
        "모든 창 합치기",
        "Merge All Windows",
    ),
    ("cmd.window.close", "창 닫기", "Close Window"),
    ("window.move_to", "다른 창으로 이동", "Move to Window"),
    ("window.name", "창 {n} — {title}", "Window {n} — {title}"),
    ("window.title", "{title} — refdesk", "{title} — refdesk"),
    ("cmd.file.open", "파일 열기…", "Open File…"),
    ("cmd.tab.close", "탭 닫기", "Close Tab"),
    ("cmd.tab.next", "다음 탭", "Next Tab"),
    ("cmd.tab.prev", "이전 탭", "Previous Tab"),
    ("cmd.tab.duplicate", "탭 복제", "Duplicate Tab"),
    (
        "cmd.tab.close_others",
        "다른 탭 모두 닫기",
        "Close Other Tabs",
    ),
    ("cmd.tab.split_right", "오른쪽으로 분할", "Split Right"),
    ("cmd.tab.split_down", "아래로 분할", "Split Down"),
    (
        "cmd.tab.focus_next_pane",
        "다음 분할 영역으로 이동",
        "Focus Next Pane",
    ),
    (
        "cmd.tab.reopen_closed",
        "닫은 탭 다시 열기",
        "Reopen Closed Tab",
    ),
    ("tab.nth", "{name} ({n})", "{name} ({n})"),
    ("dock.eject", "새 창으로 분리", "Move to New Window"),
    ("dock.hide_tab_bar", "탭 줄 숨기기", "Hide Tab Bar"),
    ("dock.show_tab_bar", "탭 줄 보이기", "Show Tab Bar"),
    (
        "dock.not_closable",
        "닫을 수 없는 탭이 있습니다",
        "Some tabs here cannot be closed",
    ),
    ("dock.close_all", "모든 탭 닫기", "Close All Tabs"),
    (
        "dock.close_all_hint",
        "Shift 를 누르거나 오른쪽 클릭하면 모든 탭을 닫습니다",
        "Hold Shift or right-click to close all tabs",
    ),
    ("dock.minimize", "접기", "Collapse"),
    (
        "dock.minimize_hint",
        "Shift 를 누르거나 오른쪽 클릭하면 접습니다",
        "Hold Shift or right-click to collapse",
    ),
    ("dock.expand", "펼치기", "Expand"),
    ("dock.add_tab", "탭 추가", "Add Tab"),
    ("cmd.edit.copy", "복사", "Copy"),
    ("cmd.edit.find", "찾기…", "Find…"),
    ("cmd.edit.find_next", "다음 찾기", "Find Next"),
    ("cmd.edit.find_prev", "이전 찾기", "Find Previous"),
    (
        "cmd.edit.search_selection",
        "이 텍스트로 검색",
        "Search for This Text",
    ),
    ("find.hint", "문서에서 찾기", "Find in document"),
    ("find.count", "{n} / {total}", "{n} / {total}"),
    (
        "find.count_searching",
        "{n} / {total}… {percent}%",
        "{n} / {total}… {percent}%",
    ),
    ("find.none", "일치 없음", "No match"),
    (
        "find.prev",
        "이전 일치 (Shift+Enter)",
        "Previous match (Shift+Enter)",
    ),
    ("find.next", "다음 일치 (Enter)", "Next match (Enter)"),
    ("find.case", "대소문자 구분", "Match case"),
    ("find.word", "단어 단위", "Whole word"),
    ("find.word_label", "단어", "Word"),
    ("find.regex", "정규식", "Regular expression"),
    ("find.list", "목록", "List"),
    (
        "find.list_hint",
        "결과 목록 켜기/끄기",
        "Show or hide the result list",
    ),
    ("find.close", "닫기 (Esc)", "Close (Esc)"),
    (
        "find.wrapped_start",
        "처음부터 다시 찾습니다",
        "Continued from the start",
    ),
    (
        "find.wrapped_end",
        "끝에서부터 다시 찾습니다",
        "Continued from the end",
    ),
    (
        "find.regex_error",
        "정규식 오류: {}",
        "Invalid regular expression: {}",
    ),
    ("find.results_title", "검색 결과", "Search results"),
    (
        "find.results_empty",
        "찾을 글을 입력하세요",
        "Type the text to find",
    ),
    ("find.match_count", "{n}건", "{n}"),
    ("sel.copied", "복사됨 ({} 글자)", "Copied ({} characters)"),
    ("cmd.view.zoom_in", "확대", "Zoom In"),
    ("cmd.view.zoom_out", "축소", "Zoom Out"),
    ("cmd.view.zoom_reset", "100%", "Actual Size"),
    ("cmd.view.fit_width", "폭 맞춤", "Fit Width"),
    ("cmd.view.fit_page", "쪽 맞춤", "Fit Page"),
    ("cmd.app.quit", "종료", "Quit"),
    ("cmd.view.toggle_toc", "목차 패널", "Table of Contents"),
    (
        "cmd.view.toggle_breadcrumb",
        "절 경로 표시 켜기/끄기",
        "Toggle Section Path",
    ),
    ("cmd.diff.start", "문서 비교…", "Compare Documents…"),
    ("cmd.diff.next_change", "다음 변경", "Next Change"),
    ("cmd.diff.prev_change", "이전 변경", "Previous Change"),
    ("cmd.diff.toggle_changes", "변경 목록 패널", "Change List"),
    ("toc.title", "목차", "Contents"),
    ("toc.filter_hint", "목차 필터", "Filter contents"),
    (
        "toc.no_document",
        "목차를 보려면 PDF 문서를 여세요.",
        "Open a PDF document to see its contents.",
    ),
    (
        "toc.empty",
        "이 문서에는 목차가 없습니다",
        "This document has no table of contents",
    ),
    (
        "toc.no_match",
        "일치하는 항목이 없습니다",
        "No matching entries",
    ),
    ("toc.open_new_tab", "새 탭에서 열기", "Open in New Tab"),
    ("toc.open_split", "옆 분할에서 열기", "Open in Split Beside"),
    ("toc.copy_title", "제목 복사", "Copy Title"),
    ("toc.expand_all", "모두 펼치기", "Expand All"),
    ("toc.collapse_all", "모두 접기", "Collapse All"),
    ("tab.start", "시작", "Start"),
    ("tab.unknown", "(알 수 없음)", "(unknown)"),
    ("start.open_button", "파일 열기", "Open a file"),
    ("start.recent", "최근 문서", "Recent documents"),
    (
        "start.no_recent",
        "최근에 연 문서가 없습니다",
        "No recently opened documents",
    ),
    ("start.missing", "파일 없음", "File not found"),
    ("start.shortcuts", "단축키", "Shortcuts"),
    ("time.just_now", "방금", "just now"),
    ("time.minutes", "{n}분 전", "{n} min ago"),
    ("time.hours", "{n}시간 전", "{n} h ago"),
    ("time.days", "{n}일 전", "{n} days ago"),
    ("time.months", "{n}개월 전", "{n} months ago"),
    ("time.years", "{n}년 전", "{n} years ago"),
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
    ("doc.retry", "다시 시도", "Try Again"),
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
  --no-restore           마지막 세션을 복원하지 않는다
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
  --no-restore           do not restore the last session
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
    (
        "diff.tab_title",
        "비교: {left} ↔ {right}",
        "Compare: {left} ↔ {right}",
    ),
    (
        "diff.counts",
        "추가 {added} · 삭제 {removed} · 변경 {modified}",
        "{added} added · {removed} removed · {modified} changed",
    ),
    ("diff.sync", "동기 스크롤", "Sync Scrolling"),
    ("diff.options", "옵션", "Options"),
    (
        "diff.opt_skip_repeated",
        "반복되는 머리글·바닥글 무시",
        "Ignore repeated headers and footers",
    ),
    ("diff.opt_ignore_case", "대소문자 무시", "Ignore case"),
    ("diff.swap", "좌우 바꾸기", "Swap Sides"),
    ("diff.running", "비교하는 중…", "Comparing…"),
    ("diff.cancel", "취소", "Cancel"),
    (
        "diff.cancelled",
        "비교를 취소했습니다",
        "The comparison was cancelled",
    ),
    ("diff.rerun", "다시 비교", "Compare Again"),
    (
        "diff.failed",
        "비교할 수 없습니다: {}",
        "Could not compare: {}",
    ),
    (
        "diff.no_document",
        "문서가 열려 있지 않습니다",
        "The document is not open",
    ),
    ("diff.no_changes", "차이가 없습니다", "No differences"),
    (
        "diff.approximate",
        "일부 구간은 대략적으로 비교했습니다",
        "Some parts were compared only roughly",
    ),
    (
        "diff.stale",
        "문서가 바뀌었습니다 — 다시 비교하세요",
        "A document has changed — compare again",
    ),
    (
        "diff.first_change",
        "첫 변경입니다",
        "This is the first change",
    ),
    (
        "diff.last_change",
        "마지막 변경입니다",
        "This is the last change",
    ),
    ("changes.title", "변경 목록", "Changes"),
    ("changes.kind_added", "추가", "Added"),
    ("changes.kind_removed", "삭제", "Removed"),
    ("changes.kind_modified", "변경", "Changed"),
    (
        "changes.filter_hint",
        "이 종류를 목록에서 보이거나 숨깁니다",
        "Show or hide this kind in the list",
    ),
    ("changes.start", "문서 처음", "Start of document"),
    ("changes.page", "{n}쪽", "p. {n}"),
    (
        "changes.page_both",
        "이전 {left}쪽 → 이후 {right}쪽",
        "old p. {left} → new p. {right}",
    ),
    ("changes.tip_left", "이전", "Old"),
    ("changes.tip_right", "이후", "New"),
    (
        "changes.all_hidden",
        "표시할 종류가 모두 꺼져 있습니다",
        "Every kind is switched off",
    ),
    (
        "changes.no_result",
        "비교 결과가 없습니다",
        "There is no comparison result",
    ),
    ("diff.compare_with", "다른 문서와 비교", "Compare With"),
    (
        "diff.no_other_docs",
        "열린 다른 문서가 없습니다",
        "No other open documents",
    ),
    ("diff.dialog_title", "문서 비교", "Compare Documents"),
    ("diff.dialog_left", "이전(왼쪽)", "Older (left)"),
    ("diff.dialog_right", "이후(오른쪽)", "Newer (right)"),
    ("diff.dialog_pick", "문서를 고르세요", "Choose a document"),
    ("diff.dialog_from_file", "파일에서…", "From File…"),
    ("diff.dialog_compare", "비교", "Compare"),
    ("diff.dialog_close", "닫기", "Close"),
    (
        "diff.dialog_file_title",
        "비교할 PDF 고르기",
        "Choose a PDF to compare",
    ),
    ("cmd.view.goto_page", "쪽으로 이동…", "Go to Page…"),
    ("cmd.nav.back", "뒤로", "Back"),
    ("cmd.nav.forward", "앞으로", "Forward"),
    ("link.page", "{page}쪽", "Page {page}"),
    (
        "link.page_title",
        "{page}쪽 — {title}",
        "Page {page} — {title}",
    ),
    (
        "link.file_page",
        "{name} — {page}쪽",
        "{name} — page {page}",
    ),
    ("cmd.palette.commands", "명령 팔레트…", "Command Palette…"),
    ("cmd.palette.quick_open", "빠른 열기…", "Quick Open…"),
    (
        "palette.hint_commands",
        "명령 이름을 입력하세요",
        "Type a command",
    ),
    (
        "palette.hint_quick_open",
        "열린 탭이나 최근 문서의 이름을 입력하세요",
        "Type the name of an open tab or a recent document",
    ),
    ("palette.no_match", "일치하는 항목이 없습니다", "No matches"),
    ("palette.recent", "최근 문서", "Recent"),
    ("palette.window_n", "창 {n}", "Window {n}"),
    (
        "status.page",
        "{page} / {count} 쪽",
        "Page {page} of {count}",
    ),
    ("status.goto", "이동할 쪽:", "Go to page:"),
    (
        "status.preloading",
        "미리 불러오는 중 {done} / {total}",
        "Preloading {done} / {total}",
    ),
    ("status.zoom_choose", "배율 고르기", "Choose the zoom"),
    (
        "status.zoom_hint",
        "Ctrl+휠로도 확대·축소할 수 있습니다",
        "Ctrl+wheel zooms too",
    ),
    ("doc.password_submit", "열기", "Open"),
    (
        "doc.password_wrong",
        "암호가 맞지 않습니다",
        "The password is not correct",
    ),
];

/// Picks the language from the value of `LANG`.
pub fn lang_from_env_value(value: Option<&str>) -> Lang {
    match value {
        Some(v) if v.starts_with("ko") => Lang::Ko,
        _ => Lang::En,
    }
}

/// The language of this process, decided once: [`force_lang`] or else `LANG`.
static LANG: OnceLock<Lang> = OnceLock::new();

/// Sets the language for the rest of the process (the `language` setting). Has no effect once
/// any text was looked up; returns whether it took effect.
pub fn force_lang(lang: Lang) -> bool {
    LANG.set(lang).is_ok()
}

/// The language of this process, decided once from [`force_lang`] or `LANG`.
pub fn current_lang() -> Lang {
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
