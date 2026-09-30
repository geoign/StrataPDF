//! Characters that MuPDF decodes wrongly in symbol and "special character" fonts.
//!
//! Many pi fonts (Springer/Wiley "Advent", Elsevier "Els-ent", "ScienceTypeCustomPi", "MathTechnical",
//! TeX symbol fonts) have a private glyph order and a PDF encoding that tells MuPDF some ASCII character
//! for a glyph that is something else: the degree sign is a "8" (`12.78N`), "=" a "¼", "<" a "b", the
//! copyright sign a "V C", the closing quote a "^", the Greek letters ASCII letters. Numbers in the text
//! silently change. In these fonts the same code draws the same glyph in every paper, so a table by
//! (font, decoded character) repairs them.
//!
//! The rows come from the glyph outlines of 4,675 papers (`char_probe`, first 40 pages): for every
//! (font, decoded character, outline) the outline was compared with the character, and the glyphs that do
//! not look like it were rendered at high zoom with their page context to decide what they are. Every row
//! occurs in at least two papers (the number of papers per row is in the comments), and no paper of
//! the corpus draws the decoded character itself with that (font, character) pair. Fonts that are not listed
//! are never touched. An empty string drops the character: pieces of assembled delimiters, the Springer
//! logo, the tall radical bar that MuPDF reads as "ffi".

use crate::glyphs::family;

/// The `(decoded character, text)` rows of a font family, sorted by family name.
type Table = &'static [(char, &'static str)];

/// (family, rows). Family names as [`family`] gives them.
static TABLES: &[(&str, Table)] = &[
    // AdvCORRESAST, papers per row: U+21D1=34
    ("advcorresast", &[('\u{21d1}', "*")]),
    // AdvEls-ent2, papers per row: w=7
    ("advels-ent2", &[('w', "†")]),
    // AdvEls-ent3, papers per row: r=19
    ("advels-ent3", &[('r', "©")]),
    // AdvEls-ent4, papers per row: o=36 r=7 p=2
    ("advels-ent4", &[('o', "<"), ('r', "≤"), ('p', "≤")]),
    // AdvEls-ent5, papers per row: 1=14 Z=6
    ("advels-ent5", &[('1', "°"), ('Z', "≥")]),
    // AdvEls-ent7, papers per row: B=23 E=3
    ("advels-ent7", &[('B', "∼"), ('E', "≈")]),
    // AdvEls-ent8, papers per row: 7=9
    ("advels-ent8", &[('7', "±")]),
    // AdvMacMthSyN, papers per row: U+00DE=85 U+00F0=85 U+00BC=64 U+00FE=47 U+00BD=23 0=16 p=9 j=8 4=7 f=4 g=4 !=4 h=3 i=3 1=2
    ("advmacmthsyn", &[('Þ', ")"), ('ð', "("), ('¼', "="), ('þ', "+"), ('½', "["), ('0', "′"), ('p', "√"), ('j', "|"), ('4', "Δ"), ('f', "{"), ('g', "}"), ('!', "→"), ('h', "⟨"), ('i', "⟩"), ('1', "∞")]),
    // AdvMathSymb, papers per row: U+00BC=13 U+00FE=5 0=5 U+00DE=4 U+00F0=4
    ("advmathsymb", &[('¼', "="), ('þ', "+"), ('0', "′"), ('Þ', ")"), ('ð', "(")]),
    // AdvMT_SY, papers per row: U+00BC=5 U+00FE=4
    ("advmt_sy", &[('¼', "="), ('þ', "+")]),
    // AdvP0004, papers per row: B=41 0=9 I=7
    ("advp0004", &[('B', "“"), ('0', "="), ('I', "·")]),
    // AdvP0005, papers per row: ^=41 b=4 R=3
    ("advp0005", &[('^', "”"), ('b', "▶"), ('R', "◀")]),
    // AdvP3EAA99, papers per row: D=5 m=4 d=3 s=2
    ("advp3eaa99", &[('D', "Δ"), ('m', "μ"), ('d', "δ"), ('s', "σ")]),
    // AdvP3F4C13, papers per row: m=8 s=7 d=5 D=5 a=2
    ("advp3f4c13", &[('m', "μ"), ('s', "σ"), ('d', "δ"), ('D', "Δ"), ('a', "α")]),
    // AdvP44E6F4, papers per row: u=11
    ("advp44e6f4", &[('u', "°")]),
    // AdvP4C4E46, papers per row: U+FB03=118 X=51 !=43 #=33 "=33 i=28 h=28 Z=28 p=28 q=26 s=23 <=23 8=23 :=23 P=21 r=21 >=19 2=16 3=16 4=16 5=16
    ("advp4c4e46", &[('ﬃ', ""), ('X', "∑"), ('!', ")"), ('#', "]"), ('"', "["), ('i', "]"), ('h', "["), ('Z', "∫"), ('p', "√"), ('q', "√"), ('s', "√"), ('<', "{"), ('8', ""), (':', ""), ('P', "∑"), ('r', "√"), ('>', ""), ('2', "["), ('3', "]"), ('4', ""), ('5', "")]),
    // AdvP4C4E51, papers per row: :=279 ==254 ;=215 @=26 "=5 U+2019=3 U+2018=2
    ("advp4c4e51", &[(':', "."), ('=', "/"), (';', ","), ('@', "∂"), ('"', "ε"), ('’', "φ"), ('‘', "ℓ")]),
    // AdvP4C4E59, papers per row: U+20AC=56 _=43 }=3
    ("advp4c4e59", &[('\u{20ac}', "¨"), ('_', "˙"), ('}', "˝")]),
    // AdvP4C4E74, papers per row: U+00BC=356 U+00DE=316 U+00F0=316 U+00FE=265 0=134 U+00BD=76 p=60 j=44 f=19 g=19 1=15 !=14 r=10 h=8 i=8 /=8 U+FB03=5 U+2019=5 ,=3 k=3 ?=3
    ("advp4c4e74", &[('¼', "="), ('Þ', ")"), ('ð', "("), ('þ', "+"), ('0', "′"), ('½', "["), ('p', "√"), ('j', "|"), ('f', "{"), ('g', "}"), ('1', "∞"), ('!', "→"), ('r', "∇"), ('h', "⟨"), ('i', "⟩"), ('/', "∝"), ('ﬃ', "≅"), ('’', "≃"), (',', "⇔"), ('k', "‖"), ('?', "⊥")]),
    // AdvP697C, papers per row: m=2
    ("advp697c", &[('m', "μ")]),
    // AdvP7DA6, papers per row: 8=20 ,=10 2=8 .=8 5=6 6=5 9=5 3=4 $=2 #=2
    ("advp7da6", &[('8', "°"), (',', "<"), ('2', "−"), ('.', ">"), ('5', "="), ('6', "±"), ('9', "′"), ('3', "×"), ('$', "≥"), ('#', "≤")]),
    // AdvP7DB7, papers per row: ,=11
    ("advp7db7", &[(',', "∼")]),
    // AdvP80516, papers per row: V=43
    ("advp80516", &[('V', "©")]),
    // AdvP80675, papers per row: 6=36 2=26 5=25 3=21 1=18 d=2
    ("advp80675", &[('6', "±"), ('2', "−"), ('5', "="), ('3', "×"), ('1', "+"), ('d', "δ")]),
    // AdvPi1, papers per row: 8=37 4=37 m=30 *=14 +=10 D=7 s=7 a=5 d=5 5=5 {=3 6=3 g=3 F=2 &=2
    ("advpi1", &[('8', "°"), ('4', ">"), ('m', "μ"), ('*', "∼"), ('+', "±"), ('D', "Δ"), ('s', "σ"), ('a', "α"), ('d', "δ"), ('5', "<"), ('{', "†"), ('6', "×"), ('g', "γ"), ('F', "Φ"), ('&', "≈")]),
    // AdvPi2, papers per row: s=16 f=10 r=9 %=9 a=9 l=7 m=7 t=7 Z=6 n=5 p=5 d=5 y=4 g=4 ?=4 x=3 b=3 z=3 H=2 F=2 D=2 {=2
    ("advpi2", &[('s', "σ"), ('f', "φ"), ('r', "ρ"), ('%', "‰"), ('a', "α"), ('l', "λ"), ('m', "μ"), ('t', "τ"), ('Z', "η"), ('n', "ν"), ('p', "π"), ('d', "δ"), ('y', "θ"), ('g', "γ"), ('?', "→"), ('x', "ξ"), ('b', "β"), ('z', "ζ"), ('H', "√"), ('F', "Φ"), ('D', "Δ"), ('{', "‡")]),
    // AdvPi3, papers per row: #=137 2=3 1=3
    ("advpi3", &[('#', "©"), ('2', "™"), ('1', "®")]),
    // AdvPS3F4C13, papers per row: m=2 d=2 b=2 a=2
    ("advps3f4c13", &[('m', "μ"), ('d', "δ"), ('b', "β"), ('a', "α")]),
    // AdvPS3FDD77, papers per row: w=11 z=4 Z=3
    ("advps3fdd77", &[('w', "∼"), ('z', "≈"), ('Z', "=")]),
    // AdvPS44A44B, papers per row: e=31 d=11 C=3 $=2 G=2
    ("advps44a44b", &[('e', "–"), ('d', "—"), ('C', "+"), ('$', "·"), ('G', "±")]),
    // AdvPS4721B4, papers per row: s=6 b=4 a=3 d=3 g=2 r=2 4=2
    ("advps4721b4", &[('s', "σ"), ('b', "β"), ('a', "α"), ('d', "δ"), ('g', "γ"), ('r', "ρ"), ('4', "φ")]),
    // AdvPS48994E, papers per row: ^=6
    ("advps48994e", &[('^', "±")]),
    // AdvPS7DA6, papers per row: ,=17 2=15 .=15 8=10 #=3 $=2
    ("advps7da6", &[(',', "<"), ('2', "−"), ('.', ">"), ('8', "°"), ('#', "≤"), ('$', "≥")]),
    // AdvPS_SSYI, papers per row: m=5 s=5 h=4 r=3 p=2 t=2 g=2 k=2 f=2
    ("advps_ssyi", &[('m', "μ"), ('s', "σ"), ('h', "η"), ('r', "ρ"), ('p', "π"), ('t', "τ"), ('g', "γ"), ('k', "κ"), ('f', "φ")]),
    // AdvPS_SSYR, papers per row: D=3 U+00A3=2 U+00BB=2
    ("advps_ssyr", &[('D', "Δ"), ('\u{a3}', "≤"), ('\u{bb}', "≈")]),
    // AdvPSMP10, papers per row: r=39 q=31 l=25 d=21 a=21 s=21 g=19 /=15 h=12 j=10 b=10 k=10 c=7 v=6 u=6 p=6 m=5 D=4 P=3
    ("advpsmp10", &[('r', "σ"), ('q', "ρ"), ('l', "μ"), ('d', "δ"), ('a', "α"), ('s', "τ"), ('g', "η"), ('/', "φ"), ('h', "θ"), ('j', "κ"), ('b', "β"), ('k', "λ"), ('c', "γ"), ('v', "χ"), ('u', "φ"), ('p', "π"), ('m', "ν"), ('D', "Δ"), ('P', "Π")]),
    // AdvPSMP13, papers per row: l=37 D=22 r=21 d=13 R=5 U=4 X=4 a=4 q=4 v=4 b=3 P=2
    ("advpsmp13", &[('l', "μ"), ('D', "Δ"), ('r', "σ"), ('d', "δ"), ('R', "Σ"), ('U', "Φ"), ('X', "Ω"), ('a', "α"), ('q', "ρ"), ('v', "χ"), ('b', "β"), ('P', "Π")]),
    // AdvPSMP4, papers per row: \=14 [=10 P=3
    ("advpsmp4", &[('\\', "<"), ('[', ">"), ('P', "≥")]),
    // AdvPSSPS-AS, papers per row: &=16 )=4
    ("advpssps-as", &[('&', "✉"), (')', "−")]),
    // AdvSPRING-R, papers per row: 1=13 3=13
    ("advspring-r", &[('1', ""), ('3', "")]),
    // AdvTA42, papers per row: &=22 n=16
    ("advta42", &[('&', "©"), ('n', "*")]),
    // AdvTir_symb, papers per row: 9=14 ?=5 C=4 B=2
    ("advtir_symb", &[('9', "×"), ('?', "+"), ('C', "≥"), ('B', "≤")]),
    // AdvTT1143c7e7, papers per row: Q=15 b=15 T=8 d=6
    ("advtt1143c7e7", &[('Q', "”"), ('b', "“"), ('T', "’"), ('d', "‘")]),
    // AdvTT32a07b98, papers per row: F=23 f=8
    ("advtt32a07b98", &[('F', "±"), ('f', "∼")]),
    // AdvTT3f84ef53, papers per row: U+00B4=10
    ("advtt3f84ef53", &[('\u{b4}', "×")]),
    // AdvTT432ab7ab, papers per row: V=18 A=10 D=3 j=2 W=2
    ("advtt432ab7ab", &[('V', "′"), ('A', "μ"), ('D', "η"), ('j', "σ"), ('W', "″")]),
    // AdvTT454a7a89, papers per row: b=204 N=173 8=6 4=5 d=3
    ("advtt454a7a89", &[('b', "<"), ('N', ">"), ('8', ","), ('4', "∗"), ('d', "·")]),
    // AdvTT4ff65459, papers per row: *=114
    ("advtt4ff65459", &[('*', "✉")]),
    // AdvTT52b432ec, papers per row: D=25 G=2
    ("advtt52b432ec", &[('D', "Δ"), ('G', "Γ")]),
    // AdvTTa420abf6, papers per row: D=40
    ("advtta420abf6", &[('D', "©")]),
    // AdvTTab7e17fd, papers per row: m=16 a=15 s=13 r=13 b=10 q=9 d=9 f=9 t=8 p=8 e=7 h=7 w=7 n=6 c=6 k=5
    ("advttab7e17fd", &[('m', "μ"), ('a', "α"), ('s', "σ"), ('r', "ρ"), ('b', "β"), ('q', "θ"), ('d', "δ"), ('f', "φ"), ('t', "τ"), ('p', "π"), ('e', "ε"), ('h', "η"), ('w', "ω"), ('n', "ν"), ('c', "χ"), ('k', "κ")]),
    // AdvTTb19eb3c6, papers per row: D=16 g=7
    ("advttb19eb3c6", &[('D', "Δ"), ('g', "γ")]),
    // AdvTTcd7bcd39, papers per row: s=12 D=11 m=8 r=5 h=4 p=4 d=3 e=3 q=3
    ("advttcd7bcd39", &[('s', "σ"), ('D', "Δ"), ('m', "μ"), ('r', "ρ"), ('h', "η"), ('p', "π"), ('d', "δ"), ('e', "ε"), ('q', "θ")]),
    // ElsevierSpecialPi, papers per row: U+00D5=20
    ("elsevierspecialpi", &[('Õ', "v")]),
    // EuclidSymbol, papers per row: U+00B4=4 U+00A2=3 U+00B2=2 U+00B3=2
    ("euclidsymbol", &[('\u{b4}', "×"), ('\u{a2}', "′"), ('\u{b2}', "″"), ('\u{b3}', "≥")]),
    // EuropeanPi-Three, papers per row: Y=13
    ("europeanpi-three", &[('Y', "✉")]),
    // MathematicalPi-One, papers per row: 8=20 m=13 s=9 f=8 a=7 D=6 p=5 h=4 b=3 C=3 g=3 k=3
    ("mathematicalpi-one", &[('8', "°"), ('m', "μ"), ('s', "σ"), ('f', "φ"), ('a', "α"), ('D', "Δ"), ('p', "π"), ('h', "η"), ('b', "β"), ('C', "Ψ"), ('g', "γ"), ('k', "κ")]),
    // MathTechnicalP01, papers per row: ~=14 1=8 `=5 ^=4 k=2 K=2
    ("mathtechnicalp01", &[('~', "<"), ('1', ">"), ('`', ">"), ('^', "≤"), ('k', "≫"), ('K', "≪")]),
    // MathTechnicalP03, papers per row: c=14 p=12 F=11 B=9 P=7 ;=3
    ("mathtechnicalp03", &[('c', "+"), ('p', "="), ('F', "∼"), ('B', "±"), ('P', "−"), (';', "≈")]),
    // MathTechnicalP04, papers per row: !=12
    ("mathtechnicalp04", &[('!', "×")]),
    // MathTechnicalP07, papers per row: 7=16
    ("mathtechnicalp07", &[('7', "·")]),
    // MTSY, papers per row: C=5 U+0141=5 0=4 D=3 U+0161=3 U+00BE=2
    ("mtsy", &[('C', "+"), ('Ł', "∗"), ('0', "′"), ('D', "="), ('š', "±"), ('¾', "∼")]),
    // ScienceTypeCustomPi-No3T, papers per row: -=18 )=16 "=12 ==11 ;=11 X=5 (=2
    ("sciencetypecustompi-no3t", &[('-', "<"), (')', ">"), ('"', "±"), ('=', "×"), (';', "∼"), ('X', "′"), ('(', "≤")]),
    // ScienceTypeCustomPi-No4T, papers per row: )=20 4=4 <=3
    ("sciencetypecustompi-no4t", &[(')', "∗"), ('4', "≫"), ('<', "≪")]),
    // ScienceTypeCustomPi-No5T, papers per row: r=20 q=19 s=18 y=13 x=8 w=8 F=7 G=4 f=3
    ("sciencetypecustompi-no5t", &[('r', "/"), ('q', "+"), ('s', "="), ('y', "−"), ('x', "]"), ('w', "["), ('F', "≤"), ('G', "≥"), ('f', "≈")]),
    // SizedSym151, papers per row: .=20 U+017D=20 /=6 U+017E=6 (=2
    ("sizedsym151", &[('.', ")"), ('Ž', "("), ('/', ")"), ('ž', "("), ('(', "√")]),
    // Springnew-Regular, papers per row: 1=44 3=44
    ("springnew-regular", &[('1', ""), ('3', "")]),
    // Springnew-Regular2, papers per row: 1=27 3=27
    ("springnew-regular2", &[('1', ""), ('3', "")]),
    // Springnew-Regular3, papers per row: 1=26 3=26
    ("springnew-regular3", &[('1', ""), ('3', "")]),
    // Symbol, papers per row: d=11 s=11 m=10 h=6 U+00B4=6 r=6 U+00D3=5 t=4 U+00B3=4 n=3 b=3 e=3 f=2 Y=2 U+00D2=2 Q=2
    ("symbol", &[('d', "δ"), ('s', "σ"), ('m', "μ"), ('h', "η"), ('\u{b4}', "×"), ('r', "ρ"), ('Ó', "©"), ('t', "τ"), ('\u{b3}', "≥"), ('n', "ν"), ('b', "β"), ('e', "ε"), ('f', "φ"), ('Y', "Ψ"), ('Ò', "®"), ('Q', "Θ")]),
    // SymbolMT, papers per row: r=6 s=5 a=3 t=3 m=2
    ("symbolmt", &[('r', "ρ"), ('s', "σ"), ('a', "α"), ('t', "τ"), ('m', "μ")]),
    // TeX_CM_Maths_Italic, papers per row: :=7 ==6 ;=5
    ("tex_cm_maths_italic", &[(':', "."), ('=', "/"), (';', ",")]),
    // TeX_CM_Maths_Symbols, papers per row: U+00BC=16 U+00DE=12 U+00F0=12 U+00FE=11 U+00BD=6 0=4 j=4 !=3 r=3
    ("tex_cm_maths_symbols", &[('¼', "="), ('Þ', ")"), ('ð', "("), ('þ', "+"), ('½', "["), ('0', "′"), ('j', "|"), ('!', "→"), ('r', "∇")]),
    // TeXCMMathsSymbols, papers per row: U+00BC=12 U+00DE=9 U+00F0=9 U+00FE=8 0=2
    ("texcmmathssymbols", &[('¼', "="), ('Þ', ")"), ('ð', "("), ('þ', "+"), ('0', "′")]),
    // Universal-NewswithCommPi, papers per row: q=20 Q=16 7=13 6=9 a=2
    ("universal-newswithcommpi", &[('q', "©"), ('Q', "©"), ('7', "°"), ('6', "@"), ('a', "#")]),
    // Wingdings-Regular, papers per row: *=132 U+00E0=2
    ("wingdings-regular", &[('*', "✉"), ('à', "→")]),
];

/// The rows of the font `font` (as MuPDF names it), if the font is one of the listed families.
pub(crate) fn table(font: &str) -> Option<Table> {
    let f = family(font);
    // TeX symbol fonts come in numbered variants.
    let f = if f.starts_with("tex_cm_maths_symbols") { f.trim_end_matches(|c: char| c.is_ascii_digit()) } else { &f };
    TABLES.iter().find(|(name, _)| *name == f).map(|(_, rows)| *rows)
}

/// The characters that `text` of the row `c` of `font` swallows: the circle of the copyright sign in
/// `AdvP80516` is drawn as "V" and its "C" is a letter of another font, next to it.
pub(crate) fn swallows(font: &str, c: char) -> Option<char> {
    (c == 'V' && family(font) == "advp80516").then_some('C')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_well_formed() {
        for w in TABLES.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
        }
        for (name, rows) in TABLES {
            assert_eq!(*name, name.to_ascii_lowercase());
            let mut seen = std::collections::HashSet::new();
            for (c, t) in *rows {
                assert!(seen.insert(*c), "{name}: {c:?} twice");
                assert_ne!(t.chars().next(), Some(*c), "{name}: {c:?} maps to itself");
            }
        }
    }

    #[test]
    fn rows() {
        let t = table("ABCDEF+AdvPi1").unwrap();
        assert!(t.contains(&('8', "°")) && t.contains(&('4', ">")));
        assert!(table("XxkbtbAdvP4C4E74").unwrap().contains(&('¼', "=")));
        assert!(table("TeX_CM_Maths_Symbols16").unwrap().contains(&('Þ', ")")));
        assert!(table("ABCDEF+ScienceTypeCustomPi-No5T").unwrap().contains(&('r', "/")));
        assert!(table("ABCDEF+AdvPi3").unwrap().contains(&('#', "©")));
        // Ordinary fonts and unknown families are never touched.
        for f in ["Times-Roman", "ABCDEF+Arial-BoldMT", "AdvPi", "AdvPi11", "Symbolic", "AdvTimes", "AdvPSSym"] {
            assert!(table(f).is_none(), "{f}");
        }
        assert_eq!(swallows("ABCDEF+AdvP80516", 'V'), Some('C'));
        assert_eq!(swallows("ABCDEF+AdvPi1", 'V'), None);
    }
}
