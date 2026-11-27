# Demo documents

Fictional specifications used in the README pictures and for trying refdesk out. They were written
for this project and describe nothing real.

| File | What it shows |
|---|---|
| `process-manager-v1.0.pdf` | A 25-page specification: three-level outline, page labels (i, ii, … then 1, 2, …), requirement IDs with internal links, tables, code, a figure |
| `process-manager-v1.1.pdf` | The next version: added paragraphs and a section, reworded requirements, changed table values (for comparing) |
| `watchdog-interface-v1.0.pdf` | A short document the first one refers to (for reading two documents side by side) |

The sources are in `src/`. Parts that belong to one version only are marked
`<!--v1.1-->…<!--/v1.1-->`. To rebuild the PDFs (needs Google Chrome or Chromium and `pdftotext`):

```sh
python3 assets/demo/build.py
```

Chrome runs headless with a temporary profile. The build is reproducible, and it fails if the home
folder, the user name or the build folder ends up in a PDF. `crates/core/tests/demo_documents.rs`
checks the outline, the page labels and the links. `scripts/screenshots.sh` takes the README pictures.
