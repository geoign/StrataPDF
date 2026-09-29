fn main() {
    println!("cargo:rerun-if-changed=assets/strata.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/strata.ico");
        res.set("ProductName", "StrataPDF");
        res.set("FileDescription", "StrataPDF");
        res.set("LegalCopyright", "Private use only (AGPL-3.0 components)");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon resource not embedded: {e}");
        }
    }
}
