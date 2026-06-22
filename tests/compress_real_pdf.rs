use lopdf::Document;

/// Diagnose a real-world PDF: what images does it contain and why might compression skip them?
/// Run with: `cargo test diagnose_real_pdf -- --ignored`
#[test]
#[ignore]
fn diagnose_real_pdf() {
    let path = std::env::var("TEST_PDF").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap();
        format!("{}/Downloads/10840.pdf", home)
    });

    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Could not read {}: {} — skipping test", path, e);
            return;
        }
    };

    eprintln!(
        "Loaded: {} ({:.1} MB)",
        path,
        data.len() as f64 / 1024.0 / 1024.0
    );

    let doc = Document::load_mem(&data).unwrap();
    eprintln!("Pages: {}", doc.get_pages().len());

    let mut image_count = 0;
    let mut would_process = 0;
    let mut skip_reasons: Vec<String> = Vec::new();

    for (&id, obj) in &doc.objects {
        let stream = match obj.as_stream() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let dict = &stream.dict;

        // Check if this is an image
        let is_image = dict
            .get(b"Subtype")
            .ok()
            .and_then(|v| v.as_name().ok())
            .map(|n| n == b"Image")
            .unwrap_or(false);
        if !is_image {
            continue;
        }

        image_count += 1;
        let w = dict
            .get(b"Width")
            .ok()
            .and_then(|v| v.as_i64().ok())
            .unwrap_or(-1);
        let h = dict
            .get(b"Height")
            .ok()
            .and_then(|v| v.as_i64().ok())
            .unwrap_or(-1);
        let bpc = dict
            .get(b"BitsPerComponent")
            .ok()
            .and_then(|v| v.as_i64().ok())
            .unwrap_or(-1);
        let content_len = stream.content.len();
        let has_smask = dict.has(b"SMask");
        let is_mask = dict
            .get(b"ImageMask")
            .ok()
            .and_then(|v| v.as_bool().ok())
            .unwrap_or(false);

        // Raw colorspace and filter for debugging
        let cs_debug = dict
            .get(b"ColorSpace")
            .ok()
            .map(|v| debug_object(&doc, v))
            .unwrap_or("NONE".into());
        let filter_debug = dict
            .get(b"Filter")
            .ok()
            .map(|v| debug_object(&doc, v))
            .unwrap_or("NONE".into());

        eprintln!(
            "\nImage {:?}: {}x{} bpc={} content={}B smask={} mask={}",
            id, w, h, bpc, content_len, has_smask, is_mask
        );
        eprintln!("  ColorSpace: {}", cs_debug);
        eprintln!("  Filter: {}", filter_debug);

        // Simulate our skip logic
        if is_mask {
            let r = format!("{:?}: ImageMask", id);
            eprintln!("  -> SKIP: {}", r);
            skip_reasons.push(r);
            continue;
        }
        if w == 0 || h == 0 || (bpc != 8 && bpc != -1) {
            let r = format!("{:?}: bad dimensions/bpc ({}x{} bpc={})", id, w, h, bpc);
            eprintln!("  -> SKIP: {}", r);
            skip_reasons.push(r);
            continue;
        }
        if has_smask {
            let r = format!("{:?}: has SMask", id);
            eprintln!("  -> SKIP: {}", r);
            skip_reasons.push(r);
            continue;
        }

        // Resolve colorspace
        let cs_obj = dict.get(b"ColorSpace").ok();
        let channels = cs_obj.and_then(|cs| resolve_channels(&doc, cs));

        match channels {
            Some(ch) => {
                eprintln!("  -> channels={}, WOULD PROCESS", ch);
                would_process += 1;
            }
            None => {
                let r = format!("{:?}: unsupported colorspace: {}", id, cs_debug);
                eprintln!("  -> SKIP: {}", r);
                skip_reasons.push(r);
            }
        }
    }

    eprintln!("\n=== SUMMARY ===");
    eprintln!("Total images: {}", image_count);
    eprintln!("Would process: {}", would_process);
    eprintln!("Skipped: {}", skip_reasons.len());
    for r in &skip_reasons {
        eprintln!("  - {}", r);
    }

    // Now actually run compression
    eprintln!("\n=== RUNNING COMPRESSION ===");
    let options = veilpdf_core::compress::CompressOptions {
        image_quality: 40,
        max_image_dimension: 1024,
        target_dpi: 0,
        strip_metadata: true,
    };
    let result = veilpdf_core::compress::compress_pdf_with_options(&data, &options).unwrap();
    eprintln!(
        "Result: {:.1} MB -> {:.1} MB ({:.1}% reduction)",
        result.input_size as f64 / 1024.0 / 1024.0,
        result.output_size as f64 / 1024.0 / 1024.0,
        result.reduction_percent
    );

    if image_count > 0 && would_process > 0 {
        assert!(
            result.output_size < result.input_size,
            "Should compress: {} -> {}",
            result.input_size,
            result.output_size
        );
    }
}

fn resolve_channels(doc: &Document, cs: &lopdf::Object) -> Option<u32> {
    // Direct name
    if let Ok(name) = cs.as_name() {
        return match name {
            b"DeviceRGB" | b"CalRGB" => Some(3),
            b"DeviceGray" | b"CalGray" => Some(1),
            _ => {
                eprintln!(
                    "  [channels] unknown name: {:?}",
                    String::from_utf8_lossy(name)
                );
                None
            }
        };
    }
    // Array
    if let Ok(arr) = cs.as_array() {
        if let Some(first) = arr.first() {
            if let Ok(name) = first.as_name() {
                match name {
                    b"ICCBased" => {
                        if let Some(icc_ref) = arr.get(1) {
                            if let Ok(icc_id) = icc_ref.as_reference() {
                                if let Ok(icc_obj) = doc.get_object(icc_id) {
                                    if let Ok(icc_stream) = icc_obj.as_stream() {
                                        let n = icc_stream
                                            .dict
                                            .get(b"N")
                                            .ok()
                                            .and_then(|v| v.as_i64().ok())
                                            .unwrap_or(0);
                                        eprintln!("  [channels] ICCBased N={}", n);
                                        if n == 3 {
                                            return Some(3);
                                        }
                                        if n == 1 {
                                            return Some(1);
                                        }
                                        return None;
                                    }
                                }
                            }
                            // Maybe the reference is inlined
                            eprintln!("  [channels] ICCBased ref failed: {:?}", icc_ref);
                        }
                        return None;
                    }
                    b"CalRGB" => return Some(3),
                    b"CalGray" => return Some(1),
                    b"Indexed" => {
                        // [/Indexed baseCS maxVal lookupData]
                        if let Some(base) = arr.get(1) {
                            eprintln!("  [channels] Indexed, resolving base...");
                            return resolve_channels(doc, base);
                        }
                        return None;
                    }
                    _ => {
                        eprintln!(
                            "  [channels] unknown array CS: {:?}",
                            String::from_utf8_lossy(name)
                        );
                        return None;
                    }
                }
            }
        }
    }
    // Indirect reference — resolve it
    if let Ok(ref_id) = cs.as_reference() {
        eprintln!(
            "  [channels] ColorSpace is indirect ref {:?}, resolving...",
            ref_id
        );
        if let Ok(resolved) = doc.get_object(ref_id) {
            return resolve_channels(doc, resolved);
        }
    }
    eprintln!("  [channels] unhandled CS type: {:?}", cs);
    None
}

fn debug_object(doc: &Document, obj: &lopdf::Object) -> String {
    match obj {
        lopdf::Object::Name(n) => format!("/{}", String::from_utf8_lossy(n)),
        lopdf::Object::Array(arr) => {
            let items: Vec<String> = arr.iter().map(|o| debug_object(doc, o)).collect();
            format!("[{}]", items.join(", "))
        }
        lopdf::Object::Reference(id) => {
            let resolved = doc
                .get_object(*id)
                .map(|o| debug_object(doc, o))
                .unwrap_or("???".into());
            format!("ref{:?}->{}", id, resolved)
        }
        lopdf::Object::Integer(i) => format!("{}", i),
        lopdf::Object::Real(f) => format!("{}", f),
        lopdf::Object::Boolean(b) => format!("{}", b),
        lopdf::Object::String(s, _) => format!("\"{}\"", String::from_utf8_lossy(s)),
        lopdf::Object::Stream(s) => format!("<stream len={}>", s.content.len()),
        lopdf::Object::Dictionary(_) => "<dict>".into(),
        lopdf::Object::Null => "null".into(),
    }
}
