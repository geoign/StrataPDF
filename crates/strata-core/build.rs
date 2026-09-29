//! Compiles the region features of PyMuPDF Layout (vendor/pymupdf_layout) against the
//! MuPDF headers of the linked mupdf-sys (vendor/mupdf-include).
fn main() {
    let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor");
    let src = vendor.join("pymupdf_layout");
    for f in ["features.c", "features_decls.h", "strata_features.c"] {
        println!("cargo:rerun-if-changed={}", src.join(f).display());
    }
    cc::Build::new()
        .file(src.join("features.c"))
        .file(src.join("strata_features.c"))
        .include(&src)
        .include(vendor.join("mupdf-include"))
        .warnings(false)
        .compile("strata_layout_features");
}
