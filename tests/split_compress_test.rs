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

/// Build an N-page PDF by iteratively merging single-page PDFs via FFI.
fn make_multi_page_pdf(n: usize) -> Vec<u8> {
    assert!(n >= 2, "need at least 2 pages");
    let mut acc = make_one_page_pdf("Page 1");
    for i in 2..=n {
        let next = make_one_page_pdf(&format!("Page {i}"));
        let buf = unsafe {
            veilpdf_core::ffi::veil_merge(acc.as_ptr(), acc.len(), next.as_ptr(), next.len())
        };
        assert!(buf.error.is_null(), "merge failed while building multi-page PDF");
        acc = unsafe { std::slice::from_raw_parts(buf.data, buf.len) }.to_vec();
        unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
    }
    acc
}

/// Parse the length-prefixed split buffer into individual page PDFs.
fn parse_split_pages(data: &[u8]) -> Vec<Vec<u8>> {
    let mut pages = Vec::new();
    let mut offset = 0;
    while offset + 8 <= data.len() {
        let len = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()) as usize;
        offset += 8;
        assert!(offset + len <= data.len(), "split buffer truncated");
        pages.push(data[offset..offset + len].to_vec());
        offset += len;
    }
    pages
}

// ─── Split tests ───

#[test]
fn test_split_single_page() {
    let pdf = make_one_page_pdf("Solo");

    let buf = unsafe { veilpdf_core::ffi::veil_split(pdf.as_ptr(), pdf.len()) };
    assert!(buf.error.is_null(), "split should succeed");

    let data = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let pages = parse_split_pages(data);
    assert_eq!(pages.len(), 1, "single-page PDF should produce 1 page");

    assert!(pages[0].starts_with(b"%PDF"), "output must be valid PDF");
    let doc = Document::load_mem(&pages[0]).expect("split page must load");
    assert_eq!(doc.get_pages().len(), 1);

    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

#[test]
fn test_split_multi_page() {
    let pdf = make_multi_page_pdf(3);

    let buf = unsafe { veilpdf_core::ffi::veil_split(pdf.as_ptr(), pdf.len()) };
    if !buf.error.is_null() && buf.error_len > 0 {
        let err = unsafe { std::slice::from_raw_parts(buf.error, buf.error_len) };
        panic!("split error: {}", String::from_utf8_lossy(err));
    }

    let data = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    let pages = parse_split_pages(data);
    assert_eq!(pages.len(), 3, "3-page PDF should produce 3 pages");

    for (i, page_data) in pages.iter().enumerate() {
        assert!(page_data.starts_with(b"%PDF"), "page {i} must be valid PDF");
        let doc = Document::load_mem(page_data).unwrap_or_else(|_| panic!("page {i} must load"));
        assert_eq!(doc.get_pages().len(), 1, "page {i} must have exactly 1 page");
    }

    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

#[test]
fn test_split_empty_input() {
    let buf = unsafe { veilpdf_core::ffi::veil_split(b"".as_ptr(), 0) };
    assert!(!buf.error.is_null(), "empty input should return error");
    assert!(buf.error_len > 0);
    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

#[test]
fn test_split_invalid_input() {
    let garbage = b"this is not a PDF at all";
    let buf = unsafe { veilpdf_core::ffi::veil_split(garbage.as_ptr(), garbage.len()) };
    assert!(!buf.error.is_null(), "garbage input should return error");
    assert!(buf.error_len > 0);
    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

// ─── Compress tests ───

#[test]
fn test_compress_valid_pdf() {
    let pdf = make_one_page_pdf("Compress me");

    let buf = unsafe { veilpdf_core::ffi::veil_compress(pdf.as_ptr(), pdf.len()) };
    if !buf.error.is_null() && buf.error_len > 0 {
        let err = unsafe { std::slice::from_raw_parts(buf.error, buf.error_len) };
        panic!("compress error: {}", String::from_utf8_lossy(err));
    }

    assert!(!buf.data.is_null(), "data must not be null");
    assert!(buf.len > 0);

    let data = unsafe { std::slice::from_raw_parts(buf.data, buf.len) };
    assert!(data.starts_with(b"%PDF"), "compressed output must be valid PDF");

    let doc = Document::load_mem(data).expect("compressed PDF must load");
    assert_eq!(doc.get_pages().len(), 1, "page count must be preserved");

    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

#[test]
fn test_compress_empty_input() {
    let buf = unsafe { veilpdf_core::ffi::veil_compress(b"".as_ptr(), 0) };
    assert!(!buf.error.is_null(), "empty input should return error");
    assert!(buf.error_len > 0);
    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}

#[test]
fn test_compress_invalid_input() {
    let garbage = b"definitely not a PDF";
    let buf = unsafe { veilpdf_core::ffi::veil_compress(garbage.as_ptr(), garbage.len()) };
    assert!(!buf.error.is_null(), "garbage input should return error");
    assert!(buf.error_len > 0);
    unsafe { veilpdf_core::ffi::veil_free_buffer(buf) };
}
