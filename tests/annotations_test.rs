use lopdf::{dictionary, Document, Object, Stream};
use veilpdf_core::annotations::{remove_annotations, FLAG_LINKS, FLAG_TEXT_MARKUP};

fn make_annotated_text_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    let content = b"BT /F1 24 Tf 100 700 Td (SELECTABLE TEXT) Tj ET".to_vec();
    let content_id = doc.add_object(Stream::new(dictionary! {}, content));

    let resources = dictionary! {
        "Font" => dictionary! {
            "F1" => Object::Reference(font_id),
        },
    };

    let highlight_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Highlight",
        "Rect" => vec![95.into(), 695.into(), 300.into(), 730.into()],
        "QuadPoints" => vec![
            95.into(), 730.into(), 300.into(), 730.into(),
            95.into(), 695.into(), 300.into(), 695.into(),
        ],
    });
    let widget_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "Rect" => vec![100.into(), 620.into(), 260.into(), 650.into()],
        "T" => Object::string_literal("name"),
    });
    let link_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Link",
        "Rect" => vec![100.into(), 560.into(), 260.into(), 590.into()],
    });

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
        "Resources" => resources,
        "Annots" => vec![
            Object::Reference(highlight_id),
            Object::Reference(widget_id),
            Object::Reference(link_id),
        ],
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
    doc.save_to(&mut buf).expect("save test PDF");
    buf
}

fn page_annotation_subtypes(doc: &Document) -> Vec<Vec<u8>> {
    let page_id = doc.get_pages().values().next().copied().unwrap();
    let page = doc.get_dictionary(page_id).unwrap();
    let annots = page.get(b"Annots").unwrap();
    let values = match annots {
        Object::Array(values) => values.clone(),
        Object::Reference(id) => match doc.get_object(*id).unwrap() {
            Object::Array(values) => values.clone(),
            other => panic!("unexpected annotation array object: {other:?}"),
        },
        other => panic!("unexpected Annots value: {other:?}"),
    };

    values
        .iter()
        .map(|value| match value {
            Object::Reference(id) => doc.get_dictionary(*id).unwrap(),
            Object::Dictionary(dict) => dict,
            other => panic!("unexpected annotation object: {other:?}"),
        })
        .map(|dict| dict.get(b"Subtype").unwrap().as_name().unwrap().to_vec())
        .collect()
}

#[test]
fn removes_selected_annotations_without_touching_page_content() {
    let input = make_annotated_text_pdf();
    let input_doc = Document::load_mem(&input).expect("input loads");
    let page_id = input_doc.get_pages().values().next().copied().unwrap();
    let input_content = input_doc.get_page_content(page_id).expect("input content");

    let output = remove_annotations(&input, FLAG_TEXT_MARKUP).expect("remove annotations");
    let output_doc = Document::load_mem(&output).expect("output loads");
    let output_page_id = output_doc.get_pages().values().next().copied().unwrap();
    let output_content = output_doc
        .get_page_content(output_page_id)
        .expect("output content");

    assert_eq!(
        String::from_utf8_lossy(&output_content),
        String::from_utf8_lossy(&input_content),
        "annotation removal must not rewrite or rasterize page content"
    );

    let subtypes = page_annotation_subtypes(&output_doc);
    assert!(!subtypes.iter().any(|subtype| subtype == b"Highlight"));
    assert!(subtypes.iter().any(|subtype| subtype == b"Widget"));
    assert!(subtypes.iter().any(|subtype| subtype == b"Link"));
}

#[test]
fn removes_links_only_when_requested() {
    let input = make_annotated_text_pdf();
    let output =
        remove_annotations(&input, FLAG_TEXT_MARKUP | FLAG_LINKS).expect("remove annotations");
    let output_doc = Document::load_mem(&output).expect("output loads");

    let subtypes = page_annotation_subtypes(&output_doc);
    assert!(!subtypes.iter().any(|subtype| subtype == b"Highlight"));
    assert!(!subtypes.iter().any(|subtype| subtype == b"Link"));
    assert!(subtypes.iter().any(|subtype| subtype == b"Widget"));
}
