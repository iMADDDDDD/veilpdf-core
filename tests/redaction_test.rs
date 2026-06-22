use lopdf::{dictionary, Document, Object, Stream};
use veilpdf_core::{apply_redactions, RedactionRect};

fn make_text_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    let content = b"BT /F1 24 Tf 100 700 Td (KEEP REDACT KEEP) Tj ET".to_vec();
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));

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

    doc.get_dictionary_mut(page_id)
        .unwrap()
        .set("Parent", Object::Reference(pages_id));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

fn make_flipped_two_line_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    let content = b"q 1 0 0 -1 0 792 cm \
BT 12 0 0 -12 72 163 Tm /F1 1 Tf (Lorem ipsum dolor sit amet, consectetuer adipiscing elit.) Tj ET \
BT 12 0 0 -12 72 178 Tm /F1 1 Tf (Curabitur suscipit. Nullam vel nisi.) Tj ET Q"
        .to_vec();
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));

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

    doc.get_dictionary_mut(page_id)
        .unwrap()
        .set("Parent", Object::Reference(pages_id));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

fn first_page_content(data: &[u8]) -> Vec<u8> {
    let doc = Document::load_mem(data).expect("PDF should parse");
    let page_id = doc.get_pages().values().next().copied().unwrap();
    doc.get_page_content(page_id).expect("page content")
}

#[test]
fn redaction_removes_selected_text_without_rasterizing_page() {
    let input = make_text_pdf();
    let output = apply_redactions(
        &input,
        &[RedactionRect {
            page_index: 0,
            x: 158.0,
            y: 690.0,
            width: 82.0,
            height: 40.0,
        }],
    )
    .expect("redaction should succeed");

    let reloaded = Document::load_mem(&output).expect("output should parse");
    assert_eq!(reloaded.get_pages().len(), 1);

    let content = first_page_content(&output);
    let content_text = String::from_utf8_lossy(&content);
    assert!(
        content_text.contains("KEEP"),
        "text outside redaction should remain in the content stream: {content_text}"
    );
    assert!(
        !content_text.contains("REDACT"),
        "redacted text should not remain as a contiguous content-string literal: {content_text}"
    );

    let has_image_xobject = reloaded.objects.values().any(|object| {
        let Object::Stream(stream) = object else {
            return false;
        };
        stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            == Some(b"Image")
    });
    assert!(
        !has_image_xobject,
        "redaction must not replace the page with an image XObject"
    );
}

#[test]
fn redaction_does_not_remove_adjacent_line_on_small_vertical_overlap() {
    let input = make_flipped_two_line_pdf();
    let output = apply_redactions(
        &input,
        &[RedactionRect {
            page_index: 0,
            x: 70.5,
            y: 624.9,
            width: 154.0,
            height: 17.2,
        }],
    )
    .expect("redaction should succeed");

    let content = first_page_content(&output);
    let content_text = String::from_utf8_lossy(&content);

    assert!(
        !content_text.contains("Lorem ipsum dolor sit amet"),
        "selected first-line text should be removed: {content_text}"
    );
    assert!(
        content_text.contains("consectetuer"),
        "unselected first-line text should remain: {content_text}"
    );
    assert!(
        content_text.contains("Curabitur"),
        "next-line text must not be removed by a tiny vertical overlap: {content_text}"
    );
}

#[test]
fn rejects_empty_redactions() {
    let input = make_text_pdf();
    let result = apply_redactions(&input, &[]);
    assert!(result.is_err(), "empty redaction set must be rejected");
}
