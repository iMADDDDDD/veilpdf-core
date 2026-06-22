use crate::limits::check_object_count;
use crate::{Result, VeilError};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, RgbImage};
use lopdf::content::Content;
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::path::Path;

/// Max decompressed size to prevent zip bomb OOM (256 MB).
const MAX_DECOMPRESSED_SIZE: u64 = 256 * 1024 * 1024;

/// Max image pixel count to prevent OOM on decode (100 megapixels).
pub const MAX_DECODE_PIXELS: u64 = 100_000_000;

/// Decompress FlateDecode data with bounded output size to prevent zip bombs.
/// Tries zlib first, then raw deflate on failure.
pub fn decompress_bounded(data: &[u8]) -> Option<Vec<u8>> {
    // Try zlib (RFC 1950) first — PDF spec requires this
    let mut reader = flate2::read::ZlibDecoder::new(data).take(MAX_DECOMPRESSED_SIZE + 1);
    let mut inflated = Vec::new();
    if reader.read_to_end(&mut inflated).is_ok() {
        if inflated.len() as u64 > MAX_DECOMPRESSED_SIZE {
            return None; // Exceeded limit — likely a zip bomb
        }
        return Some(inflated);
    }

    // Fallback: try raw deflate (RFC 1951) — some generators omit zlib header
    inflated.clear();
    let mut reader = flate2::read::DeflateDecoder::new(data).take(MAX_DECOMPRESSED_SIZE + 1);
    if reader.read_to_end(&mut inflated).is_ok() {
        if inflated.len() as u64 > MAX_DECOMPRESSED_SIZE {
            return None;
        }
        return Some(inflated);
    }

    None
}

/// Compression options controlling image quality, dimensions, and metadata.
pub struct CompressOptions {
    /// JPEG quality for recompressed images (1–100).
    pub image_quality: u8,
    /// Maximum pixels on the longest edge; larger images are downscaled.
    pub max_image_dimension: u32,
    /// Target effective DPI for downsampling. `0` disables DPI-aware
    /// downsampling (only the max-edge cap applies). When non-zero, each
    /// image XObject's *on-page* drawn size is computed from the page
    /// content-stream's CTM at the `Do` operator and any image whose
    /// effective DPI exceeds the target is shrunk to match.
    ///
    /// Typical values:
    /// - 96 / 120 — screen / aggressive (Low)
    /// - 150 — Ghostscript /ebook sweet spot (Medium)
    /// - 200 — light touch (High) — usually combined with `0` so only
    ///   the quality drop applies.
    pub target_dpi: u32,
    /// Remove document metadata (Info dict, XMP, thumbnails).
    pub strip_metadata: bool,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            image_quality: 75,
            max_image_dimension: 2048,
            target_dpi: 0,
            strip_metadata: false,
        }
    }
}

/// Compress a PDF file using lopdf's native stream compression.
///
/// Returns an error for encrypted PDFs.
pub fn compress_pdf<P: AsRef<Path>>(path: P) -> Result<CompressResult> {
    let data = std::fs::read(path.as_ref())?;
    compress_pdf_from_bytes(&data)
}

/// Compress a PDF from bytes (stream compression only, no image recompression).
pub fn compress_pdf_from_bytes(data: &[u8]) -> Result<CompressResult> {
    let input_size = data.len() as u64;
    let mut doc = Document::load_mem(data)?;
    check_object_count(&doc)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    doc.compress();
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    let output_size = buf.len() as u64;

    let reduction = if input_size > 0 {
        ((input_size as f64 - output_size as f64) / input_size as f64) * 100.0
    } else {
        0.0
    };

    Ok(CompressResult {
        data: buf,
        input_size,
        output_size,
        reduction_percent: reduction,
    })
}

/// Compress a PDF with full image recompression, resizing, and optional metadata stripping.
pub fn compress_pdf_with_options(data: &[u8], options: &CompressOptions) -> Result<CompressResult> {
    let input_size = data.len() as u64;
    let mut doc = Document::load_mem(data)?;
    check_object_count(&doc)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    // Phase 1: Recompress images. A5: skip any image that is referenced as
    // an /SMask or /Mask elsewhere in the doc — re-encoding those as JPEG
    // strips alpha and turns transparent regions black.
    let mask_ids = collect_mask_referenced_ids(&doc);
    let image_ids: Vec<ObjectId> = find_image_xobjects(&doc)
        .into_iter()
        .filter(|id| !mask_ids.contains(id))
        .collect();
    // Per-XObject effective DPI from page content streams (CTM scale
    // at each `Do` operator). Always computed — the parse is cheap
    // (one Content::decode per page) and load-bearing for orphan-image
    // detection regardless of preset. DPI-aware *downsampling* is
    // still gated on `target_dpi > 0` further down.
    let dpi_map: HashMap<ObjectId, f32> = compute_effective_dpi_map(&doc);
    // Drop orphan image XObjects: any image that's not placed via a
    // `Do` on any page or Form, and isn't acting as an /SMask, is not
    // rendered. The most common source is Adobe's `/CompositeImage`
    // editing-metadata sidecar (Illustrator / InDesign "save for
    // round-trip" preserves a high-res CMYK source alongside the
    // page-visible image). End users don't see it; iLovePDF and similar
    // compressors strip it. Runs at *every* preset — editing-metadata
    // bloat is unrelated to the user's quality choice. Gated only by
    // non-empty dpi_map so a parse failure doesn't nuke real images.
    if !dpi_map.is_empty() {
        let orphans: Vec<ObjectId> = image_ids
            .iter()
            .copied()
            .filter(|id| !dpi_map.contains_key(id))
            .collect();
        if !orphans.is_empty() {
            let orphan_set: HashSet<ObjectId> = orphans.iter().copied().collect();
            for id in &orphans {
                doc.objects.remove(id);
            }
            // Scrub dangling references to the dropped images so
            // readers don't trip over unresolved `N 0 R` values in
            // editing-metadata dicts.
            scrub_references_to(&mut doc, &orphan_set);
        }
    }
    let kept_image_ids: Vec<ObjectId> = image_ids
        .into_iter()
        .filter(|id| doc.objects.contains_key(id))
        .collect();
    for id in kept_image_ids {
        let effective_dpi = dpi_map.get(&id).copied();
        recompress_image(&mut doc, id, options, effective_dpi);
    }

    // Phase 2: Strip metadata
    if options.strip_metadata {
        strip_metadata(&mut doc);
    }

    // Phase 3: Stream compression on everything else
    doc.compress();
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    let output_size = buf.len() as u64;

    let reduction = if input_size > 0 {
        ((input_size as f64 - output_size as f64) / input_size as f64) * 100.0
    } else {
        0.0
    };

    Ok(CompressResult {
        data: buf,
        input_size,
        output_size,
        reduction_percent: reduction,
    })
}

/// Find all image XObject stream IDs in the document.
pub fn find_image_xobjects(doc: &Document) -> Vec<ObjectId> {
    let mut ids = Vec::new();
    for (&id, obj) in &doc.objects {
        if let Ok(stream) = obj.as_stream() {
            let dict = &stream.dict;
            // Check /Subtype /Image
            let is_image = dict
                .get(b"Subtype")
                .ok()
                .and_then(|v| v.as_name().ok())
                .map(|n: &[u8]| n == b"Image")
                .unwrap_or(false);
            if !is_image {
                continue;
            }
            // Skip ImageMask objects
            let is_mask = dict
                .get(b"ImageMask")
                .ok()
                .and_then(|v| v.as_bool().ok())
                .unwrap_or(false);
            if is_mask {
                continue;
            }
            ids.push(id);
        }
    }
    ids
}

/// Determine the number of color channels from a PDF ColorSpace value.
/// Returns Some(channels) for supported spaces, None for unsupported (CMYK, Indexed, etc.)
pub fn get_channels(doc: &Document, cs_obj: &Object) -> Option<u32> {
    // Simple name: /DeviceRGB, /DeviceGray
    if let Ok(name) = cs_obj.as_name() {
        return match name {
            b"DeviceRGB" | b"CalRGB" => Some(3),
            b"DeviceGray" | b"CalGray" => Some(1),
            _ => None, // DeviceCMYK, etc.
        };
    }
    // Array form: [/ICCBased <ref>], [/CalRGB ...], etc.
    if let Ok(arr) = cs_obj.as_array() {
        if let Some(first) = arr.first() {
            if let Ok(name) = first.as_name() {
                match name {
                    b"ICCBased" => {
                        // Second element is a reference to ICC profile stream with /N
                        if let Some(icc_ref) = arr.get(1) {
                            if let Ok(icc_id) = icc_ref.as_reference() {
                                if let Ok(icc_stream) =
                                    doc.get_object(icc_id).and_then(|o| o.as_stream())
                                {
                                    let n = get_int(&icc_stream.dict, b"N").unwrap_or(0);
                                    if n == 3 {
                                        return Some(3);
                                    }
                                    if n == 1 {
                                        return Some(1);
                                    }
                                    return None; // CMYK (4) or unknown
                                }
                            }
                        }
                        return None;
                    }
                    b"CalRGB" => return Some(3),
                    b"CalGray" => return Some(1),
                    _ => return None,
                }
            }
        }
    }
    None
}

/// Get the primary filter name from a /Filter entry (handles both name and array forms).
pub fn get_filter_name(dict: &lopdf::Dictionary) -> Option<Vec<u8>> {
    let filter_obj = dict.get(b"Filter").ok()?;
    // Simple name: /DCTDecode
    if let Ok(name) = filter_obj.as_name() {
        return Some(name.to_vec());
    }
    // Array form: [/FlateDecode] or [/ASCII85Decode /DCTDecode] — use last filter
    if let Ok(arr) = filter_obj.as_array() {
        if let Some(last) = arr.last() {
            if let Ok(name) = last.as_name() {
                return Some(name.to_vec());
            }
        }
    }
    None
}

/// Attempt to recompress a single image XObject.
///
/// `effective_dpi` is the maximum on-page pixel density observed for this
/// XObject across all its `Do` uses (computed once per document upstream).
/// `None` means we couldn't determine an on-page placement — typically an
/// orphan image or content-stream parse failure; the recompressor then
/// falls back to the max-edge cap only.
///
/// Silently skips on any failure — this is intentional for best-effort
/// compression where partial success is better than aborting the entire document.
fn recompress_image(
    doc: &mut Document,
    id: ObjectId,
    options: &CompressOptions,
    effective_dpi: Option<f32>,
) {
    // Clone the stream to read properties without borrow conflicts
    let stream = match doc.get_object(id).and_then(|o| o.as_stream().cloned()) {
        Ok(s) => s,
        Err(_) => return,
    };

    let dict = &stream.dict;

    let width = get_int(dict, b"Width").unwrap_or(0) as u32;
    let height = get_int(dict, b"Height").unwrap_or(0) as u32;
    let bpc = get_int(dict, b"BitsPerComponent").unwrap_or(8);
    if width == 0 || height == 0 || bpc != 8 {
        return;
    }

    // Skip images that are too large to safely decode in memory
    if (width as u64) * (height as u64) > MAX_DECODE_PIXELS {
        return;
    }

    // Skip if image has SMask (transparency — can't store in JPEG)
    if dict.has(b"SMask") {
        return;
    }

    let preserved_decode = dict.get(b"Decode").ok().cloned();
    let filter = get_filter_name(dict);
    let is_jpeg = filter.as_deref() == Some(b"DCTDecode");

    let cs_obj = dict.get(b"ColorSpace").ok().cloned();
    let channels = cs_obj
        .as_ref()
        .and_then(|cs| get_channels(doc, cs))
        .unwrap_or(0);

    // A4: classify the source colour space so recompression preserves
    // ICC-based colour when present, and bails out (rather than
    // destroying colour) for non-Device, non-ICCBased spaces such as
    // DeviceN / Separation.
    let cs_kind = classify_colorspace(doc, cs_obj.as_ref());
    match &cs_kind {
        ColorSpaceKind::Device | ColorSpaceKind::ICCBased(_) => {
            // Raw-pixel (Flate) path needs known channels to build the
            // source image; JPEG path detects channels from the
            // decoded DynamicImage via `image::load_from_memory`.
            if !is_jpeg && channels != 1 && channels != 3 {
                return;
            }
        }
        ColorSpaceKind::Indexed { .. } => {
            // Handled by the Indexed branch below — decodes through
            // the palette and doesn't need a pre-set channels hint.
        }
        ColorSpaceKind::Cmyk => {
            // image::load_from_memory's jpeg-decoder backend handles
            // CMYK → RGB internally. Only JPEG sources are supported;
            // Flate CMYK is rare and would need an explicit colour-
            // space conversion step we haven't implemented.
            if !is_jpeg {
                return;
            }
        }
        ColorSpaceKind::OtherUnsupported => return,
    }
    // Channels-derived RGB hint is only meaningful for the Flate
    // branch. JPEG branches derive their `is_rgb_out` from the
    // decoded DynamicImage variant.
    let is_rgb_source = channels == 3;

    let original_stream_size = stream.content.len();

    // Indexed source: decode through the palette to RGB / Gray, then
    // re-encode as a Device-colourspace JPEG. The new image's colour
    // space is the Indexed base (DeviceRGB or DeviceGray); the palette
    // overhead and per-pixel index are eliminated. Common in screenshot
    // PDFs and old-scanner workflows — previously skipped wholesale.
    if let ColorSpaceKind::Indexed {
        palette,
        base_channels,
    } = &cs_kind
    {
        let pixel_count = (width as usize) * (height as usize);
        let raw_indices: Vec<u8> = if filter.as_deref() == Some(b"FlateDecode") {
            match decompress_bounded(&stream.content) {
                Some(d) => d,
                None => return,
            }
        } else {
            stream.content.clone()
        };
        if raw_indices.len() < pixel_count {
            return;
        }
        let bc = *base_channels as usize;
        let mut decoded: Vec<u8> = Vec::with_capacity(pixel_count * bc);
        for &index_byte in raw_indices.iter().take(pixel_count) {
            let off = (index_byte as usize) * bc;
            if off + bc <= palette.len() {
                decoded.extend_from_slice(&palette[off..off + bc]);
            } else {
                // Out-of-bounds index — fill with zero (black). Defensive
                // against malformed palettes; spec says reader should
                // treat out-of-range indices as undefined.
                decoded.extend(std::iter::repeat_n(0, bc));
            }
        }

        let img: DynamicImage = if bc == 3 {
            match RgbImage::from_raw(width, height, decoded) {
                Some(i) => DynamicImage::ImageRgb8(i),
                None => return,
            }
        } else {
            match image::GrayImage::from_raw(width, height, decoded) {
                Some(i) => DynamicImage::ImageLuma8(i),
                None => return,
            }
        };

        let img = apply_downsample(img, options, effective_dpi);
        let new_w = img.width();
        let new_h = img.height();

        let mut jpeg_buf = Vec::new();
        {
            let mut encoder =
                JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_buf), options.image_quality);
            if encoder.encode_image(&img).is_err() {
                return;
            }
        }
        if !should_accept_recompress(jpeg_buf.len(), original_stream_size, options.image_quality) {
            return;
        }

        // Replace Indexed with the base Device space — `build_image_dict`
        // falls through to a Name(`DeviceRGB`/`DeviceGray`) for anything
        // that isn't ICCBased, which matches our intent.
        let is_rgb_out = bc == 3;
        let new_dict = build_image_dict(
            new_w,
            new_h,
            is_rgb_out,
            &ColorSpaceKind::Device,
            preserved_decode.as_ref(),
            jpeg_buf.len() as i64,
        );
        doc.set_object(id, Object::Stream(Stream::new(new_dict, jpeg_buf)));
        return;
    }

    // Decode image to raw pixels
    if is_jpeg {
        // image::load_from_memory routes through jpeg-decoder. CMYK
        // sources come out as RGB via the decoder's built-in colour
        // conversion; gray sources stay Luma. Derive the output
        // channel count from the decoded variant rather than from
        // the PDF-declared ColorSpace (which doesn't know the
        // decoder did the conversion).
        let img = match image::load_from_memory(&stream.content) {
            Ok(i) => i,
            Err(_) => return,
        };
        let is_rgb_out = !matches!(
            img,
            DynamicImage::ImageLuma8(_)
                | DynamicImage::ImageLuma16(_)
                | DynamicImage::ImageLumaA8(_)
                | DynamicImage::ImageLumaA16(_)
        );

        let img = apply_downsample(img, options, effective_dpi);
        let new_width = img.width();
        let new_height = img.height();
        let mut jpeg_buf = Vec::new();
        {
            let mut encoder =
                JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_buf), options.image_quality);
            if encoder.encode_image(&img).is_err() {
                return;
            }
        }
        if !should_accept_recompress(jpeg_buf.len(), original_stream_size, options.image_quality) {
            return;
        }
        // CMYK sources: the decoder already converted to RGB, so the
        // output dict must be DeviceRGB — preserving the original
        // CMYK ICC reference would mislabel the JPEG and produce wrong
        // colours on render. For Device / ICCBased RGB|Gray sources,
        // preserve the original cs_kind so ICC profiles round-trip.
        let new_dict = match &cs_kind {
            ColorSpaceKind::Cmyk => build_image_dict(
                new_width,
                new_height,
                is_rgb_out,
                &ColorSpaceKind::Device,
                preserved_decode.as_ref(),
                jpeg_buf.len() as i64,
            ),
            _ => build_image_dict(
                new_width,
                new_height,
                is_rgb_out,
                &cs_kind,
                preserved_decode.as_ref(),
                jpeg_buf.len() as i64,
            ),
        };
        doc.set_object(id, Object::Stream(Stream::new(new_dict, jpeg_buf)));
        return;
    }

    // FlateDecode or uncompressed: decompress to get raw pixel data
    let pixels: Vec<u8> = if filter.as_deref() == Some(b"FlateDecode") {
        // lopdf's decompress() silently fails on some streams.
        // Use flate2 directly with bounded decompression (zip bomb protection).
        match decompress_bounded(&stream.content) {
            Some(data) => data,
            None => return,
        }
    } else {
        stream.content.clone()
    };

    // Verify we have enough pixel data
    let channels_usize = channels as usize;
    let expected = (width as usize) * (height as usize) * channels_usize;
    if pixels.len() < expected {
        return;
    }

    // Build image from raw pixels
    let img: DynamicImage = if is_rgb_source {
        match RgbImage::from_raw(width, height, pixels[..expected].to_vec()) {
            Some(i) => DynamicImage::ImageRgb8(i),
            None => return,
        }
    } else {
        match image::GrayImage::from_raw(width, height, pixels[..expected].to_vec()) {
            Some(i) => DynamicImage::ImageLuma8(i),
            None => return,
        }
    };

    let img = apply_downsample(img, options, effective_dpi);
    let new_width = img.width();
    let new_height = img.height();

    // Re-encode as JPEG
    let mut jpeg_buf = Vec::new();
    {
        let mut encoder =
            JpegEncoder::new_with_quality(Cursor::new(&mut jpeg_buf), options.image_quality);
        if encoder.encode_image(&img).is_err() {
            return;
        }
    }

    // Always replace FlateDecode with JPEG — JPEG is almost always
    // smaller for photographic content. For FlateDecode, the raw
    // pixels are huge, so even a large JPEG is a win. Quality-aware
    // guard still bails on the pathological "JPEG bigger than Flate"
    // case (strict at quality > 70, 5% slack at Low / Medium).
    if !should_accept_recompress(jpeg_buf.len(), original_stream_size, options.image_quality) {
        return;
    }

    // Build replacement stream — Flate branch keeps the source
    // ColorSpace (Device or ICCBased), which matches the channel
    // count we built the source image from.
    let new_dict = build_image_dict(
        new_width,
        new_height,
        is_rgb_source,
        &cs_kind,
        preserved_decode.as_ref(),
        jpeg_buf.len() as i64,
    );

    let new_stream = Stream::new(new_dict, jpeg_buf);
    doc.set_object(id, Object::Stream(new_stream));
}

/// Classification of a source image's `/ColorSpace` for the recompressor.
enum ColorSpaceKind {
    /// `/DeviceRGB`, `/DeviceGray` (or `/CalRGB` / `/CalGray` which we
    /// approximate as Device-equivalent for channel count).
    Device,
    /// `[/ICCBased <ref>]` — preserve the ICC profile reference verbatim.
    ICCBased(ObjectId),
    /// `[/Indexed, baseCS, hival, lookup]` — resolve the palette so the
    /// recompressor can decode each source sample to RGB/Gray and
    /// re-encode as `DeviceRGB`/`DeviceGray` JPEG. The output is no
    /// longer Indexed; the palette becomes implicit in the pixel data.
    /// Carries the resolved lookup bytes and the base colour space's
    /// channel count (1 = gray, 3 = RGB).
    Indexed {
        palette: Vec<u8>,
        base_channels: u32,
    },
    /// `/DeviceCMYK` — JPEG-sourced CMYK photos (common in academic /
    /// print-prepared PDFs from InDesign + Distiller). The `image`
    /// crate's `jpeg-decoder` backend auto-converts CMYK to RGB on
    /// decode, so we route CMYK JPEGs through the JPEG branch and
    /// write the output as `DeviceRGB`. Non-JPEG (Flate) CMYK would
    /// need an explicit colour-space conversion and is skipped.
    Cmyk,
    /// `DeviceN`, raw `Separation`, etc. — recompression would destroy
    /// color fidelity, so the caller skips these.
    OtherUnsupported,
}

fn classify_colorspace(doc: &Document, cs_obj: Option<&Object>) -> ColorSpaceKind {
    let cs = match cs_obj {
        Some(c) => c,
        None => return ColorSpaceKind::Device,
    };
    if let Ok(name) = cs.as_name() {
        return match name {
            b"DeviceRGB" | b"DeviceGray" | b"CalRGB" | b"CalGray" => ColorSpaceKind::Device,
            b"DeviceCMYK" => ColorSpaceKind::Cmyk,
            _ => ColorSpaceKind::OtherUnsupported,
        };
    }
    if let Ok(arr) = cs.as_array() {
        if let Some(first) = arr.first() {
            if let Ok(name) = first.as_name() {
                if name == b"ICCBased" {
                    if let Some(icc_ref) = arr.get(1) {
                        if let Ok(icc_id) = icc_ref.as_reference() {
                            // Peek the ICC stream's /N entry: 1 = gray,
                            // 3 = RGB, 4 = CMYK. CMYK ICC profiles
                            // (common in InDesign / Distiller output)
                            // need to route through the Cmyk path so
                            // jpeg-decoder converts to RGB on decode.
                            // Leaving them as ICCBased would mislabel
                            // the recompressed RGB JPEG.
                            if let Ok(stream) = doc.get_object(icc_id).and_then(|o| o.as_stream()) {
                                let n = get_int(&stream.dict, b"N").unwrap_or(0);
                                if n == 4 {
                                    return ColorSpaceKind::Cmyk;
                                }
                            }
                            return ColorSpaceKind::ICCBased(icc_id);
                        }
                    }
                    return ColorSpaceKind::OtherUnsupported;
                }
                if name == b"CalRGB" || name == b"CalGray" {
                    return ColorSpaceKind::Device;
                }
                if name == b"Indexed" && arr.len() >= 4 {
                    // [/Indexed, baseCS, hival, lookup]
                    let base_channels = get_channels(doc, &arr[1]).unwrap_or(0);
                    if base_channels == 1 || base_channels == 3 {
                        if let Some(palette) = resolve_indexed_lookup(doc, &arr[3]) {
                            if !palette.is_empty() {
                                return ColorSpaceKind::Indexed {
                                    palette,
                                    base_channels,
                                };
                            }
                        }
                    }
                    return ColorSpaceKind::OtherUnsupported;
                }
            }
        }
    }
    ColorSpaceKind::OtherUnsupported
}

/// Resolves the `lookup` slot of an Indexed colour space into raw
/// palette bytes. Per PDF spec it can be either a literal byte string
/// or an indirect reference to a stream — handle both. Stream content
/// may be FlateDecode-compressed; bounded decompress matches the rest
/// of the pipeline (zip-bomb safety).
fn resolve_indexed_lookup(doc: &Document, lookup: &Object) -> Option<Vec<u8>> {
    match lookup {
        Object::String(bytes, _) => Some(bytes.clone()),
        Object::Reference(id) => {
            let resolved = doc.get_object(*id).ok()?;
            if let Ok(stream) = resolved.as_stream() {
                let filter = get_filter_name(&stream.dict);
                if filter.as_deref() == Some(b"FlateDecode") {
                    decompress_bounded(&stream.content)
                } else {
                    Some(stream.content.clone())
                }
            } else if let Object::String(bytes, _) = resolved {
                Some(bytes.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Quality-aware acceptance test for the re-encoded JPEG output.
///
/// - **High preset (`quality > 70`)**: strict — must be strictly smaller.
///   The user explicitly picked "best quality" and would rather we
///   left the image alone than grow it.
/// - **Low / Medium preset (`quality <= 70`)**: accept up to 5% growth.
///   The user picked an aggressive preset because they want a smaller
///   file; the strict-no-growth guard otherwise rejects nearly every
///   already-JPEG source whose re-encode lands at a comparable byte
///   count, which is the "Medium gives 1% reduction on a photo PDF"
///   user complaint. 4:2:0 chroma subsampling (`image` crate auto-on
///   at quality < 95) still drops perceptual data even when the byte
///   delta is small, so the user gets the lossy result they asked for.
fn should_accept_recompress(new_size: usize, original_size: usize, quality: u8) -> bool {
    if quality > 70 {
        new_size < original_size
    } else {
        // (new / orig) <= 1.05  ↔  new * 100 <= orig * 105
        (new_size as u64) * 100 <= (original_size as u64) * 105
    }
}

/// Build the replacement image XObject dictionary, preserving the source
/// ColorSpace (including ICC references) and any `/Decode` array.
fn build_image_dict(
    width: u32,
    height: u32,
    is_rgb: bool,
    cs_kind: &ColorSpaceKind,
    preserved_decode: Option<&Object>,
    length: i64,
) -> lopdf::Dictionary {
    let cs_value: Object = match cs_kind {
        ColorSpaceKind::ICCBased(icc_id) => Object::Array(vec![
            Object::Name(b"ICCBased".to_vec()),
            Object::Reference(*icc_id),
        ]),
        // ColorSpaceKind::Device falls back to the channel-derived Device space.
        // Same fallback for any unexpected variant — recompress() filters
        // OtherUnsupported out before reaching here.
        _ => Object::Name(if is_rgb {
            b"DeviceRGB".to_vec()
        } else {
            b"DeviceGray".to_vec()
        }),
    };

    let mut dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width as i64,
        "Height" => height as i64,
        "ColorSpace" => cs_value,
        "BitsPerComponent" => 8,
        "Filter" => "DCTDecode",
        "Length" => length,
    };
    if let Some(decode) = preserved_decode {
        dict.set("Decode", decode.clone());
    }
    dict
}

/// A5: collect every object ID that is referenced as `/SMask` or `/Mask`
/// from any image XObject. Those objects must be skipped by the recompressor
/// — re-encoding an alpha mask as JPEG strips alpha, leaving a black hole.
fn collect_mask_referenced_ids(doc: &Document) -> HashSet<ObjectId> {
    let mut masks: HashSet<ObjectId> = HashSet::new();
    for obj in doc.objects.values() {
        if let Ok(stream) = obj.as_stream() {
            for key in [b"SMask".as_slice(), b"Mask".as_slice()] {
                if let Ok(value) = stream.dict.get(key) {
                    // /Mask may be a reference to an image XObject OR an
                    // array of color values. We only care about the indirect-
                    // reference form (the image XObject case).
                    if let Ok(ref_id) = value.as_reference() {
                        masks.insert(ref_id);
                    }
                }
            }
        }
    }
    masks
}

/// Strip metadata from the document.
fn strip_metadata(doc: &mut Document) {
    // Remove /Info entries except /Title
    if let Ok(info_ref) = doc.trailer.get(b"Info") {
        if let Ok(id) = info_ref.as_reference() {
            if let Ok(dict) = doc.get_dictionary_mut(id) {
                let keys_to_remove: Vec<Vec<u8>> = dict
                    .iter()
                    .map(|(k, _)| k.to_vec())
                    .filter(|k| k.as_slice() != b"Title")
                    .collect();
                for key in keys_to_remove {
                    dict.remove(&key);
                }
            }
        }
    }

    // A7: detect XMP metadata streams by either /Subtype /XML *or* /Type
    // /Metadata. Adobe / Ghostscript / macOS PDFKit commonly emit the latter
    // form with no /Subtype at all, which the old check missed entirely.
    let ids_to_clean: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, obj)| {
            if is_metadata_stream(obj) {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    for id in ids_to_clean {
        doc.objects.remove(&id);
    }

    // Belt-and-braces: scrub orphan /Metadata pointers from the catalog
    // and from every page so the file no longer claims to have metadata it
    // doesn't ship.
    if let Ok(root_ref) = doc.trailer.get(b"Root") {
        if let Ok(root_id) = root_ref.as_reference() {
            if let Ok(catalog) = doc.get_dictionary_mut(root_id) {
                catalog.remove(b"Metadata");
            }
        }
    }

    // Remove /Thumb and /Metadata entries from pages
    for &page_id in doc.get_pages().values() {
        if let Ok(dict) = doc.get_dictionary_mut(page_id) {
            dict.remove(b"Thumb");
            dict.remove(b"Metadata");
        }
    }
}

/// Return true for streams that look like XMP metadata. Matches both the
/// `/Subtype /XML` form and the `/Type /Metadata` form (which PDFs from
/// Adobe / Ghostscript / macOS PDFKit frequently emit).
pub fn is_metadata_stream(obj: &Object) -> bool {
    let stream = match obj.as_stream() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let subtype_xml = stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|v| v.as_name().ok())
        .map(|n: &[u8]| n == b"XML")
        .unwrap_or(false);
    if subtype_xml {
        return true;
    }
    let type_metadata = stream
        .dict
        .get(b"Type")
        .ok()
        .and_then(|v| v.as_name().ok())
        .map(|n: &[u8]| n == b"Metadata")
        .unwrap_or(false);
    type_metadata
}

/// Helper to read an integer from a PDF dictionary.
pub fn get_int(dict: &lopdf::Dictionary, key: &[u8]) -> Option<i64> {
    dict.get(key).ok().and_then(|v| v.as_i64().ok())
}

/// Diagnostic-only public wrapper around the engine's effective-DPI
/// computation. Used by the `dpi_dump` example to inspect why specific
/// images survive Compress at full resolution. Not part of the stable
/// API.
#[doc(hidden)]
pub fn compute_dpi_map_for_diag(doc: &Document) -> HashMap<ObjectId, f32> {
    compute_effective_dpi_map(doc)
}

/// Diagnostic-only public wrapper around the mask-skip set. Used by the
/// `dpi_dump` example. Not part of the stable API.
#[doc(hidden)]
pub fn collect_mask_ids_for_diag(doc: &Document) -> HashSet<ObjectId> {
    collect_mask_referenced_ids(doc)
}

// =====================================================================
// DPI-aware downsampling
//
// PDF's image-placement model is fixed: an image XObject is always
// rendered into the unit square (0,0)-(1,1) in user space, then the
// CTM at the `Do` operator scales/rotates/translates that square onto
// the page. So the drawn width-in-points = magnitude of the CTM's
// X-axis vector = sqrt(a² + b²); drawn height = sqrt(c² + d²). The
// effective DPI = pixel_width × 72 / drawn_width_pts.
//
// Per-XObject we track the *maximum* effective DPI across every page
// that references it. Taking the max is a defensive choice — if the
// same image is used as both a high-res hero photo and a small
// thumbnail, downsampling for the thumbnail would damage the hero.
// =====================================================================

/// Drop an image to the target pixel dimensions implied by the combined
/// max-edge and DPI caps. Either cap may be disabled (`max_dim = 0` or
/// `target_dpi = 0`); the more aggressive of the two wins.
fn apply_downsample(
    img: DynamicImage,
    options: &CompressOptions,
    effective_dpi: Option<f32>,
) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let (target_w, target_h) = target_dimensions(
        w,
        h,
        options.max_image_dimension,
        effective_dpi,
        options.target_dpi,
    );
    if target_w == w && target_h == h {
        img
    } else {
        img.resize_exact(target_w, target_h, FilterType::Lanczos3)
    }
}

/// Compute new pixel dimensions for an image given the two downsampling
/// caps. Returns (width, height) unchanged when neither cap fires.
///
/// Both caps express a scale ratio in [0, 1]; we take the minimum so the
/// more aggressive cap wins. The ratio is clamped to `0.05` (5%) so a
/// pathologically tiny on-page drawn size can't collapse an image to
/// a single pixel.
fn target_dimensions(
    width: u32,
    height: u32,
    max_dim: u32,
    effective_dpi: Option<f32>,
    target_dpi: u32,
) -> (u32, u32) {
    let max_dim_ratio = if max_dim > 0 && (width > max_dim || height > max_dim) {
        let longest = width.max(height) as f32;
        (max_dim as f32) / longest
    } else {
        1.0
    };
    let dpi_ratio = match effective_dpi {
        Some(eff) if target_dpi > 0 && eff > target_dpi as f32 => (target_dpi as f32) / eff,
        _ => 1.0,
    };
    let ratio = max_dim_ratio.min(dpi_ratio).clamp(0.05, 1.0);
    if ratio >= 0.999 {
        return (width, height);
    }
    let new_w = ((width as f32) * ratio).round().max(1.0) as u32;
    let new_h = ((height as f32) * ratio).round().max(1.0) as u32;
    (new_w, new_h)
}

/// Walk every page's content stream — and any Form XObjects invoked from
/// it — and record the maximum effective DPI observed for each image
/// XObject. Returns an empty map if no images could be placed (e.g.
/// malformed content streams); recompression then falls back to
/// max-edge cap only.
fn compute_effective_dpi_map(doc: &Document) -> HashMap<ObjectId, f32> {
    const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut map: HashMap<ObjectId, f32> = HashMap::new();
    for &page_id in doc.get_pages().values() {
        let xobjects = resolve_page_xobjects(doc, page_id);
        if xobjects.is_empty() {
            continue;
        }
        let content_bytes = match doc.get_page_content(page_id) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let content = match Content::decode(&content_bytes) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut visited: HashSet<ObjectId> = HashSet::new();
        walk_content(doc, &content, &xobjects, IDENTITY, &mut map, &mut visited);
    }
    map
}

/// Walk the operations in `content`, tracking the CTM (graphics-state
/// `q`/`Q` stack + `cm` composition), and update `dpi_map` for any image
/// XObject placed via `Do /Name`. When `Do` resolves to a Form XObject,
/// recurse into the form's content stream — its own /Resources and
/// /Matrix multiply onto the inherited CTM. `visited` short-circuits
/// any cycle (rare but legal).
fn walk_content(
    doc: &Document,
    content: &Content,
    xobjects: &HashMap<Vec<u8>, ObjectId>,
    initial_ctm: [f32; 6],
    dpi_map: &mut HashMap<ObjectId, f32>,
    visited: &mut HashSet<ObjectId>,
) {
    let mut stack: Vec<[f32; 6]> = Vec::new();
    let mut ctm = initial_ctm;
    for op in &content.operations {
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(prev) = stack.pop() {
                    ctm = prev;
                }
            }
            "cm" => {
                if let Some(m) = read_matrix_array(&op.operands) {
                    ctm = multiply_cm(m, ctm);
                }
            }
            "Do" => {
                let name_bytes = match op.operands.first() {
                    Some(Object::Name(n)) => n.as_slice(),
                    _ => continue,
                };
                let xobject_id = match xobjects.get(name_bytes) {
                    Some(&id) => id,
                    None => continue,
                };
                let stream = match doc.get_object(xobject_id).and_then(|o| o.as_stream()) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let subtype = stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|v| v.as_name().ok())
                    .map(<[u8]>::to_vec);
                match subtype.as_deref() {
                    Some(b"Image") => {
                        let w = get_int(&stream.dict, b"Width").unwrap_or(0) as f32;
                        let h = get_int(&stream.dict, b"Height").unwrap_or(0) as f32;
                        if w <= 0.0 || h <= 0.0 {
                            continue;
                        }
                        let drawn_w = (ctm[0].powi(2) + ctm[1].powi(2)).sqrt();
                        let drawn_h = (ctm[2].powi(2) + ctm[3].powi(2)).sqrt();
                        if drawn_w < 1.0 || drawn_h < 1.0 {
                            continue;
                        }
                        let dpi_x = w * 72.0 / drawn_w;
                        let dpi_y = h * 72.0 / drawn_h;
                        let dpi = dpi_x.max(dpi_y);
                        let entry = dpi_map.entry(xobject_id).or_insert(0.0);
                        if dpi > *entry {
                            *entry = dpi;
                        }
                    }
                    Some(b"Form") => {
                        if !visited.insert(xobject_id) {
                            continue;
                        }
                        let form_matrix = stream
                            .dict
                            .get(b"Matrix")
                            .ok()
                            .and_then(|m| m.as_array().ok())
                            .and_then(|arr| read_matrix_array(arr))
                            .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
                        let form_ctm = multiply_cm(form_matrix, ctm);
                        let form_xobjects = resolve_form_xobjects(doc, &stream.dict);
                        let form_content = match Content::decode(&stream.content) {
                            Ok(c) => c,
                            Err(_) => {
                                visited.remove(&xobject_id);
                                continue;
                            }
                        };
                        walk_content(
                            doc,
                            &form_content,
                            &form_xobjects,
                            form_ctm,
                            dpi_map,
                            visited,
                        );
                        visited.remove(&xobject_id);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Form XObject's `/Resources/XObject` sub-dict resolved to a name → id
/// map. Forms can have their own resources independent of the parent
/// page; if absent, the form inherits from the page (caller passes the
/// parent map, not this one — but Forms with no /Resources are rare).
fn resolve_form_xobjects(
    doc: &Document,
    form_dict: &lopdf::Dictionary,
) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let resources = match form_dict.get(b"Resources") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Reference(id)) => match doc.get_dictionary(*id) {
            Ok(d) => d.clone(),
            Err(_) => return map,
        },
        _ => return map,
    };
    let dict = match resources.get(b"XObject") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Reference(id)) => match doc.get_dictionary(*id) {
            Ok(d) => d.clone(),
            Err(_) => return map,
        },
        _ => return map,
    };
    for (name, value) in dict.iter() {
        if let Ok(id) = value.as_reference() {
            map.insert(name.to_vec(), id);
        }
    }
    map
}

/// Collect the page's /Resources/XObject sub-dictionary as a
/// name → ObjectId map, walking up the page tree for inherited
/// /Resources entries (PDF 32000-1 §7.7.3.4).
fn resolve_page_xobjects(doc: &Document, page_id: ObjectId) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let resources = resolve_page_resources(doc, page_id);
    let dict = match resources.get(b"XObject") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Reference(id)) => match doc.get_dictionary(*id) {
            Ok(d) => d.clone(),
            Err(_) => return map,
        },
        _ => return map,
    };
    for (name, value) in dict.iter() {
        if let Ok(id) = value.as_reference() {
            map.insert(name.to_vec(), id);
        }
    }
    map
}

fn resolve_page_resources(doc: &Document, page_id: ObjectId) -> lopdf::Dictionary {
    let mut current = page_id;
    for _ in 0..16 {
        let dict = match doc.get_dictionary(current) {
            Ok(d) => d,
            Err(_) => return lopdf::Dictionary::new(),
        };
        if let Ok(res) = dict.get(b"Resources") {
            match res {
                Object::Dictionary(d) => return d.clone(),
                Object::Reference(id) => {
                    if let Ok(d) = doc.get_dictionary(*id) {
                        return d.clone();
                    }
                }
                _ => {}
            }
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(parent_id)) => current = *parent_id,
            _ => return lopdf::Dictionary::new(),
        }
    }
    lopdf::Dictionary::new()
}

/// Decode six numeric operands (PDF Real / Integer mix) into a 3x2 affine
/// matrix used by both the `cm` operator and the Form XObject `/Matrix`
/// dictionary entry.
fn read_matrix_array(operands: &[Object]) -> Option<[f32; 6]> {
    if operands.len() != 6 {
        return None;
    }
    let mut m = [0.0_f32; 6];
    for (i, op) in operands.iter().enumerate() {
        m[i] = if let Ok(f) = op.as_float() {
            f
        } else if let Ok(n) = op.as_i64() {
            n as f32
        } else {
            return None;
        };
    }
    Some(m)
}

/// Replace every `Reference(id)` whose `id` is in `dropped` with
/// `Object::Null`. Used after orphan-image deletion to keep references
/// in editing-metadata dicts (e.g. Adobe's `/CompositeImage`) from
/// pointing at non-existent objects. PDF readers treat Null in place of
/// a missing dict value as "key not present", which is exactly the
/// semantics we want.
fn scrub_references_to(doc: &mut Document, dropped: &HashSet<ObjectId>) {
    let mut trailer = std::mem::take(&mut doc.trailer);
    scrub_dict(&mut trailer, dropped);
    doc.trailer = trailer;
    for obj in doc.objects.values_mut() {
        scrub_obj(obj, dropped);
    }
}

fn scrub_obj(obj: &mut Object, dropped: &HashSet<ObjectId>) {
    match obj {
        Object::Reference(id) if dropped.contains(id) => {
            *obj = Object::Null;
        }
        Object::Array(arr) => {
            for item in arr {
                scrub_obj(item, dropped);
            }
        }
        Object::Dictionary(d) => scrub_dict(d, dropped),
        Object::Stream(s) => scrub_dict(&mut s.dict, dropped),
        _ => {}
    }
}

fn scrub_dict(d: &mut lopdf::Dictionary, dropped: &HashSet<ObjectId>) {
    for (_, v) in d.iter_mut() {
        scrub_obj(v, dropped);
    }
}

/// PDF row-vector convention: a point [x y 1] is transformed as
/// [x y 1] * M. After `cm M_op`, points are transformed first by
/// M_op then by the old CTM, so the new CTM = M_op * old_CTM.
fn multiply_cm(op: [f32; 6], old: [f32; 6]) -> [f32; 6] {
    [
        op[0] * old[0] + op[1] * old[2],
        op[0] * old[1] + op[1] * old[3],
        op[2] * old[0] + op[3] * old[2],
        op[2] * old[1] + op[3] * old[3],
        op[4] * old[0] + op[5] * old[2] + old[4],
        op[4] * old[1] + op[5] * old[3] + old[5],
    ]
}

#[derive(Debug)]
pub struct CompressResult {
    pub data: Vec<u8>,
    pub input_size: u64,
    pub output_size: u64,
    pub reduction_percent: f64,
}
