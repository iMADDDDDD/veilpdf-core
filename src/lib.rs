//! # VeilPDF Core
//!
//! Privacy-first PDF manipulation library. Merge, split, and compress PDFs
//! entirely offline — your documents never leave your machine.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use veilpdf_core::{merge_pdfs, split_pdf, compress_pdf};
//!
//! // Merge two PDFs
//! let merged = merge_pdfs(&["a.pdf", "b.pdf"]).unwrap();
//! std::fs::write("merged.pdf", merged).unwrap();
//!
//! // Split a PDF into individual pages
//! let pages = split_pdf("multi.pdf").unwrap();
//! for (i, page) in pages.iter().enumerate() {
//!     std::fs::write(format!("page_{}.pdf", i + 1), page).unwrap();
//! }
//!
//! // Compress a PDF
//! let result = compress_pdf("large.pdf").unwrap();
//! std::fs::write("compressed.pdf", result.data).unwrap();
//! println!("Reduced by {:.1}%", result.reduction_percent);
//! ```

pub mod merge;
pub mod split;
pub mod compress;
pub mod sanitize;
pub mod extract;
pub mod ffi;
pub mod limits;

pub use merge::{merge_pdfs, merge_pdfs_from_bytes};
pub use split::{split_pdf, split_pdf_from_bytes};
pub use compress::{compress_pdf, compress_pdf_from_bytes, compress_pdf_with_options, CompressOptions, CompressResult};
pub use sanitize::sanitize_pdf;
pub use extract::{extract_images, ExtractedImage};

use std::fmt;

#[derive(Debug)]
pub enum VeilError {
    IoError(std::io::Error),
    PdfError(lopdf::Error),
    InvalidInput(String),
}

impl fmt::Display for VeilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VeilError::IoError(e) => write!(f, "IO error: {e}"),
            VeilError::PdfError(e) => write!(f, "PDF error: {e}"),
            VeilError::InvalidInput(msg) => write!(f, "Invalid input: {msg}"),
        }
    }
}

impl std::error::Error for VeilError {}

impl From<std::io::Error> for VeilError {
    fn from(e: std::io::Error) -> Self {
        VeilError::IoError(e)
    }
}

impl From<lopdf::Error> for VeilError {
    fn from(e: lopdf::Error) -> Self {
        VeilError::PdfError(e)
    }
}

pub type Result<T> = std::result::Result<T, VeilError>;
