use crate::limits::check_object_count;
use crate::{Result, VeilError};
use lopdf::{Document, Object};

pub const FLAG_TEXT_MARKUP: u32 = 1 << 0;
pub const FLAG_NOTES: u32 = 1 << 1;
pub const FLAG_DRAWINGS: u32 = 1 << 2;
pub const FLAG_STAMPS: u32 = 1 << 3;
pub const FLAG_FREE_TEXT: u32 = 1 << 4;
pub const FLAG_OTHER: u32 = 1 << 5;
pub const FLAG_LINKS: u32 = 1 << 6;

/// Remove selected page annotations without replaying or rasterizing page
/// content streams. Form widgets are always preserved.
pub fn remove_annotations(data: &[u8], flags: u32) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem(data)?;
    check_object_count(&doc)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    if flags == 0 {
        return Ok(data.to_vec());
    }

    let page_ids: Vec<_> = doc.get_pages().values().copied().collect();
    for page_id in page_ids {
        let annots_obj = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|dict| dict.get(b"Annots").ok())
            .cloned();
        let Some(annots_obj) = annots_obj else {
            continue;
        };
        let Some(annots) = annotation_array(&doc, &annots_obj) else {
            continue;
        };

        let retained: Vec<Object> = annots
            .into_iter()
            .filter(|annotation| !should_remove_annotation(&doc, annotation, flags))
            .collect();

        if let Ok(page) = doc.get_dictionary_mut(page_id) {
            if retained.is_empty() {
                page.remove(b"Annots");
            } else {
                page.set("Annots", Object::Array(retained));
            }
        }
    }

    doc.compress();
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    Ok(buf)
}

fn annotation_array(doc: &Document, obj: &Object) -> Option<Vec<Object>> {
    match obj {
        Object::Array(values) => Some(values.clone()),
        Object::Reference(id) => match doc.get_object(*id).ok()? {
            Object::Array(values) => Some(values.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn should_remove_annotation(doc: &Document, annotation: &Object, flags: u32) -> bool {
    let subtype = annotation_subtype(doc, annotation);
    let Some(subtype) = subtype else {
        return flags & FLAG_OTHER != 0;
    };

    match subtype.as_slice() {
        b"Widget" => false,
        b"Highlight" | b"Underline" | b"StrikeOut" | b"Squiggly" => flags & FLAG_TEXT_MARKUP != 0,
        b"Text" | b"Popup" | b"Caret" => flags & FLAG_NOTES != 0,
        b"Ink" | b"Line" | b"Square" | b"Circle" | b"Polygon" | b"PolyLine" => {
            flags & FLAG_DRAWINGS != 0
        }
        b"Stamp" => flags & FLAG_STAMPS != 0,
        b"FreeText" => flags & FLAG_FREE_TEXT != 0,
        b"Link" => flags & FLAG_LINKS != 0,
        _ => flags & FLAG_OTHER != 0,
    }
}

fn annotation_subtype(doc: &Document, annotation: &Object) -> Option<Vec<u8>> {
    match annotation {
        Object::Dictionary(dict) => dict
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            .map(|name| name.to_vec()),
        Object::Reference(id) => doc
            .get_dictionary(*id)
            .ok()
            .and_then(|dict| dict.get(b"Subtype").ok())
            .and_then(|value| value.as_name().ok())
            .map(|name| name.to_vec()),
        Object::Stream(stream) => stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            .map(|name| name.to_vec()),
        _ => None,
    }
}
