//! 离线、受控SVG栅格化：来源保持原字节，只有验证过的本地矢量引用进入渲染。
//! XML、引用展开、路径复杂度及图层按保守逻辑工作区计费；这不是分配器硬上限。
//! XML解析、SVG转换和单次渲染没有取消钩子，调用前后检查取消，不发布取消后的像素。
use crate::{
    capture::Image,
    image_color_management::{ColorConversion, ManagedMetadata},
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
    image_decode::DecodedImage,
    image_export_metadata::SourceMetadata,
    image_metadata::{ImageMetadata, SourceFormat},
    Result,
};
use resvg::{tiny_skia, usvg};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const MAX_INPUT: usize = 8 * 1024 * 1024;
const MAX_XML_NODES: u32 = 16_384;
const MAX_ELEMENTS: usize = 8192;
const MAX_ATTRIBUTES: usize = 65_536;
const MAX_NAMESPACE_DECLARATIONS: usize = 128;
const MAX_DEPTH: u32 = 64;
const MAX_REFERENCES: usize = 32_768;
const MAX_PATH_BYTES: u64 = 1024 * 1024;
const MAX_EXPANDED_NODES: u64 = 16_384;
const MAX_WORKSPACE: u64 = 128 * 1024 * 1024;
const FIXED_WORKSPACE: u64 = 16 * 1024 * 1024;
const SVG_NS: &str = "http://www.w3.org/2000/svg";

fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "SVG结构、引用或尺寸无效",
        "Invalid SVG structure, reference, or dimensions"
    )
    .into()
}

fn unsupported() -> Box<dyn std::error::Error> {
    tr!(
        "SVG包含当前离线栅格化不支持的资源、文本、效果或引用结构",
        "SVG contains resources, text, effects, or references unsupported by offline rasterization"
    )
    .into()
}

fn budget() -> Box<dyn std::error::Error> {
    tr!(
        "SVG来源、复杂度或栅格化工作区超过预算",
        "SVG input, complexity, or rasterization workspace exceeds the budget"
    )
    .into()
}

fn checkpoint(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("SVG栅格化已取消", "SVG rasterization canceled").into())
    } else {
        Ok(())
    }
}

/// 仅判断是否应按XML来源尝试；实际SVG根、命名空间及功能由decode验证。
pub(crate) fn looks_like_svg(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(bytes)
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(&b'<')
}

fn sum(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or_else(budget)
}

fn product(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).ok_or_else(budget)
}

fn visible_budget(source: u64, workspace: u64, output: u64, layers: u64) -> Result<()> {
    if sum(sum(source, workspace)?, sum(output, layers)?)? > MAX_BYTES {
        return Err(budget());
    }
    Ok(())
}

fn output_bytes((width, height): (u32, u32), limit: u64) -> Result<u64> {
    let pixels = product(u64::from(width), u64::from(height))?;
    let bytes = product(pixels, 4)?;
    if width == 0
        || height == 0
        || width > 65_535
        || height > 65_535
        || pixels > 32_000_000
        || bytes > limit.min(MAX_BYTES)
    {
        return Err(budget());
    }
    Ok(bytes)
}

fn terminated(bytes: &[u8], mut index: usize, end: &[u8], cancel: &AtomicBool) -> Result<usize> {
    while index < bytes.len() {
        if index % (64 * 1024) == 0 {
            checkpoint(cancel)?;
        }
        if bytes[index..].starts_with(end) {
            return Ok(index + end.len());
        }
        index += 1;
    }
    Err(invalid())
}

/// 只计结构，不解析名称、命名空间或实体；完整XML正确性仍由XML库判断。
/// 深度及属性限制必须先于DOM分配，避免命名空间继承先产生大量引用。
fn lexical_structure(bytes: &[u8], cancel: &AtomicBool) -> Result<()> {
    let (mut index, mut depth) = (0usize, 0u32);
    while index < bytes.len() {
        if index % (64 * 1024) == 0 {
            checkpoint(cancel)?;
        }
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"<!--") {
            index = terminated(bytes, index + 4, b"-->", cancel)?;
            continue;
        }
        if bytes[index..].starts_with(b"<![CDATA[") {
            index = terminated(bytes, index + 9, b"]]>", cancel)?;
            continue;
        }
        if bytes[index..].starts_with(b"<?") {
            // 只保留XML声明；外部stylesheet和其它处理指令不进入DOM。
            if !bytes[index + 2..].starts_with(b"xml")
                || !bytes.get(index + 5).is_some_and(u8::is_ascii_whitespace)
            {
                return Err(unsupported());
            }
            index = terminated(bytes, index + 5, b"?>", cancel)?;
            continue;
        }
        if bytes[index..].starts_with(b"<!") {
            return Err(unsupported());
        }
        let closing = bytes.get(index + 1) == Some(&b'/');
        let mut cursor = index + if closing { 2 } else { 1 };
        let mut quote = None;
        let (mut attributes, mut last) = (0usize, 0u8);
        loop {
            if cursor % (64 * 1024) == 0 {
                checkpoint(cancel)?;
            }
            let byte = *bytes.get(cursor).ok_or_else(invalid)?;
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    quote = None;
                }
            } else {
                match byte {
                    b'\'' | b'"' => quote = Some(byte),
                    b'>' => break,
                    b'<' => return Err(invalid()),
                    b'=' => {
                        attributes += 1;
                        if attributes > 64 {
                            return Err(budget());
                        }
                    }
                    _ => {}
                }
                if !byte.is_ascii_whitespace() {
                    last = byte;
                }
            }
            cursor += 1;
        }
        if closing {
            depth = depth.checked_sub(1).ok_or_else(invalid)?;
        } else {
            if depth >= MAX_DEPTH {
                return Err(budget());
            }
            if last != b'/' {
                depth += 1;
            }
        }
        index = cursor + 1;
    }
    if depth != 0 {
        return Err(invalid());
    }
    checkpoint(cancel)
}

fn lexical_budget(bytes: &[u8], reserved: u64, cancel: &AtomicBool) -> Result<u64> {
    if bytes.len() > MAX_INPUT || reserved < bytes.len() as u64 || reserved > MAX_BYTES {
        return Err(budget());
    }
    let (mut tags, mut attributes, mut namespaces) = (0usize, 0usize, 0usize);
    // XML解析器先按这些符号数量预分配，不能只依赖其后生效的nodes_limit。
    for chunk in bytes.chunks(64 * 1024) {
        checkpoint(cancel)?;
        tags += chunk.iter().filter(|byte| **byte == b'<').count();
        attributes += chunk.iter().filter(|byte| **byte == b'=').count();
        if tags > MAX_XML_NODES as usize || attributes > MAX_ATTRIBUTES {
            return Err(budget());
        }
    }
    // xmlns名称不能用实体拼写；保守地连注释/值中的同串也计费。
    // 跨块窗口单独扫描，避免块边界漏掉声明。
    for (index, window) in bytes.windows(5).enumerate() {
        if index % (64 * 1024) == 0 {
            checkpoint(cancel)?;
        }
        if window == b"xmlns" {
            namespaces += 1;
            if namespaces > MAX_NAMESPACE_DECLARATIONS {
                return Err(budget());
            }
        }
    }
    lexical_structure(bytes, cancel)?;
    let workspace = sum(
        sum(FIXED_WORKSPACE, product(bytes.len() as u64, 8)?)?,
        sum(
            sum(product(tags as u64, 256)?, product(attributes as u64, 128)?)?,
            // 每节点最多继承全部声明，按u16索引再留一倍Vec容量余量。
            product(product(tags as u64, namespaces as u64)?, 4)?,
        )?,
    )?;
    if workspace > MAX_WORKSPACE {
        return Err(budget());
    }
    visible_budget(reserved, workspace, 0, 0)?;
    Ok(workspace)
}

fn local_reference(value: &str) -> Result<&str> {
    let value = value.trim();
    let id = value
        .strip_prefix('#')
        .filter(|id| !id.is_empty())
        .ok_or_else(unsupported)?;
    if id.len() > 256
        || id.bytes().any(|byte| {
            byte.is_ascii_whitespace() || matches!(byte, b'\'' | b'"' | b'(' | b')' | b'#' | b'\\')
        })
    {
        return Err(unsupported());
    }
    Ok(id)
}

fn unsupported_css_effect(value: &str) -> bool {
    let bytes = value.as_bytes();
    for name in [
        "filter",
        "mask",
        "marker",
        "marker-start",
        "marker-mid",
        "marker-end",
        "stroke-dasharray",
    ] {
        for (index, window) in bytes.windows(name.len()).enumerate() {
            if !window.eq_ignore_ascii_case(name.as_bytes()) {
                continue;
            }
            let mut end = index + name.len();
            while bytes.get(end).is_some_and(u8::is_ascii_whitespace) {
                end += 1;
            }
            if bytes.get(end) == Some(&b':') {
                return true;
            }
        }
    }
    false
}

/// 只识别需要计费的本地url引用；CSS语义仍交给SVG库，不自行实现CSS解析器。
/// 转义、注释和at-rule超出此受控口径，明确拒绝，避免把资源指令误判为普通文本。
fn css_references<'a>(value: &'a str, references: &mut Vec<&'a str>) -> Result<()> {
    if value.contains('\\')
        || value.contains("/*")
        || value.contains('@')
        || unsupported_css_effect(value)
    {
        return Err(unsupported());
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        if bytes[index..index + 3].eq_ignore_ascii_case(b"url") {
            let mut open = index + 3;
            while bytes.get(open).is_some_and(u8::is_ascii_whitespace) {
                open += 1;
            }
            if bytes.get(open) == Some(&b'(') {
                let close = bytes[open + 1..]
                    .iter()
                    .position(|byte| *byte == b')')
                    .map(|offset| open + 1 + offset)
                    .ok_or_else(invalid)?;
                let mut target = value[open + 1..close].trim();
                if target.starts_with('"') || target.starts_with('\'') {
                    let quote = target.as_bytes()[0];
                    if target.len() < 2 || target.as_bytes().last() != Some(&quote) {
                        return Err(invalid());
                    }
                    target = &target[1..target.len() - 1];
                }
                if references.len() >= MAX_REFERENCES {
                    return Err(budget());
                }
                references.push(local_reference(target)?);
                index = close + 1;
                continue;
            }
        }
        index += 1;
    }
    Ok(())
}

struct Spec {
    edges: Vec<usize>,
    work: u64,
    paths: u64,
    drawables: u64,
}

#[derive(Clone, Copy, Default)]
struct Cost {
    nodes: u64,
    work: u64,
    paths: u64,
    drawables: u64,
    height: u32,
}

fn expanded_cost(
    index: usize,
    specs: &[Spec],
    state: &mut [u8],
    memo: &mut [Cost],
    depth: u32,
    cancel: &AtomicBool,
) -> Result<Cost> {
    checkpoint(cancel)?;
    if depth > MAX_DEPTH {
        return Err(budget());
    }
    if state[index] == 1 {
        return Err(unsupported());
    }
    if state[index] == 2 {
        if depth + memo[index].height - 1 > MAX_DEPTH {
            return Err(budget());
        }
        return Ok(memo[index]);
    }
    state[index] = 1;
    let spec = &specs[index];
    let mut cost = Cost {
        nodes: 1,
        work: spec.work,
        paths: spec.paths,
        drawables: spec.drawables,
        height: 1,
    };
    for &edge in &spec.edges {
        let child = expanded_cost(edge, specs, state, memo, depth + 1, cancel)?;
        cost.nodes = sum(cost.nodes, child.nodes)?;
        cost.work = sum(cost.work, child.work)?;
        cost.paths = sum(cost.paths, child.paths)?;
        cost.drawables = sum(cost.drawables, child.drawables)?;
        cost.height = cost.height.max(child.height + 1);
        if cost.nodes > MAX_EXPANDED_NODES
            || cost.work > MAX_WORKSPACE
            || cost.paths > MAX_PATH_BYTES
            || cost.height > MAX_DEPTH
        {
            return Err(budget());
        }
    }
    state[index] = 2;
    memo[index] = cost;
    Ok(cost)
}

fn gradient_stop_workspace(drawables: u64, stops: u64) -> Result<u64> {
    // objectBoundingBox渐变即使只在祖先声明，也会为每个实际图形派生独立stop Vec。
    // 两个paint槽（fill/stroke）分别计源缓存和逐图形副本；不假定继承或本地paint链共享。
    // 单次context转换的临时副本及渲染器stop表仍由FIXED_WORKSPACE覆盖。
    product(
        product(drawables, stops)?,
        product(4, std::mem::size_of::<usvg::Stop>() as u64)?,
    )
}

fn validate_xml(
    doc: &usvg::roxmltree::Document<'_>,
    bytes: usize,
    cancel: &AtomicBool,
) -> Result<u64> {
    let root = doc.root_element();
    if root.tag_name().name() != "svg" || root.tag_name().namespace() != Some(SVG_NS) {
        return Err(invalid());
    }
    let mut nodes = Vec::new();
    let mut indexes = HashMap::new();
    let mut ids = HashMap::new();
    let mut attributes = 0;
    for node in doc.descendants() {
        checkpoint(cancel)?;
        if node.is_pi() {
            return Err(unsupported());
        }
        if !node.is_element() {
            continue;
        }
        if nodes.len() >= MAX_ELEMENTS || node.ancestors().count() as u32 > MAX_DEPTH {
            return Err(budget());
        }
        if node.tag_name().namespace() != Some(SVG_NS) {
            return Err(unsupported());
        }
        match node.tag_name().name() {
            "svg" | "g" | "defs" | "desc" | "title" | "metadata" | "switch" | "a" | "symbol"
            | "use" | "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon"
            | "linearGradient" | "radialGradient" | "stop" | "clipPath" | "style" => {}
            _ => return Err(unsupported()),
        }
        let count = node.attributes().len();
        attributes += count;
        if count > 64 || attributes > MAX_ATTRIBUTES {
            return Err(budget());
        }
        let index = nodes.len();
        indexes.insert(node.id(), index);
        if let Some(id) = node.attribute("id") {
            if id.is_empty() || id.len() > 256 || ids.insert(id, index).is_some() {
                return Err(invalid());
            }
        }
        nodes.push(node);
    }
    let mut specs = Vec::new();
    let mut all_references = 0usize;
    let stops = nodes
        .iter()
        .filter(|node| node.has_tag_name((SVG_NS, "stop")))
        .count() as u64;
    for node in &nodes {
        checkpoint(cancel)?;
        let mut edges = Vec::new();
        for child in node.children().filter(|child| child.is_element()) {
            edges.push(*indexes.get(&child.id()).ok_or_else(invalid)?);
        }
        let mut references = Vec::new();
        let mut work = 4096u64;
        let mut paths = 0;
        for attribute in node.attributes() {
            checkpoint(cancel)?;
            let (name, value) = (attribute.name(), attribute.value());
            if value.len() > 128 * 1024 {
                return Err(budget());
            }
            work = sum(work, product(value.len() as u64, 16)?)?;
            if name == "base" || name.starts_with("on") {
                return Err(unsupported());
            }
            if matches!(
                name,
                "filter"
                    | "mask"
                    | "marker"
                    | "marker-start"
                    | "marker-mid"
                    | "marker-end"
                    | "stroke-dasharray"
            ) && value.trim() != "none"
            {
                return Err(unsupported());
            }
            if name == "href" {
                references.push(local_reference(value)?);
            } else {
                css_references(value, &mut references)?;
            }
            if matches!(name, "d" | "points") {
                paths = sum(paths, value.len() as u64)?;
                if value.len() > 128 * 1024 || paths > MAX_PATH_BYTES {
                    return Err(budget());
                }
            }
            if name == "clip-path"
                && value.trim() != "none"
                && node
                    .ancestors()
                    .any(|ancestor| ancestor.has_tag_name((SVG_NS, "clipPath")))
            {
                return Err(unsupported());
            }
        }
        if node.has_tag_name((SVG_NS, "style")) {
            for child in node.children().filter(|child| child.is_text()) {
                let value = child.text().unwrap_or_default();
                if value.len() > 32 * 1024
                    || value.bytes().filter(|byte| *byte == b'{').count() > 256
                {
                    return Err(budget());
                }
                let before = references.len();
                css_references(value, &mut references)?;
                // 样式表选择器可把引用施加给任意节点；首批只接presentation/inline引用，
                // 使每条引用都能在转换前绑定到准确来源节点并检查展开与循环。
                if references.len() != before {
                    return Err(unsupported());
                }
                work = sum(work, product(value.len() as u64, 16)?)?;
            }
        }
        all_references += references.len();
        if all_references > MAX_REFERENCES {
            return Err(budget());
        }
        for id in references {
            edges.push(*ids.get(id).ok_or_else(invalid)?);
        }
        let drawables = u64::from(matches!(
            node.tag_name().name(),
            "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon"
        ));
        specs.push(Spec {
            edges,
            work,
            paths,
            drawables,
        });
    }
    let mut state = vec![0; specs.len()];
    let mut memo = vec![Cost::default(); specs.len()];
    let cost = expanded_cost(0, &specs, &mut state, &mut memo, 1, cancel)?;
    let workspace = sum(
        sum(sum(FIXED_WORKSPACE, product(bytes as u64, 8)?)?, cost.work)?,
        gradient_stop_workspace(cost.drawables, stops)?,
    )?;
    if workspace > MAX_WORKSPACE {
        return Err(budget());
    }
    Ok(workspace)
}

fn device_bounds(bounds: usvg::Rect, transform: tiny_skia::Transform) -> Result<()> {
    let bounds = bounds.transform(transform).ok_or_else(budget)?;
    if [bounds.x(), bounds.y(), bounds.width(), bounds.height()]
        .iter()
        .any(|value| !value.is_finite() || value.abs() > 1_000_000.0)
    {
        return Err(budget());
    }
    Ok(())
}

fn simple_clip(
    group: &usvg::Group,
    transform: tiny_skia::Transform,
    depth: u32,
    cancel: &AtomicBool,
) -> Result<()> {
    checkpoint(cancel)?;
    if depth > MAX_DEPTH {
        return Err(budget());
    }
    if group.clip_path().is_some() || group.mask().is_some() || !group.filters().is_empty() {
        return Err(unsupported());
    }
    let transform = transform.pre_concat(group.transform());
    for node in group.children() {
        checkpoint(cancel)?;
        match node {
            usvg::Node::Group(child) => simple_clip(child, transform, depth + 1, cancel)?,
            usvg::Node::Path(path) => {
                if path.is_visible() {
                    device_bounds(path.stroke_bounding_box(), transform)?;
                }
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(())
}

fn layer_budget(
    group: &usvg::Group,
    transform: tiny_skia::Transform,
    dimensions: (u32, u32),
    nodes: &mut u64,
    segments: &mut u64,
    depth: u32,
    cancel: &AtomicBool,
) -> Result<u64> {
    checkpoint(cancel)?;
    if depth > MAX_DEPTH {
        return Err(budget());
    }
    if group.mask().is_some() || !group.filters().is_empty() {
        return Err(unsupported());
    }
    if let Some(clip) = group.clip_path() {
        simple_clip(
            clip.root(),
            transform
                .pre_concat(group.abs_transform())
                .pre_concat(clip.transform()),
            1,
            cancel,
        )?;
    }
    let mut layers = 0;
    if group.should_isolate() {
        let bounds = group
            .abs_layer_bounding_box()
            .transform(transform)
            .ok_or_else(invalid)?;
        if [bounds.x(), bounds.y(), bounds.width(), bounds.height()]
            .iter()
            .any(|value| !value.is_finite() || value.abs() > 1_000_000.0)
        {
            return Err(budget());
        }
        // 包括抗锯齿外扩与向整数取整；库自身再裁至5W×5H区域。
        let width = (f64::from(bounds.width()).ceil() as u64 + 8).min(u64::from(dimensions.0) * 5);
        let height =
            (f64::from(bounds.height()).ceil() as u64 + 8).min(u64::from(dimensions.1) * 5);
        let factor = if group.clip_path().is_some() { 9 } else { 4 };
        layers = product(product(width, height)?, factor)?;
    }
    for node in group.children() {
        checkpoint(cancel)?;
        *nodes = sum(*nodes, 1)?;
        if *nodes > MAX_EXPANDED_NODES {
            return Err(budget());
        }
        match node {
            usvg::Node::Group(child) => {
                layers = sum(
                    layers,
                    layer_budget(
                        child,
                        transform,
                        dimensions,
                        nodes,
                        segments,
                        depth + 1,
                        cancel,
                    )?,
                )?;
            }
            usvg::Node::Path(path) => {
                // 渲染器对极端坐标可能只跳过绘制；不能把这种透明结果标为成功栅格化。
                if path.is_visible() {
                    device_bounds(path.abs_stroke_bounding_box(), transform)?;
                }
                // 极小dash间距会按几何长度扩张路径，不能仅按来源命令数计费。
                if path
                    .stroke()
                    .is_some_and(|stroke| stroke.dasharray().is_some())
                {
                    return Err(unsupported());
                }
                *segments = sum(*segments, path.data().len() as u64)?;
                if *segments > 262_144 {
                    return Err(budget());
                }
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(layers)
}

fn raster_layout(
    tree: &usvg::Tree,
    requested: Option<(u32, u32)>,
    limit: u64,
) -> Result<((u32, u32), tiny_skia::Transform, u64)> {
    let size = tree.size();
    let (width, height) = (f64::from(size.width()), f64::from(size.height()));
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(invalid());
    }
    let dimensions = if let Some(dimensions) = requested {
        dimensions
    } else {
        if width.ceil() > 65_535.0 || height.ceil() > 65_535.0 {
            return Err(budget());
        }
        (width.ceil() as u32, height.ceil() as u32)
    };
    let bytes = output_bytes(dimensions, limit)?;
    let scale = (f64::from(dimensions.0) / width).min(f64::from(dimensions.1) / height);
    let x = (f64::from(dimensions.0) - width * scale) * 0.5;
    let y = (f64::from(dimensions.1) - height * scale) * 0.5;
    if !scale.is_finite()
        || scale <= 0.0
        || !(scale as f32).is_normal()
        || !x.is_finite()
        || !y.is_finite()
    {
        return Err(invalid());
    }
    Ok((
        dimensions,
        tiny_skia::Transform::from_row(scale as f32, 0.0, 0.0, scale as f32, x as f32, y as f32),
        bytes,
    ))
}

fn straight_bgra(bytes: &mut [u8], width: u32, cancel: &AtomicBool) -> Result<()> {
    for row in bytes.chunks_exact_mut(width as usize * 4) {
        checkpoint(cancel)?;
        for pixel in row.chunks_exact_mut(4) {
            let alpha = u32::from(pixel[3]);
            if alpha == 0 {
                pixel.fill(0);
            } else {
                for channel in &mut pixel[..3] {
                    *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
                pixel.swap(0, 2);
            }
        }
    }
    checkpoint(cancel)
}

pub(crate) fn decode(
    bytes: &[u8],
    source_reserved_bytes: u64,
    requested: Option<(u32, u32)>,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    checkpoint(cancel)?;
    let result = decode_inner(
        bytes,
        source_reserved_bytes,
        requested,
        max_output_bytes,
        cancel,
    );
    checkpoint(cancel)?;
    result
}

fn decode_inner(
    bytes: &[u8],
    reserved: u64,
    requested: Option<(u32, u32)>,
    limit: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let lexical_workspace = lexical_budget(bytes, reserved, cancel)?;
    if let Some(dimensions) = requested {
        visible_budget(
            reserved,
            lexical_workspace,
            output_bytes(dimensions, limit)?,
            0,
        )?;
    }
    if !looks_like_svg(bytes) {
        return Err(unsupported());
    }
    let text = std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))?;
    let parsed = usvg::roxmltree::Document::parse_with_options(
        text,
        usvg::roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
        },
    );
    checkpoint(cancel)?;
    let doc = parsed?;
    let workspace = validate_xml(&doc, bytes.len(), cancel)?.max(lexical_workspace);
    visible_budget(reserved, workspace, 0, 0)?;
    let denied_resource = AtomicBool::new(false);
    let options = usvg::Options {
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| {
                denied_resource.store(true, Ordering::Relaxed);
                None
            }),
            resolve_string: Box::new(|_, _| {
                denied_resource.store(true, Ordering::Relaxed);
                None
            }),
        },
        ..usvg::Options::default()
    };
    // 默认fontdb为空；本入口不调用系统字体、目录或字体文件加载，XML文本已明确拒绝。
    let converted = usvg::Tree::from_xmltree(&doc, &options);
    checkpoint(cancel)?;
    if denied_resource.load(Ordering::Relaxed) {
        return Err(unsupported());
    }
    let tree = converted?;
    drop(doc);
    // 文本由转换前XML白名单及下面的逐节点检查拒绝；不能依赖不准确的Tree文本摘要。
    if !tree.filters().is_empty() || !tree.masks().is_empty() || !tree.patterns().is_empty() {
        return Err(unsupported());
    }
    let (dimensions, transform, length) = raster_layout(&tree, requested, limit)?;
    for clip in tree.clip_paths() {
        checkpoint(cancel)?;
        if clip.clip_path().is_some() {
            return Err(unsupported());
        }
        simple_clip(
            clip.root(),
            transform.pre_concat(clip.transform()),
            1,
            cancel,
        )?;
    }
    let (mut nodes, mut segments) = (0, 0);
    let layers = layer_budget(
        tree.root(),
        transform,
        dimensions,
        &mut nodes,
        &mut segments,
        1,
        cancel,
    )?;
    visible_budget(reserved, workspace, length, layers)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(usize::try_from(length).map_err(|_| budget())?)?;
    visible_budget(reserved, workspace, pixels.capacity() as u64, layers)?;
    pixels.resize(length as usize, 0);
    checkpoint(cancel)?;
    {
        let mut target = tiny_skia::PixmapMut::from_bytes(&mut pixels, dimensions.0, dimensions.1)
            .ok_or_else(invalid)?;
        resvg::render(&tree, transform, &mut target);
    }
    checkpoint(cancel)?;
    // 复用目标缓冲，不保留第二份整图；透明像素没有可恢复的隐藏RGB。
    straight_bgra(&mut pixels, dimensions.0, cancel)?;
    let absent = FieldSummary {
        status: MetadataStatus::Absent,
        encoded_bytes: 0,
        decoded_bytes: None,
    };
    let facts = ManagedMetadata {
        source: Summary {
            icc: absent,
            exif: absent,
            color_model: ColorModel::Rgb,
            bits_per_channel: None,
            orientation: None,
        },
        profile: None,
        color_conversion: ColorConversion::RenderedSrgb,
    };
    let source = Arc::new(SourceMetadata::new(facts, None)?);
    Ok(DecodedImage {
        image: Image {
            width: dimensions.0,
            height: dimensions.1,
            bgra: pixels,
            readback_ms: 0.0,
        },
        metadata: ImageMetadata {
            format: SourceFormat::Svg,
            encoded_dimensions: None,
            display_dimensions: dimensions,
            orientation: None,
            encoded_bytes: Some(bytes.len() as u64),
            decoded_color: Some(image::ColorType::Rgba8),
            details: Some(facts),
        },
        warning: None,
        metadata_source: Some(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str, target: Option<(u32, u32)>) -> Result<DecodedImage> {
        decode(
            source.as_bytes(),
            source.len() as u64,
            target,
            MAX_BYTES,
            &AtomicBool::new(false),
        )
    }

    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * image.width + x) * 4) as usize;
        image.bgra[index..index + 4].try_into().unwrap()
    }

    #[test]
    fn image_svg_empty_and_path_only_trees_are_not_mistaken_for_text() {
        let empty = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
        assert_eq!(pixel(&render(empty, None).unwrap().image, 0, 0), [0; 4]);
        let path = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><path d="M0 0H1V1H0Z" fill="red"/></svg>"#;
        assert_eq!(
            pixel(&render(path, None).unwrap().image, 0, 0),
            [0, 0, 255, 255]
        );
        let text =
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><text>A</text></svg>"#;
        assert!(render(text, None).is_err());
    }

    #[test]
    fn image_svg_actual_render_preserves_straight_alpha_and_source_identity() {
        let source = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="2"><style>.blue{fill:blue}</style><rect width="2" height="2" fill="#ff0000" fill-opacity="0.5"/><rect class="blue" x="2" width="1" height="2"/></svg>"##;
        let decoded = render(source, None).unwrap();
        assert_eq!(decoded.metadata.format, SourceFormat::Svg);
        assert_eq!(decoded.metadata.encoded_dimensions, None);
        assert_eq!(decoded.metadata.display_dimensions, (4, 2));
        assert_eq!(pixel(&decoded.image, 0, 0), [0, 0, 255, 128]);
        assert_eq!(pixel(&decoded.image, 2, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&decoded.image, 3, 0), [0, 0, 0, 0]);
        let facts = decoded.metadata_source.unwrap().facts();
        assert_eq!(facts.color_conversion, ColorConversion::RenderedSrgb);
        assert_eq!(facts.source.icc.status, MetadataStatus::Absent);
        assert_eq!(facts.source.bits_per_channel, None);
        assert_eq!(
            decoded.metadata.decoded_color,
            Some(image::ColorType::Rgba8)
        );
    }

    #[test]
    fn image_svg_exact_viewport_uses_uniform_meet_and_transparent_centering() {
        let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="1" viewBox="10 20 4 2"><rect x="10" y="20" width="4" height="2" fill="lime"/></svg>"#;
        let decoded = render(source, Some((8, 8))).unwrap();
        assert_eq!(decoded.metadata.display_dimensions, (8, 8));
        assert_eq!(pixel(&decoded.image, 4, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&decoded.image, 0, 2), [0, 255, 0, 255]);
        assert_eq!(pixel(&decoded.image, 7, 5), [0, 255, 0, 255]);
        assert_eq!(pixel(&decoded.image, 4, 7), [0, 0, 0, 0]);
    }

    #[test]
    fn image_svg_local_use_gradient_transform_and_simple_clip_render_actual_pixels() {
        let source = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="4"><defs><linearGradient id="g"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient><clipPath id="c"><rect width="4" height="4"/></clipPath><g id="shape"><rect width="8" height="4" style="fill:url(#g)"/></g></defs><g clip-path="url(#c)"><use href="#shape" transform="translate(1 0)"/></g></svg>"##;
        let decoded = render(source, None).unwrap();
        let left = pixel(&decoded.image, 1, 1);
        let middle = pixel(&decoded.image, 3, 1);
        assert!(left[2] > left[0] && left[3] == 255);
        assert!(middle[0] > left[0] && middle[2] < left[2]);
        assert_eq!(pixel(&decoded.image, 0, 1), [0, 0, 0, 0]);
        assert_eq!(pixel(&decoded.image, 6, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn image_svg_external_data_css_resources_and_dtd_are_rejected_before_rendering() {
        for body in [
            r#"<image href="file:///not-readable.png"/>"#,
            r#"<image href="data:image/png;base64,AAAA"/>"#,
            r#"<use href="https://example.invalid/a.svg#x"/>"#,
            r#"<rect width="2" height="2" fill="url(other.svg#g)"/>"#,
            r#"<style>@import "other.css";</style>"#,
            r#"<style>rect{fill:u\72l(other.svg#g)}</style>"#,
            r#"<rect style="fill:url('C:\not-readable.png')"/>"#,
        ] {
            let source = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">{body}</svg>"#
            );
            assert!(render(&source, None).is_err(), "{body}");
        }
        let dtd = r#"<!DOCTYPE svg [<!ENTITY x "test">]><svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><desc>&x;</desc></svg>"#;
        assert!(render(dtd, None).is_err());
        assert!(render("<html/>", None).is_err());
    }

    #[test]
    fn image_svg_unsupported_effects_text_and_reference_cycles_are_explicit_errors() {
        for body in [
            "<filter id='f'/>", "<mask id='m'/>", "<pattern id='p'/>", "<text>Hello</text>",
            "<foreignObject/>", "<script/>", "<animate/>",
            "<path d='M0 0L2 2' stroke='red' stroke-dasharray='0.00000001 0.00000001'/>",
            "<g style='filter:blur(1e38)'><rect width='1' height='1'/></g>",
            "<defs><g id='a'><use href='#b'/></g><g id='b'><use href='#a'/></g></defs><use href='#a'/>",
            "<defs><g id='a'/><g id='a'/></defs>",
        ] {
            let source = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">{body}</svg>"#);
            assert!(render(&source, None).is_err(), "{body}");
        }
    }

    #[test]
    fn image_svg_complexity_and_source_reservation_are_checked_before_library_work() {
        let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"/>"#;
        let cancel = AtomicBool::new(false);
        assert!(decode(
            source.as_bytes(),
            source.len() as u64 - 1,
            None,
            MAX_BYTES,
            &cancel
        )
        .is_err());
        assert!(decode(source.as_bytes(), MAX_BYTES, None, MAX_BYTES, &cancel).is_err());
        for dimensions in [(0, 1), (65_536, 1), (8000, 4001)] {
            assert!(render(source, Some(dimensions)).is_err());
        }
        assert!(decode(source.as_bytes(), source.len() as u64, None, 15, &cancel).is_err());
        let mut deep =
            String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">"#);
        deep.push_str(&"<g>".repeat(MAX_DEPTH as usize));
        deep.push_str(&"</g>".repeat(MAX_DEPTH as usize));
        deep.push_str("</svg>");
        assert!(render(&deep, None).is_err());
        let markers = vec![b'<'; MAX_XML_NODES as usize + 1];
        assert!(lexical_budget(&markers, markers.len() as u64, &cancel).is_err());
        let mut source = String::from(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><defs><g id="a0"><rect width="1" height="1"/></g>"#,
        );
        for index in 1..16 {
            source.push_str(&format!(
                r##"<g id="a{index}"><use href="#a{}"/><use href="#a{}"/></g>"##,
                index - 1,
                index - 1
            ));
        }
        source.push_str(r##"</defs><use href="#a15"/></svg>"##);
        assert!(render(&source, None).is_err());
        assert!(visible_budget(MAX_BYTES - 24, 8, 8, 8).is_ok());
        assert!(visible_budget(MAX_BYTES - 23, 8, 8, 8).is_err());
        assert!(visible_budget(u64::MAX, 8, 0, 0).is_err());
    }

    #[test]
    fn image_svg_clipped_layers_are_budgeted_before_allocating_the_target_pixels() {
        let source = r##"<svg xmlns="http://www.w3.org/2000/svg" width="3" height="2"><defs><clipPath id="c"><rect width="3" height="2"/></clipPath></defs><g clip-path="url(#c)"><rect width="3" height="2" fill="lime"/></g></svg>"##;
        let small = render(source, Some((6, 4))).unwrap();
        assert_eq!(pixel(&small.image, 2, 2), [0, 255, 0, 255]);
        // 最终24MP虽合法，group RGBA层、clip RGBA和alpha mask仍须另外计费。
        let error = render(source, Some((6000, 4000)))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("预算") || error.contains("budget"));
    }

    #[test]
    fn image_svg_inherited_gradient_stop_product_is_rejected_before_svg_conversion() {
        fn preflight(source: &str) -> Result<u64> {
            let cancel = AtomicBool::new(false);
            lexical_budget(source.as_bytes(), source.len() as u64, &cancel)?;
            let doc = usvg::roxmltree::Document::parse_with_options(
                source,
                usvg::roxmltree::ParsingOptions {
                    allow_dtd: false,
                    nodes_limit: MAX_XML_NODES,
                },
            )?;
            validate_xml(&doc, source.len(), &cancel)
        }

        // paint声明在祖先，别名通过本地href取stop，几何由use展开。
        // 原XML与引用图都很小，但每个实际图形有fill/stroke两个派生stop数组。
        let mut source = String::from(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1" fill="url(#alias)" stroke="url(#alias)"><defs><linearGradient id="paint">"##,
        );
        for index in 0..1000 {
            source.push_str(&format!(
                r#"<stop offset="{}%" stop-color="{}"/>"#,
                index as f64 / 10.0,
                if index % 2 == 0 { "red" } else { "blue" },
            ));
        }
        source.push_str(
            r##"</linearGradient><linearGradient id="alias" href="#paint"/><g id="tile">"##,
        );
        for _ in 0..20 {
            source.push_str(r#"<rect width="1" height="1"/>"#);
        }
        source.push_str("</g></defs>");
        for _ in 0..150 {
            source.push_str(r##"<use href="#tile"/>"##);
        }
        source.push_str("</svg>");
        // 同一结构/属性/引用图的非stop控制样本通过，排除节点、路径或XML原有上限。
        // 只调用前置XML预算，不能先进入usvg再依靠事后检查拒绝大数组。
        assert!(preflight(&source.replace("<stop ", "<title ")).is_ok());
        assert!(preflight(&source).is_err());
        assert!(gradient_stop_workspace(3020, 1000).unwrap() > MAX_WORKSPACE);
        assert!(gradient_stop_workspace(u64::MAX, 2).is_err());
        let per_drawable = gradient_stop_workspace(1, 1).unwrap();
        let boundary = MAX_WORKSPACE / per_drawable;
        assert!(gradient_stop_workspace(boundary, 1).unwrap() <= MAX_WORKSPACE);
        assert!(gradient_stop_workspace(boundary + 1, 1).unwrap() > MAX_WORKSPACE);
    }

    #[test]
    fn image_svg_small_inherited_fill_and_stroke_gradients_still_render() {
        let source = r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="2"><defs><linearGradient id="paint"><stop offset="0" stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient><linearGradient id="alias" href="#paint"/></defs><g fill="url(#alias)" stroke="url(#alias)" stroke-width="0.1"><rect width="2" height="2"/><rect x="2" width="2" height="2"/></g></svg>"##;
        let decoded = render(source, None).unwrap();
        let left = pixel(&decoded.image, 0, 0);
        let right = pixel(&decoded.image, 1, 0);
        assert_eq!(left[3], 255);
        assert_eq!(right[3], 255);
        assert!(left[2] > left[0]);
        assert!(right[0] > right[2]);
        let repeated = pixel(&decoded.image, 2, 0);
        assert_eq!(repeated[3], 255);
        assert!(repeated[2] > repeated[0]);
    }

    #[test]
    fn image_svg_namespace_depth_and_declarations_are_bounded_before_xml_parsing() {
        let mut deep = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg">"#);
        for index in 0..MAX_DEPTH {
            deep.push_str(&format!(r#"<g xmlns:p{index}="urn:{index}">"#));
        }
        deep.push_str(&"</g>".repeat(MAX_DEPTH as usize));
        deep.push_str("</svg>");
        assert!(
            lexical_budget(deep.as_bytes(), deep.len() as u64, &AtomicBool::new(false)).is_err()
        );
        let mut wide = String::from(r#"<svg xmlns="http://www.w3.org/2000/svg">"#);
        for index in 0..MAX_NAMESPACE_DECLARATIONS {
            wide.push_str(&format!(r#"<g xmlns:p{index}="urn:{index}"/>"#));
        }
        wide.push_str("</svg>");
        assert!(
            lexical_budget(wide.as_bytes(), wide.len() as u64, &AtomicBool::new(false)).is_err()
        );
        let harmless = r##"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="1" height="1" data-note=" > / > "><!-- <g><g/> </svg> --><desc><![CDATA[<g><g/> </svg> >]]></desc><rect width="1" height="1" fill="red"/></svg>"##;
        assert!(lexical_budget(
            harmless.as_bytes(),
            harmless.len() as u64,
            &AtomicBool::new(false)
        )
        .is_ok());
        assert_eq!(
            pixel(&render(harmless, None).unwrap().image, 0, 0),
            [0, 0, 255, 255]
        );
    }

    #[test]
    fn image_svg_extreme_visible_geometry_cannot_succeed_as_a_silently_skipped_path() {
        for body in [
            r#"<rect width="1e38" height="1" fill="red"/>"#,
            r##"<defs><clipPath id="c"><rect width="1e38" height="1"/></clipPath></defs><g clip-path="url(#c)"><rect width="1" height="1" fill="red"/></g>"##,
        ] {
            let source = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">{body}</svg>"#
            );
            assert!(render(&source, None).is_err());
        }
    }

    #[test]
    fn image_svg_pre_cancel_wins_over_invalid_source_and_pixel_conversion_keeps_alpha() {
        let cancel = AtomicBool::new(true);
        let reason = decode(b"not xml", 0, Some((0, 0)), 0, &cancel)
            .err()
            .unwrap()
            .to_string();
        assert!(reason.contains("取消") || reason.contains("canceled"));
        let mut bytes = [128, 0, 0, 128, 0, 0, 0, 0, 0, 0, 255, 255];
        straight_bgra(&mut bytes, 3, &AtomicBool::new(false)).unwrap();
        assert_eq!(bytes, [0, 0, 255, 128, 0, 0, 0, 0, 255, 0, 0, 255]);
        let mut cancelled = [1, 2, 3, 4];
        assert!(straight_bgra(&mut cancelled, 1, &cancel).is_err());
        assert_eq!(cancelled, [1, 2, 3, 4]);
        assert!(looks_like_svg(
            b"\xef\xbb\xbf \r\n<?xml version='1.0'?><svg/>"
        ));
        assert!(!looks_like_svg(b"\x1f\x8bSVGZ"));
    }
}
