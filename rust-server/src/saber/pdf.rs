//! Renders a parsed Saber note to a PDF, mirroring what the app draws on screen: background
//! colour and ruling, images, highlighter strokes (translucent, darken blend), pen strokes as
//! filled outlines from the perfect-freehand algorithm, pencil strokes as lighter fills, shapes
//! as stroked paths, and typed text as plain Helvetica lines.
//!
//! Not rendered (drawn as labelled placeholders): pages of imported PDFs and SVG images.

use super::freehand::{get_stroke, Vec2};
use super::sbn::{BoxFit, Image, Note, Page, Rect as NoteRect, Shape, Stroke, Tool};
use anyhow::{anyhow, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use pdf_writer::types::{BlendMode, LineCapStyle, LineJoinStyle};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, Str};
use std::collections::HashMap;
use std::io::Write;

/// Flutter `Colors.blue` / `Colors.red`, the primary/secondary colours Saber's exporter theme
/// uses for ruling lines.
const PATTERN_PRIMARY: u32 = 0xff21_96f3;
const PATTERN_SECONDARY: u32 = 0xfff4_4336;
const PATTERN_ALPHA: f32 = 0.2;
/// `Highlighter.alpha` (0-255).
const HIGHLIGHTER_ALPHA: f32 = 100.0 / 255.0;
const DEFAULT_BACKGROUND: u32 = 0xffff_ffff;
const BEZIER_CIRCLE: f64 = 0.552_284_75;
const TEXT_MARGIN_FACTOR: f64 = 1.0;
const TEXT_SIZE_FACTOR: f64 = 0.6;

/// Bytes of the asset files (`<note>.sbn2.<index>`) referenced by the note.
pub type Assets = HashMap<usize, Vec<u8>>;

struct PendingText {
    x: f64,
    baseline: f64,
    size: f64,
    text: String,
    gray: f32,
}

struct ImageXObject {
    id: Ref,
    width: u32,
    height: u32,
}

enum ImageData {
    Rgb(Vec<u8>),
    Gray(Vec<u8>),
    Jpeg(Vec<u8>, bool),
}

struct DecodedImage {
    width: u32,
    height: u32,
    data: ImageData,
    alpha: Option<Vec<u8>>,
}

struct Renderer<'a> {
    pdf: Pdf,
    next_ref: i32,
    assets: &'a Assets,
    highlighter_gs: Ref,
    pattern_gs: Ref,
    font: Ref,
    image_cache: HashMap<usize, ImageXObject>,
}

/// Renders the note. The last page is dropped when it is empty, as Saber's own export does.
pub fn render(note: &Note, assets: &Assets) -> Result<Vec<u8>> {
    let mut pages: Vec<&Page> = note.pages.iter().collect();
    if pages.len() > 1 && pages.last().is_some_and(|page| page.is_empty()) {
        pages.pop();
    }
    if pages.is_empty() {
        return Err(anyhow!("saber note has no pages"));
    }

    let mut renderer = Renderer {
        pdf: Pdf::new(),
        next_ref: 1,
        assets,
        highlighter_gs: Ref::new(1),
        pattern_gs: Ref::new(1),
        font: Ref::new(1),
        image_cache: HashMap::new(),
    };
    let catalog_id = renderer.alloc();
    let page_tree_id = renderer.alloc();
    renderer.highlighter_gs = renderer.alloc();
    renderer.pattern_gs = renderer.alloc();
    renderer.font = renderer.alloc();

    renderer
        .pdf
        .ext_graphics(renderer.highlighter_gs)
        .non_stroking_alpha(HIGHLIGHTER_ALPHA)
        .stroking_alpha(HIGHLIGHTER_ALPHA)
        .blend_mode(BlendMode::Darken);
    renderer
        .pdf
        .ext_graphics(renderer.pattern_gs)
        .non_stroking_alpha(PATTERN_ALPHA)
        .stroking_alpha(PATTERN_ALPHA);
    renderer
        .pdf
        .type1_font(renderer.font)
        .base_font(Name(b"Helvetica"))
        .encoding_predefined(Name(b"WinAnsiEncoding"));

    let mut page_ids = Vec::with_capacity(pages.len());
    for page in &pages {
        let page_id = renderer.render_page(note, page, page_tree_id)?;
        page_ids.push(page_id);
    }

    renderer
        .pdf
        .pages(page_tree_id)
        .kids(page_ids.iter().copied())
        .count(page_ids.len() as i32);
    renderer.pdf.catalog(catalog_id).pages(page_tree_id);
    Ok(renderer.pdf.finish())
}

impl<'a> Renderer<'a> {
    fn alloc(&mut self) -> Ref {
        let id = Ref::new(self.next_ref);
        self.next_ref += 1;
        id
    }

    fn render_page(&mut self, note: &Note, page: &Page, page_tree_id: Ref) -> Result<Ref> {
        let page_id = self.alloc();
        let content_id = self.alloc();
        let width = page.width.max(1.0);
        let height = page.height.max(1.0);

        let mut content = Content::new();
        let mut texts: Vec<PendingText> = Vec::new();
        let mut images_used: Vec<(Name<'static>, Ref)> = Vec::new();
        let mut image_names: HashMap<i32, String> = HashMap::new();

        // Everything below is drawn in Saber coordinates (origin top-left, y down).
        content.save_state();
        content.transform([1.0, 0.0, 0.0, -1.0, 0.0, height as f32]);

        let background = note.background_color.unwrap_or(DEFAULT_BACKGROUND);
        let (r, g, b) = rgb(background);
        content.set_fill_rgb(r, g, b);
        content.rect(0.0, 0.0, width as f32, height as f32);
        content.fill_nonzero();

        self.draw_pattern(&mut content, note, width, height);

        if let Some(image) = &page.background_image {
            let dst = background_rect(image, width, height, self.image_size(image));
            self.draw_image(
                &mut content,
                image,
                dst,
                &mut images_used,
                &mut image_names,
                &mut texts,
            );
        }
        for image in &page.images {
            let dst = image.dst;
            self.draw_image(
                &mut content,
                image,
                dst,
                &mut images_used,
                &mut image_names,
                &mut texts,
            );
        }

        self.draw_strokes(&mut content, page, background);

        content.restore_state();

        // Typed text is drawn last, in PDF coordinates, so glyphs are not mirrored.
        self.queue_text(page, note, width, height, &mut texts);
        for text in &texts {
            content.save_state();
            content.set_fill_gray(text.gray);
            content.begin_text();
            content.set_font(Name(b"F1"), text.size as f32);
            content.next_line(text.x as f32, (height - text.baseline) as f32);
            content.show(Str(&win_ansi(&text.text)));
            content.end_text();
            content.restore_state();
        }

        let stream = compress(&content.finish());
        self.pdf
            .stream(content_id, &stream)
            .filter(Filter::FlateDecode);

        let mut page_obj = self.pdf.page(page_id);
        page_obj
            .parent(page_tree_id)
            .media_box(Rect::new(0.0, 0.0, width as f32, height as f32))
            .contents(content_id);
        let mut resources = page_obj.resources();
        resources
            .ext_g_states()
            .pair(Name(b"GH"), self.highlighter_gs)
            .pair(Name(b"GP"), self.pattern_gs);
        resources.fonts().pair(Name(b"F1"), self.font);
        if !images_used.is_empty() {
            let mut x_objects = resources.x_objects();
            for (name, id) in &images_used {
                x_objects.pair(*name, *id);
            }
        }
        resources.finish();
        page_obj.finish();
        Ok(page_id)
    }

    fn draw_pattern(&self, content: &mut Content, note: &Note, width: f64, height: f64) {
        let elements =
            pattern_elements(note.pattern, width, height, note.line_height.max(1) as f64);
        if elements.is_empty() {
            return;
        }
        let thickness = note.line_thickness.max(1) as f32;
        content.save_state();
        content.set_parameters(Name(b"GP"));
        content.set_line_width(thickness);
        content.set_line_cap(LineCapStyle::ButtCap);
        content.rect(0.0, 0.0, width as f32, height as f32);
        content.clip_nonzero();
        content.end_path();
        for element in elements {
            let (r, g, b) = rgb(if element.secondary {
                PATTERN_SECONDARY
            } else {
                PATTERN_PRIMARY
            });
            if element.is_line {
                content.set_stroke_rgb(r, g, b);
                content.move_to(element.start.x as f32, element.start.y as f32);
                content.line_to(element.end.x as f32, element.end.y as f32);
                content.stroke();
            } else {
                content.set_fill_rgb(r, g, b);
                circle_path(
                    content,
                    element.start.x,
                    element.start.y,
                    thickness as f64 * 4.0 / 3.0,
                );
                content.fill_nonzero();
            }
        }
        content.restore_state();
    }

    fn draw_strokes(&self, content: &mut Content, page: &Page, background: u32) {
        for stroke in page
            .strokes
            .iter()
            .filter(|stroke| stroke.tool == Tool::Highlighter)
        {
            content.save_state();
            content.set_parameters(Name(b"GH"));
            let (r, g, b) = rgb(stroke.color);
            content.set_fill_rgb(r, g, b);
            content.set_stroke_rgb(r, g, b);
            draw_stroke_shape(content, stroke);
            content.restore_state();
        }
        for stroke in page
            .strokes
            .iter()
            .filter(|stroke| stroke.tool != Tool::Highlighter)
        {
            let color = if stroke.tool == Tool::Pencil {
                lerp_color(background, stroke.color, 0.6)
            } else {
                stroke.color
            };
            let (r, g, b) = rgb(color);
            content.save_state();
            content.set_fill_rgb(r, g, b);
            content.set_stroke_rgb(r, g, b);
            draw_stroke_shape(content, stroke);
            content.restore_state();
        }
    }

    fn queue_text(
        &self,
        page: &Page,
        note: &Note,
        width: f64,
        _height: f64,
        texts: &mut Vec<PendingText>,
    ) {
        if page.text_lines.iter().all(|line| line.trim().is_empty()) {
            return;
        }
        let line_height = note.line_height.max(8) as f64;
        let size = line_height * TEXT_SIZE_FACTOR;
        let margin = line_height * TEXT_MARGIN_FACTOR;
        let max_chars = (((width - 2.0 * margin) / (size * 0.5)).floor() as usize).max(8);
        let mut row = 0_usize;
        for line in &page.text_lines {
            for wrapped in wrap_line(line, max_chars) {
                let baseline = line_height * 2.0 + row as f64 * line_height - line_height * 0.2;
                texts.push(PendingText {
                    x: margin,
                    baseline,
                    size,
                    text: wrapped,
                    gray: 0.0,
                });
                row += 1;
            }
        }
    }

    fn image_size(&self, image: &Image) -> Option<(u32, u32)> {
        let bytes = self.image_bytes(image)?;
        image::load_from_memory(bytes)
            .ok()
            .map(|decoded| (decoded.width(), decoded.height()))
    }

    fn image_bytes<'i>(&self, image: &'i Image) -> Option<&'i [u8]>
    where
        'a: 'i,
    {
        if let Some(index) = image.asset_index {
            if let Some(bytes) = self.assets.get(&index) {
                return Some(bytes.as_slice());
            }
        }
        image.inline_bytes.as_deref()
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_image(
        &mut self,
        content: &mut Content,
        image: &Image,
        dst: NoteRect,
        images_used: &mut Vec<(Name<'static>, Ref)>,
        image_names: &mut HashMap<i32, String>,
        texts: &mut Vec<PendingText>,
    ) {
        if dst.is_empty() {
            return;
        }
        let extension = image.extension.to_ascii_lowercase();
        if extension == ".pdf" || extension == ".svg" {
            let label = if extension == ".pdf" {
                format!(
                    "PDF page {} is not rendered in this export",
                    image.pdf_page.map(|page| page + 1).unwrap_or(1)
                )
            } else {
                "SVG image is not rendered in this export".to_string()
            };
            draw_placeholder(content, dst, texts, label);
            return;
        }
        let Some(xobject) = self.ensure_image_xobject(image) else {
            draw_placeholder(
                content,
                dst,
                texts,
                "Image could not be decoded".to_string(),
            );
            return;
        };
        let name_string = image_names
            .entry(xobject.id.get())
            .or_insert_with(|| format!("Im{}", xobject.id.get()))
            .clone();
        let leaked: &'static str = Box::leak(name_string.into_boxed_str());
        let name = Name(leaked.as_bytes());
        if !images_used.iter().any(|(_, id)| *id == xobject.id) {
            images_used.push((name, xobject.id));
        }

        let natural_width = if image.natural_width > 0.0 {
            image.natural_width
        } else {
            xobject.width as f64
        };
        let natural_height = if image.natural_height > 0.0 {
            image.natural_height
        } else {
            xobject.height as f64
        };
        let (draw_left, draw_top, draw_width, draw_height) = match image.src {
            Some(src) if !src.is_empty() && natural_width > 0.0 && natural_height > 0.0 => {
                let scale_x = dst.width / src.width;
                let scale_y = dst.height / src.height;
                (
                    dst.left - src.left * scale_x,
                    dst.top - src.top * scale_y,
                    natural_width * scale_x,
                    natural_height * scale_y,
                )
            }
            _ => (dst.left, dst.top, dst.width, dst.height),
        };

        content.save_state();
        content.rect(
            dst.left as f32,
            dst.top as f32,
            dst.width as f32,
            dst.height as f32,
        );
        content.clip_nonzero();
        content.end_path();
        // The page CTM is mirrored vertically; flip the image back so it is upright.
        content.transform([
            draw_width as f32,
            0.0,
            0.0,
            -(draw_height as f32),
            draw_left as f32,
            (draw_top + draw_height) as f32,
        ]);
        content.x_object(name);
        content.restore_state();
    }

    fn ensure_image_xobject(&mut self, image: &Image) -> Option<ImageXObject> {
        let cache_key = image.asset_index;
        if let Some(index) = cache_key {
            if let Some(existing) = self.image_cache.get(&index) {
                return Some(ImageXObject {
                    id: existing.id,
                    width: existing.width,
                    height: existing.height,
                });
            }
        }
        let bytes = self.image_bytes(image)?;
        let decoded = decode_image(bytes)?;
        let id = self.alloc();
        let mask_id = decoded.alpha.as_ref().map(|_| self.alloc());
        {
            let (data, filter, gray) = match &decoded.data {
                ImageData::Rgb(raw) => (compress(raw), Filter::FlateDecode, false),
                ImageData::Gray(raw) => (compress(raw), Filter::FlateDecode, true),
                ImageData::Jpeg(raw, gray) => (raw.clone(), Filter::DctDecode, *gray),
            };
            let mut xobject = self.pdf.image_xobject(id, &data);
            xobject
                .width(decoded.width as i32)
                .height(decoded.height as i32)
                .bits_per_component(8)
                .filter(filter);
            if gray {
                xobject.color_space().device_gray();
            } else {
                xobject.color_space().device_rgb();
            }
            if let Some(mask_id) = mask_id {
                xobject.s_mask(mask_id);
            }
        }
        if let (Some(mask_id), Some(alpha)) = (mask_id, &decoded.alpha) {
            let data = compress(alpha);
            let mut mask = self.pdf.image_xobject(mask_id, &data);
            mask.width(decoded.width as i32)
                .height(decoded.height as i32)
                .bits_per_component(8)
                .filter(Filter::FlateDecode);
            mask.color_space().device_gray();
        }
        let xobject = ImageXObject {
            id,
            width: decoded.width,
            height: decoded.height,
        };
        if let Some(index) = cache_key {
            self.image_cache.insert(
                index,
                ImageXObject {
                    id,
                    width: decoded.width,
                    height: decoded.height,
                },
            );
        }
        Some(xobject)
    }
}

fn draw_placeholder(
    content: &mut Content,
    dst: NoteRect,
    texts: &mut Vec<PendingText>,
    label: String,
) {
    content.save_state();
    content.set_fill_gray(0.94);
    content.set_stroke_gray(0.75);
    content.set_line_width(1.0);
    content.rect(
        dst.left as f32,
        dst.top as f32,
        dst.width as f32,
        dst.height as f32,
    );
    content.fill_nonzero_and_stroke();
    content.restore_state();
    let size = (dst.height * 0.08).clamp(8.0, 18.0);
    texts.push(PendingText {
        x: dst.left + 8.0,
        baseline: dst.top + size + 8.0,
        size,
        text: label,
        gray: 0.45,
    });
}

fn draw_stroke_shape(content: &mut Content, stroke: &Stroke) {
    match &stroke.shape {
        Shape::Freehand(points) => {
            if points.is_empty() {
                return;
            }
            let outline = get_stroke(points, &stroke.options);
            if outline.len() < 3 {
                return;
            }
            smooth_closed_path(content, &outline);
            content.fill_nonzero();
        }
        Shape::Circle { cx, cy, radius } => {
            if *radius <= 0.0 {
                return;
            }
            content.set_line_width(stroke.options.size as f32);
            circle_path(content, *cx, *cy, *radius);
            content.stroke();
        }
        Shape::Rect {
            left,
            top,
            width,
            height,
        } => {
            if *width <= 0.0 || *height <= 0.0 {
                return;
            }
            content.set_line_width(stroke.options.size as f32);
            content.set_line_join(LineJoinStyle::RoundJoin);
            rounded_rect_path(
                content,
                *left,
                *top,
                *width,
                *height,
                stroke.options.size / 4.0,
            );
            content.stroke();
        }
    }
}

/// `Stroke.smoothPathFromPolygon`: quadratic curves through the midpoints of the polygon,
/// written as cubic Béziers because PDF has no quadratic operator.
fn smooth_closed_path(content: &mut Content, polygon: &[Vec2]) {
    let first = polygon[0];
    content.move_to(first.x as f32, first.y as f32);
    let mut current = first;
    for i in 1..polygon.len() - 1 {
        let control = polygon[i];
        let next = polygon[i + 1];
        let end = Vec2::new((control.x + next.x) / 2.0, (control.y + next.y) / 2.0);
        let c1 = Vec2::new(
            current.x + 2.0 / 3.0 * (control.x - current.x),
            current.y + 2.0 / 3.0 * (control.y - current.y),
        );
        let c2 = Vec2::new(
            end.x + 2.0 / 3.0 * (control.x - end.x),
            end.y + 2.0 / 3.0 * (control.y - end.y),
        );
        content.cubic_to(
            c1.x as f32,
            c1.y as f32,
            c2.x as f32,
            c2.y as f32,
            end.x as f32,
            end.y as f32,
        );
        current = end;
    }
    content.close_path();
}

fn circle_path(content: &mut Content, cx: f64, cy: f64, radius: f64) {
    let k = BEZIER_CIRCLE * radius;
    content.move_to((cx + radius) as f32, cy as f32);
    content.cubic_to(
        (cx + radius) as f32,
        (cy + k) as f32,
        (cx + k) as f32,
        (cy + radius) as f32,
        cx as f32,
        (cy + radius) as f32,
    );
    content.cubic_to(
        (cx - k) as f32,
        (cy + radius) as f32,
        (cx - radius) as f32,
        (cy + k) as f32,
        (cx - radius) as f32,
        cy as f32,
    );
    content.cubic_to(
        (cx - radius) as f32,
        (cy - k) as f32,
        (cx - k) as f32,
        (cy - radius) as f32,
        cx as f32,
        (cy - radius) as f32,
    );
    content.cubic_to(
        (cx + k) as f32,
        (cy - radius) as f32,
        (cx + radius) as f32,
        (cy - k) as f32,
        (cx + radius) as f32,
        cy as f32,
    );
    content.close_path();
}

fn rounded_rect_path(
    content: &mut Content,
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    let r = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let k = BEZIER_CIRCLE * r;
    let right = left + width;
    let bottom = top + height;
    content.move_to((left + r) as f32, top as f32);
    content.line_to((right - r) as f32, top as f32);
    content.cubic_to(
        (right - r + k) as f32,
        top as f32,
        right as f32,
        (top + r - k) as f32,
        right as f32,
        (top + r) as f32,
    );
    content.line_to(right as f32, (bottom - r) as f32);
    content.cubic_to(
        right as f32,
        (bottom - r + k) as f32,
        (right - r + k) as f32,
        bottom as f32,
        (right - r) as f32,
        bottom as f32,
    );
    content.line_to((left + r) as f32, bottom as f32);
    content.cubic_to(
        (left + r - k) as f32,
        bottom as f32,
        left as f32,
        (bottom - r + k) as f32,
        left as f32,
        (bottom - r) as f32,
    );
    content.line_to(left as f32, (top + r) as f32);
    content.cubic_to(
        left as f32,
        (top + r - k) as f32,
        (left + r - k) as f32,
        top as f32,
        (left + r) as f32,
        top as f32,
    );
    content.close_path();
}

struct PatternElement {
    start: Vec2,
    end: Vec2,
    is_line: bool,
    secondary: bool,
}

fn line(x1: f64, y1: f64, x2: f64, y2: f64) -> PatternElement {
    PatternElement {
        start: Vec2::new(x1, y1),
        end: Vec2::new(x2, y2),
        is_line: true,
        secondary: false,
    }
}

/// `CanvasBackgroundPainter.getPatternElements`.
fn pattern_elements(
    pattern: super::sbn::Pattern,
    width: f64,
    height: f64,
    line_height: f64,
) -> Vec<PatternElement> {
    use super::sbn::Pattern;
    let mut elements = Vec::new();
    match pattern {
        Pattern::None => {}
        Pattern::CollegeLtr | Pattern::CollegeRtl | Pattern::Lined => {
            let mut y = line_height * 2.0;
            while y < height {
                elements.push(line(0.0, y, width, y));
                y += line_height;
            }
            if pattern == Pattern::CollegeLtr {
                let mut element = line(line_height * 2.0, 0.0, line_height * 2.0, height);
                element.secondary = true;
                elements.push(element);
            } else if pattern == Pattern::CollegeRtl {
                let x = width - line_height * 2.0;
                let mut element = line(x, 0.0, x, height);
                element.secondary = true;
                elements.push(element);
            }
        }
        Pattern::Grid => {
            let mut y = line_height * 2.0;
            while y < height {
                elements.push(line(0.0, y, width, y));
                y += line_height;
            }
            let mut x = 0.0;
            while x < width {
                elements.push(line(x, line_height * 2.0, x, height));
                x += line_height;
            }
        }
        Pattern::Dots => {
            let mut y = line_height * 2.0;
            while y <= height {
                let mut x = 0.0;
                while x <= width {
                    elements.push(PatternElement {
                        start: Vec2::new(x, y),
                        end: Vec2::new(x, y),
                        is_line: false,
                        secondary: false,
                    });
                    x += line_height;
                }
                y += line_height;
            }
        }
        Pattern::Staffs | Pattern::Tablature => {
            let staff_spaces = if pattern == Pattern::Staffs { 4 } else { 5 };
            let staff_height = line_height * staff_spaces as f64;
            let staff_spacing = line_height * 3.0;
            let mut top = staff_spacing - line_height;
            while top + staff_height < height {
                for index in 0..=staff_spaces {
                    let y = top + line_height * index as f64;
                    elements.push(line(line_height, y, width - line_height, y));
                }
                elements.push(line(line_height, top, line_height, top + staff_height));
                elements.push(line(
                    width - line_height,
                    top,
                    width - line_height,
                    top + staff_height,
                ));
                top += staff_height + staff_spacing;
            }
        }
        Pattern::Cornell => {
            elements.push(line(
                line_height,
                line_height * 2.0,
                width / 2.0 - line_height / 2.0,
                line_height * 2.0,
            ));
            elements.push(line(
                width / 2.0 + line_height / 2.0,
                line_height * 2.0,
                width - line_height,
                line_height * 2.0,
            ));
            elements.push(line(
                line_height,
                line_height * 3.0,
                width - line_height,
                line_height * 3.0,
            ));
            let left = width * 0.35;
            let bottom = height * 0.7;
            let mut y = line_height * 5.0;
            while y < bottom {
                elements.push(line(left, y, width - line_height, y));
                y += line_height;
            }
        }
    }
    elements
}

fn background_rect(
    image: &Image,
    width: f64,
    height: f64,
    decoded: Option<(u32, u32)>,
) -> NoteRect {
    let (natural_width, natural_height) = if image.natural_width > 0.0 && image.natural_height > 0.0
    {
        (image.natural_width, image.natural_height)
    } else if let Some((w, h)) = decoded {
        (w as f64, h as f64)
    } else {
        return NoteRect {
            left: 0.0,
            top: 0.0,
            width,
            height,
        };
    };
    let contain = (width / natural_width).min(height / natural_height);
    let scale = match image.fit {
        BoxFit::Fill => {
            return NoteRect {
                left: 0.0,
                top: 0.0,
                width,
                height,
            }
        }
        BoxFit::Contain => contain,
        BoxFit::Cover => (width / natural_width).max(height / natural_height),
        BoxFit::FitWidth => width / natural_width,
        BoxFit::FitHeight => height / natural_height,
        BoxFit::NoScale => 1.0,
        BoxFit::ScaleDown => contain.min(1.0),
    };
    let draw_width = natural_width * scale;
    let draw_height = natural_height * scale;
    NoteRect {
        left: (width - draw_width) / 2.0,
        top: (height - draw_height) / 2.0,
        width: draw_width,
        height: draw_height,
    }
}

fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    let format = image::guess_format(bytes).ok();
    let decoded = image::load_from_memory(bytes).ok()?;
    let width = decoded.width();
    let height = decoded.height();
    if width == 0 || height == 0 {
        return None;
    }
    if format == Some(image::ImageFormat::Jpeg) {
        match decoded.color() {
            image::ColorType::Rgb8 => {
                return Some(DecodedImage {
                    width,
                    height,
                    data: ImageData::Jpeg(bytes.to_vec(), false),
                    alpha: None,
                })
            }
            image::ColorType::L8 => {
                return Some(DecodedImage {
                    width,
                    height,
                    data: ImageData::Jpeg(bytes.to_vec(), true),
                    alpha: None,
                })
            }
            _ => {}
        }
    }
    let has_alpha = decoded.color().has_alpha();
    let rgba = decoded.into_rgba8();
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    let mut alpha = Vec::with_capacity((width * height) as usize);
    let mut any_transparent = false;
    for pixel in rgba.pixels() {
        rgb.extend_from_slice(&pixel.0[..3]);
        alpha.push(pixel.0[3]);
        if pixel.0[3] != 255 {
            any_transparent = true;
        }
    }
    let (pixels, _) = rgb.as_chunks::<3>();
    let is_gray = pixels
        .iter()
        .all(|chunk| chunk[0] == chunk[1] && chunk[1] == chunk[2]);
    let data = if is_gray {
        ImageData::Gray(pixels.iter().map(|chunk| chunk[0]).collect())
    } else {
        ImageData::Rgb(rgb)
    };
    Some(DecodedImage {
        width,
        height,
        data,
        alpha: (has_alpha && any_transparent).then_some(alpha),
    })
}

fn compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("in-memory write");
    encoder.finish().expect("in-memory finish")
}

fn rgb(argb: u32) -> (f32, f32, f32) {
    (
        ((argb >> 16) & 0xff) as f32 / 255.0,
        ((argb >> 8) & 0xff) as f32 / 255.0,
        (argb & 0xff) as f32 / 255.0,
    )
}

/// Flutter `Color.lerp(a, b, t)` on the colour channels.
fn lerp_color(a: u32, b: u32, t: f64) -> u32 {
    let channel = |shift: u32| {
        let from = ((a >> shift) & 0xff) as f64;
        let to = ((b >> shift) & 0xff) as f64;
        ((from + (to - from) * t).round().clamp(0.0, 255.0) as u32) << shift
    };
    0xff00_0000 | channel(16) | channel(8) | channel(0)
}

fn wrap_line(line: &str, max_chars: usize) -> Vec<String> {
    if line.chars().count() <= max_chars {
        return vec![line.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in line.split(' ') {
        let candidate_len =
            current.chars().count() + word.chars().count() + usize::from(!current.is_empty());
        if candidate_len > max_chars && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
        while current.chars().count() > max_chars {
            let head: String = current.chars().take(max_chars).collect();
            let tail: String = current.chars().skip(max_chars).collect();
            lines.push(head);
            current = tail;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Encodes text for the WinAnsi (CP1252-like) Helvetica font; characters outside Latin-1 are
/// replaced with `?`.
fn win_ansi(text: &str) -> Vec<u8> {
    text.chars()
        .map(|ch| match ch {
            '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => ch as u8,
            '\u{2019}' => 0x92,
            '\u{2018}' => 0x91,
            '\u{201c}' => 0x93,
            '\u{201d}' => 0x94,
            '\u{2013}' => 0x96,
            '\u{2014}' => 0x97,
            '\u{2026}' => 0x85,
            '\u{20ac}' => 0x80,
            '\t' => b' ',
            _ => b'?',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::sbn::tests::sample_note;
    use super::*;

    fn tiny_png() -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 128])
            }
        });
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        buffer.into_inner()
    }

    #[test]
    fn renders_sample_note_to_a_pdf() {
        let mut bytes = Vec::new();
        sample_note().to_writer(&mut bytes).unwrap();
        let note = Note::parse(&bytes).unwrap();
        let mut assets = Assets::new();
        assets.insert(0, tiny_png());
        let pdf = render(&note, &assets).unwrap();
        assert!(pdf.starts_with(b"%PDF-1."));
        assert!(pdf.ends_with(b"%%EOF\n") || pdf.ends_with(b"%%EOF"));
        let text = String::from_utf8_lossy(&pdf);
        // One page: the empty trailing page is dropped.
        assert!(text.contains("/Count 1"), "page count missing");
        assert!(
            text.contains("/SMask"),
            "alpha channel should become a soft mask"
        );
        assert!(text.contains("/Helvetica"));
    }

    #[test]
    fn renders_without_assets_using_placeholders() {
        let mut bytes = Vec::new();
        sample_note().to_writer(&mut bytes).unwrap();
        let note = Note::parse(&bytes).unwrap();
        let pdf = render(&note, &Assets::new()).unwrap();
        assert!(pdf.starts_with(b"%PDF-1."));
    }

    /// Writes a richer sample to the path in `OBSIDISYNC_SABER_SAMPLE_PDF` for eyeballing.
    #[test]
    #[ignore]
    fn writes_sample_pdf_for_inspection() {
        use super::super::sbn::tests::point;
        use bson::{doc, Bson};
        let Ok(path) = std::env::var("OBSIDISYNC_SABER_SAMPLE_PDF") else {
            return;
        };
        let mut wave = Vec::new();
        for i in 0..80 {
            let t = i as f32 / 79.0;
            wave.push(point(
                100.0 + t * 800.0,
                400.0 + (t * 12.0).sin() * 60.0,
                Some(0.3 + 0.6 * t),
            ));
        }
        let mut loop_points = Vec::new();
        for i in 0..120 {
            let a = i as f32 / 119.0 * std::f32::consts::TAU;
            loop_points.push(point(
                500.0 + a.cos() * 150.0 + a * 20.0,
                900.0 + a.sin() * 120.0,
                None,
            ));
        }
        let document = doc! {
            "v": 19_i32, "ni": 1_i32, "p": "college", "l": 40_i32, "lt": 3_i32,
            "z": [{
                "w": 1000.0, "h": 1400.0,
                "s": [
                    { "shape": Bson::Null, "p": wave, "i": 0_i32, "ty": "fountainPen", "pe": true, "c": 0xff1a_1a1a_i64, "s": 14.0 },
                    { "shape": Bson::Null, "p": loop_points.clone(), "i": 0_i32, "ty": "ballpointPen", "pe": false, "c": 0xff00_55cc_i64, "s": 6.0 },
                    { "shape": Bson::Null, "p": loop_points, "i": 0_i32, "ty": "Pencil", "pe": false, "c": 0xff00_8800_i64, "s": 20.0, "ox": 0.0, "oy": 250.0 },
                    { "shape": Bson::Null, "p": [point(90.0, 400.0, None), point(910.0, 400.0, None)], "i": 0_i32, "ty": "Highlighter", "pe": false, "c": 0xffff_ee00_i64, "s": 60.0 },
                    { "shape": "circle", "i": 0_i32, "cx": 800.0, "cy": 700.0, "r": 90.0, "pe": true, "c": 0xffcc_0000_i64, "ty": "ShapePen", "s": 5.0 },
                    { "shape": "rect", "i": 0_i32, "rl": 100.0, "rt": 600.0, "rw": 250.0, "rh": 160.0, "pe": true, "c": 0xff55_0088_i64, "ty": "ShapePen", "s": 8.0 }
                ],
                "i": [ { "id": 0_i32, "e": ".png", "i": 0_i32, "x": 600.0, "y": 100.0, "w": 300.0, "h": 150.0, "a": 0_i32 } ],
                "q": [ { "insert": "Typed heading for this page\nSecond line of text with some more words to wrap around the page width and see it working.\n" } ]
            }, { "w": 1000.0, "h": 1400.0, "s": [ { "shape": Bson::Null, "p": [point(200.0, 200.0, None), point(600.0, 500.0, None)], "i": 1_i32, "ty": "fountainPen", "c": 0xff00_0000_i64 } ] }],
            "c": 0_i32
        };
        let mut bytes = Vec::new();
        document.to_writer(&mut bytes).unwrap();
        let note = Note::parse(&bytes).unwrap();
        let mut assets = Assets::new();
        let img = image::RgbaImage::from_fn(60, 30, |x, y| {
            if (x / 10 + y / 10) % 2 == 0 {
                image::Rgba([220, 40, 40, 255])
            } else {
                image::Rgba([40, 40, 220, 120])
            }
        });
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        assets.insert(0, buffer.into_inner());
        std::fs::write(path, render(&note, &assets).unwrap()).unwrap();
    }

    #[test]
    fn wraps_long_lines() {
        let lines = wrap_line("aaa bbb ccc ddd", 7);
        assert_eq!(lines, vec!["aaa bbb", "ccc ddd"]);
        assert_eq!(wrap_line("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn lerps_colours_like_flutter() {
        assert_eq!(lerp_color(0xffff_ffff, 0xff00_0000, 0.6), 0xff66_6666);
    }
}
