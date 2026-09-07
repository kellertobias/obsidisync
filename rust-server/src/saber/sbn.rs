//! Reader for Saber's `.sbn2` note format (BSON, file version 19 and earlier).
//!
//! Only what the PDF renderer needs is modelled: pages, strokes, images, background settings and
//! the plain text of typed notes. Field names follow `EditorCoreInfo.toJson`, `EditorPage.toJson`,
//! `Stroke.toJson`, `EditorImage.toJson` and `StrokeOptions.toJson` in the Saber sources.

use super::freehand::{EndOptions, InputPoint, StrokeOptions};
use anyhow::{anyhow, Result};
use bson::{Bson, Document};

/// Saber's `EditorPage.defaultWidth`/`defaultHeight`.
pub const DEFAULT_PAGE_WIDTH: f64 = 1000.0;
pub const DEFAULT_PAGE_HEIGHT: f64 = 1400.0;
/// `Prefs.lastLineHeight` / `lastLineThickness` defaults; used when a file predates them.
const DEFAULT_LINE_HEIGHT: i64 = 40;
const DEFAULT_LINE_THICKNESS: i64 = 3;
/// Newest file version this reader understands (`EditorCoreInfo.sbnVersion`).
pub const SUPPORTED_VERSION: i64 = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Highlighter,
    FountainPen,
    BallpointPen,
    Pencil,
    ShapePen,
    Other,
}

impl Tool {
    fn parse(id: Option<&str>, fallback: Tool) -> Tool {
        match id {
            None => fallback,
            Some("Highlighter") => Tool::Highlighter,
            Some("fountainPen") | Some("Pen") => Tool::FountainPen,
            Some("ballpointPen") => Tool::BallpointPen,
            Some("Pencil") => Tool::Pencil,
            Some("ShapePen") => Tool::ShapePen,
            Some(_) => Tool::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    None,
    CollegeLtr,
    CollegeRtl,
    Lined,
    Grid,
    Dots,
    Staffs,
    Tablature,
    Cornell,
}

impl Pattern {
    fn parse(name: Option<&str>) -> Pattern {
        match name {
            Some("college") => Pattern::CollegeLtr,
            Some("college-rtl") => Pattern::CollegeRtl,
            Some("lined") => Pattern::Lined,
            Some("grid") => Pattern::Grid,
            Some("dots") => Pattern::Dots,
            Some("staffs") => Pattern::Staffs,
            Some("tablature") => Pattern::Tablature,
            Some("cornell") => Pattern::Cornell,
            _ => Pattern::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Freehand(Vec<InputPoint>),
    Circle {
        cx: f64,
        cy: f64,
        radius: f64,
    },
    Rect {
        left: f64,
        top: f64,
        width: f64,
        height: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub tool: Tool,
    /// ARGB, like Flutter's `Color.value`.
    pub color: u32,
    pub pressure_enabled: bool,
    pub options: StrokeOptions,
    pub shape: Shape,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// Flutter `BoxFit` indices as stored in the `f` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxFit {
    Fill,
    Contain,
    Cover,
    FitWidth,
    FitHeight,
    NoScale,
    ScaleDown,
}

impl BoxFit {
    fn from_index(index: Option<i64>) -> BoxFit {
        match index {
            Some(0) => BoxFit::Fill,
            Some(2) => BoxFit::Cover,
            Some(3) => BoxFit::FitWidth,
            Some(4) => BoxFit::FitHeight,
            Some(5) => BoxFit::NoScale,
            Some(6) => BoxFit::ScaleDown,
            _ => BoxFit::Contain,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    /// Index of the `<note>.sbn2.<index>` asset file, or `None` for files that still embed
    /// the bytes inline (`b`, versions before 11).
    pub asset_index: Option<usize>,
    pub inline_bytes: Option<Vec<u8>>,
    pub extension: String,
    pub dst: Rect,
    pub src: Option<Rect>,
    pub natural_width: f64,
    pub natural_height: f64,
    pub fit: BoxFit,
    /// Page of an imported PDF, for `.pdf` assets.
    pub pdf_page: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub strokes: Vec<Stroke>,
    pub images: Vec<Image>,
    pub background_image: Option<Image>,
    /// Plain text of the typed (Quill) content, one entry per line.
    pub text_lines: Vec<String>,
}

impl Page {
    fn new(width: f64, height: f64) -> Self {
        Self {
            width,
            height,
            strokes: Vec::new(),
            images: Vec::new(),
            background_image: None,
            text_lines: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty()
            && self.images.is_empty()
            && self.background_image.is_none()
            && self.text_lines.iter().all(|line| line.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub version: i64,
    /// ARGB background colour; `None` means Saber's default white.
    pub background_color: Option<u32>,
    pub pattern: Pattern,
    pub line_height: i64,
    pub line_thickness: i64,
    pub pages: Vec<Page>,
}

impl Note {
    pub fn parse(bytes: &[u8]) -> Result<Note> {
        let document = Document::from_reader(std::io::Cursor::new(bytes))
            .map_err(|error| anyhow!("saber note is not valid BSON: {error}"))?;
        Self::from_document(&document)
    }

    fn from_document(json: &Document) -> Result<Note> {
        let version = int(json.get("v")).unwrap_or(0);
        let inline_assets: Vec<Vec<u8>> = json
            .get_array("a")
            .map(|assets| assets.iter().map(asset_bytes).collect())
            .unwrap_or_default();
        let fallback_size = match (float(json.get("w")), float(json.get("h"))) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => None,
        };

        let mut pages = parse_pages(json.get("z"), &inline_assets, version, fallback_size);

        // Versions before 8 keep strokes and images at the top level with a page index.
        if let Ok(strokes) = json.get_array("s") {
            for stroke_json in strokes.iter().filter_map(Bson::as_document) {
                let page_index = int(stroke_json.get("i")).unwrap_or(0).max(0) as usize;
                ensure_page(&mut pages, page_index, fallback_size);
                if let Some(stroke) = parse_stroke(stroke_json, version) {
                    pages[page_index].strokes.push(stroke);
                }
            }
        }
        if let Ok(images) = json.get_array("i") {
            for image_json in images.iter().filter_map(Bson::as_document) {
                let page_index = int(image_json.get("i")).unwrap_or(0).max(0) as usize;
                ensure_page(&mut pages, page_index, fallback_size);
                pages[page_index]
                    .images
                    .push(parse_image(image_json, &inline_assets));
            }
        }
        if pages.is_empty() {
            ensure_page(&mut pages, 0, fallback_size);
        }
        for page in &mut pages {
            sort_strokes(&mut page.strokes);
        }

        Ok(Note {
            version,
            background_color: int(json.get("b")).map(|value| value as u32),
            pattern: Pattern::parse(json.get_str("p").ok()),
            line_height: int(json.get("l")).unwrap_or(DEFAULT_LINE_HEIGHT),
            line_thickness: int(json.get("lt")).unwrap_or(DEFAULT_LINE_THICKNESS),
            pages,
        })
    }

    /// Every asset index referenced by the note, so the caller can fetch the files.
    pub fn asset_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .pages
            .iter()
            .flat_map(|page| page.images.iter().chain(page.background_image.iter()))
            .filter_map(|image| image.asset_index)
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

fn ensure_page(pages: &mut Vec<Page>, index: usize, fallback_size: Option<(f64, f64)>) {
    while pages.len() <= index {
        let (width, height) = fallback_size.unwrap_or((DEFAULT_PAGE_WIDTH, DEFAULT_PAGE_HEIGHT));
        pages.push(Page::new(width, height));
    }
}

fn parse_pages(
    value: Option<&Bson>,
    inline_assets: &[Vec<u8>],
    version: i64,
    fallback_size: Option<(f64, f64)>,
) -> Vec<Page> {
    let Some(Bson::Array(pages)) = value else {
        return Vec::new();
    };
    pages
        .iter()
        .map(|page| match page {
            // Old format: `[width, height]`.
            Bson::Array(size) => {
                let width = size
                    .first()
                    .and_then(|v| float(Some(v)))
                    .unwrap_or(DEFAULT_PAGE_WIDTH);
                let height = size
                    .get(1)
                    .and_then(|v| float(Some(v)))
                    .unwrap_or(DEFAULT_PAGE_HEIGHT);
                Page::new(width, height)
            }
            Bson::Document(json) => parse_page(json, inline_assets, version),
            _ => {
                let (width, height) =
                    fallback_size.unwrap_or((DEFAULT_PAGE_WIDTH, DEFAULT_PAGE_HEIGHT));
                Page::new(width, height)
            }
        })
        .collect()
}

fn parse_page(json: &Document, inline_assets: &[Vec<u8>], version: i64) -> Page {
    let mut page = Page::new(
        float(json.get("w")).unwrap_or(DEFAULT_PAGE_WIDTH),
        float(json.get("h")).unwrap_or(DEFAULT_PAGE_HEIGHT),
    );
    if let Ok(strokes) = json.get_array("s") {
        page.strokes = strokes
            .iter()
            .filter_map(Bson::as_document)
            .filter_map(|stroke| parse_stroke(stroke, version))
            .collect();
    }
    if let Ok(images) = json.get_array("i") {
        page.images = images
            .iter()
            .filter_map(Bson::as_document)
            .map(|image| parse_image(image, inline_assets))
            .collect();
    }
    if let Ok(background) = json.get_document("b") {
        page.background_image = Some(parse_image(background, inline_assets));
    }
    if let Ok(quill) = json.get_array("q") {
        page.text_lines = quill_plain_text(quill);
    }
    page
}

/// Flattens a Quill delta into plain text lines. Formatting is dropped.
fn quill_plain_text(ops: &[Bson]) -> Vec<String> {
    let mut text = String::new();
    for op in ops.iter().filter_map(Bson::as_document) {
        if let Ok(insert) = op.get_str("insert") {
            text.push_str(insert);
        } else if op.get("insert").is_some() {
            // Embeds (images, videos) have no text representation.
            text.push('\u{fffc}');
        }
    }
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    // Quill documents always end with a newline.
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn parse_stroke(json: &Document, version: i64) -> Option<Stroke> {
    let color = int(json.get("c"))
        .map(|value| value as u32)
        .unwrap_or(0xff00_0000);
    let pressure_enabled = json.get_bool("pe").unwrap_or(true);
    let mut options = parse_stroke_options(json);

    let shape = match json.get_str("shape").ok() {
        Some("circle") => Shape::Circle {
            cx: float(json.get("cx")).unwrap_or(0.0),
            cy: float(json.get("cy")).unwrap_or(0.0),
            radius: float(json.get("r")).unwrap_or(0.0),
        },
        Some("rect") => Shape::Rect {
            left: float(json.get("rl")).unwrap_or(0.0),
            top: float(json.get("rt")).unwrap_or(0.0),
            width: float(json.get("rw")).unwrap_or(0.0),
            height: float(json.get("rh")).unwrap_or(0.0),
        },
        Some(_) => return None,
        None => {
            let offset_x = float(json.get("ox")).unwrap_or(0.0);
            let offset_y = float(json.get("oy")).unwrap_or(0.0);
            let points = json
                .get_array("p")
                .ok()?
                .iter()
                .filter_map(|point| parse_point(point, version, offset_x, offset_y))
                .filter(|point| point.x.is_finite() && point.y.is_finite())
                .collect();
            Shape::Freehand(points)
        }
    };

    let fallback = if matches!(shape, Shape::Freehand(_)) {
        Tool::FountainPen
    } else {
        Tool::ShapePen
    };
    let tool = Tool::parse(json.get_str("ty").ok(), fallback);
    if tool == Tool::ShapePen {
        // Mirrors Stroke.fromJson: shape pen strokes ignore smoothing and streamline.
        options.smoothing = 0.0;
        options.streamline = 0.0;
    }
    if !matches!(shape, Shape::Freehand(_)) {
        options.is_complete = true;
    }
    if !pressure_enabled {
        options.simulate_pressure = false;
    }

    Some(Stroke {
        tool,
        color,
        pressure_enabled,
        options,
        shape,
    })
}

fn parse_stroke_options(json: &Document) -> StrokeOptions {
    let defaults = StrokeOptions::default();
    StrokeOptions {
        size: float(json.get("s")).unwrap_or(defaults.size),
        thinning: float(json.get("t")).unwrap_or(defaults.thinning),
        smoothing: float(json.get("sm")).unwrap_or(defaults.smoothing),
        streamline: float(json.get("sl")).unwrap_or(defaults.streamline),
        simulate_pressure: json.get_bool("sp").unwrap_or(defaults.simulate_pressure),
        start: EndOptions::from_json(float(json.get("ts")), json.get_bool("cs").unwrap_or(true)),
        end: EndOptions::from_json(float(json.get("te")), json.get_bool("ce").unwrap_or(true)),
        is_complete: json.get_bool("f").unwrap_or(true),
    }
}

fn parse_point(point: &Bson, version: i64, offset_x: f64, offset_y: f64) -> Option<InputPoint> {
    match point {
        Bson::Binary(binary) if version >= 13 || !matches!(point, Bson::Document(_)) => {
            let floats: Vec<f32> = binary
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| f32::from_le_bytes(*chunk))
                .collect();
            if floats.len() < 2 {
                return None;
            }
            Some(InputPoint {
                x: floats[0] as f64 + offset_x,
                y: floats[1] as f64 + offset_y,
                pressure: floats.get(2).map(|value| *value as f64),
            })
        }
        Bson::Document(json) => Some(InputPoint {
            x: float(json.get("x"))? + offset_x,
            y: float(json.get("y"))? + offset_y,
            pressure: float(json.get("p")),
        }),
        _ => None,
    }
}

fn parse_image(json: &Document, inline_assets: &[Vec<u8>]) -> Image {
    let asset_index = int(json.get("a")).and_then(|index| usize::try_from(index).ok());
    let inline_bytes = match json.get("b") {
        Some(Bson::Binary(binary)) => Some(binary.bytes.clone()),
        Some(Bson::Array(values)) => Some(
            values
                .iter()
                .filter_map(|value| int(Some(value)))
                .map(|value| value as u8)
                .collect(),
        ),
        _ => asset_index.and_then(|index| inline_assets.get(index).cloned()),
    };
    let src = match (
        float(json.get("sx")),
        float(json.get("sy")),
        float(json.get("sw")),
        float(json.get("sh")),
    ) {
        (_, _, None, None) => None,
        (sx, sy, sw, sh) => Some(Rect {
            left: sx.unwrap_or(0.0),
            top: sy.unwrap_or(0.0),
            width: sw.unwrap_or(0.0),
            height: sh.unwrap_or(0.0),
        }),
    };
    Image {
        asset_index,
        inline_bytes,
        extension: json.get_str("e").unwrap_or(".png").to_string(),
        dst: Rect {
            left: float(json.get("x")).unwrap_or(0.0),
            top: float(json.get("y")).unwrap_or(0.0),
            width: float(json.get("w")).unwrap_or(0.0),
            height: float(json.get("h")).unwrap_or(0.0),
        },
        src,
        natural_width: float(json.get("nw")).unwrap_or(0.0),
        natural_height: float(json.get("nh")).unwrap_or(0.0),
        fit: BoxFit::from_index(int(json.get("f"))),
        pdf_page: int(json.get("pdfi")),
    }
}

/// `EditorPage.sortStrokes`: by tool z-order, highlighters additionally by colour.
fn sort_strokes(strokes: &mut [Stroke]) {
    strokes.sort_by(|a, b| {
        let order = tool_order(a.tool).cmp(&tool_order(b.tool));
        if order != std::cmp::Ordering::Equal {
            return order;
        }
        if a.tool != Tool::Highlighter {
            return std::cmp::Ordering::Equal;
        }
        a.color.cmp(&b.color)
    });
}

fn tool_order(tool: Tool) -> u8 {
    // ToolId ids compared as strings: "Highlighter" < "Pencil" < "ShapePen" < "ballpointPen" <
    // "fountainPen" (uppercase sorts first).
    match tool {
        Tool::Highlighter => 0,
        Tool::Pencil => 1,
        Tool::ShapePen => 2,
        Tool::BallpointPen => 3,
        Tool::FountainPen => 4,
        Tool::Other => 5,
    }
}

fn asset_bytes(asset: &Bson) -> Vec<u8> {
    match asset {
        Bson::Binary(binary) => binary.bytes.clone(),
        Bson::String(base64_text) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(base64_text)
                .unwrap_or_default()
        }
        Bson::Array(values) => values
            .iter()
            .filter_map(|value| int(Some(value)))
            .map(|value| value as u8)
            .collect(),
        _ => Vec::new(),
    }
}

fn float(value: Option<&Bson>) -> Option<f64> {
    match value? {
        Bson::Double(v) => Some(*v),
        Bson::Int32(v) => Some(*v as f64),
        Bson::Int64(v) => Some(*v as f64),
        _ => None,
    }
}

fn int(value: Option<&Bson>) -> Option<i64> {
    match value? {
        Bson::Int32(v) => Some(*v as i64),
        Bson::Int64(v) => Some(*v),
        Bson::Double(v) if v.fract() == 0.0 => Some(*v as i64),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bson::spec::BinarySubtype;
    use bson::{doc, Binary};

    pub fn point(x: f32, y: f32, pressure: Option<f32>) -> Bson {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&x.to_le_bytes());
        bytes.extend_from_slice(&y.to_le_bytes());
        if let Some(pressure) = pressure {
            bytes.extend_from_slice(&pressure.to_le_bytes());
        }
        Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes,
        })
    }

    pub fn sample_note() -> Document {
        doc! {
            "v": 19_i32,
            "ni": 1_i32,
            "b": 0xffff_ffff_i64,
            "p": "lined",
            "l": 40_i32,
            "lt": 3_i32,
            "z": [
                {
                    "w": 1000.0,
                    "h": 1400.0,
                    "s": [
                        {
                            "shape": Bson::Null,
                            "p": [point(100.0, 100.0, Some(0.5)), point(200.0, 120.0, Some(0.6)), point(300.0, 100.0, Some(0.4))],
                            "i": 0_i32,
                            "ty": "fountainPen",
                            "pe": true,
                            "c": 0xff00_0000_i64,
                            "s": 12.0
                        },
                        {
                            "shape": "circle",
                            "i": 0_i32,
                            "cx": 500.0, "cy": 500.0, "r": 80.0,
                            "pe": true,
                            "c": 0xffff_0000_i64,
                            "ty": "ShapePen"
                        },
                        {
                            "shape": Bson::Null,
                            "p": [point(100.0, 300.0, None), point(400.0, 300.0, None)],
                            "i": 0_i32,
                            "ty": "Highlighter",
                            "pe": false,
                            "c": 0xffff_ff00_i64,
                            "s": 30.0
                        }
                    ],
                    "i": [
                        { "id": 0_i32, "e": ".png", "i": 0_i32, "v": true, "f": 1_i32, "x": 600.0, "y": 100.0, "w": 200.0, "h": 100.0, "a": 0_i32, "nw": 2.0, "nh": 1.0 }
                    ],
                    "q": [ { "insert": "Hello\nWorld\n" } ]
                },
                { "w": 1000.0, "h": 1400.0 }
            ],
            "c": 0_i32
        }
    }

    #[test]
    fn parses_pages_strokes_images_and_text() {
        let mut bytes = Vec::new();
        sample_note().to_writer(&mut bytes).unwrap();
        let note = Note::parse(&bytes).unwrap();
        assert_eq!(note.version, 19);
        assert_eq!(note.pattern, Pattern::Lined);
        assert_eq!(note.pages.len(), 2);
        let page = &note.pages[0];
        assert_eq!(page.strokes.len(), 3);
        // Sorted: highlighter first.
        assert_eq!(page.strokes[0].tool, Tool::Highlighter);
        assert!(!page.strokes[0].options.simulate_pressure);
        assert_eq!(page.strokes[1].tool, Tool::ShapePen);
        assert!(matches!(page.strokes[1].shape, Shape::Circle { radius, .. } if radius == 80.0));
        assert_eq!(page.strokes[2].options.size, 12.0);
        match &page.strokes[2].shape {
            Shape::Freehand(points) => {
                assert_eq!(points.len(), 3);
                assert_eq!(points[1].pressure, Some(0.6_f32 as f64));
            }
            other => panic!("unexpected shape {other:?}"),
        }
        assert_eq!(page.images[0].asset_index, Some(0));
        assert_eq!(page.text_lines, vec!["Hello", "World"]);
        assert!(note.pages[1].is_empty());
        assert_eq!(note.asset_indices(), vec![0]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Note::parse(b"not bson").is_err());
    }
}
