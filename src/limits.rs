//! DoS-defense limits applied to every parsed PDF.
//!
//! Pathological PDFs (e.g. zip-bomb-style object-stream amplification) can
//! produce documents with billions of objects, which would hang every
//! downstream operation. We cap the object count to a generous-but-finite
//! number and reject anything above it.

use crate::{Result, VeilError};
use lopdf::Document;

/// Hard upper bound on object count. Real-world PDFs almost never exceed a
/// few hundred thousand objects; anything above this is almost certainly an
/// attack or a broken generator.
pub const MAX_OBJECT_COUNT: u32 = 500_000;

/// Reject documents whose object count exceeds [`MAX_OBJECT_COUNT`].
///
/// Checks both `max_id` (the high-water mark allocated by the parser) and
/// `objects.len()` (the actual table size). Either crossing the threshold is
/// disqualifying.
pub fn check_object_count(doc: &Document) -> Result<()> {
    if doc.max_id > MAX_OBJECT_COUNT || doc.objects.len() > MAX_OBJECT_COUNT as usize {
        return Err(VeilError::InvalidInput(
            "PDF object count exceeds limit".into(),
        ));
    }
    Ok(())
}
