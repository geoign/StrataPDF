# Vendored crate patches

## mupdf 0.8.0 (AGPL-3.0)
- `src/device/native.rs`: `align_of::<max_align_t>()` replaced by `16`.
  bindgen on MSVC does not emit `max_align_t`, so upstream fails to compile on Windows.
- `src/text_page.rs`: `TextPage::as_raw()` exposes the `fz_stext_page` pointer so that
  structure and grid blocks (segmentation, table hunting) can be walked.
- `src/font.rs`: `Font::as_raw()` exposes the `fz_font` pointer (glyph names in the corpus tools).
- `src/context.rs`: `raw_context()` exposes the calling thread's `fz_context` for direct FFI calls.
- `src/image.rs`: `Image::from_raw_keep()` wraps an image found in a structured-text page
  (used to save embedded images).

## mupdf-sys 0.8.0 (AGPL-3.0): MuPDF 1.27.2 with security fixes
Not vendored here (MuPDF is about 65 MB): `Cargo.toml` patches it to branch
`strata/v0.8.0-security` of https://github.com/geoign/mupdf-rs, pinned by commit.
No mupdf-sys release carries MuPDF 1.28, which has the fixes below.
- 9013e3d: the MuPDF submodule is replaced by the files of the published crate, byte for byte
  (`.gitattributes` keeps their line endings), so the git dependency builds without
  MuPDF's third-party submodules.
- 682584e: `source/fitz/draw-unpack.c` and `source/pdf/pdf-image.c` get the image size checks
  of upstream a26f0142, ed461a8e and a08c5ee1 (Bug 708990, CVE-2026-3308: an image row's
  stride could overflow an int and leave a heap buffer too small for the row), with the
  checked arithmetic of later MuPDF written out. `source/pdf/pdf-xref.c` gets upstream
  3984925b (Bug 709768, CVE-2026-112036: a double free in `free_char_list`).
- CVE-2026-92413 (`pdf_open_filter`) does not apply: 1.27.2 cannot open external streams.
- Checked: 255 pages (30 random papers with 13 one-bit images, and a generated PDF with 1-, 2-,
  4- and 16-bit, indexed, colour-key and image-mask images) render pixel for pixel as before.
- Drop the patch when a mupdf-sys release brings MuPDF 1.28 or later; that update also needs
  `vendor/mupdf-include` and `features.c` brought in line, and a check of the text view,
  because 1.28 splits lines differently.
