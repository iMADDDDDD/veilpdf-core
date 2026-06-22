use crate::limits::check_object_count;
use crate::{Result, VeilError};
use lopdf::{Document, Object, ObjectId};
use std::collections::BTreeMap;
use std::path::Path;

/// Merge multiple PDF files into a single document.
///
/// Returns the merged PDF as bytes.
pub fn merge_pdfs<P: AsRef<Path>>(paths: &[P]) -> Result<Vec<u8>> {
    if paths.len() < 2 {
        return Err(VeilError::InvalidInput(
            "At least 2 PDF files are required for merge".into(),
        ));
    }

    let mut base = Document::load(paths[0].as_ref())?;
    reject_encrypted(&base)?;
    check_object_count(&base)?;

    for path in &paths[1..] {
        let incoming = Document::load(path.as_ref())?;
        reject_encrypted(&incoming)?;
        check_object_count(&incoming)?;
        append_document(&mut base, &incoming)?;
    }

    base.trailer.remove(b"Prev");
    base.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    base.save_to(&mut buf)?;
    Ok(buf)
}

/// Merge multiple PDFs from byte slices.
pub fn merge_pdfs_from_bytes(documents: &[&[u8]]) -> Result<Vec<u8>> {
    if documents.len() < 2 {
        return Err(VeilError::InvalidInput(
            "At least 2 PDF documents are required for merge".into(),
        ));
    }

    let mut base = Document::load_mem(documents[0])?;
    reject_encrypted(&base)?;
    check_object_count(&base)?;

    for doc_bytes in &documents[1..] {
        let incoming = Document::load_mem(doc_bytes)?;
        reject_encrypted(&incoming)?;
        check_object_count(&incoming)?;
        append_document(&mut base, &incoming)?;
    }

    // Remove stale file-offset trailer entries — they reference byte positions
    // in the *original* file and are meaningless in the rewritten output.
    base.trailer.remove(b"Prev");
    base.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    base.save_to(&mut buf)?;
    Ok(buf)
}

fn append_document(base: &mut Document, incoming: &Document) -> Result<()> {
    // Use max of both the highest key and base.max_id to avoid collisions
    // with deleted objects that left gaps in the object table
    let max_id = base
        .objects
        .keys()
        .map(|&(num, _)| num)
        .max()
        .unwrap_or(0)
        .max(base.max_id);

    let mut id_map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
    let mut next_id = max_id + 1;

    for &old_id in incoming.objects.keys() {
        id_map.insert(old_id, (next_id, 0));
        next_id += 1;
    }

    // A2: collect inherited page attributes by walking each page's parent
    // chain in the *incoming* document BEFORE we sever the chain. PDF 1.7
    // §7.7.3.4 lets MediaBox / CropBox / Resources / Rotate inherit from
    // ancestor /Pages nodes; once we reparent the page to the base catalog
    // those ancestors are gone and the values are lost forever.
    let incoming_pages = incoming.get_pages();
    let mut inherited_attrs: BTreeMap<ObjectId, Vec<(&'static [u8], Object)>> = BTreeMap::new();
    for &old_page_id in incoming_pages.values() {
        let attrs = collect_inherited_page_attrs(incoming, old_page_id);
        if !attrs.is_empty() {
            let new_id = id_map[&old_page_id];
            inherited_attrs.insert(new_id, attrs);
        }
    }

    for (old_id, object) in &incoming.objects {
        let new_id = id_map[old_id];
        let remapped = remap_refs(object.clone(), &id_map);
        base.objects.insert(new_id, remapped);
    }
    base.max_id = next_id;

    let incoming_page_ids: Vec<ObjectId> = incoming_pages
        .values()
        .map(|&old_id| id_map[&old_id])
        .collect();

    let pages_id = base
        .catalog()
        .ok()
        .and_then(|cat| cat.get(b"Pages").ok())
        .and_then(|p| p.as_reference().ok())
        .ok_or_else(|| VeilError::InvalidInput("Cannot find Pages in catalog".into()))?;

    let pages_dict = base
        .get_dictionary_mut(pages_id)
        .map_err(|_| VeilError::InvalidInput("Cannot get Pages dictionary".into()))?;

    match pages_dict.get_mut(b"Kids") {
        Ok(Object::Array(ref mut arr)) => {
            for page_id in &incoming_page_ids {
                arr.push(Object::Reference(*page_id));
            }
        }
        _ => {
            return Err(VeilError::InvalidInput(
                "Pages dictionary has missing or invalid Kids array".into(),
            ));
        }
    }

    let old_count = pages_dict
        .get(b"Count")
        .ok()
        .and_then(|c| c.as_i64().ok())
        .unwrap_or(0);
    let new_count = old_count + incoming_page_ids.len() as i64;
    pages_dict.set("Count", Object::Integer(new_count));

    for page_id in &incoming_page_ids {
        if let Ok(page_dict) = base.get_dictionary_mut(*page_id) {
            page_dict.set("Parent", Object::Reference(pages_id));
            // A2: materialize inherited attributes onto the page so that
            // re-parenting under the base catalog does not silently drop them.
            if let Some(attrs) = inherited_attrs.get(page_id) {
                for (key, value) in attrs {
                    if !page_dict.has(key) {
                        page_dict.set(*key, value.clone());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Walk a page's `/Parent` chain in the incoming document and collect any
/// `MediaBox` / `CropBox` / `Resources` / `Rotate` defined on an ancestor
/// `/Pages` node but missing on the page itself. Returns the values to be
/// materialized directly on the page after re-parenting.
fn collect_inherited_page_attrs(
    incoming: &Document,
    page_id: ObjectId,
) -> Vec<(&'static [u8], Object)> {
    const INHERITABLE: [&[u8]; 4] = [b"MediaBox", b"CropBox", b"Resources", b"Rotate"];
    let mut out: Vec<(&'static [u8], Object)> = Vec::new();

    let page = match incoming.get_dictionary(page_id) {
        Ok(d) => d,
        Err(_) => return out,
    };

    // Anything already defined on the page itself is not inherited.
    let mut needed: Vec<&'static [u8]> = INHERITABLE
        .iter()
        .copied()
        .filter(|k| !page.has(k))
        .collect();
    if needed.is_empty() {
        return out;
    }

    // Walk parent chain. Cap iterations to defend against /Parent cycles.
    let mut current = page;
    let mut visited = std::collections::HashSet::new();
    visited.insert(page_id);
    for _ in 0..64 {
        let parent_ref = match current.get(b"Parent") {
            Ok(p) => p,
            Err(_) => break,
        };
        let parent_id = match parent_ref.as_reference() {
            Ok(id) => id,
            Err(_) => break,
        };
        if !visited.insert(parent_id) {
            break;
        }
        let parent = match incoming.get_dictionary(parent_id) {
            Ok(d) => d,
            Err(_) => break,
        };
        needed.retain(|key| {
            if let Ok(value) = parent.get(key) {
                out.push((*key, value.clone()));
                false
            } else {
                true
            }
        });
        if needed.is_empty() {
            break;
        }
        current = parent;
    }

    out
}

fn remap_refs(obj: Object, map: &BTreeMap<ObjectId, ObjectId>) -> Object {
    match obj {
        // A1: PDF 1.7 §7.3.10 — an indirect reference to an undefined object
        // shall be treated as a reference to the null object. Falling back to
        // the original ID risks colliding with an unrelated base-document
        // object after merging, silently corrupting the output.
        Object::Reference(id) => match map.get(&id) {
            Some(new_id) => Object::Reference(*new_id),
            None => Object::Null,
        },
        Object::Array(arr) => Object::Array(arr.into_iter().map(|o| remap_refs(o, map)).collect()),
        Object::Dictionary(mut dict) => {
            for (_, val) in dict.iter_mut() {
                *val = remap_refs(std::mem::replace(val, Object::Null), map);
            }
            Object::Dictionary(dict)
        }
        Object::Stream(mut stream) => {
            for (_, val) in stream.dict.iter_mut() {
                *val = remap_refs(std::mem::replace(val, Object::Null), map);
            }
            Object::Stream(stream)
        }
        other => other,
    }
}

fn reject_encrypted(doc: &Document) -> Result<()> {
    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }
    Ok(())
}
