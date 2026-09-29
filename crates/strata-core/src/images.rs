//! Images embedded in a page: where they are drawn, and saving one at its own
//! resolution. Images are found through the image blocks of a structured-text
//! page built with `PRESERVE_IMAGES`, in drawing order.

use std::path::Path;

use mupdf::{ColorParams, Colorspace, Device, Image, Matrix, Pixmap, TextPage, TextPageFlags};
use mupdf_sys::*;

use crate::geom::RectF;

/// Largest image decoded for saving (pixels).
const MAX_PIXELS: u64 = 150_000_000;

pub fn flags() -> TextPageFlags {
    TextPageFlags::PRESERVE_IMAGES
}

#[derive(Clone, Debug)]
pub struct PageImage {
    /// Where the image is drawn, in page space.
    pub bbox: RectF,
    pub width: u32,
    pub height: u32,
    /// The embedded data is a JPEG file that can be saved unchanged.
    pub jpeg: bool,
}

unsafe fn walk(mut b: *mut fz_stext_block, depth: u32, f: &mut dyn FnMut(&fz_stext_block)) {
    while !b.is_null() {
        unsafe {
            let blk = &*b;
            match blk.type_ {
                FZ_STEXT_BLOCK_IMAGE if !blk.u.i.image.is_null() => f(blk),
                FZ_STEXT_BLOCK_STRUCT => {
                    let s = blk.u.s.down;
                    if !s.is_null() && depth < 64 {
                        walk((*s).first_block, depth + 1, f);
                    }
                }
                _ => {}
            }
            b = blk.next;
        }
    }
}

/// The image blocks of a text page, in drawing order (later ones are on top).
fn blocks(tp: &TextPage) -> Vec<fz_stext_block> {
    let mut out = Vec::new();
    // SAFETY: the text page outlives the walk; blocks are only read.
    unsafe { walk((*tp.as_raw()).first_block, 0, &mut |b| out.push(*b)) };
    out
}

unsafe fn jpeg_data<'a>(img: *mut fz_image) -> Option<&'a [u8]> {
    unsafe {
        let cb = fz_compressed_image_buffer(mupdf::context::raw_context(), img);
        if cb.is_null() || (*cb).params.type_ != FZ_IMAGE_JPEG || (*cb).buffer.is_null() {
            return None;
        }
        let buf = &*(*cb).buffer;
        (!buf.data.is_null() && buf.len > 0).then(|| std::slice::from_raw_parts(buf.data, buf.len))
    }
}

/// Must be called on the thread that created `tp`.
pub fn list(tp: &TextPage) -> Vec<PageImage> {
    blocks(tp)
        .iter()
        .map(|b| unsafe {
            let img = b.u.i.image;
            PageImage {
                bbox: RectF { x0: b.bbox.x0, y0: b.bbox.y0, x1: b.bbox.x1, y1: b.bbox.y1 },
                width: (*img).w.max(0) as u32,
                height: (*img).h.max(0) as u32,
                jpeg: jpeg_data(img).is_some(),
            }
        })
        .collect()
}

/// Save the `index`-th image of a text page to `path`: the embedded JPEG data
/// unchanged when `as_jpeg` is set and the image is a JPEG, a PNG otherwise.
/// Returns the image size in pixels. Must be called on the thread that created `tp`.
pub fn save(tp: &TextPage, index: usize, as_jpeg: bool, path: &Path) -> Result<(u32, u32), String> {
    let b = blocks(tp).get(index).copied().ok_or("画像が見つかりません")?;
    let raw = unsafe { b.u.i.image };
    let (w, h) = unsafe { ((*raw).w.max(0) as u32, (*raw).h.max(0) as u32) };
    if as_jpeg && let Some(data) = unsafe { jpeg_data(raw) } {
        std::fs::write(path, data).map_err(|e| e.to_string())?;
        return Ok((w, h));
    }
    if w == 0 || h == 0 {
        return Err("大きさのない画像です".into());
    }
    if w as u64 * h as u64 > MAX_PIXELS {
        return Err(format!("{w}×{h} ピクセルの画像は大きすぎて変換できません"));
    }
    // Draw the image into a pixmap of its own size: this applies the colour
    // space, decode arrays and soft masks the way the page shows them.
    // SAFETY: `raw` belongs to `tp`, which is alive on this thread.
    let img = unsafe { Image::from_raw_keep(raw) };
    let (stencil, masked) = unsafe { ((*raw).imagemask() != 0, !(*raw).mask.is_null() || (*raw).use_colorkey() != 0) };
    let alpha = stencil || masked;
    let e = |e: mupdf::Error| e.to_string();
    let mut pix = Pixmap::new_with_w_h(&Colorspace::device_rgb(), w as i32, h as i32, alpha).map_err(e)?;
    if alpha { pix.clear().map_err(e)? } else { pix.clear_with(255).map_err(e)? }
    {
        let dev = Device::from_pixmap(&pix).map_err(e)?;
        let ctm = Matrix::new(w as f32, 0.0, 0.0, h as f32, 0.0, 0.0);
        if stencil {
            dev.fill_image_mask(&img, &ctm, &Colorspace::device_rgb(), &[0.0, 0.0, 0.0], 1.0, ColorParams::default()).map_err(e)?;
        } else {
            dev.fill_image(&img, &ctm, 1.0, ColorParams::default()).map_err(e)?;
        }
    }
    let mut png = Vec::new();
    pix.write_to(&mut png, mupdf::ImageFormat::PNG).map_err(e)?;
    std::fs::write(path, png).map_err(|e| e.to_string())?;
    Ok((w, h))
}
