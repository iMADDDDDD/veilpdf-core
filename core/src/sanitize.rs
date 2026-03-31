use crate::{Result, VeilError};
use lopdf::{Document, ObjectId};

/// Flag constants for sanitization.
pub const FLAG_STRIP_METADATA: u32  = 0b00001;
pub const FLAG_REMOVE_JS: u32       = 0b00010;
pub const FLAG_REMOVE_EMBEDDED: u32 = 0b00100;
pub const FLAG_REMOVE_ACTIONS: u32  = 0b01000;
pub const FLAG_REMOVE_XMP: u32      = 0b10000;

/// Sanitize a PDF by removing potentially dangerous or sensitive elements.
///
/// Returns an error for encrypted PDFs.
pub fn sanitize_pdf(data: &[u8], flags: u32) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem(data)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    if flags & FLAG_STRIP_METADATA != 0 {
        strip_info_dict(&mut doc);
    }

    if flags & FLAG_REMOVE_XMP != 0 {
        strip_xmp(&mut doc);
        strip_thumbnails(&mut doc);
    }

    if flags & FLAG_REMOVE_JS != 0 {
        strip_javascript(&mut doc);
    }

    if flags & FLAG_REMOVE_ACTIONS != 0 {
        strip_actions(&mut doc);
    }

    if flags & FLAG_REMOVE_EMBEDDED != 0 {
        strip_embedded_files(&mut doc);
    }

    doc.compress();
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    Ok(buf)
}

/// Remove /Info dictionary entries except /Title.
fn strip_info_dict(doc: &mut Document) {
    if let Ok(info_ref) = doc.trailer.get(b"Info") {
        if let Ok(id) = info_ref.as_reference() {
            if let Ok(dict) = doc.get_dictionary_mut(id) {
                let keys_to_remove: Vec<Vec<u8>> = dict
                    .iter()
                    .map(|(k, _)| k.to_vec())
                    .filter(|k| k.as_slice() != b"Title")
                    .collect();
                for key in keys_to_remove {
                    dict.remove(&key);
                }
            }
        }
    }
}

/// Remove XMP metadata streams (Subtype == XML).
fn strip_xmp(doc: &mut Document) {
    let ids_to_remove: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            if let Ok(stream) = obj.as_stream() {
                let is_xmp = stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|v| v.as_name().ok())
                    .map(|n: &[u8]| n == b"XML")
                    .unwrap_or(false);
                if is_xmp {
                    return Some(id);
                }
            }
            None
        })
        .collect();

    for id in ids_to_remove {
        doc.objects.remove(&id);
    }
}

/// Remove /Thumb entries from all pages.
fn strip_thumbnails(doc: &mut Document) {
    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for page_id in page_ids {
        if let Ok(dict) = doc.get_dictionary_mut(page_id) {
            dict.remove(b"Thumb");
        }
    }
}

/// Remove JavaScript from the document.
fn strip_javascript(doc: &mut Document) {
    // Remove /JavaScript from catalog's /Names dictionary
    if let Ok(root_ref) = doc.trailer.get(b"Root") {
        if let Ok(root_id) = root_ref.as_reference() {
            if let Ok(catalog) = doc.get_dictionary_mut(root_id) {
                catalog.remove(b"JavaScript");
            }
            // Also check /Names -> /JavaScript
            if let Ok(catalog) = doc.get_dictionary(root_id) {
                if let Ok(names_ref) = catalog.get(b"Names") {
                    if let Ok(names_id) = names_ref.as_reference() {
                        if let Ok(names) = doc.get_dictionary_mut(names_id) {
                            names.remove(b"JavaScript");
                        }
                    }
                }
            }
        }
    }

    // Remove streams with /Subtype /JavaScript
    let js_stream_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            if let Ok(stream) = obj.as_stream() {
                let is_js = stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|v| v.as_name().ok())
                    .map(|n: &[u8]| n == b"JavaScript")
                    .unwrap_or(false);
                if is_js {
                    return Some(id);
                }
            }
            None
        })
        .collect();

    for id in js_stream_ids {
        doc.objects.remove(&id);
    }
}

/// Remove document actions (/OpenAction, /AA from catalog and pages).
fn strip_actions(doc: &mut Document) {
    // Remove from catalog
    if let Ok(root_ref) = doc.trailer.get(b"Root") {
        if let Ok(root_id) = root_ref.as_reference() {
            if let Ok(catalog) = doc.get_dictionary_mut(root_id) {
                catalog.remove(b"OpenAction");
                catalog.remove(b"AA");
            }
        }
    }

    // Remove /AA from all pages
    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for page_id in page_ids {
        if let Ok(dict) = doc.get_dictionary_mut(page_id) {
            dict.remove(b"AA");
        }
    }
}

/// Remove embedded files from /Names dictionary.
fn strip_embedded_files(doc: &mut Document) {
    if let Ok(root_ref) = doc.trailer.get(b"Root") {
        if let Ok(root_id) = root_ref.as_reference() {
            if let Ok(catalog) = doc.get_dictionary(root_id) {
                if let Ok(names_ref) = catalog.get(b"Names") {
                    if let Ok(names_id) = names_ref.as_reference() {
                        if let Ok(names) = doc.get_dictionary_mut(names_id) {
                            names.remove(b"EmbeddedFiles");
                        }
                    }
                }
            }
        }
    }
}
