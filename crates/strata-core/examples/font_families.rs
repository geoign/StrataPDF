//! List the installed font families the text view's font picker offers.
//! `cargo run --example font_families [-- --ja]`
fn main() {
    let ja_only = std::env::args().any(|a| a == "--ja");
    for f in strata_core::fonts::families() {
        if !ja_only || f.japanese {
            println!("{}\t{}\t{}", if f.japanese { "ja" } else { "--" }, f.name, f.display);
        }
    }
}
