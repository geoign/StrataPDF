//! StrataPDF core: document access and tile rendering on top of MuPDF.

pub mod annot;
pub mod doc;
pub mod fonts;
pub mod geom;
pub mod ocr;
pub mod reflow;
pub mod render;
pub mod rich;
pub mod text;

pub use doc::{AnnotOp, AnnotResult, SaveOptions, DocClient, DocId, DocInfo, Document, LinkInfo, LinkTarget, OpenError, OutlineItem, Pending, SearchEvent, Waker};
pub use geom::{QuadF, RectF, SizeF};
pub use render::{RenderPool, RenderedTile, TileKey, ViewId};
pub use text::{CharPos, PageText};
