//! Bake a text watermark into PDF content streams.
//!
//! Phase 3.5 of the document-first UI: makes `WatermarkAnnotation` styling
//! survive save+reload. PDFKit doesn't serialise `PDFAnnotation` subclass
//! state and doesn't auto-generate appearance streams from `draw(...)`
//! overrides, so we emit standard PDF text-showing ops (`Tj`) directly into
//! each page's content stream. The result is cross-reader compatible and
//! preserves AcroForms, text selection, and vector quality (the legacy
//! `AddWatermarkView` rasterises every page, which is the bug-class-#4
//! anti-pattern we want to retire).
//!
//! Phase 3.6 unlocks Unicode by embedding a caller-supplied TrueType font
//! as a Type0 / CIDFontType2 font with Identity-H encoding. Text bytes
//! become 2-byte glyph indices straight from the font's cmap; advances
//! come from `hmtx`. Phase 3.7 routes the embedded font through
//! `subsetter` so only the glyphs the watermark uses ship in the PDF —
//! the per-file size hit drops from ~415 KB to a few KB.
//!
//! Text shaping (3.8) is still deferred: Latin / Cyrillic / Greek render
//! correctly; Arabic and Indic scripts won't reorder or form ligatures.

use crate::font::EmbeddedFont;
use crate::limits::check_object_count;
use crate::shape::{shape_text, ShapedGlyph};
use crate::{Result, VeilError};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat};
use subsetter::GlyphRemapper;

/// RGB triple in 0.0..=1.0.
#[derive(Clone, Copy, Debug)]
pub struct WatermarkColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// Caller-supplied watermark parameters. Mirrors the Swift
/// `WatermarkConfig`, minus the live-overlay-only fields.
#[derive(Clone, Debug)]
pub struct WatermarkOptions {
    pub text: String,
    pub font_size: f32,
    pub color: WatermarkColor,
    pub opacity: f32,
    /// Counter-clockwise rotation in degrees, applied around the page
    /// centre. `0` = horizontal, `-45` = diagonal lower-left to upper-right.
    pub rotation_deg: f32,
}

/// Maximum number of `/Parent` hops we'll follow when resolving inherited
/// page attributes. Real-world PDFs don't nest /Pages trees more than a
/// handful deep; the cap guards against malformed cyclic trees.
const MAX_PARENT_HOPS: usize = 16;

/// Default US Letter dimensions (in points), used when a page has no
/// resolvable MediaBox. Real input from the macOS app passes through
/// `PDFDocumentGuard` first and always has a MediaBox; this only keeps
/// pathological inputs from divide-by-zero.
const DEFAULT_PAGE_W: f32 = 612.0;
const DEFAULT_PAGE_H: f32 = 792.0;

/// Apply `options` as a watermark to every page of `data`, using
/// `font_bytes` as the embedded TrueType font. Returns the re-serialised
/// PDF bytes.
pub fn apply_text_watermark(
    data: &[u8],
    options: &WatermarkOptions,
    font_bytes: &[u8],
) -> Result<Vec<u8>> {
    if options.text.is_empty() {
        return Err(VeilError::InvalidInput("watermark text is empty".into()));
    }
    if font_bytes.is_empty() {
        return Err(VeilError::InvalidInput(
            "watermark font bytes are required".into(),
        ));
    }

    let font = EmbeddedFont::new(font_bytes)?;

    let mut doc = Document::load_mem(data)?;
    check_object_count(&doc)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    let opacity = options.opacity.clamp(0.0, 1.0);
    let font_size = options.font_size.max(1.0);

    // Shape the input through unicode-bidi + rustybuzz so contextual
    // forms (Arabic), bidi reordering (Hebrew), and ligatures all work.
    // For Latin text without any GSUB/GPOS context, this collapses to
    // the same character-by-character cmap lookup the pre-3.8 path did.
    // Holes (characters the font has no glyph for) are dropped
    // silently — no `.notdef` boxes.
    let glyphs = shape_text(font_bytes, &options.text).ok_or_else(|| {
        VeilError::InvalidInput("watermark font could not be parsed for shaping".into())
    })?;
    if glyphs.is_empty() {
        return Err(VeilError::InvalidInput(
            "watermark text has no renderable glyphs in this font".into(),
        ));
    }
    let original_gids: Vec<u16> = glyphs.iter().map(|g| g.gid).collect();

    // Total width = sum of post-shape x_advance (rustybuzz applies GPOS
    // kerning + Arabic mark positioning + ligature widths). Falls back
    // to /W-equivalent values for plain Latin where no GPOS runs.
    let upem = font.metrics().units_per_em.max(1) as f32;
    let total_advance: i64 = glyphs.iter().map(|g| g.x_advance as i64).sum();
    let text_width = total_advance as f32 * font_size / upem;
    // Approximate cap-height fraction for the vertical centre — using the
    // font's actual cap-height in design units keeps the baseline near the
    // page midline without depending on per-glyph metrics.
    let cap_height_em = font.metrics().cap_height as f32 / upem;
    let text_height = cap_height_em * font_size;

    // Subset the font so the PDF only carries the glyphs we use. The
    // remapper renumbers original GIDs to a contiguous 0..N space (with
    // .notdef pinned at 0), which is what `subsetter` writes into the
    // output TTF. We carry the remapper forward so the Identity-H byte
    // payload and the /W array use the new GIDs.
    let remapper = GlyphRemapper::new_from_glyphs(&original_gids);
    let subset_bytes = subsetter::subset(font.bytes, 0, &remapper)
        .map_err(|e| VeilError::InvalidInput(format!("watermark font subsetting failed: {e:?}")))?;
    // Build the TJ array once — same on every page. Most glyphs flow
    // through as 2-byte hex pairs concatenated into a single string
    // element; we only break out a numeric adjustment when rustybuzz's
    // post-shape advance disagrees with the font's hmtx width (i.e.
    // GPOS kerning ran). Without this, text positioning would track
    // unshaped /W advances and the watermark would be visibly off at
    // larger sizes for fonts with active kerning tables.
    let tj_array = build_tj_array(&font, &glyphs, &remapper, upem);

    // ToUnicode CMap so PDF search, copy/paste, and accessibility readers
    // can recover the original codepoints from the Identity-H glyph stream.
    // Without it Identity-H fonts are write-only from the reader's
    // perspective (PDF Reference §9.10.3).
    let to_unicode_id = build_to_unicode_cmap(&mut doc, &remapper, &glyphs);

    let type0_id = build_type0_font(&mut doc, &font, &remapper, &subset_bytes, to_unicode_id)?;

    // Shared ExtGState carrying the fill/stroke alpha.
    let mut gs_dict = Dictionary::new();
    gs_dict.set("Type", Object::Name(b"ExtGState".to_vec()));
    gs_dict.set("ca", Object::Real(opacity));
    gs_dict.set("CA", Object::Real(opacity));
    let gs_id = doc.add_object(gs_dict);

    let page_ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    if page_ids.is_empty() {
        return Err(VeilError::InvalidInput("PDF has no pages".into()));
    }

    for page_id in page_ids {
        let (page_w, page_h) = page_dimensions(&doc, page_id);
        let cx = page_w / 2.0;
        let cy = page_h / 2.0;

        let theta = options.rotation_deg.to_radians();
        let c = theta.cos();
        let s = theta.sin();

        let mut ops: Vec<Operation> = Vec::with_capacity(10);
        ops.push(Operation::new("q", vec![]));
        // CTM rows are [a b c d e f]; PDF reference §8.3.4. We translate to
        // the page centre after rotating so the text origin sits at (cx,cy).
        ops.push(Operation::new(
            "cm",
            vec![
                Object::Real(c),
                Object::Real(s),
                Object::Real(-s),
                Object::Real(c),
                Object::Real(cx),
                Object::Real(cy),
            ],
        ));
        ops.push(Operation::new(
            "gs",
            vec![Object::Name(b"VeilGs1".to_vec())],
        ));
        let col = &options.color;
        ops.push(Operation::new(
            "rg",
            vec![
                Object::Real(col.r.clamp(0.0, 1.0)),
                Object::Real(col.g.clamp(0.0, 1.0)),
                Object::Real(col.b.clamp(0.0, 1.0)),
            ],
        ));
        ops.push(Operation::new("BT", vec![]));
        ops.push(Operation::new(
            "Tf",
            vec![Object::Name(b"VeilF1".to_vec()), Object::Real(font_size)],
        ));
        ops.push(Operation::new(
            "Td",
            vec![
                Object::Real(-text_width / 2.0),
                Object::Real(-text_height / 2.0),
            ],
        ));
        // Identity-H glyph stream via TJ. Each glyph occupies 2 bytes
        // (big-endian GID) inside hex strings; numeric entries between
        // strings are the GPOS kerning deltas in 1/1000 em — PDF
        // subtracts them from the running text-matrix x, so a positive
        // entry pulls the next glyph closer to the previous one.
        ops.push(Operation::new("TJ", vec![Object::Array(tj_array.clone())]));
        ops.push(Operation::new("ET", vec![]));
        ops.push(Operation::new("Q", vec![]));

        let content_bytes = Content { operations: ops }
            .encode()
            .map_err(VeilError::PdfError)?;
        let watermark_stream = Stream::new(Dictionary::new(), content_bytes);
        let watermark_id = doc.add_object(watermark_stream);

        append_to_page_contents(&mut doc, page_id, watermark_id)?;
        ensure_page_resources(&mut doc, page_id, type0_id, gs_id)?;
    }

    doc.compress();
    // Bug-class #1: lopdf leaves the stale /Prev xref offset on the trailer
    // even after rewriting the table. Stripping it before save_to() prevents
    // some readers from following a dangling reference back into the old
    // structure.
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    Ok(buf)
}

// --- Font construction ------------------------------------------------------

/// Build the Type0 / CIDFontType2 / FontFile2 chain and return the
/// Type0 object id (the one we reference from page `/Font` dicts).
///
/// Structure:
/// ```text
/// Type0 → DescendantFonts[CIDFontType2] → FontDescriptor → FontFile2 (TTF stream)
/// ```
///
/// `subset_bytes` is the already-subsetted TTF; `remapper` carries the
/// original-GID → subset-GID mapping so we can width-key the /W array
/// using subset GIDs (CIDs under Identity-H / `CIDToGIDMap /Identity`).
/// `to_unicode_id` references the CMap stream that lets PDF readers
/// recover the source codepoints for search / copy / accessibility.
fn build_type0_font(
    doc: &mut Document,
    font: &EmbeddedFont<'_>,
    remapper: &GlyphRemapper,
    subset_bytes: &[u8],
    to_unicode_id: ObjectId,
) -> Result<ObjectId> {
    let metrics = font.metrics();
    let ps_name = pdf_name(&metrics.postscript_name);

    // /FontFile2 — subsetted TTF bytes. `/Length1` is the uncompressed
    // byte length, required by the spec so readers can reconstruct the
    // table directory even after our final `doc.compress()` pass.
    let mut ff2_dict = Dictionary::new();
    ff2_dict.set("Length1", Object::Integer(subset_bytes.len() as i64));
    let font_file_id = doc.add_object(Stream::new(ff2_dict, subset_bytes.to_vec()));

    // /FontDescriptor
    let bbox = metrics.bbox;
    let mut descriptor = Dictionary::new();
    descriptor.set("Type", Object::Name(b"FontDescriptor".to_vec()));
    descriptor.set("FontName", Object::Name(ps_name.clone()));
    descriptor.set("Flags", Object::Integer(metrics.flags as i64));
    descriptor.set(
        "FontBBox",
        Object::Array(vec![
            Object::Integer(bbox.0 as i64),
            Object::Integer(bbox.1 as i64),
            Object::Integer(bbox.2 as i64),
            Object::Integer(bbox.3 as i64),
        ]),
    );
    descriptor.set("ItalicAngle", Object::Real(metrics.italic_angle));
    descriptor.set("Ascent", Object::Integer(metrics.ascent as i64));
    descriptor.set("Descent", Object::Integer(metrics.descent as i64));
    descriptor.set("CapHeight", Object::Integer(metrics.cap_height as i64));
    descriptor.set("StemV", Object::Integer(metrics.stem_v as i64));
    descriptor.set("FontFile2", Object::Reference(font_file_id));
    let descriptor_id = doc.add_object(descriptor);

    // /W array — emit one entry per remapped glyph. Widths read from the
    // original font's hmtx (the glyph is identical, only the index moved),
    // keyed by the new subset GID. Width is in 1/units_per_em and PDF
    // expects 1/1000-em, so scale per the spec.
    let upem = metrics.units_per_em.max(1) as f32;
    let mut w_pairs: Vec<(u16, i64)> = remapper
        .remapped_gids()
        .enumerate()
        .map(|(new_gid, original_gid)| {
            let raw = font.advance(original_gid) as f32;
            let scaled = (raw * 1000.0 / upem).round() as i64;
            (new_gid as u16, scaled)
        })
        .collect();
    w_pairs.sort_by_key(|&(gid, _)| gid);
    let mut w_array: Vec<Object> = Vec::with_capacity(w_pairs.len() * 2);
    for (new_gid, scaled) in w_pairs {
        w_array.push(Object::Integer(new_gid as i64));
        w_array.push(Object::Array(vec![Object::Integer(scaled)]));
    }

    // /CIDSystemInfo for Identity-H is the standard Adobe Identity entry.
    let mut cid_system_info = Dictionary::new();
    cid_system_info.set(
        "Registry",
        Object::String(b"Adobe".to_vec(), StringFormat::Literal),
    );
    cid_system_info.set(
        "Ordering",
        Object::String(b"Identity".to_vec(), StringFormat::Literal),
    );
    cid_system_info.set("Supplement", Object::Integer(0));

    // CIDFontType2 — the descendant font.
    let mut cid_font = Dictionary::new();
    cid_font.set("Type", Object::Name(b"Font".to_vec()));
    cid_font.set("Subtype", Object::Name(b"CIDFontType2".to_vec()));
    cid_font.set("BaseFont", Object::Name(ps_name.clone()));
    cid_font.set("CIDSystemInfo", Object::Dictionary(cid_system_info));
    cid_font.set("FontDescriptor", Object::Reference(descriptor_id));
    cid_font.set("CIDToGIDMap", Object::Name(b"Identity".to_vec()));
    cid_font.set("DW", Object::Integer(1000));
    cid_font.set("W", Object::Array(w_array));
    let cid_font_id = doc.add_object(cid_font);

    // Type0 wrapper — what page resources actually reference.
    let mut type0 = Dictionary::new();
    type0.set("Type", Object::Name(b"Font".to_vec()));
    type0.set("Subtype", Object::Name(b"Type0".to_vec()));
    type0.set("BaseFont", Object::Name(ps_name));
    type0.set("Encoding", Object::Name(b"Identity-H".to_vec()));
    type0.set(
        "DescendantFonts",
        Object::Array(vec![Object::Reference(cid_font_id)]),
    );
    type0.set("ToUnicode", Object::Reference(to_unicode_id));
    Ok(doc.add_object(type0))
}

/// Build a `/ToUnicode` CMap stream that maps subset GIDs back to the
/// source codepoints, then return its object id.
///
/// Why this exists: an Identity-H font where the character codes are
/// glyph indices is write-only from a reader's perspective — there's
/// no built-in path from a glyph back to its Unicode source. PDF
/// readers rely on the `/ToUnicode` CMap to make text searchable,
/// copy-pasteable, and accessible (PDF Reference §9.10.3). Without
/// it, watermarked PDFs are visually correct but treated as opaque
/// glyph soup by every downstream tool.
///
/// The CMap format is a tiny PostScript-flavoured DSL. For each used
/// glyph we emit one `bfchar` entry: `<subset-gid-hex> <codepoint-hex>`.
/// Codepoints outside the BMP are encoded as UTF-16 surrogate pairs
/// per the CMap spec (`<HHHHHHHH>`, two big-endian UTF-16 code units).
fn build_to_unicode_cmap(
    doc: &mut Document,
    remapper: &GlyphRemapper,
    glyphs: &[ShapedGlyph],
) -> ObjectId {
    // Walk the shaped glyph stream and keep the first-seen cluster
    // mapping for each subset GID. Glyphs with an empty `source_chars`
    // (non-first glyph of a cluster — e.g. mark stacks) are skipped:
    // the CMap only needs one bfchar entry per cluster.
    //
    // If a glyph is reused for multiple distinct clusters (rare —
    // ligatures aside, most fonts don't share GIDs across characters),
    // the first-seen mapping wins. Roundtrip is lossy for the
    // duplicates rather than ambiguous.
    let mut entries: std::collections::BTreeMap<u16, Vec<char>> = std::collections::BTreeMap::new();
    for g in glyphs {
        if g.source_chars.is_empty() {
            continue;
        }
        let Some(new_gid) = remapper.get(g.gid) else {
            continue;
        };
        entries
            .entry(new_gid)
            .or_insert_with(|| g.source_chars.clone());
    }
    let pairs: Vec<(u16, Vec<char>)> = entries.into_iter().collect();

    // CMap `bfchar` blocks have a 100-entry limit per the PDF spec.
    let mut body = String::new();
    for chunk in pairs.chunks(100) {
        body.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (new_gid, chars) in chunk {
            body.push_str(&format!("<{:04X}> {}\n", new_gid, unicode_hex_multi(chars)));
        }
        body.push_str("endbfchar\n");
    }

    let cmap = format!(
        "/CIDInit /ProcSet findresource begin\n\
         12 dict begin\n\
         begincmap\n\
         /CIDSystemInfo\n\
         <</Registry (Adobe)\n/Ordering (UCS)\n/Supplement 0\n>> def\n\
         /CMapName /Adobe-Identity-UCS def\n\
         /CMapType 2 def\n\
         1 begincodespacerange\n\
         <0000> <FFFF>\n\
         endcodespacerange\n\
         {body}\
         endcmap\n\
         CMapName currentdict /CMap defineresource pop\n\
         end\n\
         end\n"
    );

    doc.add_object(Stream::new(Dictionary::new(), cmap.into_bytes()))
}

/// Encode one or more codepoints as a single CMap `<HHHH…>` token —
/// concatenation of each codepoint's UTF-16BE hex inside one set of
/// angle brackets. Single-char clusters get 4 hex digits (BMP) or 8
/// (UTF-16 surrogate pair for supplementary planes). Multi-char
/// clusters cover ligatures and other shape-time merges (e.g. fi, ﻻ).
fn unicode_hex_multi(chars: &[char]) -> String {
    let mut buf = [0u16; 2];
    let mut out = String::with_capacity(2 + chars.len() * 4);
    out.push('<');
    for &ch in chars {
        let units = ch.encode_utf16(&mut buf);
        for &u in units.iter() {
            out.push_str(&format!("{:04X}", u));
        }
    }
    out.push('>');
    out
}

/// PDF name keys forbid the literal `#` and several whitespace/delimiter
/// bytes; the simplest safe transform replaces anything outside the
/// printable-ASCII letter / digit / dash / underscore set with `_`.
fn pdf_name(raw: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for b in raw.bytes() {
        let ok = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'+';
        out.push(if ok { b } else { b'_' });
    }
    if out.is_empty() {
        out.extend_from_slice(b"Embedded");
    }
    out
}

// --- Text encoding / measurement -------------------------------------------

/// Build the operand array for the `TJ` operator: alternating hex
/// strings (concatenated 2-byte big-endian subset GIDs) and integer
/// kerning deltas (in 1/1000 em, the PDF spec's text-space unit). A
/// glyph whose post-shape `x_advance` matches the font's hmtx width
/// for its original GID contributes only to the current string; the
/// non-zero delta breaks the string and inserts a numeric adjustment.
///
/// Why: PDF's `Tj` only advances by `/W`-supplied widths (which we
/// populate from hmtx). When rustybuzz applied GPOS kerning, the
/// real visual advance differs — without a TJ adjustment the watermark
/// would track unshaped widths and drift glyph-by-glyph relative to
/// the layout rustybuzz computed.
fn build_tj_array(
    font: &EmbeddedFont<'_>,
    glyphs: &[ShapedGlyph],
    remapper: &GlyphRemapper,
    upem: f32,
) -> Vec<Object> {
    let mut tj_array: Vec<Object> = Vec::new();
    let mut bytes: Vec<u8> = Vec::with_capacity(glyphs.len() * 2);

    for glyph in glyphs {
        let new_gid = remapper.get(glyph.gid).unwrap_or(0);
        bytes.push((new_gid >> 8) as u8);
        bytes.push((new_gid & 0xff) as u8);

        let default_advance = font.advance(glyph.gid) as i32;
        let delta_du = default_advance - glyph.x_advance;
        if delta_du == 0 {
            continue;
        }
        // PDF subtracts (delta / 1000) * font_size from the running x,
        // so a POSITIVE delta narrows the gap to the next glyph — which
        // is exactly the sign of `default - actual` we already have.
        let delta_thousandths = (delta_du as f32 * 1000.0 / upem).round() as i64;
        if delta_thousandths == 0 {
            continue;
        }
        tj_array.push(Object::String(
            std::mem::take(&mut bytes),
            StringFormat::Hexadecimal,
        ));
        tj_array.push(Object::Integer(delta_thousandths));
    }

    if !bytes.is_empty() {
        tj_array.push(Object::String(bytes, StringFormat::Hexadecimal));
    }

    tj_array
}

// --- Page plumbing (unchanged from Phase 3.5) ------------------------------

/// Resolve the page's `/MediaBox`, walking `/Parent` until found. Returns
/// US Letter dimensions if nothing resolves — real input from the macOS
/// app passes through `PDFDocumentGuard` first and always has a MediaBox.
fn page_dimensions(doc: &Document, page_id: ObjectId) -> (f32, f32) {
    let mut current = page_id;
    for _ in 0..MAX_PARENT_HOPS {
        let dict = match doc.get_dictionary(current) {
            Ok(d) => d,
            Err(_) => break,
        };
        if let Ok(mb) = dict.get(b"MediaBox") {
            if let Ok(arr) = mb.as_array() {
                if arr.len() == 4 {
                    let llx = num_or_zero(&arr[0]);
                    let lly = num_or_zero(&arr[1]);
                    let urx = num_or_zero(&arr[2]);
                    let ury = num_or_zero(&arr[3]);
                    let w = (urx - llx).abs();
                    let h = (ury - lly).abs();
                    if w > 0.0 && h > 0.0 {
                        return (w, h);
                    }
                }
            }
        }
        match dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok()) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    (DEFAULT_PAGE_W, DEFAULT_PAGE_H)
}

fn num_or_zero(o: &Object) -> f32 {
    match o {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => 0.0,
    }
}

/// Append `watermark_id` to a page's content stream, wrapping any existing
/// content in `q ... Q` to defend against unbalanced graphics-state ops
/// upstream.
fn append_to_page_contents(
    doc: &mut Document,
    page_id: ObjectId,
    watermark_id: ObjectId,
) -> Result<()> {
    let existing = doc
        .get_dictionary(page_id)
        .map_err(VeilError::PdfError)?
        .get(b"Contents")
        .ok()
        .cloned();

    let q_id = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
    let q_close_id = doc.add_object(Stream::new(Dictionary::new(), b"Q\n".to_vec()));

    let mut new_contents: Vec<Object> = Vec::new();
    new_contents.push(Object::Reference(q_id));
    match existing {
        Some(Object::Reference(id)) => new_contents.push(Object::Reference(id)),
        Some(Object::Array(arr)) => new_contents.extend(arr),
        _ => {}
    }
    new_contents.push(Object::Reference(q_close_id));
    new_contents.push(Object::Reference(watermark_id));

    let page_dict = doc
        .get_dictionary_mut(page_id)
        .map_err(VeilError::PdfError)?;
    page_dict.set("Contents", Object::Array(new_contents));
    Ok(())
}

/// Localise a `/Resources` dict on the page (cloning inherited entries if
/// needed) and inject our `/VeilF1` font and `/VeilGs1` graphics-state
/// references.
fn ensure_page_resources(
    doc: &mut Document,
    page_id: ObjectId,
    font_id: ObjectId,
    gs_id: ObjectId,
) -> Result<()> {
    let mut resources = resolve_resources(doc, page_id);

    let mut font_dict = match resources.get(b"Font") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Reference(id)) => doc.get_dictionary(*id).cloned().unwrap_or_default(),
        _ => Dictionary::new(),
    };
    font_dict.set("VeilF1", Object::Reference(font_id));
    resources.set("Font", Object::Dictionary(font_dict));

    let mut gs_dict = match resources.get(b"ExtGState") {
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Reference(id)) => doc.get_dictionary(*id).cloned().unwrap_or_default(),
        _ => Dictionary::new(),
    };
    gs_dict.set("VeilGs1", Object::Reference(gs_id));
    resources.set("ExtGState", Object::Dictionary(gs_dict));

    let page_dict = doc
        .get_dictionary_mut(page_id)
        .map_err(VeilError::PdfError)?;
    page_dict.set("Resources", Object::Dictionary(resources));
    Ok(())
}

fn resolve_resources(doc: &Document, page_id: ObjectId) -> Dictionary {
    let mut current = page_id;
    for _ in 0..MAX_PARENT_HOPS {
        let dict = match doc.get_dictionary(current) {
            Ok(d) => d,
            Err(_) => break,
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
        match dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok()) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    Dictionary::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inter_bytes() -> Vec<u8> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fonts/Inter-Bold.ttf"
        );
        std::fs::read(path).expect("Inter-Bold.ttf test fixture must exist")
    }

    #[test]
    fn rejects_empty_text() {
        let opts = WatermarkOptions {
            text: String::new(),
            font_size: 48.0,
            color: WatermarkColor {
                r: 0.5,
                g: 0.5,
                b: 0.5,
            },
            opacity: 0.3,
            rotation_deg: -45.0,
        };
        let result = apply_text_watermark(&[], &opts, &inter_bytes());
        assert!(result.is_err());
    }

    #[test]
    fn rejects_empty_font() {
        let opts = WatermarkOptions {
            text: "hello".into(),
            font_size: 48.0,
            color: WatermarkColor {
                r: 0.5,
                g: 0.5,
                b: 0.5,
            },
            opacity: 0.3,
            rotation_deg: -45.0,
        };
        let result = apply_text_watermark(&[], &opts, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn unicode_hex_encodes_bmp_and_supplementary() {
        // BMP codepoints fit in a single UTF-16 code unit (4 hex digits).
        assert_eq!(unicode_hex_multi(&['A']), "<0041>");
        assert_eq!(unicode_hex_multi(&['ä']), "<00E4>");
        assert_eq!(unicode_hex_multi(&['ё']), "<0451>");
        // Supplementary-plane codepoints encode as a UTF-16 surrogate
        // pair (8 hex digits). U+1F600 (😀) → D83D DE00.
        assert_eq!(unicode_hex_multi(&['\u{1F600}']), "<D83DDE00>");
    }

    #[test]
    fn unicode_hex_multi_concatenates_cluster_codepoints() {
        // A two-char cluster ("fi" ligature → one glyph) emits both
        // codepoints inside one set of angle brackets.
        assert_eq!(unicode_hex_multi(&['f', 'i']), "<00660069>");
        // An empty cluster collapses to an empty token; callers should
        // skip glyphs with no source chars, but the helper is safe.
        assert_eq!(unicode_hex_multi(&[]), "<>");
    }
}
