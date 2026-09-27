//! StrataPDF core: document access and tile rendering on top of MuPDF.

pub mod doc;
pub mod fonts;
pub mod geom;
pub mod render;
pub mod text;

pub use doc::{DocClient, DocId, DocInfo, Document, LinkInfo, LinkTarget, OpenError, OutlineItem, Pending, SearchEvent, Waker};
pub use geom::{QuadF, RectF, SizeF};
pub use render::{RenderPool, RenderedTile, TileKey, ViewId};
pub use text::{CharPos, PageText};
