//! OCR engines for StrataPDF.
//!
//! Engines implement [`OcrEngine`] and return lines in reading order with their
//! bounding boxes in image pixels. Model files are downloaded on first use
//! (see [`models`]). The local engine is a Rust port of NDLOCR-Lite (National
//! Diet Library, CC BY 4.0): DEIM layout detection plus PARSeq line recognition,
//! strong on Japanese including vertical text.
//!
//! Remote engines (an HTTP VLM endpoint, cloud APIs) are a planned extension:
//! they only need another `OcrEngine` implementation.

pub mod formula;
pub mod layout;
pub mod models;
pub mod ndl;
mod order;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Device {
    Cpu,
    /// DirectML (any DirectX 12 GPU); falls back to CPU if unavailable.
    #[default]
    Gpu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineKind {
    Main,
    Caption,
    Advert,
    Note,
    InlineNote,
    Title,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionKind {
    TextBlock,
    Figure,
    Advert,
    /// Running head (柱).
    Header,
    /// Page number (ノンブル).
    Folio,
    /// Furigana.
    Ruby,
    Chart,
    Equation,
    ChemicalFormula,
    Latin,
    Table,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrLine {
    /// x0, y0, x1, y1 in image pixels.
    pub bbox: [f32; 4],
    pub text: String,
    pub vertical: bool,
    pub kind: LineKind,
    pub conf: f32,
    /// Index into [`OcrPage::regions`] of the text block holding the line.
    pub block: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrRegion {
    pub bbox: [f32; 4],
    pub kind: RegionKind,
    pub conf: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OcrPage {
    pub width: u32,
    pub height: u32,
    /// Lines in reading order.
    pub lines: Vec<OcrLine>,
    pub regions: Vec<OcrRegion>,
    /// Most lines are vertical.
    pub vertical: bool,
}

impl OcrPage {
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("model files are not installed: {0}")]
    NotInstalled(String),
    #[error("inference: {0}")]
    Inference(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("download: {0}")]
    Download(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<ort::Error> for OcrError {
    fn from(e: ort::Error) -> Self {
        OcrError::Inference(e.to_string())
    }
}

pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &str;
    fn recognize(&self, img: &image::RgbImage) -> Result<OcrPage, OcrError>;
}
