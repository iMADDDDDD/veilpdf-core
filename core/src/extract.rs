use crate::compress::{decompress_bounded, find_image_xobjects, get_channels, get_filter_name, get_int, MAX_DECODE_PIXELS};
use crate::{Result, VeilError};
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ImageEncoder, RgbImage};
use lopdf::Document;
use std::io::Cursor;

/// Extracted image with metadata.
pub struct ExtractedImage {
    pub width: u32,
    pub height: u32,
    /// 0 = JPEG, 1 = PNG.
    pub format: u8,
    pub data: Vec<u8>,
}

/// Extract all images from a PDF, returning them as decoded image data.
///
/// JPEG images are passed through without re-encoding. FlateDecode images
/// are decompressed and re-encoded as PNG. Returns an error for encrypted PDFs.
pub fn extract_images(data: &[u8]) -> Result<Vec<ExtractedImage>> {
    let doc = Document::load_mem(data)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    let image_ids = find_image_xobjects(&doc);
    let mut images = Vec::new();

    for id in image_ids {
        let stream = match doc.get_object(id).and_then(|o| o.as_stream().cloned()) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let dict = &stream.dict;
        let width = get_int(dict, b"Width").unwrap_or(0) as u32;
        let height = get_int(dict, b"Height").unwrap_or(0) as u32;
        if width == 0 || height == 0 {
            continue;
        }

        // Skip images too large to safely decode
        if (width as u64) * (height as u64) > MAX_DECODE_PIXELS {
            continue;
        }

        let bpc = get_int(dict, b"BitsPerComponent").unwrap_or(8);
        if bpc != 8 {
            continue;
        }

        // Skip images with transparency
        if dict.has(b"SMask") {
            continue;
        }

        let channels = dict
            .get(b"ColorSpace")
            .ok()
            .and_then(|cs| get_channels(&doc, cs))
            .unwrap_or(0);
        if channels != 1 && channels != 3 {
            continue;
        }

        let filter = get_filter_name(dict);

        match filter.as_deref() {
            Some(b"DCTDecode") => {
                // JPEG — pass through raw bytes
                images.push(ExtractedImage {
                    width,
                    height,
                    format: 0, // JPEG
                    data: stream.content.clone(),
                });
            }
            Some(b"FlateDecode") | None => {
                // Decompress with flate2 (bounded to prevent zip bombs)
                let inflated = match decompress_bounded(&stream.content) {
                    Some(data) => data,
                    None => continue,
                };

                let is_rgb = channels == 3;
                let expected = (width as usize) * (height as usize) * (channels as usize);
                if inflated.len() < expected {
                    continue;
                }

                // Encode as PNG
                let img: DynamicImage = if is_rgb {
                    match RgbImage::from_raw(width, height, inflated[..expected].to_vec()) {
                        Some(i) => DynamicImage::ImageRgb8(i),
                        None => continue,
                    }
                } else {
                    match image::GrayImage::from_raw(width, height, inflated[..expected].to_vec()) {
                        Some(i) => DynamicImage::ImageLuma8(i),
                        None => continue,
                    }
                };

                let mut png_buf = Vec::new();
                let encoder = PngEncoder::new(Cursor::new(&mut png_buf));
                if encoder.write_image(
                    img.as_bytes(),
                    img.width(),
                    img.height(),
                    img.color().into(),
                ).is_err() {
                    continue;
                }

                images.push(ExtractedImage {
                    width,
                    height,
                    format: 1, // PNG
                    data: png_buf,
                });
            }
            _ => continue, // Skip JBIG2, JPX, CCITTFax
        }
    }

    Ok(images)
}

/// Serialize extracted images into a length-prefixed buffer for FFI.
/// Format per image: [u32 width][u32 height][u8 format][u64 data_len][bytes]
pub fn serialize_images(images: &[ExtractedImage]) -> Vec<u8> {
    let mut output = Vec::new();
    for img in images {
        output.extend_from_slice(&img.width.to_le_bytes());
        output.extend_from_slice(&img.height.to_le_bytes());
        output.push(img.format);
        output.extend_from_slice(&(img.data.len() as u64).to_le_bytes());
        output.extend_from_slice(&img.data);
    }
    output
}
