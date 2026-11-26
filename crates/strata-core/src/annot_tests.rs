//! Tests of annotation editing on blank documents: free text faces, layout and
//! rotation, image stamps, and saving.

use super::*;
use mupdf::pdf::PdfObject;
use mupdf::{Colorspace, Matrix, TextPageFlags};

fn blank(rotate: i32) -> PdfDocument {
    let mut pdf = PdfDocument::new();
    pdf.new_page((595.0f32, 842.0f32)).unwrap();
    if rotate != 0 {
        pdf.find_page(0).unwrap().dict_put("Rotate", PdfObject::new_int(rotate).unwrap()).unwrap();
    }
    pdf
}

/// Text lines of page 0 as shown, annotations included: (text without
/// spaces, origins of the characters).
fn shown_lines(pdf: &PdfDocument) -> Vec<(String, Vec<(f32, f32)>)> {
    let p = pdf.load_pdf_page(0).unwrap();
    // Text extraction from a page leaves annotations out.
    let tp = p.to_display_list(true).unwrap().to_text_page(TextPageFlags::empty()).unwrap();
    let mut out = Vec::new();
    for b in tp.blocks() {
        for l in b.lines() {
            let cs: Vec<_> = l.chars().filter(|c| c.char().is_some_and(|c| c != ' ')).collect();
            out.push((cs.iter().filter_map(|c| c.char()).collect(), cs.iter().map(|c| (c.origin().x, c.origin().y)).collect()));
        }
    }
    out
}

fn ap_font(pdf: &PdfDocument, id: i32, key: &str) -> Option<String> {
    let p = pdf.load_pdf_page(0).unwrap();
    let a = find(&p, id).unwrap();
    let f = a.object().get_dict("AP").ok()??.get_dict("N").ok()??.get_dict("Resources").ok()??.get_dict("Font").ok()??.get_dict(key).ok()??;
    Some(String::from_utf8(f.get_dict("BaseFont").ok()??.as_name().ok()?).unwrap())
}

fn spec_of(pdf: &PdfDocument, id: i32) -> AnnotSpec {
    list(pdf, 0).unwrap().into_iter().find(|a| a.id == id).unwrap().spec
}

fn text_spec(text: &str, font: TextFont) -> AnnotSpec {
    let mut s = AnnotSpec::new(AnnotKind::FreeText, [0.0, 0.0, 0.0]);
    s.rect = RectF { x0: 100.0, y0: 100.0, x1: 100.0, y1: 100.0 };
    s.typewriter = true;
    s.font = font;
    s.contents = text.into();
    s
}

/// RGB at a point of page 0 rendered at 72 dpi.
fn pixel(pdf: &PdfDocument, x: u32, y: u32) -> [u8; 3] {
    let p = pdf.load_pdf_page(0).unwrap();
    let pm = p.to_pixmap(&Matrix::IDENTITY, &Colorspace::device_rgb(), false, true).unwrap();
    let i = y as usize * pm.stride() as usize + x as usize * pm.n() as usize;
    let s = pm.samples();
    [s[i], s[i + 1], s[i + 2]]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= 2)
}

#[test]
fn free_text_gothic_with_latin_runs() {
    let mut pdf = blank(0);
    let text = "山田 太郎 (Yamada) 2026年";
    let id = create(&mut pdf, 0, &text_spec(text, TextFont::Sans)).unwrap();
    let lines = shown_lines(&pdf);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, text.replace(' ', ""));
    assert_eq!(ap_font(&pdf, id, "FC").as_deref(), Some("Gothic"));
    assert_eq!(ap_font(&pdf, id, "FL").as_deref(), Some("Helvetica"));
    let s = spec_of(&pdf, id);
    assert_eq!(s.font, TextFont::Sans);
    assert!(s.typewriter);
    assert_eq!(s.contents, text);
    assert_eq!(s.fill, None);
    let r = s.rect;
    assert!((r.x0 - 100.0).abs() < 0.01 && (r.y0 - 100.0).abs() < 0.01, "{r:?}");
    // Six CJK characters at 12 pt plus the Latin run, two points of margin each side.
    assert!(r.width() > 6.0 * 12.0 + 4.0 && r.width() < 300.0, "{r:?}");
    assert!((r.height() - (12.0 * appearance::LEADING + 4.0)).abs() < 0.01, "{r:?}");
}

#[test]
fn free_text_mincho_survives_other_edits_and_moves() {
    let mut pdf = blank(0);
    let id = create(&mut pdf, 0, &text_spec("明朝の文字", TextFont::Serif)).unwrap();
    assert_eq!(ap_font(&pdf, id, "FC").as_deref(), Some("Mincho"));
    // Another annotation makes MuPDF resynthesise what needs it, which must not be ours.
    let mut sq = AnnotSpec::new(AnnotKind::Square, [1.0, 0.0, 0.0]);
    sq.rect = RectF { x0: 300.0, y0: 300.0, x1: 400.0, y1: 400.0 };
    create(&mut pdf, 0, &sq).unwrap();
    assert_eq!(ap_font(&pdf, id, "FC").as_deref(), Some("Mincho"));
    // Moving rewrites it with the same face.
    let mut s = spec_of(&pdf, id);
    s.translate(50.0, 20.0);
    modify(&mut pdf, 0, id, &s).unwrap();
    assert_eq!(ap_font(&pdf, id, "FC").as_deref(), Some("Mincho"));
    let again = spec_of(&pdf, id);
    assert!((again.rect.x0 - 150.0).abs() < 0.01, "{:?}", again.rect);
    assert_eq!(again.font, TextFont::Serif);
}

#[test]
fn fixed_box_wraps_and_grows() {
    let mut pdf = blank(0);
    let mut s = text_spec("あいうえおかきくけこさしすせそ", TextFont::Sans);
    s.typewriter = false;
    s.rect = RectF { x0: 100.0, y0: 100.0, x1: 100.0 + 5.0 * 12.0 + 4.0, y1: 110.0 };
    let id = create(&mut pdf, 0, &s).unwrap();
    assert_eq!(shown_lines(&pdf).len(), 3);
    let r = spec_of(&pdf, id).rect;
    assert!((r.width() - 64.0).abs() < 0.01, "{r:?}");
    assert!((r.height() - (3.0 * 12.0 * appearance::LEADING + 4.0)).abs() < 0.01, "{r:?}");
}

#[test]
fn background_fill() {
    let mut pdf = blank(0);
    let mut s = text_spec("白地", TextFont::Sans);
    s.fill = Some([0.0, 1.0, 0.0]);
    let id = create(&mut pdf, 0, &s).unwrap();
    assert_eq!(spec_of(&pdf, id).fill, Some([0.0, 1.0, 0.0]));
    assert!(near(pixel(&pdf, 101, 101), [0, 255, 0]));
    s.fill = None;
    modify(&mut pdf, 0, id, &s).unwrap();
    assert_eq!(spec_of(&pdf, id).fill, None);
    assert!(near(pixel(&pdf, 101, 101), [255, 255, 255]));
}

#[test]
fn free_text_upright_on_rotated_page() {
    for rot in [90, 180, 270] {
        let mut pdf = blank(rot);
        let id = create(&mut pdf, 0, &text_spec("横書きのテキスト", TextFont::Sans)).unwrap();
        let lines = shown_lines(&pdf);
        assert_eq!(lines.len(), 1, "{rot}: {lines:?}");
        let (t, o) = &lines[0];
        assert_eq!(t, "横書きのテキスト", "{rot}");
        // Left to right along a horizontal baseline, inside the box.
        assert!(o.windows(2).all(|w| w[1].0 > w[0].0 && (w[1].1 - w[0].1).abs() < 0.5), "{rot}: {o:?}");
        let r = spec_of(&pdf, id).rect;
        assert!(o.iter().all(|&(x, y)| x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1), "{rot}: {r:?} {o:?}");
    }
}

/// A 2×1 image: red on the left, blue on the right.
const PNM: &[u8] = b"P6\n2 1\n255\n\xff\x00\x00\x00\x00\xff";

#[test]
fn image_stamp_is_placed_moved_and_recreated() {
    let mut pdf = blank(0);
    let place = ImagePlace { center: [0.0, 0.0], fit: Some(RectF { x0: 100.0, y0: 100.0, x1: 300.0, y1: 300.0 }) };
    let (id, spec) = create_image(&mut pdf, 0, PNM, place, "").unwrap();
    // 2:1 fitted into the square, centred.
    assert!((spec.rect.width() - 200.0).abs() < 0.01 && (spec.rect.height() - 100.0).abs() < 0.01, "{:?}", spec.rect);
    assert!((spec.rect.y0 - 150.0).abs() < 0.01, "{:?}", spec.rect);
    assert!(near(pixel(&pdf, 120, 200), [255, 0, 0]));
    assert!(near(pixel(&pdf, 280, 200), [0, 0, 255]));
    assert!(near(pixel(&pdf, 50, 50), [255, 255, 255]));
    let s0 = spec_of(&pdf, id);
    assert_eq!(s0.kind, AnnotKind::Stamp);
    assert_eq!(s0.image, spec.image);
    assert!((s0.rect.width() - 200.0).abs() < 0.01 && (s0.rect.height() - 100.0).abs() < 0.01, "{:?}", s0.rect);
    // Narrower and half transparent.
    let mut s = s0.clone();
    s.rect.x1 = s.rect.x0 + 100.0;
    s.opacity = 0.5;
    modify(&mut pdf, 0, id, &s).unwrap();
    assert!(near(pixel(&pdf, 120, 200), [255, 128, 128]), "{:?}", pixel(&pdf, 120, 200));
    assert!(near(pixel(&pdf, 250, 200), [255, 255, 255]));
    // Deleted and made again (undo), it still shows the image.
    let gone = delete(&mut pdf, 0, id).unwrap();
    assert!(near(pixel(&pdf, 120, 200), [255, 255, 255]));
    create(&mut pdf, 0, &gone).unwrap();
    assert!(near(pixel(&pdf, 120, 200), [255, 128, 128]));
}

#[test]
fn image_stamp_upright_on_rotated_page() {
    let mut pdf = blank(90);
    let place = ImagePlace { center: [0.0, 0.0], fit: Some(RectF { x0: 100.0, y0: 100.0, x1: 300.0, y1: 200.0 }) };
    let (id, _) = create_image(&mut pdf, 0, PNM, place, "").unwrap();
    let r = spec_of(&pdf, id).rect;
    assert!((r.x0 - 100.0).abs() < 0.01 && (r.y1 - 200.0).abs() < 0.01 && (r.width() - 200.0).abs() < 0.01, "{r:?}");
    // The page is shown 842 wide and 595 high; red stays on the left as shown.
    assert!(near(pixel(&pdf, 120, 150), [255, 0, 0]), "{:?}", pixel(&pdf, 120, 150));
    assert!(near(pixel(&pdf, 280, 150), [0, 0, 255]), "{:?}", pixel(&pdf, 280, 150));
}

#[test]
fn natural_size_is_clamped_to_the_page() {
    let mut pdf = blank(0);
    let mut big = b"P6\n4000 1000\n255\n".to_vec();
    big.resize(big.len() + 4000 * 1000 * 3, 0x80);
    let (_, spec) = create_image(&mut pdf, 0, &big, ImagePlace { center: [590.0, 10.0], fit: None }, "").unwrap();
    let r = spec.rect;
    assert!((r.width() - 0.6 * 595.0).abs() < 0.1, "{r:?}");
    assert!(r.x0 >= 0.0 && r.x1 <= 595.01 && r.y0 >= 0.0, "{r:?}");
}

#[test]
fn not_an_image() {
    let mut pdf = blank(0);
    assert!(create_image(&mut pdf, 0, b"not an image", ImagePlace { center: [0.0, 0.0], fit: None }, "").is_err());
    assert!(list(&pdf, 0).unwrap().is_empty());
}

#[test]
fn saved_and_reopened() {
    let mut pdf = blank(0);
    let tid = create(&mut pdf, 0, &text_spec("保存して開き直す", TextFont::Serif)).unwrap();
    let (_, spec) = create_image(&mut pdf, 0, PNM, ImagePlace { center: [300.0, 400.0], fit: None }, "").unwrap();
    let mut buf = Vec::new();
    pdf.write_to(&mut buf).unwrap();
    let back = PdfDocument::from_bytes(&buf).unwrap();
    let l = list(&back, 0).unwrap();
    let t = l.iter().find(|a| a.id == tid).unwrap();
    assert_eq!((t.spec.font, t.spec.typewriter, t.spec.contents.as_str()), (TextFont::Serif, true, "保存して開き直す"));
    assert!(l.iter().any(|a| a.spec.kind == AnnotKind::Stamp && a.spec.image > 0 && (a.spec.rect.width() - spec.rect.width()).abs() < 0.01));
    assert!(shown_lines(&back).iter().any(|(t, _)| t == "保存して開き直す"));
}

#[test]
fn png_transparency_is_kept() {
    // Red on the left, transparent on the right.
    let mut pm = mupdf::Pixmap::new_with_w_h(&Colorspace::device_rgb(), 2, 1, true).unwrap();
    pm.samples_mut().copy_from_slice(&[255, 0, 0, 255, 0, 0, 0, 0]);
    let mut png = Vec::new();
    pm.write_to(&mut png, mupdf::pixmap::ImageFormat::PNG).unwrap();
    let mut pdf = blank(0);
    let place = ImagePlace { center: [0.0, 0.0], fit: Some(RectF { x0: 100.0, y0: 100.0, x1: 300.0, y1: 200.0 }) };
    create_image(&mut pdf, 0, &png, place, "").unwrap();
    assert!(near(pixel(&pdf, 120, 150), [255, 0, 0]), "{:?}", pixel(&pdf, 120, 150));
    assert!(near(pixel(&pdf, 280, 150), [255, 255, 255]), "{:?}", pixel(&pdf, 280, 150));
}
