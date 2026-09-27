# Vendored crate patches

## mupdf 0.8.0 (AGPL-3.0)
- `src/device/native.rs`: `align_of::<max_align_t>()` replaced by `16`.
  bindgen on MSVC does not emit `max_align_t`, so upstream fails to compile on Windows.
- `src/text_page.rs`: `TextPage::as_raw()` exposes the `fz_stext_page` pointer so that
  structure and grid blocks (segmentation, table hunting) can be walked.
- `src/context.rs`: `raw_context()` exposes the calling thread's `fz_context` for direct FFI calls.
