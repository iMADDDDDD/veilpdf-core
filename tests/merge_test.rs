use lopdf::{dictionary, Document, Object, Stream};

fn make_one_page_pdf(label: &str) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    let content = format!("BT /F1 24 Tf 100 700 Td ({label}) Tj ET");
    let stream = Stream::new(dictionary! {}, content.into_bytes());
    let content_id = doc.add_object(stream);

    let resources = dictionary! {
        "Font" => dictionary! {
            "F1" => Object::Reference(font_id),
        },
    };

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
        "Resources" => resources,
    });

    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => 1,
    });

    if let Ok(page) = doc.get_dictionary_mut(page_id) {
        page.set("Parent", Object::Reference(pages_id));
    }

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });

    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).expect("save test PDF");
    buf
}

#[test]
fn test_merge_via_ffi() {
    let a = make_one_page_pdf("FFI A");
    let b = make_one_page_pdf("FFI B");

    assert!(a.starts_with(b"%PDF"), "input A must be valid PDF");
    assert!(b.starts_with(b"%PDF"), "input B must be valid PDF");

    let buf = unsafe { veilpdf_core::ffi::veil_merge(a.as_ptr(), a.len(), b.as_ptr(), b.len()) };

    if !buf.error.is_null() && buf.error_len > 0 {
        let err = unsafe { std::slice::from_raw_parts(buf.error, buf.error_len) };
        panic!("veil_merge error: {}", String::from_utf8_lossy(err));
    }

    assert!(!buf.data.is_null(), "data must not be null");
    assert!(buf.len > 0, "data length must be > 0");

    let data = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    assert!(
        data.starts_with(b"%PDF"),
        "merged output must start with %PDF header"
    );

    let doc = Document::load_mem(data).expect("merged PDF must be loadable by lopdf");
    let pages = doc.get_pages();
    assert_eq!(
        pages.len(),
        2,
        "merged PDF must have 2 pages, got {}",
        pages.len()
    );

    // Verify each page has required keys
    for (&_page_num, &page_id) in &pages {
        let page = doc.get_dictionary(page_id).expect("page dict");
        assert!(
            page.has(b"MediaBox") || page.has(b"Parent"),
            "page missing required keys"
        );
    }

    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

#[test]
fn test_merge_three_pdfs_iteratively() {
    // This mirrors the Swift wrapper's iterative merge approach
    let a = make_one_page_pdf("Page 1");
    let b = make_one_page_pdf("Page 2");
    let c = make_one_page_pdf("Page 3");

    // Merge a + b
    let buf_ab = unsafe { veilpdf_core::ffi::veil_merge(a.as_ptr(), a.len(), b.as_ptr(), b.len()) };
    assert!(!buf_ab.data.is_null() && buf_ab.error.is_null());
    let ab = unsafe { std::slice::from_raw_parts(buf_ab.data, buf_ab.len) }.to_vec();
    unsafe { veilpdf_core::ffi::veil_free_buffer(buf_ab) };

    // Merge (a+b) + c
    let buf_abc =
        unsafe { veilpdf_core::ffi::veil_merge(ab.as_ptr(), ab.len(), c.as_ptr(), c.len()) };
    if !buf_abc.error.is_null() && buf_abc.error_len > 0 {
        let err = unsafe { std::slice::from_raw_parts(buf_abc.error, buf_abc.error_len) };
        panic!("iterative merge error: {}", String::from_utf8_lossy(err));
    }
    assert!(!buf_abc.data.is_null());

    let abc = unsafe { std::slice::from_raw_parts(buf_abc.data, buf_abc.len) };
    let doc = Document::load_mem(abc).expect("iteratively merged PDF must load");
    assert_eq!(doc.get_pages().len(), 3, "must have 3 pages");

    unsafe { veilpdf_core::ffi::veil_free_buffer(buf_abc) };
}
