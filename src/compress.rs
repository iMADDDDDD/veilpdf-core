use crate::limits::check_object_count;
use crate::{Result, VeilError};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, RgbImage};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use std::collections::HashSet;
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
    /// Remove document metadata (Info dict, XMP, thumbnails).
    pub strip_metadata: bool,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            image_quality: 75,
            max_image_dimension: 2048,
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
    for id in image_ids {
        recompress_image(&mut doc, id, options);
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
                                if let Ok(icc_stream) = doc.get_object(icc_id).and_then(|o| o.as_stream()) {
                                    let n = get_int(&icc_stream.dict, b"N").unwrap_or(0);
                                    if n == 3 { return Some(3); }
                                    if n == 1 { return Some(1); }
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
/// Silently skips on any failure — this is intentional for best-effort
/// compression where partial success is better than aborting the entire document.
fn recompress_image(doc: &mut Document, id: ObjectId, options: &CompressOptions) {
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

    // Determine color channels from ColorSpace
    let cs_obj = dict.get(b"ColorSpace").ok().cloned();
    let channels = cs_obj
        .as_ref()
        .and_then(|cs| get_channels(doc, cs))
        .unwrap_or(0);
    if channels != 1 && channels != 3 {
        return; // CMYK or unsupported
    }
    let is_rgb = channels == 3;

    // A4: classify the source color space so that recompression preserves
    // ICC-based color when present, and bails out (rather than destroying
    // color) for non-Device, non-ICCBased spaces such as CalRGB / Indexed.
    let cs_kind = classify_colorspace(cs_obj.as_ref());
    match cs_kind {
        ColorSpaceKind::Device | ColorSpaceKind::ICCBased(_) => {}
        ColorSpaceKind::OtherUnsupported => {
            // Non-Device, non-ICCBased (CalRGB, Indexed, etc.). Recompressing
            // would silently drop the calibration / palette and shift colors.
            // Skip — this is a deliberate no-op.
            return;
        }
    }
    let preserved_decode = dict.get(b"Decode").ok().cloned();

    // Determine filter (handles both name and array forms)
    let filter = get_filter_name(dict);

    let original_stream_size = stream.content.len();

    // Decode image to raw pixels
    let is_jpeg = filter.as_deref() == Some(b"DCTDecode");

    if is_jpeg {
        // For JPEG, decode via image crate directly
        let img = match image::load_from_memory(&stream.content) {
            Ok(i) => i,
            Err(_) => return,
        };
        // Process JPEG: resize + re-encode
        let max_dim = options.max_image_dimension;
        let img = if width > max_dim || height > max_dim {
            img.resize(max_dim, max_dim, FilterType::Lanczos3)
        } else {
            img
        };
        let new_width = img.width();
        let new_height = img.height();
        let mut jpeg_buf = Vec::new();
        {
            let mut encoder = JpegEncoder::new_with_quality(
                Cursor::new(&mut jpeg_buf),
                options.image_quality,
            );
            if encoder.encode_image(&img).is_err() {
                return;
            }
        }
        if jpeg_buf.len() >= original_stream_size {
            return;
        }
        let new_dict = build_image_dict(
            new_width,
            new_height,
            is_rgb,
            &cs_kind,
            preserved_decode.as_ref(),
            jpeg_buf.len() as i64,
        );
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
    let img: DynamicImage = if is_rgb {
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

    // Resize if needed
    let max_dim = options.max_image_dimension;
    let img = if width > max_dim || height > max_dim {
        img.resize(max_dim, max_dim, FilterType::Lanczos3)
    } else {
        img
    };

    let new_width = img.width();
    let new_height = img.height();

    // Re-encode as JPEG
    let mut jpeg_buf = Vec::new();
    {
        let mut encoder = JpegEncoder::new_with_quality(
            Cursor::new(&mut jpeg_buf),
            options.image_quality,
        );
        if encoder.encode_image(&img).is_err() {
            return;
        }
    }

    // Always replace FlateDecode with JPEG — JPEG is almost always smaller
    // for photographic content. For FlateDecode, the raw pixels are huge,
    // so even a large JPEG is a win.
    // Only skip if the JPEG is somehow larger than the Flate stream.
    if jpeg_buf.len() >= original_stream_size {
        return;
    }

    // Build replacement stream
    let new_dict = build_image_dict(
        new_width,
        new_height,
        is_rgb,
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
    /// `Indexed`, `DeviceN`, raw `Separation`, etc. — recompression would
    /// destroy color fidelity, so the caller skips these.
    OtherUnsupported,
}

fn classify_colorspace(cs_obj: Option<&Object>) -> ColorSpaceKind {
    let cs = match cs_obj {
        Some(c) => c,
        None => return ColorSpaceKind::Device,
    };
    if let Ok(name) = cs.as_name() {
        return match name {
            b"DeviceRGB" | b"DeviceGray" | b"CalRGB" | b"CalGray" => ColorSpaceKind::Device,
            _ => ColorSpaceKind::OtherUnsupported,
        };
    }
    if let Ok(arr) = cs.as_array() {
        if let Some(first) = arr.first() {
            if let Ok(name) = first.as_name() {
                if name == b"ICCBased" {
                    if let Some(icc_ref) = arr.get(1) {
                        if let Ok(icc_id) = icc_ref.as_reference() {
                            return ColorSpaceKind::ICCBased(icc_id);
                        }
                    }
                    return ColorSpaceKind::OtherUnsupported;
                }
                if name == b"CalRGB" || name == b"CalGray" {
                    return ColorSpaceKind::Device;
                }
            }
        }
    }
    ColorSpaceKind::OtherUnsupported
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
        _ => Object::Name(if is_rgb { b"DeviceRGB".to_vec() } else { b"DeviceGray".to_vec() }),
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

#[derive(Debug)]
pub struct CompressResult {
    pub data: Vec<u8>,
    pub input_size: u64,
    pub output_size: u64,
    pub reduction_percent: f64,
}
