//! Minimal TTF reader used by the Phase 3.6 watermark path.
//!
//! Wraps `ttf-parser` so the rest of the crate can stay agnostic of
//! that crate's lifetimes and Option-heavy API. We pull just what the
//! watermark module needs to:
//!
//! - encode a `str` as a sequence of glyph indices (Identity-H bytes),
//! - measure that sequence's width for centring math,
//! - emit a usable `/FontDescriptor` + `/W` array next to the embedded
//!   `/FontFile2` stream.
//!
//! Subsetting and text shaping live in later phases — this module is
//! intentionally not the place to grow that complexity.

use crate::VeilError;
use ttf_parser::Face;

/// PDF `/FontDescriptor` flags. We use a minimal subset:
/// * Nonsymbolic (bit 6) — text fonts using a defined character set.
const FLAGS_NONSYMBOLIC: u32 = 1 << 5;

/// Metrics extracted once from the TTF tables and reused when building
/// the PDF font dictionary tree. All distance fields are in the font's
/// own design units (see `units_per_em`).
#[derive(Clone, Debug)]
pub struct FontMetrics {
    pub postscript_name: String,
    pub units_per_em: u16,
    pub ascent: i16,
    pub descent: i16,
    pub cap_height: i16,
    pub italic_angle: f32,
    pub weight_class: u16,
    pub flags: u32,
    pub bbox: (i16, i16, i16, i16),
    pub stem_v: u16,
}

/// Parsed TTF face plus the raw byte slice so we can stream the same
/// bytes into the PDF `/FontFile2` entry without re-parsing.
pub struct EmbeddedFont<'a> {
    pub bytes: &'a [u8],
    face: Face<'a>,
    metrics: FontMetrics,
}

impl<'a> EmbeddedFont<'a> {
    pub fn new(bytes: &'a [u8]) -> crate::Result<Self> {
        let face = Face::parse(bytes, 0)
            .map_err(|_| VeilError::InvalidInput("could not parse watermark TTF".into()))?;
        let metrics = extract_metrics(&face);
        Ok(Self {
            bytes,
            face,
            metrics,
        })
    }

    pub fn metrics(&self) -> &FontMetrics {
        &self.metrics
    }

    /// Maps a Unicode scalar to the font's glyph index, or `None` when
    /// the font has no glyph for it. Callers decide how to handle holes
    /// (the watermark module skips them today; a later phase could
    /// route through `.notdef` to make missing glyphs visible).
    pub fn glyph_id(&self, c: char) -> Option<u16> {
        self.face.glyph_index(c).map(|g| g.0)
    }

    /// Advance width for `gid` in font design units. Returns 0 when the
    /// glyph is absent from the `hmtx` table — which would only happen
    /// on malformed fonts; well-formed TTFs cover every glyph.
    pub fn advance(&self, gid: u16) -> u16 {
        self.face
            .glyph_hor_advance(ttf_parser::GlyphId(gid))
            .unwrap_or(0)
    }
}

fn extract_metrics(face: &Face<'_>) -> FontMetrics {
    // Names table: prefer the PostScript name (id 6). Fall back to a
    // generic placeholder so we never embed an empty `/BaseFont` value.
    let postscript_name = face
        .names()
        .into_iter()
        .find(|n| n.name_id == 6 && n.is_unicode())
        .and_then(|n| n.to_string())
        .unwrap_or_else(|| "Embedded".to_string());

    let units_per_em = face.units_per_em();
    let ascent = face.ascender();
    let descent = face.descender();
    // `OS/2.sCapHeight` is the trustworthy source; fall back to a
    // reasonable approximation when the font omits it.
    let cap_height = face.capital_height().unwrap_or(ascent.saturating_sub(200));
    let italic_angle = face.italic_angle();
    let weight_class = face.weight().to_number();
    let raw_bbox = face.global_bounding_box();
    let bbox = (
        raw_bbox.x_min,
        raw_bbox.y_min,
        raw_bbox.x_max,
        raw_bbox.y_max,
    );

    // StemV isn't exposed by TTF; pick a sensible value by weight.
    // Adobe's recommendation: ~80 for regular, ~120 for bold-ish.
    let stem_v = if weight_class >= 700 { 120 } else { 80 };

    FontMetrics {
        postscript_name,
        units_per_em,
        ascent,
        descent,
        cap_height,
        italic_angle,
        weight_class,
        flags: FLAGS_NONSYMBOLIC,
        bbox,
        stem_v,
    }
}
