use crate::limits::check_object_count;
use crate::{Result, VeilError};
use lopdf::{dictionary, Document, Object, ObjectId};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;

/// Split a PDF file into individual single-page documents.
///
/// Returns a vector of byte vectors, one per page.
pub fn split_pdf<P: AsRef<Path>>(path: P) -> Result<Vec<Vec<u8>>> {
    let doc = Document::load(path.as_ref())?;
    check_object_count(&doc)?;
    split_pdf_doc(&doc)
}

/// Split a PDF from bytes into individual single-page documents.
pub fn split_pdf_from_bytes(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let doc = Document::load_mem(data)?;
    check_object_count(&doc)?;
    split_pdf_doc(&doc)
}

fn split_pdf_doc(doc: &Document) -> Result<Vec<Vec<u8>>> {
    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    let pages = doc.get_pages();
    if pages.is_empty() {
        return Err(VeilError::InvalidInput("PDF has no pages".into()));
    }

    // A2-style inheritance also matters here: when we lift a page out of its
    // /Pages tree, any inherited MediaBox/Resources/etc. would be lost.
    // Materialize them before extracting.
    let mut results = Vec::with_capacity(pages.len());

    for &page_id in pages.values() {
        let mut single = build_single_page_doc(doc, page_id)?;
        let mut buf = Vec::new();
        single.save_to(&mut buf)?;
        results.push(buf);
    }

    Ok(results)
}

/// Build a brand-new single-page Document by copying the page object plus the
/// transitive closure of objects it references. This avoids the O(N) clone of
/// the source Document per page that the previous implementation did
/// (peak working set scaled with input_size × page_count).
fn build_single_page_doc(src: &Document, page_id: ObjectId) -> Result<Document> {
    // Snapshot the source page dict and materialize any inherited attributes
    // from its parent chain. Stripped of /Parent — we'll wire a fresh /Pages.
    let mut page_dict = src
        .get_dictionary(page_id)
        .map_err(|_| VeilError::InvalidInput("Cannot read source page dictionary".into()))?
        .clone();
    page_dict.remove(b"Parent");

    let inherited = collect_inherited_page_attrs(src, page_id);
    for (key, value) in inherited {
        if !page_dict.has(key) {
            page_dict.set(key, value);
        }
    }

    // Walk the page dict to collect every reachable indirect reference.
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    visited.insert(page_id);
    collect_refs_in_dict(&page_dict, &mut queue, &mut visited);

    while let Some(id) = queue.pop_front() {
        if let Ok(obj) = src.get_object(id) {
            collect_refs_in_object(obj, &mut queue, &mut visited);
        }
    }
    // Drop the page itself; it is rebuilt below with the materialized dict.
    visited.remove(&page_id);

    // Allocate fresh IDs in the destination doc and remap.
    let mut dst = Document::with_version(src.version.clone());
    let mut id_map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();

    // Reserve IDs first so remap_refs can resolve forward references.
    for &old_id in &visited {
        let new_id = (dst.new_object_id().0, 0);
        id_map.insert(old_id, new_id);
    }

    // Copy each reachable object with refs remapped.
    for &old_id in &visited {
        if let Ok(obj) = src.get_object(old_id) {
            let new_id = id_map[&old_id];
            let remapped = remap_refs(obj.clone(), &id_map);
            dst.objects.insert(new_id, remapped);
        }
    }

    // Remap the page dict itself, then add it.
    let page_obj = remap_refs(Object::Dictionary(page_dict), &id_map);
    let new_page_id = dst.add_object(page_obj);

    // Wire up /Pages and /Catalog.
    let pages_id = dst.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(new_page_id)],
        "Count" => 1,
    });

    if let Ok(Object::Dictionary(page_dict_mut)) = dst.get_object_mut(new_page_id) {
        page_dict_mut.set("Parent", Object::Reference(pages_id));
    }

    let catalog_id = dst.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    dst.trailer.set("Root", Object::Reference(catalog_id));

    // Drop trailer entries that referenced byte offsets in the original file.
    dst.trailer.remove(b"Prev");
    dst.trailer.remove(b"XRefStm");

    Ok(dst)
}

fn collect_inherited_page_attrs(
    src: &Document,
    page_id: ObjectId,
) -> Vec<(&'static [u8], Object)> {
    const INHERITABLE: [&[u8]; 4] = [b"MediaBox", b"CropBox", b"Resources", b"Rotate"];
    let mut out: Vec<(&'static [u8], Object)> = Vec::new();

    let page = match src.get_dictionary(page_id) {
        Ok(d) => d,
        Err(_) => return out,
    };

    let mut needed: Vec<&'static [u8]> =
        INHERITABLE.iter().copied().filter(|k| !page.has(k)).collect();
    if needed.is_empty() {
        return out;
    }

    let mut current = page;
    let mut visited = HashSet::new();
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
        let parent = match src.get_dictionary(parent_id) {
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

fn collect_refs_in_object(obj: &Object, queue: &mut VecDeque<ObjectId>, visited: &mut HashSet<ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if visited.insert(*id) {
                queue.push_back(*id);
            }
        }
        Object::Array(arr) => {
            for o in arr {
                collect_refs_in_object(o, queue, visited);
            }
        }
        Object::Dictionary(d) => collect_refs_in_dict(d, queue, visited),
        Object::Stream(s) => {
            // Don't revisit /Parent — split-time we are explicitly severing
            // the page tree, so following parents would drag in the entire
            // original page hierarchy and defeat the whole point.
            for (key, value) in s.dict.iter() {
                if key.as_slice() == b"Parent" {
                    continue;
                }
                collect_refs_in_object(value, queue, visited);
            }
        }
        _ => {}
    }
}

fn collect_refs_in_dict(d: &lopdf::Dictionary, queue: &mut VecDeque<ObjectId>, visited: &mut HashSet<ObjectId>) {
    for (key, value) in d.iter() {
        if key.as_slice() == b"Parent" {
            continue;
        }
        collect_refs_in_object(value, queue, visited);
    }
}

fn remap_refs(obj: Object, map: &BTreeMap<ObjectId, ObjectId>) -> Object {
    match obj {
        // Mirror the merge.rs A1 fix: dangling reference => null.
        Object::Reference(id) => match map.get(&id) {
            Some(new_id) => Object::Reference(*new_id),
            None => Object::Null,
        },
        Object::Array(arr) => {
            Object::Array(arr.into_iter().map(|o| remap_refs(o, map)).collect())
        }
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
