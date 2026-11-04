//! dalzip engine: solid archives with content-defined dedup, chained per-block
//! codecs and authenticated encryption. Used by the `dz` CLI and the `dzui` GUI.

pub mod codec;
pub mod create;
pub mod crypto;
pub mod extract;
pub mod filter;
pub mod format;
pub mod iso;
pub mod mem;
pub mod progress;
pub mod util;
pub mod zip;
pub mod zipread;

pub use create::{create, CreateOpts, CreateReport};
pub use extract::{extract, is_encrypted, Archive, ExtractReport, Target};
pub use iso::{extract_iso, is_iso, list_iso, IsoListing};
pub use progress::Progress;
pub use zip::{create_zip, ZipReport};
pub use zipread::{extract_zip, is_zip, list_zip, ZipListing};
