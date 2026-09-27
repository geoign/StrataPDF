# Vendored crate patches

## mupdf 0.8.0 (AGPL-3.0)
- `src/device/native.rs`: `align_of::<max_align_t>()` replaced by `16`.
  bindgen on MSVC does not emit `max_align_t`, so upstream fails to compile on Windows.
