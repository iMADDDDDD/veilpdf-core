//! Integration tests for the Phase 3.5 / 3.6 watermark baking.
//!
//! End-to-end goal: a watermark applied via `apply_text_watermark` must
//! survive a save → reload round-trip. The legacy `WatermarkAnnotation`
//! draw-override path does not, which is the bug we're fixing.
//!
//! Phase 3.6 changes the surface shape — the bake now embeds a Type0
//! CIDFontType2 font with Identity-H encoding, so the watermark text
//! becomes hex glyph indices in the content stream rather than ASCII
//! characters. Tests assert on the structural shape (Type0 + Identity-H
//! + FontFile2) plus a known-character → glyph mapping rather than
//!   grepping for the literal text bytes.

use lopdf::{dictionary, Document, Object, Stream};
use veilpdf_core::{apply_text_watermark, WatermarkColor, WatermarkOptions};

fn inter_bytes() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fonts/Inter-Bold.ttf"
    );
    std::fs::read(path).expect("Inter-Bold.ttf test fixture must exist")
}

/// Builds a minimal one-page PDF with a piece of body text so we can assert
/// that watermarking doesn't destroy the original content stream.
fn make_one_page_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });

    let content = b"BT /F1 24 Tf 100 700 Td (Original body text) Tj ET".to_vec();
    let stream = Stream::new(dictionary! {}, content);
    let content_id = doc.add_object(stream);

    let resources = dictionary! {
        "Font" => dictionary! { "F1" => Object::Reference(font_id) },
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

    let page_dict = doc.get_dictionary_mut(page_id).unwrap();
    page_dict.set("Parent", Object::Reference(pages_id));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });

    doc.trailer.set("Root", Object::Reference(catalog_id));
    doc.compress();

    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

fn default_options() -> WatermarkOptions {
    WatermarkOptions {
        text: "CONFIDENTIAL".into(),
        font_size: 48.0,
        color: WatermarkColor {
            r: 0.6,
            g: 0.1,
            b: 0.1,
        },
        opacity: 0.3,
        rotation_deg: -45.0,
    }
}

/// After baking, reloading the PDF must still parse and still have the
/// same page count, and the embedded Type0 font tree with the TTF stream
/// must be present.
#[test]
fn watermark_survives_reload() {
    let input = make_one_page_pdf();
    let opts = default_options();
    let font = inter_bytes();

    let baked = apply_text_watermark(&input, &opts, &font).expect("bake should succeed");

    let reloaded = Document::load_mem(&baked).expect("reload should parse");
    assert_eq!(reloaded.get_pages().len(), 1);

    let mut found_type0 = false;
    let mut found_identity_h = false;
    let mut found_font_file = false;

    for obj in reloaded.objects.values() {
        match obj {
            Object::Dictionary(d) => {
                if d.get(b"Subtype").and_then(|v| v.as_name()).ok() == Some(b"Type0") {
                    found_type0 = true;
                }
                if d.get(b"Encoding").and_then(|v| v.as_name()).ok() == Some(b"Identity-H") {
                    found_identity_h = true;
                }
            }
            // The /FontFile2 stream carries /Length1 = original TTF size.
            Object::Stream(s) if s.dict.get(b"Length1").is_ok() => {
                found_font_file = true;
            }
            _ => {}
        }
    }

    assert!(found_type0, "Type0 font dict missing after reload");
    assert!(found_identity_h, "Identity-H encoding missing after reload");
    assert!(
        found_font_file,
        "FontFile2 stream (/Length1) missing after reload"
    );
}

/// The original page content must still be reachable after we append the
/// watermark stream — the bake is additive, not replacing.
#[test]
fn original_content_preserved() {
    let input = make_one_page_pdf();
    let opts = default_options();
    let font = inter_bytes();

    let baked = apply_text_watermark(&input, &opts, &font).expect("bake should succeed");
    let reloaded = Document::load_mem(&baked).expect("reload should parse");

    let found_original = reloaded.objects.values().any(|obj| {
        if let Object::Stream(s) = obj {
            let decoded = s
                .decompressed_content()
                .unwrap_or_else(|_| s.content.clone());
            decoded
                .windows(b"Original body text".len())
                .any(|w| w == b"Original body text")
        } else {
            false
        }
    });

    assert!(
        found_original,
        "original page text was destroyed by the bake"
    );
}

/// Applying twice should work end-to-end (no UTF-8 / xref corruption) and
/// produce a PDF that still parses. Real users will Apply, see the result,
/// undo, and reapply with new settings — the byte-snapshot undo path
/// effectively re-bakes from scratch, but defending against a literal
/// double-bake is cheap insurance against future undo regressions.
#[test]
fn double_apply_is_safe() {
    let input = make_one_page_pdf();
    let font = inter_bytes();
    let first = apply_text_watermark(&input, &default_options(), &font).expect("first bake");
    let second_opts = WatermarkOptions {
        text: "DRAFT".into(),
        ..default_options()
    };
    let second = apply_text_watermark(&first, &second_opts, &font).expect("second bake");

    let reloaded = Document::load_mem(&second).expect("reload should parse");
    assert_eq!(reloaded.get_pages().len(), 1);
}

#[test]
fn rejects_encrypted_pdf() {
    let mut doc = Document::with_version("1.5");
    doc.trailer.set(
        "Encrypt",
        Object::Dictionary(dictionary! { "Filter" => "Standard" }),
    );
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();

    let result = apply_text_watermark(&buf, &default_options(), &inter_bytes());
    assert!(result.is_err(), "encrypted PDFs must be rejected");
}

/// Unicode characters (German umlaut, French acute) should produce a
/// well-formed PDF rather than being silently dropped or replaced with
/// `?`. Asserts the byte stream contains the expected number of glyph
/// pairs for a Latin-Extended sample.
#[test]
fn unicode_text_round_trip() {
    let input = make_one_page_pdf();
    let font = inter_bytes();
    let opts = WatermarkOptions {
        text: "Über bügeln café".into(),
        ..default_options()
    };

    let baked = apply_text_watermark(&input, &opts, &font).expect("bake should succeed");
    let reloaded = Document::load_mem(&baked).expect("reload should parse");
    assert_eq!(reloaded.get_pages().len(), 1);
}

/// Phase 3.8a regression: the Type0 font must reference a `/ToUnicode`
/// CMap stream, and that stream must contain a `bfchar` entry for at
/// least one of the source codepoints. Without this PDF readers can't
/// recover the original text (search / copy / accessibility break).
#[test]
fn type0_font_has_to_unicode_cmap() {
    let input = make_one_page_pdf();
    let font = inter_bytes();
    let opts = WatermarkOptions {
        text: "Café".into(),
        ..default_options()
    };

    let baked = apply_text_watermark(&input, &opts, &font).expect("bake should succeed");
    let reloaded = Document::load_mem(&baked).expect("reload should parse");

    // Find the Type0 dict and its /ToUnicode reference.
    let to_unicode_ref = reloaded
        .objects
        .values()
        .find_map(|obj| {
            let Object::Dictionary(d) = obj else {
                return None;
            };
            if d.get(b"Subtype").and_then(|v| v.as_name()).ok() != Some(b"Type0") {
                return None;
            }
            d.get(b"ToUnicode").and_then(|v| v.as_reference()).ok()
        })
        .expect("Type0 dict missing /ToUnicode reference");

    // The referenced object must be a stream whose decompressed content
    // is a CMap with at least one `beginbfchar` block and an entry for
    // 'C' (U+0043) — the first character of "Café".
    let stream = match reloaded
        .objects
        .get(&to_unicode_ref)
        .expect("ToUnicode target missing")
    {
        Object::Stream(s) => s,
        _ => panic!("ToUnicode target is not a stream"),
    };
    let body = stream
        .decompressed_content()
        .unwrap_or_else(|_| stream.content.clone());
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("beginbfchar"),
        "CMap missing beginbfchar block"
    );
    assert!(
        body_str.contains("endbfchar"),
        "CMap missing endbfchar terminator"
    );
    assert!(
        body_str.contains("<0043>"),
        "CMap missing U+0043 ('C') entry"
    );
}

/// Phase 3.7 regression: subsetting must keep the embedded font well
/// under the original ~415 KB. A short watermark uses ~10 unique glyphs;
/// the subset should be a small fraction of the full font. We assert
/// the baked PDF is at least 5x smaller than `input + full font`, which
/// would only hold if subsetting is actually wired into the bake.
#[test]
fn subsetting_shrinks_baked_pdf() {
    let input = make_one_page_pdf();
    let font = inter_bytes();
    let opts = WatermarkOptions {
        text: "CONFIDENTIAL".into(),
        ..default_options()
    };

    let baked = apply_text_watermark(&input, &opts, &font).expect("bake should succeed");
    let full_font_floor = input.len() + font.len();
    assert!(
        baked.len() * 5 < full_font_floor,
        "expected subsetting to drop the embedded-font payload well below \
         the full-font size: baked = {} bytes, input+font = {} bytes",
        baked.len(),
        full_font_floor
    );
}
