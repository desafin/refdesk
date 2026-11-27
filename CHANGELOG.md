# Changelog

All notable changes to refdesk are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

The first public release: a desktop viewer for reading several software documentation PDFs at
once, for Linux x86_64 (GNOME on Wayland; built on Ubuntu 24.04).

### Opening documents

- Open PDFs from the file chooser (several at once), from the command line
  (`refdesk a.pdf b.pdf`, `a.pdf#page=12`) or from the file manager through the `.desktop` entry.
- A single instance: opening a file while refdesk runs adds a tab to the running window.
- Opening a document that is already open goes to its tab. Broken or missing files show the
  error and the path in the tab instead of closing the app. Password-protected PDFs ask for
  the password.

### Reading

- Continuous vertical scrolling with the wheel, touchpad, scroll bar and keyboard; adjustable
  scroll speed that is remembered.
- Zoom to fit width, fit page or a given ratio; Ctrl+wheel zooms around the pointer; − / +
  buttons and a zoom menu in the status bar; area zoom (Z) and back to the previous zoom
  (Shift+Z). The view never goes blank while zooming.
- Every page is rendered in the background when a document opens, so jumping anywhere shows
  the page at once. Rendering and text extraction never block the window.
- Go to page (Ctrl+G), page labels (i, ii, A-1 …) and the current page and zoom in the status
  bar.
- Light, dark or system theme, and a dark page mode that inverts brightness only, keeping
  colours and leaving pictures as they are.
- A thumbnail grid of the pages in the sidebar, optionally only the pages with search results.

### Tabs, splits and windows

- Tabs that can be reordered and closed, and split left, right, up or down by dragging.
- Duplicate a view to read two places of the same document side by side.
- Move a tab to a new window or to another window, and merge all windows, from the tab menu or
  with shortcuts. Each window has its own sidebar.
- Reopen closed tabs (Ctrl+Shift+T).

### Links, history and outline

- Internal links jump to the exact place on the target page; links to other PDFs open them.
  Ctrl+click or middle click opens in a new tab, Ctrl+Shift+click in the split beside.
- Back and forward (Alt+← / Alt+→, mouse buttons) per view, across documents, kept across
  restarts. The arrival point is briefly highlighted.
- Hover previews of link targets, which can be pinned, moved, resized and scrolled.
- Keyboard link hints: one key labels every visible link; typing the label follows it.
- An outline panel that follows the current section, with a filter, expand and collapse all,
  and a remembered expansion state per document. The current section path is shown above the
  view and can be clicked.
- Copy the current location as `path#page=87` (opens again from the command line) or as
  "document, page, section".

### Search

- A search bar (Ctrl+F) that searches as you type, highlights every match and finds phrases
  across line breaks and end-of-line hyphens. Options: match case, whole word, regular
  expression. A result list with the page, section and context. Search can be narrowed to the
  current chapter or section.
- Search all open documents at once (Ctrl+Shift+F) in the sidebar, with results grouped by
  document, matches highlighted in every view, and F4 / Shift+F4 to step through them.
  `refdesk --find TEXT` starts such a search from the command line.
- A search history shared by both searches: ↑ / ↓ in the input, a history list, and the last
  search filled in when a tab is searched for the first time.

### Selection, copying and capture

- Select text in reading order by dragging, double click for a word and triple click for a
  line; copying joins the lines of a paragraph and words split by a hyphen.
- Copy the text of the current section or chapter.
- Capture an area of a page to the clipboard, rendered again at high resolution so it stays
  sharp at low zoom; save it as PNG (default `~/Pictures/refdesk/`).

### Comparing documents

- Compare two versions of a document (`refdesk --diff old.pdf new.pdf`, from tabs or from the
  file explorer). The comparison follows the text, so pages shifted by added content still line
  up.
- Side-by-side view with deleted, added and changed text marked and changed words emphasised,
  synchronised scrolling, and a list of changes grouped by section with next and previous
  change (F7 / Shift+F7).
- Repeated headers, footers and page numbers are left out automatically; page margins can be
  excluded by hand.

### Folders and projects

- Open a folder and browse its PDFs in a file explorer in the sidebar, with filtering,
  keyboard navigation, the current document highlighted and automatic refresh. A single click
  opens a preview tab; a double click keeps it.
- Projects: folders managed by refdesk into which PDFs are imported (copied), with duplicate
  detection, new folders, renaming and a project trash. Recent projects on the start screen.

### Translation with a local model

- A translation mode that shows the Korean meaning of the word under the pointer and the
  translation of selected text in a popup. Selected text can also be translated once from the
  menu.
- Works with Ollama or any OpenAI-compatible local server (llama.cpp's `llama-server`, LM
  Studio, vLLM). Models can be switched from the menu. Translations are cached on disk.
- `refdesk --translate TEXT` checks the setup from the command line.
- The release builds do not contain the optional built-in engine (`--features embedded-vulkan`
  when building from source).

### Memory and commands

- The session (windows, splits, tabs, positions and zoom) is saved within seconds of a change
  and restored on start. Each document reopens where it was read last, even after it was moved
  or renamed. A start screen lists recent documents, folders and projects.
- Bookmarks, one-letter marks (`m` / `'`) and highlights with notes, listed in the sidebar.
  Highlights are stored separately and never change the PDF.
- A command palette (Ctrl+Shift+P) and quick open (Ctrl+P).
- Korean and English user interface.

[0.1.0]: https://github.com/desafin/refdesk/releases/tag/v0.1.0
