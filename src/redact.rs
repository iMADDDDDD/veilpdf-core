//! Apply redaction rectangles without rasterizing whole pages.
//!
//! The implementation keeps each page as PDF content streams, appends
//! vector black rectangles above the marked regions, and removes
//! text-showing glyph bytes whose approximate glyph boxes are substantially
//! covered by those regions. This preserves selectable text outside the
//! redaction boxes, unlike the old macOS-side page rasterization path.

use crate::limits::check_object_count;
use crate::{Result, VeilError};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, StringFormat};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub struct RedactionRect {
    pub page_index: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Rect {
    fn from_xywh(x: f32, y: f32, width: f32, height: f32) -> Option<Self> {
        if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
            return None;
        }
        if width.abs() <= f32::EPSILON || height.abs() <= f32::EPSILON {
            return None;
        }
        let x2 = x + width;
        let y2 = y + height;
        Some(Self {
            min_x: x.min(x2),
            min_y: y.min(y2),
            max_x: x.max(x2),
            max_y: y.max(y2),
        })
    }

    fn width(self) -> f32 {
        self.max_x - self.min_x
    }

    fn height(self) -> f32 {
        self.max_y - self.min_y
    }

    fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
    }

    fn area(self) -> f32 {
        self.width().max(0.0) * self.height().max(0.0)
    }

    fn intersection_area(self, other: Self) -> f32 {
        let width = (self.max_x.min(other.max_x) - self.min_x.max(other.min_x)).max(0.0);
        let height = (self.max_y.min(other.max_y) - self.min_y.max(other.min_y)).max(0.0);
        width * height
    }

    fn contains_point(self, x: f32, y: f32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    fn substantially_covers_glyph(self, glyph: Self) -> bool {
        if !self.intersects(glyph) {
            return false;
        }

        let center_x = (glyph.min_x + glyph.max_x) / 2.0;
        let center_y = (glyph.min_y + glyph.max_y) / 2.0;
        if self.contains_point(center_x, center_y) {
            return true;
        }

        let glyph_area = glyph.area();
        glyph_area > 0.001 && self.intersection_area(glyph) / glyph_area >= 0.35
    }
}

#[derive(Clone, Copy, Debug)]
struct Matrix([f32; 6]);

impl Matrix {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn translate(tx: f32, ty: f32) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, tx, ty])
    }

    fn transform_point(self, x: f32, y: f32) -> (f32, f32) {
        let m = self.0;
        (x * m[0] + y * m[2] + m[4], x * m[1] + y * m[3] + m[5])
    }

    /// PDF row-vector convention: after concatenating an operator matrix,
    /// the new matrix is op * old.
    fn concat(self, old: Self) -> Self {
        let op = self.0;
        let old = old.0;
        Self([
            op[0] * old[0] + op[1] * old[2],
            op[0] * old[1] + op[1] * old[3],
            op[2] * old[0] + op[3] * old[2],
            op[2] * old[1] + op[3] * old[3],
            op[4] * old[0] + op[5] * old[2] + old[4],
            op[4] * old[1] + op[5] * old[3] + old[5],
        ])
    }
}

#[derive(Clone, Debug)]
struct FontInfo {
    kind: FontKind,
    widths: HashMap<u16, f32>,
    default_width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontKind {
    Simple,
    Type0,
}

impl FontInfo {
    fn fallback() -> Self {
        Self {
            kind: FontKind::Simple,
            widths: HashMap::new(),
            default_width: 500.0,
        }
    }

    fn code_unit_len(&self) -> usize {
        match self.kind {
            FontKind::Simple => 1,
            FontKind::Type0 => 2,
        }
    }

    fn width(&self, code: u16) -> f32 {
        self.widths
            .get(&code)
            .copied()
            .unwrap_or(self.default_width)
            .max(0.0)
    }
}

#[derive(Clone, Debug)]
struct TextState {
    text_matrix: Matrix,
    line_matrix: Matrix,
    font_name: Vec<u8>,
    font_size: f32,
    char_spacing: f32,
    word_spacing: f32,
    horizontal_scale: f32,
    leading: f32,
}

impl TextState {
    fn new() -> Self {
        Self {
            text_matrix: Matrix::IDENTITY,
            line_matrix: Matrix::IDENTITY,
            font_name: Vec::new(),
            font_size: 12.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 1.0,
            leading: 0.0,
        }
    }

    fn reset_text_object(&mut self) {
        self.text_matrix = Matrix::IDENTITY;
        self.line_matrix = Matrix::IDENTITY;
    }

    fn move_text_position(&mut self, tx: f32, ty: f32) {
        self.line_matrix = Matrix::translate(tx, ty).concat(self.line_matrix);
        self.text_matrix = self.line_matrix;
    }

    fn next_line(&mut self) {
        self.move_text_position(0.0, -self.leading);
    }

    fn advance(&mut self, dx: f32) {
        self.text_matrix = Matrix::translate(dx, 0.0).concat(self.text_matrix);
    }
}

#[derive(Clone, Debug)]
struct GraphicsState {
    ctm: Matrix,
}

/// Apply redactions to `data` and return re-serialized PDF bytes.
pub fn apply_redactions(data: &[u8], redactions: &[RedactionRect]) -> Result<Vec<u8>> {
    if redactions.is_empty() {
        return Err(VeilError::InvalidInput("no redactions to apply".into()));
    }

    let mut grouped: HashMap<usize, Vec<Rect>> = HashMap::new();
    for redaction in redactions {
        let Some(rect) =
            Rect::from_xywh(redaction.x, redaction.y, redaction.width, redaction.height)
        else {
            continue;
        };
        grouped.entry(redaction.page_index).or_default().push(rect);
    }
    if grouped.is_empty() {
        return Err(VeilError::InvalidInput(
            "no valid redaction rectangles".into(),
        ));
    }

    let mut doc = Document::load_mem(data)?;
    check_object_count(&doc)?;

    if doc.trailer.get(b"Encrypt").is_ok() {
        return Err(VeilError::InvalidInput(
            "Encrypted/password-protected PDFs are not supported".into(),
        ));
    }

    let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    if pages.is_empty() {
        return Err(VeilError::InvalidInput("PDF has no pages".into()));
    }

    for (page_index, rects) in grouped {
        let Some(page_id) = pages.get(page_index).copied() else {
            continue;
        };
        apply_page_redactions(&mut doc, page_id, &rects)?;
    }

    doc.compress();
    doc.trailer.remove(b"Prev");
    doc.trailer.remove(b"XRefStm");

    let mut buf = Vec::new();
    doc.save_to(&mut buf)?;
    Ok(buf)
}

fn apply_page_redactions(doc: &mut Document, page_id: ObjectId, rects: &[Rect]) -> Result<()> {
    let overlay_id = build_overlay_stream(doc, rects)?;

    let content_bytes = match doc.get_page_content(page_id) {
        Ok(bytes) => bytes,
        Err(_) => {
            append_overlay_to_page(doc, page_id, overlay_id)?;
            return Ok(());
        }
    };

    let content = match Content::decode(&content_bytes) {
        Ok(content) => content,
        Err(_) => {
            append_overlay_to_page(doc, page_id, overlay_id)?;
            return Ok(());
        }
    };

    let fonts = resolve_page_fonts(doc, page_id);
    let redacted = redact_content(content, rects, &fonts)?;
    let redacted_id = doc.add_object(Stream::new(Dictionary::new(), redacted.encode()?));

    let page = doc.get_dictionary_mut(page_id)?;
    page.set(
        "Contents",
        Object::Array(vec![
            Object::Reference(redacted_id),
            Object::Reference(overlay_id),
        ]),
    );
    Ok(())
}

fn build_overlay_stream(doc: &mut Document, rects: &[Rect]) -> Result<ObjectId> {
    let mut ops = Vec::with_capacity(rects.len() * 2 + 4);
    ops.push(Operation::new("q", vec![]));
    ops.push(Operation::new(
        "rg",
        vec![Object::Real(0.0), Object::Real(0.0), Object::Real(0.0)],
    ));
    for rect in rects {
        ops.push(Operation::new(
            "re",
            vec![
                Object::Real(rect.min_x),
                Object::Real(rect.min_y),
                Object::Real(rect.width()),
                Object::Real(rect.height()),
            ],
        ));
        ops.push(Operation::new("f", vec![]));
    }
    ops.push(Operation::new("Q", vec![]));

    let stream = Stream::new(Dictionary::new(), Content { operations: ops }.encode()?);
    Ok(doc.add_object(stream))
}

fn append_overlay_to_page(
    doc: &mut Document,
    page_id: ObjectId,
    overlay_id: ObjectId,
) -> Result<()> {
    let existing = doc.get_dictionary(page_id)?.get(b"Contents").ok().cloned();

    let mut contents = Vec::new();
    match existing {
        Some(Object::Array(items)) => contents.extend(items),
        Some(Object::Reference(id)) => contents.push(Object::Reference(id)),
        Some(other) => contents.push(other),
        None => {}
    }
    contents.push(Object::Reference(overlay_id));

    let page = doc.get_dictionary_mut(page_id)?;
    page.set("Contents", Object::Array(contents));
    Ok(())
}

fn redact_content(
    content: Content,
    rects: &[Rect],
    fonts: &HashMap<Vec<u8>, FontInfo>,
) -> Result<Content> {
    let mut output = Vec::with_capacity(content.operations.len());
    let mut graphics = GraphicsState {
        ctm: Matrix::IDENTITY,
    };
    let mut graphics_stack: Vec<GraphicsState> = Vec::new();
    let mut text = TextState::new();

    for op in content.operations {
        match op.operator.as_str() {
            "q" => {
                graphics_stack.push(graphics.clone());
                output.push(op);
            }
            "Q" => {
                if let Some(previous) = graphics_stack.pop() {
                    graphics = previous;
                }
                output.push(op);
            }
            "cm" => {
                if let Some(matrix) = read_matrix(&op.operands) {
                    graphics.ctm = matrix.concat(graphics.ctm);
                }
                output.push(op);
            }
            "BT" => {
                text.reset_text_object();
                output.push(op);
            }
            "ET" => output.push(op),
            "Tf" => {
                if let (Some(Object::Name(name)), Some(size)) =
                    (op.operands.first(), op.operands.get(1).and_then(num_f32))
                {
                    text.font_name = name.clone();
                    text.font_size = size.max(0.1);
                }
                output.push(op);
            }
            "Tc" => {
                if let Some(value) = op.operands.first().and_then(num_f32) {
                    text.char_spacing = value;
                }
                output.push(op);
            }
            "Tw" => {
                if let Some(value) = op.operands.first().and_then(num_f32) {
                    text.word_spacing = value;
                }
                output.push(op);
            }
            "Tz" => {
                if let Some(value) = op.operands.first().and_then(num_f32) {
                    text.horizontal_scale = (value / 100.0).max(0.01);
                }
                output.push(op);
            }
            "TL" => {
                if let Some(value) = op.operands.first().and_then(num_f32) {
                    text.leading = value;
                }
                output.push(op);
            }
            "Td" => {
                if let (Some(tx), Some(ty)) = (
                    op.operands.first().and_then(num_f32),
                    op.operands.get(1).and_then(num_f32),
                ) {
                    text.move_text_position(tx, ty);
                }
                output.push(op);
            }
            "TD" => {
                if let (Some(tx), Some(ty)) = (
                    op.operands.first().and_then(num_f32),
                    op.operands.get(1).and_then(num_f32),
                ) {
                    text.leading = -ty;
                    text.move_text_position(tx, ty);
                }
                output.push(op);
            }
            "Tm" => {
                if let Some(matrix) = read_matrix(&op.operands) {
                    text.text_matrix = matrix;
                    text.line_matrix = matrix;
                }
                output.push(op);
            }
            "T*" => {
                text.next_line();
                output.push(op);
            }
            "Tj" => {
                let font = fonts
                    .get(&text.font_name)
                    .cloned()
                    .unwrap_or_else(FontInfo::fallback);
                let replacement = redact_tj_operation(&op, &mut text, &graphics, rects, &font);
                output.extend(replacement);
            }
            "TJ" => {
                let font = fonts
                    .get(&text.font_name)
                    .cloned()
                    .unwrap_or_else(FontInfo::fallback);
                let replacement =
                    redact_tj_array_operation(&op, &mut text, &graphics, rects, &font);
                output.extend(replacement);
            }
            "'" => {
                text.next_line();
                output.push(Operation::new("T*", vec![]));
                let font = fonts
                    .get(&text.font_name)
                    .cloned()
                    .unwrap_or_else(FontInfo::fallback);
                let replacement = redact_quote_operation(&op, &mut text, &graphics, rects, &font);
                output.extend(replacement);
            }
            "\"" => {
                if let Some(value) = op.operands.first().and_then(num_f32) {
                    text.word_spacing = value;
                    output.push(Operation::new("Tw", vec![Object::Real(value)]));
                }
                if let Some(value) = op.operands.get(1).and_then(num_f32) {
                    text.char_spacing = value;
                    output.push(Operation::new("Tc", vec![Object::Real(value)]));
                }
                text.next_line();
                output.push(Operation::new("T*", vec![]));
                let font = fonts
                    .get(&text.font_name)
                    .cloned()
                    .unwrap_or_else(FontInfo::fallback);
                let replacement =
                    redact_double_quote_operation(&op, &mut text, &graphics, rects, &font);
                output.extend(replacement);
            }
            _ => output.push(op),
        }
    }

    Ok(Content { operations: output })
}

fn redact_quote_operation(
    op: &Operation,
    text: &mut TextState,
    graphics: &GraphicsState,
    rects: &[Rect],
    font: &FontInfo,
) -> Vec<Operation> {
    let Some(Object::String(bytes, format)) = op.operands.first() else {
        return vec![op.clone()];
    };
    redact_text_bytes(bytes, *format, text, graphics, rects, font)
}

fn redact_double_quote_operation(
    op: &Operation,
    text: &mut TextState,
    graphics: &GraphicsState,
    rects: &[Rect],
    font: &FontInfo,
) -> Vec<Operation> {
    let Some(Object::String(bytes, format)) = op.operands.get(2) else {
        return vec![op.clone()];
    };
    redact_text_bytes(bytes, *format, text, graphics, rects, font)
}

fn redact_tj_operation(
    op: &Operation,
    text: &mut TextState,
    graphics: &GraphicsState,
    rects: &[Rect],
    font: &FontInfo,
) -> Vec<Operation> {
    let Some(Object::String(bytes, format)) = op.operands.first() else {
        return vec![op.clone()];
    };
    let (array, removed, movement) = build_redacted_tj_array(
        std::slice::from_ref(&Object::String(bytes.clone(), *format)),
        text,
        graphics,
        rects,
        font,
    );
    text.advance(movement);
    if !removed {
        return vec![op.clone()];
    }
    vec![Operation::new("TJ", vec![Object::Array(array)])]
}

fn redact_tj_array_operation(
    op: &Operation,
    text: &mut TextState,
    graphics: &GraphicsState,
    rects: &[Rect],
    font: &FontInfo,
) -> Vec<Operation> {
    let Some(Object::Array(items)) = op.operands.first() else {
        return vec![op.clone()];
    };
    let (array, removed, movement) = build_redacted_tj_array(items, text, graphics, rects, font);
    text.advance(movement);
    if !removed {
        return vec![op.clone()];
    }
    vec![Operation::new("TJ", vec![Object::Array(array)])]
}

fn redact_text_bytes(
    bytes: &[u8],
    format: StringFormat,
    text: &mut TextState,
    graphics: &GraphicsState,
    rects: &[Rect],
    font: &FontInfo,
) -> Vec<Operation> {
    let (array, removed, movement) = build_redacted_tj_array(
        std::slice::from_ref(&Object::String(bytes.to_vec(), format)),
        text,
        graphics,
        rects,
        font,
    );
    text.advance(movement);
    if !removed {
        return vec![Operation::new(
            "Tj",
            vec![Object::String(bytes.to_vec(), format)],
        )];
    }
    vec![Operation::new("TJ", vec![Object::Array(array)])]
}

fn build_redacted_tj_array(
    items: &[Object],
    text: &TextState,
    graphics: &GraphicsState,
    rects: &[Rect],
    font: &FontInfo,
) -> (Vec<Object>, bool, f32) {
    let mut array = Vec::new();
    let mut kept_run: Vec<u8> = Vec::new();
    let mut current_text_matrix = text.text_matrix;
    let mut total_movement = 0.0;
    let mut pending_move = 0.0;
    let mut removed_any = false;

    for item in items {
        match item {
            Object::String(bytes, format) => {
                let mut offset = 0;
                while offset < bytes.len() {
                    let (code, end) = read_code_unit(bytes, offset, font.code_unit_len());
                    let glyph_bytes = &bytes[offset..end];
                    let advance = glyph_advance(code, text, font);
                    let glyph_rect =
                        glyph_rect(current_text_matrix, graphics.ctm, text.font_size, advance);
                    let remove = rects
                        .iter()
                        .any(|r| r.substantially_covers_glyph(glyph_rect));

                    if remove {
                        flush_kept_run(&mut array, &mut kept_run, *format);
                        pending_move += advance;
                        removed_any = true;
                    } else {
                        flush_pending_move(&mut array, &mut pending_move, text);
                        kept_run.extend_from_slice(glyph_bytes);
                    }

                    current_text_matrix =
                        Matrix::translate(advance, 0.0).concat(current_text_matrix);
                    total_movement += advance;
                    offset = end;
                }
            }
            Object::Integer(value) => {
                let movement = tj_adjustment_movement(*value as f32, text);
                pending_move += movement;
                current_text_matrix = Matrix::translate(movement, 0.0).concat(current_text_matrix);
                total_movement += movement;
            }
            Object::Real(value) => {
                let movement = tj_adjustment_movement(*value, text);
                pending_move += movement;
                current_text_matrix = Matrix::translate(movement, 0.0).concat(current_text_matrix);
                total_movement += movement;
            }
            _ => {}
        }
    }

    flush_kept_run(&mut array, &mut kept_run, StringFormat::Literal);
    if removed_any {
        flush_pending_move(&mut array, &mut pending_move, text);
    }

    if array.is_empty() && removed_any {
        flush_move(&mut array, total_movement, text);
    }

    (array, removed_any, total_movement)
}

fn read_code_unit(bytes: &[u8], offset: usize, unit_len: usize) -> (u16, usize) {
    if unit_len == 2 && offset + 1 < bytes.len() {
        let code = ((bytes[offset] as u16) << 8) | bytes[offset + 1] as u16;
        return (code, offset + 2);
    }
    (bytes[offset] as u16, offset + 1)
}

fn glyph_advance(code: u16, text: &TextState, font: &FontInfo) -> f32 {
    let mut advance = font.width(code) * text.font_size / 1000.0;
    advance += text.char_spacing;
    if font.kind == FontKind::Simple && code == b' ' as u16 {
        advance += text.word_spacing;
    }
    advance * text.horizontal_scale
}

fn glyph_rect(text_matrix: Matrix, ctm: Matrix, font_size: f32, advance: f32) -> Rect {
    let descent = -0.25 * font_size;
    let ascent = 0.95 * font_size;
    let width = advance.abs().max(font_size * 0.25);
    let matrix = text_matrix.concat(ctm);
    let points = [
        matrix.transform_point(0.0, descent),
        matrix.transform_point(width, descent),
        matrix.transform_point(width, ascent),
        matrix.transform_point(0.0, ascent),
    ];
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (x, y) in points {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    Rect {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

fn flush_kept_run(array: &mut Vec<Object>, kept_run: &mut Vec<u8>, format: StringFormat) {
    if kept_run.is_empty() {
        return;
    }
    array.push(Object::String(std::mem::take(kept_run), format));
}

fn flush_pending_move(array: &mut Vec<Object>, pending_move: &mut f32, text: &TextState) {
    if pending_move.abs() <= 0.001 {
        return;
    }
    flush_move(array, *pending_move, text);
    *pending_move = 0.0;
}

fn flush_move(array: &mut Vec<Object>, movement: f32, text: &TextState) {
    let denom = text.font_size * text.horizontal_scale;
    if denom.abs() <= 0.001 {
        return;
    }
    let adjustment = (-movement * 1000.0 / denom).round();
    if adjustment.abs() > 0.0 {
        array.push(Object::Integer(adjustment as i64));
    }
}

fn tj_adjustment_movement(adjustment: f32, text: &TextState) -> f32 {
    -adjustment * text.font_size * text.horizontal_scale / 1000.0
}

fn resolve_page_fonts(doc: &Document, page_id: ObjectId) -> HashMap<Vec<u8>, FontInfo> {
    let resources = resolve_resources(doc, page_id);
    let font_dict = match resources.get(b"Font") {
        Ok(Object::Dictionary(dict)) => dict.clone(),
        Ok(Object::Reference(id)) => doc.get_dictionary(*id).cloned().unwrap_or_default(),
        _ => Dictionary::new(),
    };

    let mut fonts = HashMap::new();
    for (name, value) in font_dict.iter() {
        if let Some(font) = resolve_font(doc, value) {
            fonts.insert(name.to_vec(), font);
        }
    }
    fonts
}

fn resolve_font(doc: &Document, value: &Object) -> Option<FontInfo> {
    let dict = match value {
        Object::Dictionary(dict) => dict.clone(),
        Object::Reference(id) => doc.get_dictionary(*id).cloned().ok()?,
        _ => return None,
    };

    let subtype = dict
        .get(b"Subtype")
        .ok()
        .and_then(|value| value.as_name().ok())
        .unwrap_or(b"");

    if subtype == b"Type0" {
        return Some(parse_type0_font(doc, &dict));
    }

    Some(parse_simple_font(doc, &dict))
}

fn parse_simple_font(doc: &Document, dict: &Dictionary) -> FontInfo {
    let first_char = dict
        .get(b"FirstChar")
        .ok()
        .and_then(|value| value.as_i64().ok())
        .unwrap_or(0);
    let mut widths = HashMap::new();
    if let Ok(array) = dict.get(b"Widths").and_then(|value| value.as_array()) {
        for (idx, value) in array.iter().enumerate() {
            if let Some(width) = num_f32(value) {
                let code = (first_char + idx as i64).clamp(0, u16::MAX as i64) as u16;
                widths.insert(code, width);
            }
        }
    }

    let default_width = font_descriptor_missing_width(doc, dict).unwrap_or(500.0);

    FontInfo {
        kind: FontKind::Simple,
        widths,
        default_width,
    }
}

fn parse_type0_font(doc: &Document, dict: &Dictionary) -> FontInfo {
    let descendant = dict
        .get(b"DescendantFonts")
        .ok()
        .and_then(|value| value.as_array().ok())
        .and_then(|array| array.first())
        .and_then(|value| match value {
            Object::Dictionary(dict) => Some(dict.clone()),
            Object::Reference(id) => doc.get_dictionary(*id).cloned().ok(),
            _ => None,
        });

    let Some(descendant) = descendant else {
        return FontInfo {
            kind: FontKind::Type0,
            widths: HashMap::new(),
            default_width: 1000.0,
        };
    };

    let default_width = descendant
        .get(b"DW")
        .ok()
        .and_then(num_f32)
        .unwrap_or(1000.0);
    let widths = parse_cid_widths(&descendant);

    FontInfo {
        kind: FontKind::Type0,
        widths,
        default_width,
    }
}

fn parse_cid_widths(dict: &Dictionary) -> HashMap<u16, f32> {
    let mut widths = HashMap::new();
    let Ok(array) = dict.get(b"W").and_then(|value| value.as_array()) else {
        return widths;
    };

    let mut index = 0;
    while index < array.len() {
        let Some(first) = array.get(index).and_then(|value| value.as_i64().ok()) else {
            index += 1;
            continue;
        };
        let Some(next) = array.get(index + 1) else {
            break;
        };

        if let Ok(width_array) = next.as_array() {
            for (offset, value) in width_array.iter().enumerate() {
                if let Some(width) = num_f32(value) {
                    let cid = (first + offset as i64).clamp(0, u16::MAX as i64) as u16;
                    widths.insert(cid, width);
                }
            }
            index += 2;
        } else if let (Some(last), Some(width)) =
            (next.as_i64().ok(), array.get(index + 2).and_then(num_f32))
        {
            let start = first.max(0) as u16;
            let end = last.clamp(first, u16::MAX as i64) as u16;
            for cid in start..=end {
                widths.insert(cid, width);
            }
            index += 3;
        } else {
            index += 1;
        }
    }

    widths
}

fn font_descriptor_missing_width(doc: &Document, dict: &Dictionary) -> Option<f32> {
    let descriptor = dict.get(b"FontDescriptor").ok()?;
    let descriptor = match descriptor {
        Object::Dictionary(dict) => dict.clone(),
        Object::Reference(id) => doc.get_dictionary(*id).cloned().ok()?,
        _ => return None,
    };
    descriptor.get(b"MissingWidth").ok().and_then(num_f32)
}

fn resolve_resources(doc: &Document, page_id: ObjectId) -> Dictionary {
    let mut current = page_id;
    for _ in 0..16 {
        let dict = match doc.get_dictionary(current) {
            Ok(dict) => dict,
            Err(_) => break,
        };
        if let Ok(resources) = dict.get(b"Resources") {
            match resources {
                Object::Dictionary(dict) => return dict.clone(),
                Object::Reference(id) => {
                    if let Ok(dict) = doc.get_dictionary(*id) {
                        return dict.clone();
                    }
                }
                _ => {}
            }
        }
        match dict
            .get(b"Parent")
            .ok()
            .and_then(|value| value.as_reference().ok())
        {
            Some(parent) => current = parent,
            None => break,
        }
    }
    Dictionary::new()
}

fn read_matrix(operands: &[Object]) -> Option<Matrix> {
    if operands.len() != 6 {
        return None;
    }
    let mut matrix = [0.0_f32; 6];
    for (index, value) in operands.iter().enumerate() {
        matrix[index] = num_f32(value)?;
    }
    Some(Matrix(matrix))
}

fn num_f32(value: &Object) -> Option<f32> {
    if let Ok(value) = value.as_float() {
        Some(value)
    } else if let Ok(value) = value.as_i64() {
        Some(value as f32)
    } else {
        None
    }
}
