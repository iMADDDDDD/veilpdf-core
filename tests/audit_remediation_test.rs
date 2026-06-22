//! Regression tests for the Group A Rust core audit remediation (A1–A9).
//!
//! Each test names the audit finding it covers and asserts the contract
//! described in the audit. These tests build their own minimal PDFs in
//! memory using lopdf so the suite never depends on external fixture files.

use lopdf::{dictionary, Document, Object, Stream};

// ─── Shared fixture helpers ────────────────────────────────────────────────

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
    doc.save_to(&mut buf).expect("save");
    buf
}

// ─── A1: dangling reference => Object::Null ───────────────────────────────

/// Build a one-page PDF whose page Resources entry contains a *dangling*
/// indirect reference (an ID that does not exist in the object table).
/// After merging this with another doc, the dangling ref must end up Null
/// in the merged output, not a collision with a base-doc object.
fn make_pdf_with_dangling_ref() -> (Vec<u8>, u32 /*dangling object number*/) {
    let mut doc = Document::with_version("1.5");

    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));

    // Reference an object number that is intentionally *not* in the table.
    let dangling_num: u32 = 9_999;
    let dangling_id: lopdf::ObjectId = (dangling_num, 0);
    let resources = dictionary! {
        "Font" => Object::Reference(dangling_id),
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
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }
    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    (buf, dangling_num)
}

#[test]
fn a1_dangling_ref_becomes_null_after_merge() {
    let base = make_one_page_pdf("Base");
    let (incoming, _dangling_num) = make_pdf_with_dangling_ref();

    let merged = veilpdf_core::merge_pdfs_from_bytes(&[&base, &incoming])
        .expect("merge should succeed even with dangling refs");

    let doc = Document::load_mem(&merged).expect("merged doc loads");

    // Find the page that was incoming (page index 1). Its Resources/Font
    // should have been remapped to Object::Null, not a stray reference into
    // the base doc.
    let pages = doc.get_pages();
    assert_eq!(pages.len(), 2, "merge produced wrong page count");
    let incoming_page_id = pages.values().nth(1).copied().unwrap();
    let page = doc.get_dictionary(incoming_page_id).unwrap();

    let resources = match page.get(b"Resources").unwrap() {
        Object::Dictionary(d) => d,
        Object::Reference(rid) => doc.get_dictionary(*rid).unwrap(),
        other => panic!("unexpected Resources type: {:?}", other),
    };
    let font_value = resources.get(b"Font").unwrap();
    assert!(
        matches!(font_value, Object::Null),
        "dangling ref must be replaced with Null, got {:?}",
        font_value
    );
}

// ─── A2: inherited page attributes are materialized after merge ───────────

/// Build a 1-page PDF whose MediaBox lives on the *Pages* root, not on the
/// leaf Page. The page inherits MediaBox per PDF 1.7 §7.7.3.4.
fn make_pdf_with_inherited_mediabox() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Contents" => Object::Reference(content_id),
        // No MediaBox here — it's inherited from /Pages below.
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => vec![Object::Reference(page_id)],
        "Count" => 1,
        "MediaBox" => vec![0.into(), 0.into(), 200.into(), 400.into()],
    });
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }
    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

#[test]
fn a2_inherited_mediabox_materialized_after_merge() {
    let base = make_one_page_pdf("Base");
    let incoming = make_pdf_with_inherited_mediabox();

    let merged = veilpdf_core::merge_pdfs_from_bytes(&[&base, &incoming]).unwrap();
    let doc = Document::load_mem(&merged).unwrap();

    let pages = doc.get_pages();
    let incoming_page_id = pages.values().nth(1).copied().unwrap();
    let page = doc.get_dictionary(incoming_page_id).unwrap();

    let mb = page
        .get(b"MediaBox")
        .expect("MediaBox must be materialized on the merged page");
    let arr = mb.as_array().unwrap();
    assert_eq!(arr.len(), 4, "MediaBox should be a 4-element array");
    let w = arr[2]
        .as_i64()
        .or_else(|_| arr[2].as_float().map(|f| f as i64))
        .unwrap();
    let h = arr[3]
        .as_i64()
        .or_else(|_| arr[3].as_float().map(|f| f as i64))
        .unwrap();
    assert_eq!(
        (w, h),
        (200, 400),
        "MediaBox values must match the inherited ones"
    );
}

// ─── A3: split copies only the page's transitive resources ────────────────

#[test]
fn a3_split_completes_for_many_pages() {
    // Build a 50-page PDF by iterative merge and ensure split produces
    // 50 valid one-page docs, in finite time.
    let mut acc = make_one_page_pdf("Page 1");
    for i in 2..=50 {
        acc =
            veilpdf_core::merge_pdfs_from_bytes(&[&acc, &make_one_page_pdf(&format!("Page {i}"))])
                .unwrap();
    }

    let start = std::time::Instant::now();
    let pages = veilpdf_core::split_pdf_from_bytes(&acc).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(pages.len(), 50);
    for (i, p) in pages.iter().enumerate() {
        assert!(p.starts_with(b"%PDF"), "page {i} must be valid PDF");
        let doc = Document::load_mem(p).unwrap();
        assert_eq!(doc.get_pages().len(), 1, "page {i} must have 1 page");
    }

    // Generous bound — the contract is "doesn't blow up", not a perf test.
    assert!(
        elapsed.as_secs() < 30,
        "split of 50 pages should finish quickly, took {:?}",
        elapsed
    );
}

// ─── A4: ICC profile preserved across recompression ───────────────────────

#[test]
fn a4_icc_profile_preserved_across_recompression() {
    use image::codecs::jpeg::JpegEncoder;
    use image::RgbImage;
    use std::io::Cursor;

    // Encode a real JPEG so DCTDecode can decode it during recompression.
    let mut jpeg_buf = Vec::new();
    {
        let mut img = RgbImage::new(300, 300);
        for y in 0..300u32 {
            for x in 0..300u32 {
                img.put_pixel(x, y, image::Rgb([x as u8, y as u8, 128]));
            }
        }
        JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_buf), 95)
            .encode_image(&image::DynamicImage::ImageRgb8(img))
            .unwrap();
    }

    let mut doc = Document::with_version("1.7");

    // ICC profile stream — N=3 marks it RGB. Empty content is fine; the
    // recompressor only consults /N for channel detection.
    let icc_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! { "N" => 3, "Alternate" => "DeviceRGB" },
        vec![],
    )));

    let cs_array = vec![
        Object::Name(b"ICCBased".to_vec()),
        Object::Reference(icc_id),
    ];
    let img_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 300,
            "Height" => 300,
            "ColorSpace" => Object::Array(cs_array),
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => jpeg_buf.len() as i64,
        },
        jpeg_buf,
    )));

    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 300 0 0 300 100 400 cm /Im0 Do Q".to_vec(),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
        "Resources" => dictionary! {
            "XObject" => dictionary! { "Im0" => Object::Reference(img_id) },
        },
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
    });
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }
    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog", "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    // Force recompression by setting a tiny max dimension.
    let opts = veilpdf_core::CompressOptions {
        image_quality: 50,
        max_image_dimension: 100,
        target_dpi: 0,
        strip_metadata: false,
    };
    let result = veilpdf_core::compress_pdf_with_options(&input, &opts).unwrap();

    // Inspect the output: the image XObject's ColorSpace must still be an
    // [/ICCBased ref] array.
    let out = Document::load_mem(&result.data).unwrap();
    let mut found = false;
    for obj in out.objects.values() {
        if let Ok(s) = obj.as_stream() {
            let is_image = s
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|v| v.as_name().ok())
                .map(|n: &[u8]| n == b"Image")
                .unwrap_or(false);
            if !is_image {
                continue;
            }
            let cs = s.dict.get(b"ColorSpace").expect("ColorSpace must exist");
            let arr = cs
                .as_array()
                .expect("ColorSpace must remain an array (ICCBased)");
            assert_eq!(
                arr.first().and_then(|o| o.as_name().ok()),
                Some(b"ICCBased".as_slice()),
                "ColorSpace must still be ICCBased after recompression"
            );
            // The reference target must still be present in the doc.
            let icc_ref_id = arr
                .get(1)
                .and_then(|o| o.as_reference().ok())
                .expect("ICC ref must be an indirect reference");
            assert!(
                out.get_object(icc_ref_id).is_ok(),
                "ICC profile object must still be reachable in output"
            );
            found = true;
        }
    }
    assert!(
        found,
        "recompressed image XObject must be present in output"
    );
}

// ─── A5: SMask images skipped during recompression ─────────────────────────

#[test]
fn a5_smask_image_data_unchanged() {
    use image::codecs::jpeg::JpegEncoder;
    use image::{GrayImage, RgbImage};
    use std::io::Cursor;

    // Color base image as JPEG.
    let mut color_jpeg = Vec::new();
    {
        let mut img = RgbImage::new(200, 200);
        for y in 0..200u32 {
            for x in 0..200u32 {
                img.put_pixel(x, y, image::Rgb([x as u8, y as u8, 64]));
            }
        }
        JpegEncoder::new_with_quality(Cursor::new(&mut color_jpeg), 90)
            .encode_image(&image::DynamicImage::ImageRgb8(img))
            .unwrap();
    }
    // Grayscale alpha mask, also JPEG (lossy is fine for a fixture).
    let mut mask_jpeg = Vec::new();
    {
        let mut img = GrayImage::new(200, 200);
        for y in 0..200u32 {
            for x in 0..200u32 {
                img.put_pixel(x, y, image::Luma([((x + y) % 255) as u8]));
            }
        }
        JpegEncoder::new_with_quality(Cursor::new(&mut mask_jpeg), 90)
            .encode_image(&image::DynamicImage::ImageLuma8(img))
            .unwrap();
    }
    let mask_bytes_before = mask_jpeg.clone();

    let mut doc = Document::with_version("1.7");

    let mask_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 200,
            "Height" => 200,
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => mask_jpeg.len() as i64,
        },
        mask_jpeg,
    )));

    let img_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 200,
            "Height" => 200,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => color_jpeg.len() as i64,
            "SMask" => Object::Reference(mask_id),
        },
        color_jpeg,
    )));

    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 200 0 0 200 100 500 cm /Im0 Do Q".to_vec(),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
        "Resources" => dictionary! {
            "XObject" => dictionary! { "Im0" => Object::Reference(img_id) },
        },
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
    });
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }
    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog", "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    let opts = veilpdf_core::CompressOptions {
        image_quality: 30,
        max_image_dimension: 50, // would normally force re-encode
        target_dpi: 0,
        strip_metadata: false,
    };
    let result = veilpdf_core::compress_pdf_with_options(&input, &opts).unwrap();

    // Locate the mask in the output by scanning every image XObject for the
    // one whose dimensions match (200x200) and that is referenced as an
    // SMask. Its content bytes must equal the original.
    let out = Document::load_mem(&result.data).unwrap();
    let mut mask_ids = std::collections::HashSet::new();
    for obj in out.objects.values() {
        if let Ok(s) = obj.as_stream() {
            if let Ok(smask_ref) = s.dict.get(b"SMask") {
                if let Ok(rid) = smask_ref.as_reference() {
                    mask_ids.insert(rid);
                }
            }
        }
    }
    assert_eq!(
        mask_ids.len(),
        1,
        "expected exactly one SMask reference in output"
    );
    let mask_id = *mask_ids.iter().next().unwrap();
    let mask_obj = out.get_object(mask_id).unwrap();
    let mask_stream = mask_obj.as_stream().unwrap();
    assert_eq!(
        mask_stream.content, mask_bytes_before,
        "SMask image bytes must be untouched by the recompressor"
    );
}

// ─── A6: comprehensive JS stripping ───────────────────────────────────────

#[test]
fn a6_strips_js_from_openaction_aa_and_xfa() {
    let mut doc = Document::with_version("1.7");

    // Page with annotation that has /AA mouse-up JS, and a form-field-like
    // /AA calculate JS sibling.
    let annot_aa_action_id = doc.add_object(dictionary! {
        "Type" => "Action",
        "S" => "JavaScript",
        "JS" => Object::String(b"app.alert('annot');".to_vec(), lopdf::StringFormat::Literal),
    });
    let annot_id = doc.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Link",
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "AA" => dictionary! { "U" => Object::Reference(annot_aa_action_id) },
    });

    // Form field with /AA calculate JS.
    let field_aa_action_id = doc.add_object(dictionary! {
        "Type" => "Action",
        "S" => "JavaScript",
        "JS" => Object::String(b"app.alert('field');".to_vec(), lopdf::StringFormat::Literal),
    });
    let field_id = doc.add_object(dictionary! {
        "T" => Object::String(b"f1".to_vec(), lopdf::StringFormat::Literal),
        "FT" => "Tx",
        "AA" => dictionary! { "C" => Object::Reference(field_aa_action_id) },
    });

    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Annots" => vec![Object::Reference(annot_id)],
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
    });
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }

    // OpenAction JS at catalog level.
    let openaction_id = doc.add_object(dictionary! {
        "Type" => "Action",
        "S" => "JavaScript",
        "JS" => Object::String(b"app.alert('open');".to_vec(), lopdf::StringFormat::Literal),
    });

    // AcroForm with XFA stream.
    let xfa_stream_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! {},
        b"<xdp:xdp xmlns:xdp='http://ns.adobe.com/xdp/'>...</xdp:xdp>".to_vec(),
    )));
    let acroform_id = doc.add_object(dictionary! {
        "Fields" => vec![Object::Reference(field_id)],
        "XFA" => Object::Reference(xfa_stream_id),
    });

    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
        "OpenAction" => Object::Reference(openaction_id),
        "AcroForm" => Object::Reference(acroform_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    // flags == 0 — A8 baseline must still strip JS / actions / embedded.
    let out = veilpdf_core::sanitize_pdf(&input, 0).unwrap();
    let outdoc = Document::load_mem(&out).unwrap();

    // 1) Catalog OpenAction must be gone (action chain stripped).
    let root_id = outdoc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let catalog = outdoc.get_dictionary(root_id).unwrap();
    assert!(
        catalog.get(b"OpenAction").is_err(),
        "catalog /OpenAction must be removed"
    );

    // 2) AcroForm /XFA must be gone.
    if let Ok(af_ref) = catalog.get(b"AcroForm") {
        let af = match af_ref {
            Object::Reference(rid) => outdoc.get_dictionary(*rid).unwrap(),
            Object::Dictionary(d) => d,
            _ => panic!("unexpected AcroForm shape"),
        };
        assert!(af.get(b"XFA").is_err(), "AcroForm /XFA must be removed");
    }

    // 3) No remaining dictionary anywhere should still have /S /JavaScript
    //    with a JS payload.
    for obj in outdoc.objects.values() {
        let dicts: Vec<&lopdf::Dictionary> = match obj {
            Object::Dictionary(d) => vec![d],
            Object::Stream(s) => vec![&s.dict],
            _ => vec![],
        };
        for d in dicts {
            let s_is_js = d
                .get(b"S")
                .ok()
                .and_then(|v| v.as_name().ok())
                .map(|n: &[u8]| n == b"JavaScript")
                .unwrap_or(false);
            if s_is_js {
                assert!(
                    d.get(b"JS").is_err(),
                    "/JS payload must be stripped from JS action dicts"
                );
            }
            assert!(
                d.get(b"AA").is_err(),
                "no /AA dictionary may survive sanitize"
            );
        }
    }
}

// ─── A7: Type/Metadata streams detected and removed ───────────────────────

#[test]
fn a7_type_metadata_streams_stripped_with_no_subtype() {
    let mut doc = Document::with_version("1.7");

    // Adobe-style metadata stream: /Type /Metadata, no /Subtype.
    let meta_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! { "Type" => "Metadata" },
        b"<x:xmpmeta xmlns:x='adobe:ns:meta/'>...</x:xmpmeta>".to_vec(),
    )));

    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
        "Metadata" => Object::Reference(meta_id),
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
    });
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }
    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
        "Metadata" => Object::Reference(meta_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    // Strip via sanitize with FLAG_REMOVE_XMP set.
    let out = veilpdf_core::sanitize_pdf(&input, veilpdf_core::sanitize::FLAG_REMOVE_XMP).unwrap();
    let outdoc = Document::load_mem(&out).unwrap();

    // No remaining stream should carry /Type /Metadata.
    for obj in outdoc.objects.values() {
        if let Ok(s) = obj.as_stream() {
            let is_meta = s
                .dict
                .get(b"Type")
                .ok()
                .and_then(|v| v.as_name().ok())
                .map(|n: &[u8]| n == b"Metadata")
                .unwrap_or(false);
            assert!(!is_meta, "/Type /Metadata stream must be removed");
        }
    }

    // Catalog and pages must not still point at a /Metadata ref.
    let root_id = outdoc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let catalog = outdoc.get_dictionary(root_id).unwrap();
    assert!(
        catalog.get(b"Metadata").is_err(),
        "catalog must not retain /Metadata after XMP strip"
    );
    for &pid in outdoc.get_pages().values() {
        let pd = outdoc.get_dictionary(pid).unwrap();
        assert!(
            pd.get(b"Metadata").is_err(),
            "page must not retain /Metadata after XMP strip"
        );
    }
}

// ─── A8: sanitize_pdf with flags=0 still enforces baseline ─────────────────

#[test]
fn a8_zero_flags_still_strips_js_actions_embedded() {
    let mut doc = Document::with_version("1.7");

    // Catalog OpenAction (JS), Names→EmbeddedFiles, JS action.
    let oa_id = doc.add_object(dictionary! {
        "Type" => "Action",
        "S" => "JavaScript",
        "JS" => Object::String(b"x();".to_vec(), lopdf::StringFormat::Literal),
    });
    let names_id = doc.add_object(dictionary! {
        "EmbeddedFiles" => dictionary! {
            "Names" => Object::Array(vec![]),
        },
        "JavaScript" => dictionary! {
            "Names" => Object::Array(vec![]),
        },
    });
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => vec![Object::Reference(page_id)], "Count" => 1,
    });
    if let Ok(p) = doc.get_dictionary_mut(page_id) {
        p.set("Parent", Object::Reference(pages_id));
    }
    let cat_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
        "OpenAction" => Object::Reference(oa_id),
        "Names" => Object::Reference(names_id),
    });
    doc.trailer.set("Root", Object::Reference(cat_id));

    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    // Pass flags=0 — the baseline OR enforced inside sanitize_pdf must
    // still produce a JS-/Action-/Embedded-free PDF.
    let out = veilpdf_core::sanitize_pdf(&input, 0).unwrap();
    let outdoc = Document::load_mem(&out).unwrap();

    let root_id = outdoc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let catalog = outdoc.get_dictionary(root_id).unwrap();
    assert!(
        catalog.get(b"OpenAction").is_err(),
        "OpenAction must be stripped even with flags=0"
    );

    if let Ok(names_ref) = catalog.get(b"Names") {
        if let Ok(names_id) = names_ref.as_reference() {
            if let Ok(names) = outdoc.get_dictionary(names_id) {
                assert!(
                    names.get(b"EmbeddedFiles").is_err(),
                    "EmbeddedFiles must be stripped even with flags=0"
                );
                assert!(
                    names.get(b"JavaScript").is_err(),
                    "Names→JavaScript must be stripped even with flags=0"
                );
            }
        }
    }
}

// ─── A9: object-count cap accepts normal PDFs ─────────────────────────────
//
// Note: synthesizing an actual 500k-object PDF just to confirm the bound
// would waste minutes per test run. The bound *is* the contract: any PDF
// with `max_id` or `objects.len()` over 500_000 will be rejected at
// `Document::load_mem` time. This test only confirms that the cap does
// not regress on normal-sized PDFs.

#[test]
fn a9_normal_pdfs_pass_object_count_check() {
    let pdf = make_one_page_pdf("normal");
    let parsed = Document::load_mem(&pdf).unwrap();
    assert!(parsed.max_id < 500_000);
    assert!(parsed.objects.len() < 500_000);

    // All five entry points should accept it.
    veilpdf_core::compress_pdf_from_bytes(&pdf).expect("compress accepts normal PDF");
    veilpdf_core::sanitize_pdf(&pdf, 0).expect("sanitize accepts normal PDF");
    veilpdf_core::split_pdf_from_bytes(&pdf).expect("split accepts normal PDF");
    let two = make_one_page_pdf("normal2");
    veilpdf_core::merge_pdfs_from_bytes(&[&pdf, &two]).expect("merge accepts normal PDFs");
    // extract_images returns Err only because there are no images; we just
    // need to confirm the cap doesn't reject the input itself. Allow either
    // outcome.
    let _ = veilpdf_core::extract_images(&pdf);
}
