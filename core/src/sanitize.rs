use crate::compress::is_metadata_stream;
use crate::limits::check_object_count;
use crate::{Result, VeilError};
use lopdf::{Document, Object, ObjectId};

/// Flag constants for sanitization.
pub const FLAG_STRIP_METADATA: u32  = 0b00001;
pub const FLAG_REMOVE_JS: u32       = 0b00010;
pub const FLAG_REMOVE_EMBEDDED: u32 = 0b00100;
pub const FLAG_REMOVE_ACTIONS: u32  = 0b01000;
pub const FLAG_REMOVE_XMP: u32      = 0b10000;

/// Always-on safety baseline. A8: callers cannot opt out of these — even
/// `flags == 0` must produce a JS-free, action-free, embedded-file-free PDF.
/// "Sanitize" implies a safe baseline; allowing a no-op would be a footgun.
const REQUIRED_FLAGS: u32 = FLAG_REMOVE_JS | FLAG_REMOVE_ACTIONS | FLAG_REMOVE_EMBEDDED;

/// Sanitize a PDF by removing potentially dangerous or sensitive elements.
///
/// The caller's `flags` are OR-ed with [`REQUIRED_FLAGS`] before processing.
/// JavaScript, action chains, and embedded files are *always* stripped — the
/// flags argument can only ADD optional sweeps (XMP, /Info entries, etc.),
/// never remove the safety baseline.
///
/// Returns an error for encrypted PDFs.
pub fn sanitize_pdf(data: &[u8], flags: u32) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem(data)?;
    check_object_count(&doc)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    // A8: enforce the safety baseline regardless of caller input.
    let flags = flags | REQUIRED_FLAGS;

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

/// Remove XMP metadata streams.
///
/// A7: detects both the historic `/Subtype /XML` form and the more common
/// `/Type /Metadata` form (no /Subtype), then scrubs orphan `/Metadata`
/// references from the catalog and every page so the document no longer
/// claims to ship metadata it doesn't actually contain.
fn strip_xmp(doc: &mut Document) {
    let ids_to_remove: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            if is_metadata_stream(obj) {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    for id in ids_to_remove {
        doc.objects.remove(&id);
    }

    // Belt-and-braces: remove orphan /Metadata pointers.
    if let Ok(root_ref) = doc.trailer.get(b"Root") {
        if let Ok(root_id) = root_ref.as_reference() {
            if let Ok(catalog) = doc.get_dictionary_mut(root_id) {
                catalog.remove(b"Metadata");
            }
        }
    }
    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    for page_id in page_ids {
        if let Ok(dict) = doc.get_dictionary_mut(page_id) {
            dict.remove(b"Metadata");
        }
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
///
/// A6: JS lives in many more places than the catalog's `/Names`. We must
/// also clear:
///   * Annotation `/AA` action chains (mouse-up etc.)
///   * Form field `/AA` action chains (focus/blur/calculate)
///   * `/AcroForm /XFA` streams (XFA contains XML+JS)
///   * Any nested action whose `/S` is `/JavaScript`, including those
///     reached via the catalog `/OpenAction` and `/Names → /JavaScript`
///     trees, no matter how deep.
fn strip_javascript(doc: &mut Document) {
    // ── Catalog-level scrubbing ───────────────────────────────────────────
    if let Ok(root_ref) = doc.trailer.get(b"Root") {
        if let Ok(root_id) = root_ref.as_reference() {
            if let Ok(catalog) = doc.get_dictionary_mut(root_id) {
                catalog.remove(b"JavaScript");
            }
            if let Ok(catalog) = doc.get_dictionary(root_id) {
                if let Ok(names_ref) = catalog.get(b"Names") {
                    if let Ok(names_id) = names_ref.as_reference() {
                        if let Ok(names) = doc.get_dictionary_mut(names_id) {
                            names.remove(b"JavaScript");
                        }
                    }
                }
            }
            // AcroForm /XFA — the entire XFA stream is treated as JS-bearing
            // and removed; AcroForm itself is left intact otherwise.
            let acroform_id = doc
                .get_dictionary(root_id)
                .ok()
                .and_then(|cat| cat.get(b"AcroForm").ok())
                .and_then(|af| af.as_reference().ok());
            if let Some(af_id) = acroform_id {
                if let Ok(af) = doc.get_dictionary_mut(af_id) {
                    af.remove(b"XFA");
                }
            } else if let Ok(catalog) = doc.get_dictionary_mut(root_id) {
                // /AcroForm may also be inline (not a reference).
                if let Ok(Object::Dictionary(af)) = catalog.get_mut(b"AcroForm") {
                    af.remove(b"XFA");
                }
            }
        }
    }

    // ── Document-wide JS scrubbing on every dictionary ────────────────────
    //
    // We touch every dictionary in the object table:
    //   * Strip any direct entry whose value is an action dict with /S == /JavaScript.
    //   * Strip the /JS entry on any action that previously had /S /JavaScript.
    //   * Drop entire /AA dictionaries (they are *only* triggered actions).
    //
    // Two passes: collect IDs first to avoid mutating while iterating.
    let all_ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    for id in all_ids {
        if let Ok(Object::Dictionary(dict)) = doc.get_object_mut(id) {
            scrub_js_in_dict(dict);
        }
    }
    // Streams also carry dicts — scrub those too (XFA wrappers, etc.).
    let stream_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, o)| if matches!(o, Object::Stream(_)) { Some(id) } else { None })
        .collect();
    for id in stream_ids {
        if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
            scrub_js_in_dict(&mut s.dict);
        }
    }

    // ── Remove standalone JavaScript streams ──────────────────────────────
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
                let s_is_js = stream
                    .dict
                    .get(b"S")
                    .ok()
                    .and_then(|v| v.as_name().ok())
                    .map(|n: &[u8]| n == b"JavaScript")
                    .unwrap_or(false);
                if is_js || s_is_js {
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

/// Walk a single dictionary and scrub anything that smells like JavaScript.
/// Recurses into nested inline dictionaries / arrays.
///
/// Note: we cannot follow indirect references from here because the caller
/// holds a `&mut Dictionary` and following refs needs `&Document`. The outer
/// pass over every object in the document table picks up whatever lives
/// behind references in subsequent iterations.
fn scrub_js_in_dict(dict: &mut lopdf::Dictionary) {
    // 1) /AA dictionaries are *only* triggered actions; drop wholesale.
    dict.remove(b"AA");

    // 2) If this dict itself is an action with /S /JavaScript, neuter it
    //    by removing the JS payload entries (we can't drop the dict from
    //    inside, but stripping /JS / /JavaScript yields a no-op action).
    let s_is_js = dict
        .get(b"S")
        .ok()
        .and_then(|v| v.as_name().ok())
        .map(|n: &[u8]| n == b"JavaScript")
        .unwrap_or(false);
    if s_is_js {
        dict.remove(b"JS");
        dict.remove(b"JavaScript");
        // Replace /S so subsequent passes don't try to chain a Next action
        // off a JS shell.
        dict.set("S", lopdf::Object::Name(b"Named".to_vec()));
        dict.set("N", lopdf::Object::Name(b"NoOp".to_vec()));
    }

    // 3) Recurse into inline dict/array children. Indirect references are
    //    handled by the outer document-wide loop.
    let keys: Vec<Vec<u8>> = dict.iter().map(|(k, _)| k.to_vec()).collect();
    for key in keys {
        if let Ok(value) = dict.get_mut(&key) {
            scrub_js_in_object(value);
        }
    }
}

fn scrub_js_in_object(obj: &mut Object) {
    match obj {
        Object::Dictionary(d) => scrub_js_in_dict(d),
        Object::Array(arr) => {
            for o in arr.iter_mut() {
                scrub_js_in_object(o);
            }
        }
        _ => {}
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
