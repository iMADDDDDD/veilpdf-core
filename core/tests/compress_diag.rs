use lopdf::{dictionary, Document};

/// Diagnostic test: create a PDF with an embedded JPEG image, then compress it.
#[test]
fn diagnose_image_compression() {
    // Create a real JPEG image using the image crate
    let mut jpeg_buf = Vec::new();
    {
        use image::codecs::jpeg::JpegEncoder;
        use image::RgbImage;
        use std::io::Cursor;

        // Create a 200x200 gradient image (uncompressed = 120KB, JPEG ≈ a few KB)
        let mut img = RgbImage::new(200, 200);
        for y in 0..200 {
            for x in 0..200 {
                img.put_pixel(x, y, image::Rgb([x as u8, y as u8, 128]));
            }
        }
        let mut encoder = JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_buf), 95);
        encoder.encode_image(&image::DynamicImage::ImageRgb8(img)).unwrap();
    }
    eprintln!("Created test JPEG: {} bytes", jpeg_buf.len());

    // Build a PDF with this JPEG embedded as an XObject
    let mut doc = lopdf::Document::with_version("1.5");

    let img_stream = lopdf::Stream::new(
        lopdf::dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 200,
            "Height" => 200,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => jpeg_buf.len() as i64,
        },
        jpeg_buf.clone(),
    );
    let img_id = doc.add_object(lopdf::Object::Stream(img_stream));

    let content = b"q 200 0 0 200 100 500 cm /Im0 Do Q".to_vec();
    let content_stream = lopdf::Stream::new(lopdf::dictionary! {}, content);
    let content_id = doc.add_object(content_stream);

    let resources = lopdf::dictionary! {
        "XObject" => lopdf::dictionary! {
            "Im0" => lopdf::Object::Reference(img_id),
        },
    };

    let page_id = doc.add_object(lopdf::dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => lopdf::Object::Reference(content_id),
        "Resources" => resources,
    });

    let pages_id = doc.add_object(lopdf::dictionary! {
        "Type" => "Pages",
        "Kids" => vec![lopdf::Object::Reference(page_id)],
        "Count" => 1,
    });

    if let Ok(page) = doc.get_dictionary_mut(page_id) {
        page.set("Parent", lopdf::Object::Reference(pages_id));
    }

    let catalog_id = doc.add_object(lopdf::dictionary! {
        "Type" => "Catalog",
        "Pages" => lopdf::Object::Reference(pages_id),
    });
    doc.trailer.set("Root", lopdf::Object::Reference(catalog_id));

    let mut pdf_bytes = Vec::new();
    doc.save_to(&mut pdf_bytes).unwrap();
    eprintln!("Created test PDF: {} bytes", pdf_bytes.len());

    // Now diagnose: what does find_image_xobjects see?
    let doc2 = Document::load_mem(&pdf_bytes).unwrap();
    let mut found_images = 0;
    for (&id, obj) in &doc2.objects {
        if let Ok(stream) = obj.as_stream() {
            let dict = &stream.dict;
            let subtype = dict.get(b"Subtype").ok().and_then(|v| v.as_name().ok());
            if subtype == Some(b"Image") {
                found_images += 1;
                let w = dict.get(b"Width").ok().and_then(|v| v.as_i64().ok()).unwrap_or(-1);
                let h = dict.get(b"Height").ok().and_then(|v| v.as_i64().ok()).unwrap_or(-1);
                let cs = dict.get(b"ColorSpace").ok().map(|v| format!("{:?}", v)).unwrap_or("NONE".into());
                let filter = dict.get(b"Filter").ok().map(|v| format!("{:?}", v)).unwrap_or("NONE".into());
                let content_len = stream.content.len();
                eprintln!("Image {:?}: {}x{}, cs={}, filter={}, content_bytes={}", id, w, h, cs, filter, content_len);
            }
        }
    }
    eprintln!("Total images found: {}", found_images);
    assert!(found_images > 0, "Should find at least 1 image");

    // Run compress with LOW preset
    let options = veilpdf_core::compress::CompressOptions {
        image_quality: 40,
        max_image_dimension: 100, // Force resize from 200 to 100
        strip_metadata: true,
    };

    let result = veilpdf_core::compress::compress_pdf_with_options(&pdf_bytes, &options).unwrap();
    eprintln!(
        "Compression: {} -> {} bytes ({:.1}% reduction)",
        result.input_size, result.output_size, result.reduction_percent
    );

    assert!(
        result.output_size < result.input_size,
        "Output ({}) should be smaller than input ({})",
        result.output_size,
        result.input_size
    );

    // Verify output is valid
    let doc3 = Document::load_mem(&result.data).unwrap();
    assert_eq!(doc3.get_pages().len(), 1);
}

/// Test with ICCBased color space (what real PDFs use)
#[test]
fn diagnose_iccbased_compression() {
    use image::codecs::jpeg::JpegEncoder;
    use image::RgbImage;
    use std::io::Cursor;

    // Create JPEG
    let mut jpeg_buf = Vec::new();
    {
        let mut img = RgbImage::new(300, 300);
        for y in 0..300 {
            for x in 0..300 {
                img.put_pixel(x, y, image::Rgb([(x % 256) as u8, (y % 256) as u8, 200]));
            }
        }
        let mut encoder = JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_buf), 95);
        encoder.encode_image(&image::DynamicImage::ImageRgb8(img)).unwrap();
    }

    let mut doc = lopdf::Document::with_version("1.7");

    // Create an ICCBased color space stream (3 components = RGB)
    let icc_stream = lopdf::Stream::new(
        lopdf::dictionary! {
            "N" => 3,
            "Alternate" => "DeviceRGB",
        },
        vec![], // Empty ICC profile — the /N is what matters for channel count
    );
    let icc_id = doc.add_object(lopdf::Object::Stream(icc_stream));

    // Image with ICCBased color space (array form)
    let cs_array = vec![
        lopdf::Object::Name(b"ICCBased".to_vec()),
        lopdf::Object::Reference(icc_id),
    ];

    let img_stream = lopdf::Stream::new(
        lopdf::dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 300,
            "Height" => 300,
            "ColorSpace" => lopdf::Object::Array(cs_array),
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
            "Length" => jpeg_buf.len() as i64,
        },
        jpeg_buf,
    );
    let img_id = doc.add_object(lopdf::Object::Stream(img_stream));

    let content = b"q 300 0 0 300 100 400 cm /Im0 Do Q".to_vec();
    let content_id = doc.add_object(lopdf::Stream::new(lopdf::dictionary! {}, content));

    let page_id = doc.add_object(lopdf::dictionary! {
        "Type" => "Page",
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => lopdf::Object::Reference(content_id),
        "Resources" => lopdf::dictionary! {
            "XObject" => lopdf::dictionary! {
                "Im0" => lopdf::Object::Reference(img_id),
            },
        },
    });

    let pages_id = doc.add_object(lopdf::dictionary! {
        "Type" => "Pages",
        "Kids" => vec![lopdf::Object::Reference(page_id)],
        "Count" => 1,
    });
    if let Ok(page) = doc.get_dictionary_mut(page_id) {
        page.set("Parent", lopdf::Object::Reference(pages_id));
    }
    let catalog_id = doc.add_object(lopdf::dictionary! {
        "Type" => "Catalog",
        "Pages" => lopdf::Object::Reference(pages_id),
    });
    doc.trailer.set("Root", lopdf::Object::Reference(catalog_id));

    let mut pdf_bytes = Vec::new();
    doc.save_to(&mut pdf_bytes).unwrap();
    eprintln!("ICCBased PDF: {} bytes", pdf_bytes.len());

    let options = veilpdf_core::compress::CompressOptions {
        image_quality: 40,
        max_image_dimension: 100,
        strip_metadata: true,
    };

    let result = veilpdf_core::compress::compress_pdf_with_options(&pdf_bytes, &options).unwrap();
    eprintln!(
        "ICCBased compression: {} -> {} ({:.1}%)",
        result.input_size, result.output_size, result.reduction_percent
    );

    assert!(
        result.output_size < result.input_size,
        "ICCBased output ({}) should be smaller than input ({})",
        result.output_size,
        result.input_size
    );
}
