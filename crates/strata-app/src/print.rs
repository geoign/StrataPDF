//! Printing through the classic Win32 print dialog and GDI.
//!
//! Pages are rasterised by MuPDF at the printer's resolution (capped at 600 dpi)
//! and sent with `StretchDIBits`. Permission flags of the document are ignored.

use std::sync::Arc;

use crossbeam_channel::Sender;
use strata_core::Document;
use strata_core::render::render_page_rgb;
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC, GetDeviceCaps, HALFTONE, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY,
    SRCCOPY, SetStretchBltMode, StretchDIBits, VERTRES,
};
use windows::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
use windows::Win32::UI::Controls::Dialogs::{PD_NOSELECTION, PD_PAGENUMS, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW, PrintDlgW};
use windows::core::PCWSTR;

const MAX_DPI: i32 = 600;

struct SendHdc(HDC);
// SAFETY: the printer DC is handed to exactly one worker thread and never used again here.
unsafe impl Send for SendHdc {}

/// Show the print dialog; on confirmation print in the background.
/// Messages (progress and errors) are sent to `msg`.
pub fn print_dialog(doc: &Arc<Document>, current_page: u32, msg: Sender<String>) -> Result<(), String> {
    let n = doc.page_count().clamp(1, u16::MAX as usize) as u16;
    let mut pd = PRINTDLGW {
        lStructSize: std::mem::size_of::<PRINTDLGW>() as u32,
        Flags: PD_RETURNDC | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE,
        nFromPage: (current_page + 1).min(n as u32) as u16,
        nToPage: (current_page + 1).min(n as u32) as u16,
        nMinPage: 1,
        nMaxPage: n,
        nCopies: 1,
        ..Default::default()
    };
    // SAFETY: `pd` is a properly initialised PRINTDLGW.
    let ok = unsafe { PrintDlgW(&mut pd) }.as_bool();
    unsafe {
        let _ = GlobalFree(Some(pd.hDevMode));
        let _ = GlobalFree(Some(pd.hDevNames));
    }
    if !ok {
        return Ok(()); // cancelled
    }
    if pd.hDC.is_invalid() {
        return Err("プリンタのデバイスコンテキストを取得できません".into());
    }
    let pages: Vec<u32> = if (pd.Flags.0 & PD_PAGENUMS.0) != 0 {
        let (a, b) = (pd.nFromPage.min(pd.nToPage), pd.nFromPage.max(pd.nToPage));
        (a as u32 - 1..b as u32).collect()
    } else {
        (0..n as u32).collect()
    };
    let hdc = SendHdc(pd.hDC);
    let client = doc.client();
    let name = doc.info().path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "StrataPDF".into());
    std::thread::Builder::new()
        .name("strata-print".into())
        .spawn(move || {
            let hdc = hdc;
            // Page rendering must not overlap the render pool for some files.
            let _serial = client.serialize();
            let r = print_pages(hdc.0, &name, &pages, |p| client.display_list(p), &msg);
            unsafe {
                let _ = DeleteDC(hdc.0);
            }
            match r {
                Ok(()) => {
                    let _ = msg.send(format!("{name}: {} ページを印刷キューに送りました", pages.len()));
                }
                Err(e) => {
                    let _ = msg.send(format!("{name}: 印刷に失敗しました: {e}"));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn print_pages(
    hdc: HDC,
    name: &str,
    pages: &[u32],
    display_list: impl Fn(u32) -> Result<Arc<mupdf::DisplayList>, String>,
    msg: &Sender<String>,
) -> Result<(), String> {
    let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let di = DOCINFOW { cbSize: std::mem::size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(wname.as_ptr()), ..Default::default() };
    unsafe {
        if StartDocW(hdc, &di) <= 0 {
            return Err("StartDoc が失敗しました".into());
        }
        let area_w = GetDeviceCaps(Some(hdc), HORZRES);
        let area_h = GetDeviceCaps(Some(hdc), VERTRES);
        let dpi = GetDeviceCaps(Some(hdc), LOGPIXELSX).max(GetDeviceCaps(Some(hdc), LOGPIXELSY)).max(72);
        SetStretchBltMode(hdc, HALFTONE);
        for (i, &p) in pages.iter().enumerate() {
            let dl = match display_list(p) {
                Ok(dl) => dl,
                Err(e) => {
                    let _ = AbortDoc(hdc);
                    return Err(format!("{} ページ: {e}", p + 1));
                }
            };
            let b = dl.bounds();
            let (pw, ph) = (b.x1 - b.x0, b.y1 - b.y0);
            // Rotate landscape pages onto portrait paper (and vice versa).
            let rotate = (pw > ph) != (area_w > area_h);
            let (lw, lh) = if rotate { (ph, pw) } else { (pw, ph) };
            // Fit into the printable area, rendering at no more than MAX_DPI.
            let fit = (area_w as f32 / lw).min(area_h as f32 / lh); // device px per point
            let render_scale = fit.min(MAX_DPI as f32 / 72.0).min(dpi as f32 / 72.0);
            let (w, h, rgb) = match render_page_rgb(&dl, render_scale, rotate) {
                Ok(v) => v,
                Err(e) => {
                    let _ = AbortDoc(hdc);
                    return Err(format!("{} ページの描画: {e}", p + 1));
                }
            };
            let dib = to_bgr_dib(w, h, &rgb);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 24,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let dw = (lw * fit).round() as i32;
            let dh = (lh * fit).round() as i32;
            let x = (area_w - dw) / 2;
            let y = (area_h - dh) / 2;
            if StartPage(hdc) <= 0 {
                let _ = AbortDoc(hdc);
                return Err("StartPage が失敗しました".into());
            }
            StretchDIBits(hdc, x, y, dw, dh, 0, 0, w as i32, h as i32, Some(dib.as_ptr().cast()), &bmi, DIB_RGB_COLORS, SRCCOPY);
            if EndPage(hdc) <= 0 {
                let _ = AbortDoc(hdc);
                return Err("EndPage が失敗しました".into());
            }
            if pages.len() > 1 && (i + 1) % 10 == 0 {
                let _ = msg.send(format!("{name}: 印刷 {}/{} ページ", i + 1, pages.len()));
            }
        }
        if EndDoc(hdc) <= 0 {
            return Err("EndDoc が失敗しました".into());
        }
    }
    Ok(())
}

/// RGB -> BGR with each row padded to a multiple of 4 bytes.
fn to_bgr_dib(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let mut out = vec![0u8; row * h as usize];
    for y in 0..h as usize {
        let src = &rgb[y * w as usize * 3..(y + 1) * w as usize * 3];
        let dst = &mut out[y * row..y * row + w as usize * 3];
        for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(3)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
        }
    }
    out
}
