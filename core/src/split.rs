use crate::{Result, VeilError};
use lopdf::Document;
use std::path::Path;

/// Split a PDF file into individual single-page documents.
///
/// Returns a vector of byte vectors, one per page.
pub fn split_pdf<P: AsRef<Path>>(path: P) -> Result<Vec<Vec<u8>>> {
    let doc = Document::load(path.as_ref())?;
    split_pdf_doc(&doc)
}

/// Split a PDF from bytes into individual single-page documents.
pub fn split_pdf_from_bytes(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let doc = Document::load_mem(data)?;
    split_pdf_doc(&doc)
}

fn split_pdf_doc(doc: &Document) -> Result<Vec<Vec<u8>>> {
    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    let pages = doc.get_pages();
    let page_numbers: Vec<u32> = pages.keys().copied().collect();

    if page_numbers.is_empty() {
        return Err(VeilError::InvalidInput("PDF has no pages".into()));
    }

    let mut results = Vec::with_capacity(page_numbers.len());

    for &page_num in &page_numbers {
        let mut single = doc.clone();
        let to_delete: Vec<u32> = page_numbers
            .iter()
            .copied()
            .filter(|&p| p != page_num)
            .collect();
        single.delete_pages(&to_delete);
        single.prune_objects();
        single.trailer.remove(b"Prev");
        single.trailer.remove(b"XRefStm");

        let mut buf = Vec::new();
        single.save_to(&mut buf)?;
        results.push(buf);
    }

    Ok(results)
}
