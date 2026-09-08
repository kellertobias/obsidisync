use super::{array, cbor, integer, text};
use crate::binary_store::sha256_hex;
use anyhow::{anyhow, bail, ensure, Result};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
const PT: f64 = 72.0 / 25400.0;

/// No timestamps, random identifiers or host fonts enter the PDF. Retains vector ink and PDF bases.
pub fn render(
    manifest: &Value,
    files: &BTreeMap<String, Vec<u8>>,
    base: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut doc = if let Some(base) = base {
        Document::load_mem(base)?
    } else {
        Document::with_version("1.7")
    };
    ensure!(
        !doc.is_encrypted(),
        "encrypted PDF annotations are unsupported"
    );
    let page_specs = array(manifest, "pages")?;
    let base_pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    if base.is_some() {
        ensure!(
            base_pages.len() == page_specs.len(),
            "annotation pages are fixed"
        );
    }
    let tree = if base.is_none() {
        doc.new_object_id()
    } else {
        (0, 0)
    };
    if base.is_none() {
        doc.objects.insert(
            tree,
            Object::Dictionary(
                dictionary! {"Type"=>"Pages", "Kids"=>Vec::<Object>::new(),"Count"=>0},
            ),
        );
    }
    let mut pages = Vec::new();
    let mut imported: HashMap<String, Vec<ObjectId>> = HashMap::new();
    let mut source_bytes = 0usize;
    let root = format!(".inkvault/notes/{}", text(manifest, "documentId")?);
    for (index, spec) in page_specs.iter().enumerate() {
        let path = format!("{root}/pages/{}.cbor", text(spec, "id")?);
        let bytes = files
            .get(&path)
            .ok_or_else(|| anyhow!("missing page {path}"))?;
        ensure!(
            sha256_hex(bytes) == text(spec, "sha256")?,
            "page checksum mismatch"
        );
        source_bytes += bytes.len();
        ensure!(
            source_bytes <= 64 * 1024 * 1024,
            "note exceeds 64 MiB of vector data"
        );
        let page = cbor::decode(bytes)?;
        ensure!(
            integer(&page, "schemaVersion")? == 1,
            "unsupported page schema"
        );
        let (w, h) = (integer(&page, "width")?, integer(&page, "height")?);
        ensure!(
            w == integer(spec, "width")? && h == integer(spec, "height")?,
            "page dimensions differ from manifest"
        );
        let id = if base.is_some() {
            base_pages[index]
        } else {
            doc.add_object(dictionary! {"Type" => "Page", "Parent" => tree, "MediaBox" => vec![0.into(),0.into(),Object::Real((w as f64*PT) as f32),Object::Real((h as f64*PT) as f32)], "Resources" => Dictionary::new()})
        };
        let (bounds, rotation) = page_bounds(&doc, id)?;
        let display_w = if rotation % 180 == 0 {
            bounds[2] - bounds[0]
        } else {
            bounds[3] - bounds[1]
        };
        let display_h = if rotation % 180 == 0 {
            bounds[3] - bounds[1]
        } else {
            bounds[2] - bounds[0]
        };
        ensure!(
            (w as f64 * PT - display_w).abs() <= 1.1 && (h as f64 * PT - display_h).abs() <= 1.1,
            "annotation dimensions do not match base PDF"
        );
        let mut resources = inherited(&doc, id, b"Resources")?
            .map(|v| v.as_dict().cloned())
            .transpose()?
            .unwrap_or_default();
        let mut objects = subdict(&doc, &resources, b"XObject")?;
        let mut graphics = subdict(&doc, &resources, b"ExtGState")?;
        let marker_name = free_name(&graphics, "InkVaultMarker");
        let marker = doc.add_object(dictionary!{"Type"=>"ExtGState", "CA"=>0.39215687f32, "ca"=>0.39215687f32, "BM"=>"Darken"});
        graphics.set(marker_name.as_bytes(), marker);
        let matrix = match rotation {
            0 => [PT, 0., 0., -PT, bounds[0], bounds[3]],
            90 => [0., PT, PT, 0., bounds[0], bounds[1]],
            180 => [-PT, 0., 0., PT, bounds[2], bounds[1]],
            270 => [0., -PT, -PT, 0., bounds[2], bounds[3]],
            _ => unreachable!(),
        };
        let mut content = format!(
            "q\n{} {} {} {} {} {} cm\n0 0 {w} {h} re W n\n",
            matrix[0], matrix[1], matrix[2], matrix[3], matrix[4], matrix[5]
        );
        if base.is_none() {
            content.push_str(&format!("1 1 1 rg 0 0 {w} {h} re f\n"));
        }
        if let Some(template) = spec.get("template").filter(|v| !v.is_null()) {
            ensure!(
                base.is_none(),
                "PDF annotation cannot replace its background template"
            );
            place(
                &mut doc,
                &mut objects,
                &mut imported,
                &mut content,
                files,
                text(template, "asset")?,
                0,
                0,
                w,
                h,
                Some(integer(template, "page")?),
            )?;
        }
        let items = array(&page, "objects")?;
        ensure!(items.len() <= 2048, "too many placed images");
        for obj in items {
            place(
                &mut doc,
                &mut objects,
                &mut imported,
                &mut content,
                files,
                text(obj, "asset")?,
                integer(obj, "x")?,
                integer(obj, "y")?,
                integer(obj, "width")?,
                integer(obj, "height")?,
                None,
            )?;
        }
        let strokes = array(&page, "strokes")?;
        ensure!(strokes.len() <= 50000, "too many strokes");
        let mut ids = std::collections::BTreeSet::new();
        for stroke in strokes {
            ensure!(ids.insert(text(stroke, "id")?), "duplicate stroke ID");
            let style = &stroke["style"];
            let tool = text(style, "tool")?;
            ensure!(tool == "pen" || tool == "marker", "unsupported ink tool");
            let pressure = style["pressure"]
                .as_bool()
                .ok_or_else(|| anyhow!("invalid pressure style"))?;
            let width = integer(style, "width")?;
            ensure!((50..=20000).contains(&width), "invalid stroke width");
            let color = integer(style, "color")?;
            ensure!((0..=0xffff_ffff).contains(&color), "invalid ink color");
            let samples = array(stroke, "points")?;
            ensure!(
                !samples.is_empty() && samples.len() <= 100000,
                "invalid stroke length"
            );
            let points = samples
                .iter()
                .map(|sample| {
                    let p = sample
                        .as_array()
                        .ok_or_else(|| anyhow!("invalid stroke point"))?;
                    ensure!(p.len() == 5, "unsupported point schema");
                    let x = p[0].as_i64().ok_or_else(|| anyhow!("invalid x"))?;
                    let y = p[1].as_i64().ok_or_else(|| anyhow!("invalid y"))?;
                    ensure!(
                        x.unsigned_abs() <= 10000000
                            && y.unsigned_abs() <= 10000000
                            && p[2].as_i64().is_some_and(|t| t >= 0),
                        "invalid point position/time"
                    );
                    let force = if p[3].is_null() {
                        1000
                    } else {
                        p[3].as_i64().ok_or_else(|| anyhow!("invalid pressure"))?
                    };
                    ensure!((0..=2000).contains(&force), "invalid pressure");
                    Ok((x, y, force))
                })
                .collect::<Result<Vec<_>>>()?;
            content.push_str("q 1 J 1 j\n");
            if tool == "marker" {
                content.push_str(&format!("/{marker_name} gs\n"));
            }
            content.push_str(&format!(
                "{} {} {} RG\n",
                ((color >> 16) & 255) as f64 / 255.,
                ((color >> 8) & 255) as f64 / 255.,
                (color & 255) as f64 / 255.
            ));
            for i in 0..points.len().saturating_sub(1).max(1) {
                let a = points[i];
                let b = points[(i + 1).min(points.len() - 1)];
                let thickness = width as f64
                    * if pressure {
                        (b.2 as f64 / 1000.).clamp(0.2, 2.)
                    } else {
                        1.
                    };
                // A tiny line makes taps visible with round caps in all PDF viewers.
                let bx = if a.0 == b.0 && a.1 == b.1 {
                    b.0 as f64 + 0.01
                } else {
                    b.0 as f64
                };
                content.push_str(&format!(
                    "{thickness} w {} {} m {bx} {} l S\n",
                    a.0, a.1, b.1
                ));
            }
            content.push_str("Q\n");
        }
        content.push_str("Q\n");
        ensure!(
            content.len() <= 32 * 1024 * 1024,
            "rendered page exceeds limit"
        );
        let old = doc.get_page_content_with_limit(id, 64 * 1024 * 1024)?;
        let mut combined = b"q\n".to_vec();
        combined.extend(old);
        combined.extend(b"\nQ\n");
        combined.extend(content.as_bytes());
        let stream = doc.add_object(Stream::new(Dictionary::new(), combined));
        resources.set("XObject", objects);
        resources.set("ExtGState", graphics);
        let page = doc.get_object_mut(id)?.as_dict_mut()?;
        page.set("Contents", stream);
        page.set("Resources", resources);
        pages.push(Object::Reference(id));
    }
    if base.is_none() {
        doc.objects.insert(
            tree,
            Object::Dictionary(
                dictionary! {"Type"=>"Pages", "Kids"=>pages, "Count"=>page_specs.len() as i64},
            ),
        );
        let catalog = doc.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>tree});
        doc.trailer.set("Root", catalog);
    }
    // Avoid automatic compression heuristics/timestamps; stable object allocation and ordering.
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes)?;
    ensure!(bytes.len() <= 256 * 1024 * 1024, "PDF exceeds 256 MiB");
    Ok(bytes)
}
fn resolved(doc: &Document, value: &Object) -> Result<Object> {
    Ok(match value {
        Object::Reference(id) => doc.get_object(*id)?.clone(),
        _ => value.clone(),
    })
}
fn inherited(doc: &Document, mut page: ObjectId, key: &[u8]) -> Result<Option<Object>> {
    for _ in 0..32 {
        let dict = doc.get_object(page)?.as_dict()?;
        if let Ok(value) = dict.get(key) {
            return Ok(Some(resolved(doc, value)?));
        }
        match dict.get(b"Parent").and_then(Object::as_reference) {
            Ok(parent) => page = parent,
            Err(_) => return Ok(None),
        }
    }
    bail!("cyclic PDF page tree")
}
fn subdict(doc: &Document, parent: &Dictionary, key: &[u8]) -> Result<Dictionary> {
    Ok(match parent.get(key) {
        Ok(v) => resolved(doc, v)?.as_dict()?.clone(),
        Err(_) => Dictionary::new(),
    })
}
fn number(v: &Object) -> Result<f64> {
    match v {
        Object::Integer(n) => Ok(*n as f64),
        Object::Real(n) => Ok(*n as f64),
        _ => bail!("invalid PDF coordinate"),
    }
}
fn page_bounds(doc: &Document, page: ObjectId) -> Result<([f64; 4], i64)> {
    let bbox = inherited(doc, page, b"CropBox")?
        .or(inherited(doc, page, b"MediaBox")?)
        .ok_or_else(|| anyhow!("missing PDF page box"))?;
    let values = bbox.as_array()?;
    ensure!(values.len() == 4, "invalid PDF page box");
    let bounds = [
        number(&values[0])?,
        number(&values[1])?,
        number(&values[2])?,
        number(&values[3])?,
    ];
    ensure!(
        bounds.iter().all(|n| n.is_finite()) && bounds[2] > bounds[0] && bounds[3] > bounds[1],
        "invalid PDF bounds"
    );
    let rotation = inherited(doc, page, b"Rotate")?
        .map(|v| v.as_i64())
        .transpose()?
        .unwrap_or(0)
        .rem_euclid(360);
    ensure!(
        [0, 90, 180, 270].contains(&rotation),
        "unsupported PDF rotation"
    );
    let unit = inherited(doc, page, b"UserUnit")?
        .map(|v| number(&v))
        .transpose()?
        .unwrap_or(1.);
    ensure!(unit == 1., "PDF UserUnit must be 1 for annotation");
    Ok((bounds, rotation))
}
fn free_name(dict: &Dictionary, prefix: &str) -> String {
    (0..)
        .map(|i| format!("{prefix}{i}"))
        .find(|s| !dict.has(s.as_bytes()))
        .unwrap()
}
#[allow(clippy::too_many_arguments)]
fn place(
    doc: &mut Document,
    objects: &mut Dictionary,
    imported: &mut HashMap<String, Vec<ObjectId>>,
    content: &mut String,
    files: &BTreeMap<String, Vec<u8>>,
    asset: &str,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    pdf_page: Option<i64>,
) -> Result<()> {
    ensure!(
        w > 0
            && h > 0
            && w <= 10000000
            && h <= 10000000
            && x.unsigned_abs() <= 10000000
            && y.unsigned_abs() <= 10000000,
        "invalid image transform"
    );
    let bytes = files
        .get(asset)
        .ok_or_else(|| anyhow!("missing asset {asset}"))?;
    let ext = asset.rsplit('.').next().unwrap_or_default();
    let (object, natural_w, natural_h) = if ext == "pdf" {
        if !imported.contains_key(asset) {
            let mut source = Document::load_mem(bytes)?;
            ensure!(!source.is_encrypted(), "encrypted templates unsupported");
            source.renumber_objects_with(doc.max_id + 1);
            let pages = source.get_pages().into_values().collect();
            doc.max_id = doc.max_id.max(source.max_id);
            doc.objects.extend(source.objects);
            imported.insert(asset.into(), pages);
        }
        let index = pdf_page.ok_or_else(|| anyhow!("PDF asset requires page index"))?;
        ensure!((0..500).contains(&index), "invalid template page");
        let page = *imported[asset]
            .get(index as usize)
            .ok_or_else(|| anyhow!("template page out of range"))?;
        let (b, rotation) = page_bounds(doc, page)?;
        let (nw, nh, matrix) = match rotation {
            0 => (b[2] - b[0], b[3] - b[1], [1., 0., 0., 1., -b[0], -b[1]]),
            90 => (b[3] - b[1], b[2] - b[0], [0., -1., 1., 0., -b[1], b[2]]),
            180 => (b[2] - b[0], b[3] - b[1], [-1., 0., 0., -1., b[2], b[3]]),
            _ => (b[3] - b[1], b[2] - b[0], [0., 1., -1., 0., b[3], -b[0]]),
        };
        let mut data = format!(
            "q {} {} {} {} {} {} cm\n",
            matrix[0], matrix[1], matrix[2], matrix[3], matrix[4], matrix[5]
        )
        .into_bytes();
        data.extend(doc.get_page_content_with_limit(page, 64 * 1024 * 1024)?);
        data.extend(b"\nQ\n");
        let resources =
            inherited(doc, page, b"Resources")?.unwrap_or(Object::Dictionary(Dictionary::new()));
        let object = doc.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Form","FormType"=>1,"BBox"=>vec![0.into(),0.into(),Object::Real(nw as f32),Object::Real(nh as f32)],"Resources"=>resources},data));
        (object, nw, nh)
    } else if let Some(cached) = imported.get(asset) {
        (cached[0], 1., 1.)
    } else {
        let rgba = if ext == "svg" {
            let source = std::str::from_utf8(bytes)?;
            let xml = roxmltree::Document::parse(source)?;
            ensure!(
                !xml.descendants().any(
                    |node| ["text", "foreignObject", "image"].contains(&node.tag_name().name())
                ),
                "SVG text must be converted to paths and nested images supplied separately"
            );
            let mut options = resvg::usvg::Options::default();
            options.image_href_resolver.resolve_string = Box::new(|_, _| None);
            options.image_href_resolver.resolve_data = Box::new(|_, _, _| None);
            let tree = resvg::usvg::Tree::from_data(bytes, &options)?;
            let size = tree.size();
            let scale = (1600. / size.width()).min(2000. / size.height()).min(1.);
            let mut pixmap = resvg::tiny_skia::Pixmap::new(
                (size.width() * scale).ceil() as u32,
                (size.height() * scale).ceil() as u32,
            )
            .ok_or_else(|| anyhow!("invalid SVG size"))?;
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            image::load_from_memory(&pixmap.encode_png()?)?.into_rgba8()
        } else {
            ensure!(
                ["png", "jpg", "jpeg"].contains(&ext),
                "unsupported image asset"
            );
            let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(12000);
            limits.max_image_height = Some(12000);
            limits.max_alloc = Some(128 * 1024 * 1024);
            reader.limits(limits);
            reader.decode()?.thumbnail(2000, 2000).into_rgba8()
        };
        let existing_bytes: usize = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .map(|s| s.content.len())
            .sum();
        ensure!(
            existing_bytes.saturating_add(rgba.len()) <= 128 * 1024 * 1024,
            "decoded PDF assets exceed 128 MiB"
        );
        let (nw, nh) = rgba.dimensions();
        let mut rgb = Vec::with_capacity(nw as usize * nh as usize * 3);
        let mut alpha = Vec::with_capacity(nw as usize * nh as usize);
        for p in rgba.pixels() {
            rgb.extend_from_slice(&p.0[..3]);
            alpha.push(p[3]);
        }
        let mask = doc.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Image","Width"=>nw as i64,"Height"=>nh as i64,"ColorSpace"=>"DeviceGray","BitsPerComponent"=>8},alpha));
        let image = doc.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Image","Width"=>nw as i64,"Height"=>nh as i64,"ColorSpace"=>"DeviceRGB","BitsPerComponent"=>8,"SMask"=>mask},rgb));
        imported.insert(asset.into(), vec![image]);
        (image, 1., 1.)
    };
    let name = free_name(objects, "InkVaultImage");
    objects.set(name.as_bytes(), object);
    content.push_str(&format!(
        "q {} 0 0 {} {x} {} cm /{name} Do Q\n",
        w as f64 / natural_w,
        -(h as f64) / natural_h,
        y + h
    ));
    Ok(())
}
