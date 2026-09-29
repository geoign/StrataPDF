# PyMuPDF Layout (vendored parts)

From [ArtifexSoftware/pymupdf_layout](https://github.com/ArtifexSoftware/pymupdf_layout)
commit `0c29907` (2026-09-29, pymupdf-layout 1.28.2), AGPL-3.0 (see the repository LICENSE; the
project is dual licensed AGPL-3.0 / Artifex commercial).

| File | Origin |
|---|---|
| `features.c`, `features_decls.h` | `source/`, unchanged. Region features of a structured-text page |
| `strata_features.c` | StrataPDF's shim: copies the features into the model's order |
| `../../crates/strata-ocr/models/layout_rf2.4.1_imf1.onnx` | `layout/resources/onnx/layout_rf2.4.1+imf1.onnx` (graph network) |
| `../../crates/strata-ocr/models/feature_imf1.onnx` | `layout/resources/onnx/feature_imf1.onnx` (page image network) |

The Python pipeline around them (line boxes, graph edges, image pooling, grouping) is ported to
Rust in `crates/strata-core/src/layout.rs` and `crates/strata-ocr/src/layout.rs`. The port is
checked against dumps of the Python pipeline's intermediate values (`tools/layout_golden.py`,
example `layout_golden`): given the reference lines, the labels agree on every line of the test
pages; end to end, on 99.9% of the lines. The remaining differences come from MuPDF 1.27
(mupdf-sys) versus 1.28 (PyMuPDF), which split some lines differently.

One departure from the Python pipeline: a manuscript's line numbers are left out before the model
(it groups them with the text). Reflow takes each line's class from its region (the majority of the
region's lines), as the Python pipeline's output does.

`../mupdf-include` is the include directory of MuPDF from `mupdf-sys` 0.8.0, needed to compile
`features.c` against the same MuPDF the app links. Update it together with `mupdf-sys`.
