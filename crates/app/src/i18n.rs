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
    ("cmd.file.open_folder", "폴더 열기…", "Open Folder…"),
    ("cmd.file.close_folder", "폴더 닫기", "Close Folder"),
    ("cmd.tab.close", "탭 닫기", "Close Tab"),
    ("cmd.tab.next", "다음 탭", "Next Tab"),
    ("cmd.tab.prev", "이전 탭", "Previous Tab"),
    ("cmd.tab.duplicate", "탭 복제", "Duplicate Tab"),
    ("cmd.tab.pin", "탭 고정", "Keep Tab Open"),
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
    (
        "cmd.edit.clear_search_history",
        "검색 기록 지우기",
        "Clear Search History",
    ),
    ("cmd.edit.find", "찾기…", "Find…"),
    ("cmd.edit.find_next", "다음 찾기", "Find Next"),
    ("cmd.edit.find_prev", "이전 찾기", "Find Previous"),
    (
        "cmd.edit.find_all",
        "모든 문서에서 찾기",
        "Find in All Documents",
    ),
    (
        "cmd.edit.find_all_next",
        "다음 검색 결과",
        "Next Search Result",
    ),
    (
        "cmd.edit.find_all_prev",
        "이전 검색 결과",
        "Previous Search Result",
    ),
    (
        "cmd.edit.find_all_clear",
        "모든 문서 찾기 지우기",
        "Clear Search in All Documents",
    ),
    (
        "cmd.edit.search_selection",
        "이 텍스트로 검색",
        "Search for This Text",
    ),
    (
        "cmd.edit.translate_selection",
        "선택한 글 번역",
        "Translate Selection",
    ),
    ("cmd.edit.translate", "번역", "Translate"),
    ("cmd.edit.capture_region", "영역 캡처", "Capture Area"),
    (
        "cmd.edit.capture_page",
        "현재 쪽 캡처",
        "Capture Current Page",
    ),
    (
        "shot.hint",
        "끌어서 영역 선택 · Esc 취소",
        "Drag to select an area · Esc to cancel",
    ),
    ("shot.size", "{w} × {h} px", "{w} × {h} px"),
    ("shot.working", "캡처하는 중…", "Capturing…"),
    (
        "shot.copied",
        "캡처해 복사함 {w} × {h}",
        "Captured and copied {w} × {h}",
    ),
    (
        "shot.copy_failed",
        "캡처했지만 복사하지 못함 ({err}) {w} × {h}",
        "Captured, but could not copy ({err}) {w} × {h}",
    ),
    ("shot.failed", "캡처하지 못함: {}", "Could not capture: {}"),
    ("shot.save", "저장", "Save"),
    ("shot.saving", "저장하는 중…", "Saving…"),
    ("shot.saved", "저장함: {}", "Saved: {}"),
    (
        "shot.save_failed",
        "저장하지 못함: {}",
        "Could not save: {}",
    ),
    ("find.history_label", "검색 기록", "Search history"),
    (
        "find.history_hint",
        "검색 기록 (↑ ↓)",
        "Search history (↑ ↓)",
    ),
    (
        "find.history_none",
        "검색 기록이 없습니다",
        "No search history",
    ),
    ("find.history_remove", "이 항목 지우기", "Remove this entry"),
    (
        "find.history_clear",
        "검색 기록 지우기",
        "Clear Search History",
    ),
    (
        "status.search_history_cleared",
        "검색 기록을 지웠습니다",
        "Search history cleared",
    ),
    ("find.hint", "문서에서 찾기", "Find in document"),
    (
        "cmd.edit.find_in_chapter",
        "지금 장에서 찾기",
        "Find in This Chapter",
    ),
    ("cmd.edit.copy_location", "위치 복사", "Copy Location"),
    ("cmd.edit.copy_citation", "인용 복사", "Copy Citation"),
    (
        "cmd.edit.copy_section_text",
        "지금 절의 글 복사",
        "Copy Section Text",
    ),
    (
        "cmd.edit.copy_chapter_text",
        "지금 장의 글 복사",
        "Copy Chapter Text",
    ),
    ("status.copied_text", "복사함: {}", "Copied: {}"),
    (
        "status.range_copied",
        "{title} — {chars}자 복사",
        "{title} — {chars} characters copied",
    ),
    (
        "status.range_copied_pages",
        "{title} — {chars}자 복사 ({pages}쪽)",
        "{title} — {chars} characters copied ({pages} pages)",
    ),
    (
        "status.range_empty",
        "{title} — 복사할 글이 없습니다",
        "{title} — no text to copy",
    ),
    (
        "status.no_section",
        "지금 위치에는 목차의 절이 없습니다",
        "There is no section of the table of contents here",
    ),
    (
        "toc.copy_section_text",
        "이 절의 글 복사",
        "Copy Text of This Section",
    ),
    ("find.scope", "범위", "Scope"),
    ("find.scope_label", "찾기 범위", "Search scope"),
    (
        "find.scope_hint",
        "찾을 범위를 고릅니다 (문서 전체, 지금 장·절)",
        "Choose where to search (whole document, this chapter or section)",
    ),
    (
        "find.scope_no_outline",
        "이 문서에는 목차가 없어 범위를 나눌 수 없습니다",
        "This document has no table of contents to take a scope from",
    ),
    ("find.scope_all", "문서 전체", "Whole document"),
    ("find.scope_chip", "범위: {}", "Scope: {}"),
    (
        "find.scope_clear_label",
        "찾기 범위 풀기",
        "Clear search scope",
    ),
    (
        "find.scope_clear_hint",
        "문서 전체에서 찾기",
        "Search the whole document",
    ),
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
    (
        "gsearch.hint",
        "열린 모든 문서에서 찾기",
        "Find in all open documents",
    ),
    ("gsearch.clear", "지우기", "Clear"),
    (
        "gsearch.summary",
        "문서 {docs}개에서 {n}건",
        "{n} results in {docs} documents",
    ),
    (
        "gsearch.summary_one_doc",
        "문서 {docs}개에서 {n}건",
        "{n} results in {docs} document",
    ),
    (
        "gsearch.summary_one",
        "문서 {docs}개에서 {n}건",
        "{n} result in {docs} document",
    ),
    (
        "gsearch.searching",
        "(찾는 중 {percent}%)",
        "(searching {percent}%)",
    ),
    (
        "gsearch.searching_alone",
        "찾는 중 {percent}%",
        "Searching {percent}%",
    ),
    ("gsearch.none", "결과 없음", "No results"),
    (
        "gsearch.no_docs",
        "열린 문서가 없습니다",
        "No open documents",
    ),
    (
        "gsearch.truncated",
        "일부 문서는 결과가 너무 많아 잘렸습니다",
        "Some documents have too many results; their lists are cut short",
    ),
    ("gsearch.head", "{name}, {n}건", "{name}, {n} results"),
    ("gsearch.head_one", "{name}, {n}건", "{name}, {n} result"),
    ("sidebar.files", "파일", "Files"),
    ("sidebar.search", "검색", "Search"),
    ("sidebar.pages", "쪽", "Pages"),
    ("sidebar.pages_hint", "쪽 썸네일", "Page thumbnails"),
    (
        "cmd.view.show_pages",
        "쪽 썸네일 보기",
        "Show Page Thumbnails",
    ),
    (
        "pages.no_doc",
        "쪽을 보일 문서가 없습니다",
        "No document to show pages of",
    ),
    ("pages.loading", "문서를 읽는 중…", "Loading the document…"),
    ("pages.smaller", "썸네일 작게", "Smaller thumbnails"),
    ("pages.larger", "썸네일 크게", "Larger thumbnails"),
    (
        "pages.only_matches",
        "검색 결과가 있는 쪽만",
        "Only pages with results",
    ),
    (
        "pages.only_matches_doc",
        "이 문서에서 찾은 결과가 있는 쪽만 보이고, 쪽마다 건수가 붙습니다",
        "Shows only the pages with results of this document's search, with their counts",
    ),
    (
        "pages.only_matches_all",
        "모든 문서 찾기의 이 문서 결과가 있는 쪽만 보이고, 쪽마다 건수가 붙습니다",
        "Shows only the pages with results of the search in all documents, with their counts",
    ),
    (
        "pages.only_matches_none",
        "이 문서에 검색 결과가 없습니다. 문서 안에서 찾거나 모든 문서에서 찾으면 켤 수 있습니다",
        "No search results in this document. Search in it or in all documents to use this",
    ),
    ("pages.item_count", "{page}, {n}건", "{page}, {n} results"),
    (
        "pages.item_count_one",
        "{page}, {n}건",
        "{page}, {n} result",
    ),
    ("sidebar.search_count", "검색 ({n})", "Search ({n})"),
    (
        "sidebar.files_hint",
        "탐색기와 목차 (Ctrl+Shift+E)",
        "Explorer and contents (Ctrl+Shift+E)",
    ),
    (
        "sidebar.search_hint",
        "열린 모든 문서에서 찾기 (Ctrl+Shift+F)",
        "Find in all open documents (Ctrl+Shift+F)",
    ),
    ("sidebar.marks", "표시", "Marks"),
    (
        "sidebar.marks_hint",
        "책갈피, 마크, 하이라이트",
        "Bookmarks, marks and highlights",
    ),
    ("cmd.edit.add_bookmark", "책갈피 추가", "Add Bookmark"),
    ("cmd.edit.next_bookmark", "다음 책갈피", "Next Bookmark"),
    ("cmd.edit.prev_bookmark", "이전 책갈피", "Previous Bookmark"),
    ("cmd.edit.set_mark", "마크 찍기", "Set Mark"),
    ("cmd.edit.go_to_mark", "마크로 가기", "Go to Mark"),
    (
        "cmd.edit.highlight",
        "선택한 글 하이라이트",
        "Highlight Selection",
    ),
    (
        "cmd.edit.next_highlight",
        "다음 하이라이트",
        "Next Highlight",
    ),
    (
        "cmd.edit.prev_highlight",
        "이전 하이라이트",
        "Previous Highlight",
    ),
    (
        "cmd.edit.copy_highlights",
        "지금 문서의 하이라이트 모두 복사",
        "Copy All Highlights of This Document",
    ),
    ("cmd.view.show_marks", "표시 보기", "Show Marks"),
    ("marks.scope_doc", "이 문서", "This document"),
    ("marks.scope_all", "모든 문서", "All documents"),
    ("marks.filter", "거르기", "Filter"),
    ("marks.only_color", "{}만", "Only {}"),
    (
        "marks.no_doc",
        "표시를 보일 문서가 없습니다",
        "No document to show marks of",
    ),
    ("marks.section_bookmarks", "책갈피 ({n})", "Bookmarks ({n})"),
    (
        "marks.section_highlights",
        "하이라이트 ({n})",
        "Highlights ({n})",
    ),
    (
        "marks.a11y_bookmark",
        "책갈피: {name}, {page}쪽",
        "Bookmark: {name}, p. {page}",
    ),
    (
        "marks.a11y_mark",
        "마크 {letter}, {page}",
        "Mark {letter}, {page}",
    ),
    (
        "marks.a11y_highlight",
        "하이라이트 ({color}): {text}, {page}쪽",
        "Highlight ({color}): {text}, p. {page}",
    ),
    ("marks.a11y_note", "메모: {}", "Note: {}"),
    ("marks.page_name", "p. {}", "p. {}"),
    ("marks.edit_name", "책갈피 이름", "Bookmark name"),
    ("marks.edit_note", "메모", "Note"),
    ("marks.menu.highlight", "하이라이트", "Highlight"),
    ("marks.menu.change_color", "색 바꾸기", "Change Color"),
    ("marks.menu.note", "메모…", "Note…"),
    ("marks.menu.edit_note", "메모 고치기", "Edit Note"),
    ("marks.menu.copy_text", "글 복사", "Copy Text"),
    ("marks.menu.remove", "지우기", "Remove"),
    ("marks.menu.rename", "이름 바꾸기", "Rename"),
    ("marks.color.yellow", "노랑", "yellow"),
    ("marks.color.green", "초록", "green"),
    ("marks.color.blue", "파랑", "blue"),
    ("marks.color.pink", "분홍", "pink"),
    ("marks.color.orange", "주황", "orange"),
    (
        "marks.press_letter",
        "마크: 글자를 누르세요",
        "Mark: press a letter",
    ),
    ("marks.mark_name", "마크 {}", "Mark {}"),
    (
        "marks.no_mark",
        "마크 {} 가 없습니다",
        "There is no mark {}",
    ),
    (
        "marks.no_more_bookmarks",
        "그쪽으로 책갈피가 더 없습니다",
        "No more bookmarks that way",
    ),
    (
        "marks.no_more_highlights",
        "그쪽으로 하이라이트가 더 없습니다",
        "No more highlights that way",
    ),
    (
        "marks.copy_entry",
        "[p. {page}] {text}",
        "[p. {page}] {text}",
    ),
    ("marks.copy_note", "    메모: {}", "    Note: {}"),
    (
        "marks.copied_all",
        "하이라이트 {}개를 복사함",
        "Copied {} highlights",
    ),
    ("sel.copied", "복사됨 ({} 글자)", "Copied ({} characters)"),
    ("cmd.view.zoom_in", "확대", "Zoom In"),
    ("cmd.view.zoom_out", "축소", "Zoom Out"),
    ("cmd.view.zoom_reset", "100%", "Actual Size"),
    ("cmd.view.fit_width", "폭 맞춤", "Fit Width"),
    ("cmd.view.fit_page", "쪽 맞춤", "Fit Page"),
    (
        "cmd.view.scroll_faster",
        "스크롤 속도 높이기",
        "Increase Scroll Speed",
    ),
    (
        "cmd.view.scroll_slower",
        "스크롤 속도 낮추기",
        "Decrease Scroll Speed",
    ),
    ("menu.scroll_speed", "스크롤 속도", "Scroll Speed"),
    (
        "menu.scroll_speed_default",
        "기본값으로",
        "Reset to Default",
    ),
    ("status.scroll_speed", "스크롤 속도 {}", "Scroll speed {}"),
    ("cmd.view.translate_mode", "번역 모드", "Translation Mode"),
    ("menu.translate_model", "번역 모델", "Translation Model"),
    ("menu.translate_model_loading", "불러오는 중…", "Loading…"),
    (
        "menu.translate_model_failed",
        "목록을 받지 못했습니다: {}",
        "Could not get the list: {}",
    ),
    (
        "menu.translate_model_empty",
        "서버에 모델이 없습니다",
        "The server has no models",
    ),
    (
        "menu.translate_model_config",
        "설정 파일의 모델로",
        "Use the Model of the Settings File",
    ),
    (
        "menu.translate_model_refresh",
        "목록 새로 고침",
        "Refresh the List",
    ),
    ("status.translate_on", "번역 모드 켬", "Translation mode on"),
    (
        "status.translate_off",
        "번역 모드 끔",
        "Translation mode off",
    ),
    (
        "status.translate_model",
        "번역 모델: {}",
        "Translation model: {}",
    ),
    ("translate.button", "번역", "Translate"),
    ("translate.button_on", "번역: 켜짐", "Translate: on"),
    ("translate.button_off", "번역: 꺼짐", "Translate: off"),
    (
        "translate.tooltip",
        "모델: {model}\n백엔드: {backend} ({endpoint})",
        "Model: {model}\nBackend: {backend} ({endpoint})",
    ),
    (
        "translate.loading_model",
        "모델 준비 중…",
        "Preparing the model…",
    ),
    ("translate.unavailable", "사용 불가", "Unavailable"),
    ("translate.working", "번역 중…", "Translating…"),
    (
        "translate.truncated",
        "앞부분만 번역했습니다",
        "Only the beginning was translated",
    ),
    ("translate.copy", "복사", "Copy"),
    ("translate.close", "닫기", "Close"),
    (
        "translate.err.unreachable_ollama",
        "번역 서버에 닿지 않습니다. Ollama 가 실행 중인지 확인하세요 ({endpoint})",
        "Cannot reach the translation server. Check that Ollama is running ({endpoint})",
    ),
    (
        "translate.err.unreachable_openai",
        "번역 서버에 닿지 않습니다. 서버가 실행 중인지 확인하세요 ({endpoint})",
        "Cannot reach the translation server. Check that the server is running ({endpoint})",
    ),
    (
        "translate.err.model_missing_ollama",
        "모델 {model} 이 없습니다. 터미널에서 받으세요: ollama pull {model}",
        "The model {model} is not installed. Get it in a terminal: ollama pull {model}",
    ),
    (
        "translate.err.model_missing_openai",
        "서버에 모델 {model} 이 없습니다. 서버에 올리거나 보기 ▸ 번역 모델에서 고르세요",
        "The server has no model {model}. Load it on the server or pick one in View ▸ Translation Model",
    ),
    (
        "translate.err.http",
        "번역 서버 오류 {status}: {message}",
        "Translation server error {status}: {message}",
    ),
    (
        "translate.err.protocol",
        "번역 서버의 응답을 읽을 수 없습니다: {message}",
        "Cannot read the answer of the translation server: {message}",
    ),
    ("translate.err.cancelled", "취소됨", "Cancelled"),
    (
        "translate.err.model_missing_embedded",
        "모델 파일이 없습니다({model}). 모델 파일(.gguf)을 {endpoint} 에 두세요",
        "No model file ({model}). Put a model file (.gguf) in {endpoint}",
    ),
    (
        "translate.err.not_built",
        "이 실행 파일에는 내장 번역 엔진이 들어 있지 않습니다. cargo build --features embedded-vulkan 으로 빌드하세요",
        "This executable has no embedded translation engine. Build it with cargo build --features embedded-vulkan",
    ),
    (
        "translate.err.engine",
        "모델을 읽지 못했습니다: {message}",
        "Could not load the model: {message}",
    ),
    (
        "translate.tooltip_embedded",
        "모델 파일: {model}\n엔진: 내장 (llama.cpp)\n모델 폴더: {dir}",
        "Model file: {model}\nEngine: embedded (llama.cpp)\nModels folder: {dir}",
    ),
    (
        "cmd.view.toggle_page_colors",
        "쪽 색 바꾸기",
        "Toggle Page Colors",
    ),
    ("cmd.view.cycle_theme", "테마 바꾸기", "Switch Theme"),
    ("menu.theme", "테마", "Theme"),
    ("menu.page_colors", "쪽 색", "Page Colors"),
    ("theme.system", "시스템 따름", "Follow System"),
    ("theme.light", "밝게", "Light"),
    ("theme.dark", "어둡게", "Dark"),
    ("page_colors.original", "원래대로", "Original"),
    ("page_colors.dark", "어둡게", "Dark"),
    ("page_colors.follow_theme", "테마 따름", "Follow Theme"),
    ("status.theme", "테마: {}", "Theme: {}"),
    ("status.page_colors", "쪽 색: {}", "Page colors: {}"),
    ("cmd.app.quit", "종료", "Quit"),
    (
        "cmd.view.toggle_toc",
        "사이드바 켜기/끄기",
        "Toggle Sidebar",
    ),
    ("cmd.view.show_explorer", "탐색기 보기", "Show Explorer"),
    (
        "cmd.view.toggle_explorer_section",
        "탐색기 구역 켜기/끄기",
        "Toggle Explorer Section",
    ),
    (
        "cmd.view.toggle_outline_section",
        "목차 구역 켜기/끄기",
        "Toggle Contents Section",
    ),
    (
        "cmd.view.refresh_explorer",
        "탐색기 새로 고침",
        "Refresh Explorer",
    ),
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
    ("explorer.title", "탐색기", "Explorer"),
    (
        "explorer.no_folder",
        "열린 폴더가 없습니다",
        "No folder is open",
    ),
    ("explorer.open_folder", "폴더 열기", "Open Folder"),
    (
        "explorer.filter_hint",
        "파일 이름 거르기",
        "Filter file names",
    ),
    (
        "explorer.scanning",
        "폴더를 읽는 중…",
        "Reading the folder…",
    ),
    (
        "explorer.empty",
        "이 폴더에는 PDF 파일이 없습니다",
        "This folder has no PDF files",
    ),
    (
        "explorer.truncated",
        "항목이 너무 많아 일부만 보입니다",
        "Too many entries: only some are shown",
    ),
    (
        "explorer.failed",
        "폴더를 읽을 수 없습니다: {}",
        "Cannot read the folder: {}",
    ),
    ("explorer.retry", "다시 시도", "Try Again"),
    (
        "explorer.no_match",
        "일치하는 파일이 없습니다",
        "No matching files",
    ),
    ("explorer.refresh", "새로 고침", "Refresh"),
    (
        "explorer.refresh_manual",
        "새로 고침 (이 폴더는 자동으로 갱신되지 않습니다)",
        "Refresh (this folder is not updated automatically)",
    ),
    ("explorer.collapse_all", "모두 접기", "Collapse All"),
    ("explorer.menu.open", "열기", "Open"),
    (
        "explorer.menu.open_background",
        "새 탭에서 열기(뒤에서)",
        "Open in Background Tab",
    ),
    (
        "explorer.menu.open_split",
        "옆 분할에서 열기",
        "Open in Split Beside",
    ),
    (
        "explorer.menu.open_window",
        "새 창에서 열기",
        "Open in New Window",
    ),
    (
        "explorer.menu.pick_compare",
        "비교 대상으로 선택",
        "Select for Compare",
    ),
    (
        "explorer.menu.compare_picked",
        "선택한 것과 비교",
        "Compare with Selected",
    ),
    (
        "explorer.menu.compare_current",
        "지금 문서와 비교",
        "Compare with Current Document",
    ),
    ("explorer.menu.copy_path", "경로 복사", "Copy Path"),
    (
        "explorer.menu.copy_relative",
        "상대 경로 복사",
        "Copy Relative Path",
    ),
    (
        "explorer.menu.reveal",
        "파일 관리자에서 보기",
        "Show in File Manager",
    ),
    ("explorer.menu.expand", "펼치기", "Expand"),
    ("explorer.menu.collapse", "접기", "Collapse"),
    (
        "explorer.menu.expand_all",
        "하위 모두 펼치기",
        "Expand All Below",
    ),
    (
        "explorer.picked",
        "비교 대상으로 선택됨",
        "Selected for comparison",
    ),
    ("explorer.copied", "복사했습니다: {}", "Copied: {}"),
    (
        "explorer.reveal_failed",
        "파일 관리자를 열 수 없습니다: {}",
        "Cannot open the file manager: {}",
    ),
    ("cmd.file.new_project", "새 프로젝트…", "New Project…"),
    ("cmd.file.open_project", "프로젝트 열기…", "Open Project…"),
    ("cmd.file.import", "PDF 가져오기…", "Import PDFs…"),
    (
        "cmd.file.import_folder",
        "폴더에서 가져오기…",
        "Import from Folder…",
    ),
    (
        "cmd.file.import_paste",
        "붙여넣어 가져오기",
        "Import from Clipboard",
    ),
    (
        "cmd.file.project_trash",
        "프로젝트 휴지통 보기",
        "Show Project Trash",
    ),
    (
        "cmd.file.rename_project",
        "프로젝트 이름 바꾸기…",
        "Rename Project…",
    ),
    (
        "project.rename.title",
        "프로젝트 이름 바꾸기",
        "Rename Project",
    ),
    ("project.rename.ok", "바꾸기", "Rename"),
    ("explorer.import_button", "PDF 가져오기", "Import PDFs"),
    (
        "explorer.import_tip",
        "PDF 를 이 프로젝트로 가져옵니다",
        "Import PDFs into this project",
    ),
    (
        "explorer.project_empty",
        "이 프로젝트에는 아직 문서가 없습니다",
        "This project has no documents yet",
    ),
    ("explorer.import_first", "PDF 가져오기…", "Import PDFs…"),
    (
        "explorer.paste_hint",
        "파일 관리자에서 복사한 파일을 Ctrl+V 로 붙여 넣어도 됩니다",
        "You can also paste files copied in the file manager with Ctrl+V",
    ),
    ("explorer.new_folder_label", "새 폴더:", "New folder:"),
    ("explorer.rename_label", "새 이름:", "New name:"),
    (
        "explorer.menu.import_here",
        "여기로 가져오기…",
        "Import Here…",
    ),
    (
        "explorer.menu.import_folder",
        "폴더째 가져오기…",
        "Import a Folder Here…",
    ),
    ("explorer.menu.new_folder", "새 폴더", "New Folder"),
    ("explorer.menu.rename", "이름 바꾸기", "Rename"),
    (
        "explorer.menu.trash",
        "프로젝트에서 지우기",
        "Remove from Project",
    ),
    ("project.cancel", "취소", "Cancel"),
    ("project.close", "닫기", "Close"),
    ("project.new.title", "새 프로젝트", "New Project"),
    ("project.new.name", "이름", "Name"),
    ("project.new.name_hint", "프로젝트 이름", "Project name"),
    ("project.new.location", "위치", "Location"),
    ("project.new.browse", "찾아보기…", "Browse…"),
    (
        "project.new.browse_title",
        "프로젝트를 만들 폴더",
        "Folder to make the project in",
    ),
    ("project.new.create", "만들기", "Create"),
    ("project.new.open", "열기", "Open"),
    (
        "project.new.is_project",
        "이미 프로젝트입니다: {}",
        "This folder already is a project: {}",
    ),
    (
        "project.new.empty_name",
        "이름을 입력하세요",
        "Enter a name",
    ),
    (
        "project.new.bad_location",
        "위치는 절대 경로여야 합니다",
        "The location must be an absolute path",
    ),
    (
        "project.new.unusable_name",
        "이 이름으로는 폴더를 만들 수 없습니다",
        "This name cannot be used for a folder",
    ),
    (
        "project.new.checking",
        "폴더를 확인하는 중…",
        "Checking the folder…",
    ),
    (
        "project.new.not_folder",
        "그 위치에 폴더가 아닌 파일이 있습니다",
        "There is a file, not a folder, at that location",
    ),
    (
        "project.new.existing",
        "이미 있는 폴더를 프로젝트로 삼습니다",
        "The existing folder becomes the project",
    ),
    (
        "project.new.has_pdfs",
        "이 폴더의 PDF {n}개가 프로젝트의 문서가 됩니다",
        "The {n} PDFs in this folder become the project's documents",
    ),
    ("project.open_title", "프로젝트 열기", "Open Project"),
    (
        "project.missing",
        "폴더가 없습니다: {}",
        "The folder does not exist: {}",
    ),
    ("project.import_title", "가져올 PDF", "PDFs to Import"),
    (
        "project.import_folder_title",
        "가져올 PDF 가 있는 폴더",
        "Folder to Import PDFs From",
    ),
    ("project.prompt.title", "프로젝트 열기", "Open Project"),
    (
        "project.prompt.text",
        "프로젝트가 아닙니다. 프로젝트로 만들까요?\n{}",
        "This folder is not a project. Make it one?\n{}",
    ),
    ("project.prompt.make", "만들기", "Make Project"),
    (
        "project.prompt.folder",
        "그냥 폴더로 열기",
        "Open as Folder",
    ),
    ("project.prompt.default_name", "프로젝트", "Project"),
    ("project.conflict.title", "이름이 같은 파일", "Same Name"),
    (
        "project.conflict.text",
        "이름이 같은 다른 파일이 있습니다: {}",
        "There is another file with the same name: {}",
    ),
    (
        "project.conflict.existing",
        "프로젝트에 있는 것",
        "In the project",
    ),
    ("project.conflict.incoming", "가져오는 것", "Being imported"),
    (
        "project.conflict.all",
        "남은 충돌에도 적용",
        "Apply to the remaining clashes",
    ),
    ("project.conflict.replace", "교체", "Replace"),
    ("project.conflict.keep_both", "둘 다 보관", "Keep Both"),
    ("project.conflict.skip", "건너뛰기", "Skip"),
    (
        "project.pick.title",
        "폴더에서 가져오기 — {}",
        "Import from Folder — {}",
    ),
    (
        "project.pick.scanning",
        "PDF 를 찾는 중…",
        "Looking for PDFs…",
    ),
    ("project.pick.filter", "경로로 거르기", "Filter by path"),
    ("project.pick.all", "모두 선택", "Select All"),
    ("project.pick.none", "모두 해제", "Select None"),
    (
        "project.pick.totals",
        "{n}개 선택 · {size}",
        "{n} selected · {size}",
    ),
    (
        "project.pick.keep",
        "하위 폴더 구조 유지",
        "Keep sub-folders",
    ),
    ("project.pick.into", "대상 폴더: {}", "Into: {}"),
    ("project.pick.import", "가져오기 ({n}개)", "Import ({n})"),
    (
        "project.pick.row",
        "가져오기: {path}, {size}, {check}",
        "Import: {path}, {size}, {check}",
    ),
    ("project.pick.checked", "선택됨", "checked"),
    ("project.pick.unchecked", "선택 안 됨", "unchecked"),
    ("project.pick.already", "이미 있음", "already there"),
    ("project.import.starting", "가져오는 중…", "Importing…"),
    (
        "project.import.progress",
        "가져오는 중 {i} / {n} — {name}",
        "Importing {i} / {n} — {name}",
    ),
    ("project.import.queued", " (대기 {n})", " ({n} waiting)"),
    ("project.import.done", "{n}개 가져옴", "{n} imported"),
    (
        "project.import.already",
        "{n}개는 이미 있음",
        "{n} already there",
    ),
    ("project.import.skipped", "{n}개 건너뜀", "{n} skipped"),
    ("project.import.failed", "{n}개 실패", "{n} failed"),
    ("project.import.cancelled", "취소됨 — {}", "Cancelled — {}"),
    ("project.import.cancel", "가져오기 취소", "Cancel Import"),
    ("project.import.details", "자세히", "Details"),
    ("project.details.title", "가져오기 결과", "Import Result"),
    (
        "project.detail.already",
        "이미 있음: {}",
        "already there: {}",
    ),
    ("project.detail.not_pdf", "PDF 가 아님", "not a PDF"),
    ("project.detail.hidden", "숨김 파일", "hidden file"),
    (
        "project.detail.inside",
        "이미 프로젝트 안에 있음",
        "already inside the project",
    ),
    (
        "project.detail.declined",
        "이름이 같아 건너뜀",
        "skipped: same name",
    ),
    ("project.detail.failed", "실패: {}", "failed: {}"),
    (
        "project.detail.not_pdf_count",
        "PDF 가 아닌 파일 {n}개는 건너뜀",
        "{n} files that are not PDFs were left out",
    ),
    ("project.name.empty", "이름을 입력하세요", "Enter a name"),
    (
        "project.name.invalid",
        "쓸 수 없는 이름입니다(/ 나 . 으로 시작하는 이름)",
        "This name cannot be used (/ or a leading .)",
    ),
    (
        "project.error.already",
        "이미 프로젝트입니다: {}",
        "Already a project: {}",
    ),
    (
        "project.error.exists",
        "이미 있습니다: {}",
        "Already exists: {}",
    ),
    (
        "project.error.missing",
        "없습니다: {}",
        "Does not exist: {}",
    ),
    (
        "project.error.path",
        "프로젝트 밖이거나 바꿀 수 없는 경로입니다: {}",
        "Not a path that can be changed in the project: {}",
    ),
    (
        "project.error.io",
        "파일을 다루지 못했습니다: {}",
        "File operation failed: {}",
    ),
    ("project.restored", "되살렸습니다: {}", "Restored: {}"),
    (
        "project.trashed",
        "프로젝트의 휴지통으로 옮겼습니다: {}",
        "Moved to the project's trash: {}",
    ),
    (
        "project.trash.confirm_title",
        "프로젝트에서 지우기",
        "Remove from Project",
    ),
    (
        "project.trash.confirm",
        "{}\n프로젝트의 휴지통으로 옮깁니다. 원본 파일은 건드리지 않습니다.",
        "{}\nThis moves it to the project's trash. The original file is not touched.",
    ),
    ("project.trash.move", "휴지통으로 옮기기", "Move to Trash"),
    ("project.trash.title", "프로젝트 휴지통", "Project Trash"),
    (
        "project.trash.empty",
        "휴지통이 비어 있습니다",
        "The trash is empty",
    ),
    ("project.trash.restore", "되살리기", "Restore"),
    ("project.trash.empty_button", "휴지통 비우기", "Empty Trash"),
    ("start.recent_projects", "최근 프로젝트", "Recent projects"),
    ("start.new_project_button", "새 프로젝트", "New project"),
    (
        "window.title_project",
        "{title} — {project} — refdesk",
        "{title} — {project} — refdesk",
    ),
    ("tab.start", "시작", "Start"),
    ("tab.unknown", "(알 수 없음)", "(unknown)"),
    ("start.open_button", "파일 열기", "Open a file"),
    ("start.open_folder_button", "폴더 열기", "Open a folder"),
    ("start.recent", "최근 문서", "Recent documents"),
    ("start.recent_folders", "최근 폴더", "Recent folders"),
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
    ("dialog.folder_title", "폴더 열기", "Open a folder"),
    (
        "cli.help",
        "사용법:
  refdesk [옵션] [폴더] [파일[#page=N]]...
  refdesk --find 글 [폴더] [파일...]
  refdesk --diff 이전.pdf 이후.pdf
  refdesk --translate 글

옵션:
  --find 글              연 뒤 열린 모든 문서에서 그 글을 찾는다
  --diff A B             두 PDF 를 비교한다
  --translate 글         설정된 모델로 번역해 출력하고 끝낸다 (창을 띄우지 않는다)
  --no-single-instance   실행 중인 인스턴스를 찾지 않고 새로 띄운다 (개발용)
  --no-restore           마지막 세션을 복원하지 않는다
  -h, --help             이 도움말을 보인다
  -V, --version          버전을 보인다
  --                     이 뒤의 인자는 모두 파일로 본다

폴더를 주면 그 폴더를 작업 폴더로 열어 왼쪽 탐색기에 보인다.
이미 실행 중이면 새 창을 띄우지 않고 그 창에 탭으로 연다.",
        "Usage:
  refdesk [options] [folder] [file[#page=N]]...
  refdesk --find text [folder] [file...]
  refdesk --diff old.pdf new.pdf
  refdesk --translate text

Options:
  --find text            search all open documents for the text once they are open
  --diff A B             compare two PDF files
  --translate text       translate the text with the configured model, print it and exit (no window)
  --no-single-instance   start a new instance without looking for a running one (for development)
  --no-restore           do not restore the last session
  -h, --help             show this help
  -V, --version          show the version
  --                     treat everything after it as a file

A folder opens as the workspace and is shown in the explorer on the left.
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
        "cli.find_needs_text",
        "--find 에는 찾을 글이 필요합니다",
        "--find needs the text to find",
    ),
    (
        "cli.find_twice",
        "--find 는 한 번만 쓸 수 있습니다",
        "--find can be used only once",
    ),
    (
        "cli.translate_needs_text",
        "--translate 에는 번역할 글이 필요합니다",
        "--translate needs the text to translate",
    ),
    (
        "cli.translate_alone",
        "--translate 는 다른 인자와 함께 쓸 수 없습니다",
        "--translate cannot be combined with other arguments",
    ),
    (
        "cli.find_with_diff",
        "--find 는 --diff 와 함께 쓸 수 없습니다",
        "--find cannot be combined with --diff",
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
    (
        "cmd.diff.toggle_margin_guides",
        "비교: 제외 여백 표시 켜기/끄기",
        "Compare: Toggle Excluded Margin Lines",
    ),
    ("diff.margins", "제외 여백", "Excluded Margins"),
    (
        "diff.margins_hint",
        "쪽 가장자리의 비율. 이 띠 안의 낱말은 비교하지 않습니다.",
        "Share of the page edge. Words inside these bands are not compared.",
    ),
    ("diff.margin_top", "위", "Top"),
    ("diff.margin_bottom", "아래", "Bottom"),
    ("diff.margin_left", "왼쪽", "Left"),
    ("diff.margin_right", "오른쪽", "Right"),
    ("diff.margin_reset", "모두 0 으로", "Reset All to 0"),
    (
        "diff.margin_summary",
        "여백 제외: {}",
        "Margins excluded: {}",
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
    ("cmd.view.zoom_region", "영역 확대", "Zoom to Area"),
    ("cmd.view.zoom_back", "이전 배율로", "Previous Zoom"),
    (
        "zoom_region.hint",
        "확대할 영역을 끌어 잡으세요 — Esc 로 취소",
        "Drag over the area to zoom to — Esc to cancel",
    ),
    ("cmd.nav.link_hints", "링크 힌트", "Link Hints"),
    (
        "cmd.nav.link_hints_new_tab",
        "링크 힌트: 새 탭에서",
        "Link Hints: Open in New Tab",
    ),
    ("hints.none", "보이는 링크가 없습니다", "No links in view"),
    (
        "hints.a11y",
        "링크 힌트 {label}: {target}",
        "Link hint {label}: {target}",
    ),
    ("pin.button", "미리보기 고정", "Pin preview"),
    (
        "pin.window",
        "고정한 미리보기: {title}",
        "Pinned preview: {title}",
    ),
    ("pin.go", "여기로 가기", "Go there"),
    ("pin.close", "고정한 미리보기 닫기", "Close pinned preview"),
    ("pin.resize", "크기 바꾸기", "Resize"),
    (
        "cmd.view.close_pinned",
        "고정한 미리보기 모두 닫기",
        "Close All Pinned Previews",
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
    ("palette.folder", "작업 폴더", "Folder"),
    (
        "palette.folder_more",
        "작업 폴더의 파일 {n}개 더 — 이름을 입력하면 나옵니다",
        "{n} more files in the folder — type to find them",
    ),
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
