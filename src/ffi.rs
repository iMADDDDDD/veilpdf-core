//! C-compatible FFI for calling veilpdf-core from Swift/Objective-C.
//!
//! All functions operate on byte buffers — no file paths cross the FFI boundary.
//! The caller is responsible for freeing returned buffers via `veil_free_buffer`.

use std::ptr;
use std::slice;

/// Maximum input size accepted via FFI (512 MB).
const MAX_INPUT_SIZE: usize = 512 * 1024 * 1024;

/// Result of an FFI operation. Contains either data or an error message.
#[repr(C)]
pub struct VeilBuffer {
    pub data: *mut u8,
    pub len: usize,
    pub error: *mut u8,
    pub error_len: usize,
}

/// Page-space redaction rectangle passed over the FFI boundary.
#[repr(C)]
pub struct VeilRedactionRect {
    pub page_index: u32,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl VeilBuffer {
    fn success(data: Vec<u8>) -> Self {
        if data.is_empty() {
            return VeilBuffer {
                data: ptr::null_mut(),
                len: 0,
                error: ptr::null_mut(),
                error_len: 0,
            };
        }
        let mut boxed = data.into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        let len = boxed.len();
        std::mem::forget(boxed);
        VeilBuffer {
            data: ptr,
            len,
            error: ptr::null_mut(),
            error_len: 0,
        }
    }

    fn error(msg: String) -> Self {
        let mut boxed = msg.into_bytes().into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        let len = boxed.len();
        std::mem::forget(boxed);
        VeilBuffer {
            data: ptr::null_mut(),
            len: 0,
            error: ptr,
            error_len: len,
        }
    }
}

/// Free a buffer returned by any veil_* function.
///
/// # Safety
/// `buf` must be a valid VeilBuffer returned by a veil_* function.
/// Must only be called once per buffer.
#[no_mangle]
pub unsafe extern "C" fn veil_free_buffer(buf: VeilBuffer) {
    if !buf.data.is_null() && buf.len > 0 {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(buf.data, buf.len)) });
    }
    if !buf.error.is_null() && buf.error_len > 0 {
        drop(unsafe {
            Box::from_raw(std::ptr::slice_from_raw_parts_mut(buf.error, buf.error_len))
        });
    }
}

/// Merge two PDF documents from byte buffers.
///
/// # Safety
/// `a_ptr`/`b_ptr` must be valid pointers to `a_len`/`b_len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_merge(
    a_ptr: *const u8,
    a_len: usize,
    b_ptr: *const u8,
    b_len: usize,
) -> VeilBuffer {
    // Validate pointers and sizes before catch_unwind — these are FFI boundary checks
    if a_ptr.is_null() || b_ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if a_len == 0 || b_len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if a_len > MAX_INPUT_SIZE || b_len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    // Create slices in the unsafe context, then wrap safe logic in catch_unwind
    let a = unsafe { slice::from_raw_parts(a_ptr, a_len) };
    let b = unsafe { slice::from_raw_parts(b_ptr, b_len) };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::merge::merge_pdfs_from_bytes(&[a, b]) {
            Ok(data) => VeilBuffer::success(data),
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during merge".into()))
}

/// Split a PDF document into individual pages.
///
/// Returns pages concatenated with 8-byte little-endian length prefixes:
/// [len1: u64][page1_bytes][len2: u64][page2_bytes]...
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_split(ptr: *const u8, len: usize) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::split::split_pdf_from_bytes(data) {
            Ok(pages) => {
                if pages.is_empty() {
                    return VeilBuffer::error("PDF has no pages".into());
                }
                let mut output = Vec::new();
                for page in &pages {
                    output.extend_from_slice(&(page.len() as u64).to_le_bytes());
                    output.extend_from_slice(page);
                }
                VeilBuffer::success(output)
            }
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during split".into()))
}

/// Compress a PDF document.
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_compress(ptr: *const u8, len: usize) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::compress::compress_pdf_from_bytes(data) {
            Ok(result) => {
                // If compression made the file larger, return the original
                if result.data.len() >= data.len() {
                    VeilBuffer::success(data.to_vec())
                } else {
                    VeilBuffer::success(result.data)
                }
            }
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during compress".into()))
}

/// Compress a PDF with image recompression, resizing, and optional metadata stripping.
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_compress_ex(
    ptr: *const u8,
    len: usize,
    image_quality: u8,
    max_image_dimension: u32,
    target_dpi: u32,
    strip_metadata: u8,
) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let options = crate::compress::CompressOptions {
            image_quality: image_quality.clamp(1, 100),
            max_image_dimension,
            target_dpi,
            strip_metadata: strip_metadata != 0,
        };

        match crate::compress::compress_pdf_with_options(data, &options) {
            Ok(result) => {
                if result.data.len() >= data.len() {
                    VeilBuffer::success(data.to_vec())
                } else {
                    VeilBuffer::success(result.data)
                }
            }
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during compress_ex".into()))
}

/// Sanitize a PDF by removing dangerous or sensitive elements.
///
/// `flags` is a bitmask: bit 0 = metadata, bit 1 = JavaScript, bit 2 = embedded files,
/// bit 3 = actions, bit 4 = XMP.
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_sanitize(ptr: *const u8, len: usize, flags: u32) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || match crate::sanitize::sanitize_pdf(data, flags) {
                Ok(result) => VeilBuffer::success(result),
                Err(e) => VeilBuffer::error(e.to_string()),
            },
        ));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during sanitize".into()))
}

/// Remove selected page annotations without replaying page content streams.
///
/// `flags` is a bitmask:
/// bit 0 = text markup, bit 1 = notes, bit 2 = drawings/shapes,
/// bit 3 = stamps, bit 4 = free text, bit 5 = other, bit 6 = links.
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_remove_annotations(
    ptr: *const u8,
    len: usize,
    flags: u32,
) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::annotations::remove_annotations(data, flags) {
            Ok(result) => VeilBuffer::success(result),
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during remove_annotations".into()))
}

/// Extract all images from a PDF.
///
/// Returns images as a length-prefixed buffer:
/// `[u32 width][u32 height][u8 format (0=jpeg, 1=png)][u64 data_len][bytes]...`
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of PDF data.
#[no_mangle]
pub unsafe extern "C" fn veil_extract_images(ptr: *const u8, len: usize) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::extract::extract_images(data) {
            Ok(images) => {
                if images.is_empty() {
                    return VeilBuffer::error("No images found in PDF".into());
                }
                VeilBuffer::success(crate::extract::serialize_images(&images))
            }
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during extract_images".into()))
}

/// Apply a text watermark to every page of a PDF.
///
/// Bakes the watermark into each page's content stream so the styling
/// survives save+reload across every PDF reader (Phase 3.5 of the
/// document-first UI). Phase 3.6 embeds the caller-supplied TrueType
/// font via Type0 / CIDFontType2 / Identity-H so Unicode text renders
/// correctly across Latin Extended, Cyrillic, Greek, Vietnamese, etc.
/// Complex-script shaping (Arabic, Indic) is not handled yet.
///
/// `r`, `g`, `b`, `opacity` are 0.0..=1.0. `rotation_deg` is
/// counter-clockwise around the page centre (0 = horizontal,
/// -45 = diagonal).
///
/// # Safety
/// `ptr` must point to `len` bytes of PDF data. `text_ptr` must point to
/// `text_len` bytes of UTF-8 text. `font_ptr` must point to `font_len`
/// bytes of a valid TrueType font file.
#[no_mangle]
pub unsafe extern "C" fn veil_apply_watermark(
    ptr: *const u8,
    len: usize,
    text_ptr: *const u8,
    text_len: usize,
    font_ptr: *const u8,
    font_len: usize,
    font_size: f32,
    r: f32,
    g: f32,
    b: f32,
    opacity: f32,
    rotation_deg: f32,
) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }
    if text_ptr.is_null() {
        return VeilBuffer::error("null watermark text pointer".into());
    }
    if text_len == 0 {
        return VeilBuffer::error("empty watermark text".into());
    }
    // Watermark text is rendered into a content stream once per page, so a
    // multi-megabyte string here would explode output size for no useful
    // reason. 64 KB is well past any plausible label.
    if text_len > 64 * 1024 {
        return VeilBuffer::error("watermark text exceeds maximum size (64 KB)".into());
    }
    if font_ptr.is_null() {
        return VeilBuffer::error("null font pointer".into());
    }
    if font_len == 0 {
        return VeilBuffer::error("empty font buffer".into());
    }
    // 16 MB is comfortably larger than any single-style TTF; bigger inputs
    // are almost certainly the wrong file.
    if font_len > 16 * 1024 * 1024 {
        return VeilBuffer::error("font exceeds maximum size (16 MB)".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let text_bytes = unsafe { slice::from_raw_parts(text_ptr, text_len) };
    let font_bytes = unsafe { slice::from_raw_parts(font_ptr, font_len) };
    let text = match std::str::from_utf8(text_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return VeilBuffer::error("watermark text is not valid UTF-8".into()),
    };

    let options = crate::watermark::WatermarkOptions {
        text,
        font_size,
        color: crate::watermark::WatermarkColor { r, g, b },
        opacity,
        rotation_deg,
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::watermark::apply_text_watermark(data, &options, font_bytes) {
            Ok(out) => VeilBuffer::success(out),
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during apply_watermark".into()))
}

/// Apply page-space redaction rectangles without rasterizing whole pages.
///
/// # Safety
/// `ptr` must point to `len` bytes of PDF data. `rects_ptr` must point to
/// `rects_len` valid `VeilRedactionRect` entries.
#[no_mangle]
pub unsafe extern "C" fn veil_apply_redactions(
    ptr: *const u8,
    len: usize,
    rects_ptr: *const VeilRedactionRect,
    rects_len: usize,
) -> VeilBuffer {
    if ptr.is_null() {
        return VeilBuffer::error("null input pointer".into());
    }
    if len == 0 {
        return VeilBuffer::error("empty input buffer".into());
    }
    if len > MAX_INPUT_SIZE {
        return VeilBuffer::error("input exceeds maximum size (512 MB)".into());
    }
    if rects_ptr.is_null() {
        return VeilBuffer::error("null redaction pointer".into());
    }
    if rects_len == 0 {
        return VeilBuffer::error("no redactions to apply".into());
    }
    if rects_len > 100_000 {
        return VeilBuffer::error("too many redactions".into());
    }

    let data = unsafe { slice::from_raw_parts(ptr, len) };
    let ffi_rects = unsafe { slice::from_raw_parts(rects_ptr, rects_len) };
    let rects: Vec<crate::redact::RedactionRect> = ffi_rects
        .iter()
        .map(|rect| crate::redact::RedactionRect {
            page_index: rect.page_index as usize,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        })
        .collect();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match crate::redact::apply_redactions(data, &rects) {
            Ok(out) => VeilBuffer::success(out),
            Err(e) => VeilBuffer::error(e.to_string()),
        }
    }));

    result.unwrap_or_else(|_| VeilBuffer::error("internal panic during apply_redactions".into()))
}
