//! Bidi-aware text shaping for the Phase 3.8 watermark path.
//!
//! Runs the Unicode Bidirectional Algorithm (UAX #9 via `unicode-bidi`)
//! to segment the input into LTR/RTL runs, then shapes each run with
//! `rustybuzz` (HarfBuzz's shaping algorithm in pure Rust). The output
//! is a flat list of glyphs in *visual* order (left-to-right on the
//! page), ready to encode into the Identity-H content stream.
//!
//! Each `ShapedGlyph` carries:
//! - the original-font GID,
//! - the post-shape horizontal advance in font design units (which may
//!   differ from the `hmtx` width when GPOS kerning runs),
//! - x/y offsets relative to the current text-matrix position,
//! - and the source codepoints that produced its cluster — but only on
//!   the first glyph of each cluster; later glyphs in the same cluster
//!   carry an empty `source_chars` so the ToUnicode CMap emits the
//!   cluster contents once.
//!
//! What this module is *not*: a layout engine. Multi-line wrapping,
//! line breaking, justification, baseline alignment — none of that is
//! in scope. We shape one short string per watermark.

use rustybuzz::{shape, Direction, Face, UnicodeBuffer};
use unicode_bidi::BidiInfo;

/// A single shaped glyph in visual order.
#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    pub gid: u16,
    /// Post-shape advance in font design units. Caller scales by
    /// `font_size / units_per_em` to get PDF points.
    pub x_advance: i32,
    pub x_offset: i32,
    pub y_offset: i32,
    /// Source codepoints for this glyph's cluster. Populated on the
    /// *first* glyph of each cluster only — subsequent glyphs that
    /// share the cluster (e.g. mark stacks above a base) get an
    /// empty Vec. Drives the ToUnicode CMap.
    pub source_chars: Vec<char>,
}

/// Shape `text` against `font_bytes` and return the glyph sequence in
/// visual order. Returns `None` only when the font fails to parse —
/// empty input returns an empty vector.
pub fn shape_text(font_bytes: &[u8], text: &str) -> Option<Vec<ShapedGlyph>> {
    let face = Face::from_slice(font_bytes, 0)?;
    if text.is_empty() {
        return Some(Vec::new());
    }

    let bidi = BidiInfo::new(text, None);
    let mut out: Vec<ShapedGlyph> = Vec::with_capacity(text.len());

    for para in &bidi.paragraphs {
        let para_range = para.range.clone();
        let (_run_levels, run_ranges) = bidi.visual_runs(para, para_range);

        for run_range in run_ranges {
            let level = bidi.levels[run_range.start];
            let direction = if level.is_rtl() {
                Direction::RightToLeft
            } else {
                Direction::LeftToRight
            };
            let run_text = &text[run_range.clone()];
            shape_run_into(&face, run_text, direction, run_range.start, &mut out);
        }
    }

    Some(out)
}

/// Shape a single bidi run and append the result to `out` in visual
/// order. `run_byte_offset` is the start of the run in the original
/// text (for cluster-to-source-char back-translation).
fn shape_run_into(
    face: &Face<'_>,
    run_text: &str,
    direction: Direction,
    run_byte_offset: usize,
    out: &mut Vec<ShapedGlyph>,
) {
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(run_text);
    buffer.set_direction(direction);
    // Skipping explicit set_script — rustybuzz auto-detects from the
    // text content, which is accurate for our short watermark strings
    // and avoids leaking script-tag plumbing into the caller.

    let glyphs = shape(face, &[], buffer);
    let infos = glyphs.glyph_infos();
    let positions = glyphs.glyph_positions();
    debug_assert_eq!(infos.len(), positions.len());

    if infos.is_empty() {
        return;
    }

    // Per-glyph cluster mapping. rustybuzz's `cluster` is a byte index
    // *into the run text*; we translate it to a byte index in the
    // original input via `run_byte_offset` so callers can correlate
    // it back to the source. For the ToUnicode CMap we only need the
    // characters, not the offset, so we materialise them here.
    //
    // For LTR runs, `cluster` is monotonically non-decreasing across
    // glyph_infos. For RTL runs, it's non-INcreasing (the first visual
    // glyph corresponds to the LAST source bytes in the run). Either
    // way, the *unique* cluster indices form the set of source-text
    // anchors; we group glyphs by cluster value, then derive the chars
    // by slicing the run text between adjacent (sorted) cluster
    // boundaries.

    // 1. Collect unique cluster byte indices in sorted order.
    let mut sorted_clusters: Vec<u32> = infos.iter().map(|i| i.cluster).collect();
    sorted_clusters.sort_unstable();
    sorted_clusters.dedup();

    // 2. Build a map: cluster byte index → Vec<char> for that cluster.
    // The cluster covers bytes [c, next_c) in the run text (where
    // next_c is the next sorted cluster index, or run_text.len()).
    let run_bytes = run_text.as_bytes();
    let mut cluster_chars: std::collections::BTreeMap<u32, Vec<char>> =
        std::collections::BTreeMap::new();
    for (i, &c) in sorted_clusters.iter().enumerate() {
        let next = sorted_clusters
            .get(i + 1)
            .copied()
            .unwrap_or(run_bytes.len() as u32);
        let start = c as usize;
        let end = (next as usize).min(run_bytes.len());
        if start >= end {
            cluster_chars.insert(c, Vec::new());
            continue;
        }
        // Run text is guaranteed valid UTF-8 (it's a &str slice of the
        // original input), so this slice is also valid UTF-8 and the
        // .chars() iteration is safe.
        let chars: Vec<char> = run_text[start..end].chars().collect();
        cluster_chars.insert(c, chars);
    }

    // 3. Emit glyphs in shape() order, attributing source chars only
    //    to the FIRST glyph of each cluster we encounter — subsequent
    //    glyphs of the same cluster (e.g. mark stacks) get an empty
    //    Vec so the ToUnicode CMap doesn't double-count.
    let mut seen_clusters: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (info, pos) in infos.iter().zip(positions.iter()) {
        let cluster_key = info.cluster;
        let source_chars = if seen_clusters.insert(cluster_key) {
            cluster_chars.get(&cluster_key).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        out.push(ShapedGlyph {
            gid: info.glyph_id as u16,
            x_advance: pos.x_advance,
            x_offset: pos.x_offset,
            y_offset: pos.y_offset,
            source_chars,
        });
        let _ = run_byte_offset; // currently unused; reserved for future PDF positioning that needs original-text anchors.
    }
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

    fn arabic_bytes() -> Vec<u8> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fonts/NotoSansArabic-Bold.ttf"
        );
        std::fs::read(path).expect("NotoSansArabic-Bold.ttf test fixture must exist")
    }

    #[test]
    fn shapes_latin_to_non_empty_glyphs() {
        let glyphs = shape_text(&inter_bytes(), "Hello").expect("shape ok");
        assert_eq!(glyphs.len(), 5, "one glyph per ASCII char in Inter");
        // First glyph carries its cluster's source chars; others may
        // either carry their own char or empty if Inter merges them.
        assert_eq!(glyphs[0].source_chars, vec!['H']);
        assert!(glyphs.iter().all(|g| g.gid != 0), "no .notdef boxes");
    }

    #[test]
    fn shapes_arabic_into_contextual_forms() {
        // "السلام" — Arabic for "peace". 6 codepoints; rustybuzz applies
        // GSUB to substitute contextual forms (initial/medial/final).
        let text = "السلام";
        let glyphs = shape_text(&arabic_bytes(), text).expect("shape ok");
        // We don't pin the exact glyph count (depends on font version +
        // lam-alef ligature), but it must be > 0 and contain no notdefs.
        assert!(!glyphs.is_empty(), "Arabic must produce glyphs");
        assert!(
            glyphs.iter().all(|g| g.gid != 0),
            "no .notdef boxes for covered text"
        );

        // The cluster source-char attribution must cover every input
        // codepoint at least once across the glyph stream.
        let collected: std::collections::HashSet<char> = glyphs
            .iter()
            .flat_map(|g| g.source_chars.iter().copied())
            .collect();
        for c in text.chars() {
            assert!(
                collected.contains(&c),
                "source char {:?} missing from cluster attribution",
                c
            );
        }
    }

    #[test]
    fn empty_text_returns_empty_glyphs() {
        let glyphs = shape_text(&inter_bytes(), "").expect("shape ok");
        assert!(glyphs.is_empty());
    }
}
