//! 混合矢量文字文档的组合模型；只使用真实 Document 命令与磁盘 DTO。
//! 不创建图片，不证明宿主保存、Scene 像素或实际 GUI；由主线统一编译执行。
use super::*;
use crate::annotation::{Bounds, HighlighterBlend, HighlighterShape};

const MIXED_SIZE: (u32, u32) = (640, 480);

fn mixed_editor() -> Editor {
    let full = Rect::full(MIXED_SIZE.0, MIXED_SIZE.1);
    Editor::new(full, Some(full))
}

fn mixed_fonts() -> [String; 2] {
    let names: Vec<_> = crate::text_format::fonts()
        .iter()
        .filter(|name| !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control))
        .take(2)
        .cloned()
        .collect();
    assert_eq!(names.len(), 2, "组合模型需要两个已安装命名字体");
    [names[0].clone(), names[1].clone()]
}

fn mixed_text(body: &str, light: bool, font: &str) -> Annotation {
    let mut annotation = Annotation::new(
        Tool::Text,
        Point { x: 8., y: 9. },
        Style {
            color: crate::pin_text_source::colors(light).1,
            font_size: 23.,
            text: TextStyle {
                font: FontFamily::named(font),
                italic: true,
                outline: true,
                background: true,
                alignment: Alignment::Right,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    annotation.update(Point { x: 380., y: 120. });
    annotation.text = body.into();
    annotation.rotation = 13.;
    annotation
}

fn mixed_mark(tool: Tool, x: f32, color: u32) -> Annotation {
    let mut annotation = Annotation::new(
        tool,
        Point { x, y: 150. },
        Style {
            color,
            width: 7.,
            opacity: 0.61,
            block_size: 19,
            radius: 6.,
            arrow_start: true,
            arrow_end: false,
            dashed: true,
            highlighter_blend: HighlighterBlend::Multiply,
            highlighter_shape: HighlighterShape::Line,
            ..Default::default()
        },
    );
    annotation.update(Point {
        x: x + 60.,
        y: 185.,
    });
    annotation
}

fn mixed_effect(tool: Tool, x: f32, managed: bool) -> Annotation {
    let mut annotation = mixed_mark(tool, x, 0x6a3b81);
    annotation
        .set_effect_regions(
            vec![
                Bounds {
                    x,
                    y: 210.,
                    width: 21.,
                    height: 17.,
                },
                Bounds {
                    x: x + 34.,
                    y: 239.,
                    width: 31.,
                    height: 23.,
                },
            ],
            managed,
        )
        .unwrap();
    annotation
}

fn mixed_source(light: bool) -> (Editor, Vec<u64>) {
    let fonts = mixed_fonts();
    let mut source = mixed_editor();
    // 名称遇到的顺序与本机目录索引相反，不能把局部符号当作 FontFamily 索引。
    let ids = source.document.insert_many(vec![
        mixed_text("混合文档 🙂\n原文", light, &fonts[1]),
        mixed_text("自选文字", light, &fonts[0]),
        mixed_mark(
            Tool::Highlighter,
            12.,
            crate::pin_text_source::colors(light).1,
        ),
        mixed_mark(Tool::Rectangle, 93., 0x2573a1),
        mixed_mark(Tool::Arrow, 180., 0xd24b31),
        mixed_effect(Tool::Mosaic, 265., true),
        mixed_effect(Tool::Blur, 370., false),
    ]);
    let mut custom_text = source.document.get(ids[1]).unwrap().clone();
    custom_text.style.color = 0x8a3f65;
    source.document.replace(ids[1], custom_text);
    assert!(source.document.set_group(&[ids[0], ids[3], ids[4]], true));
    let mut text = source.document.get(ids[0]).unwrap().clone();
    text.text = "已修改的正文 🙂".into();
    text.style.bold = true;
    let mut arrow = source.document.get(ids[4]).unwrap().clone();
    arrow.style.arrow_end = true;
    arrow.style.width = 12.;
    source
        .document
        .replace_many(&[(ids[0], text), (ids[4], arrow)]);
    source.document.remove(ids[2]);
    source.document.undo();
    assert!(source.document.can_undo() && source.document.can_redo());
    (source, ids)
}

fn mixed_saved(source: &Editor, light: bool) -> WireTextDocument {
    WireTextDocument::try_from_vector_text_editor(source, MIXED_SIZE, light).unwrap()
}

fn mixed_roundtrip(document: &WireTextDocument) -> WireTextDocument {
    serde_json::from_slice(&serde_json::to_vec(document).unwrap()).unwrap()
}

fn mixed_encoded(source: &Editor, light: bool) -> serde_json::Value {
    serde_json::to_value(mixed_saved(source, light)).unwrap()
}

fn assert_mixed_current(source: &Editor, restored: &Editor, from_light: bool, to_light: bool) {
    let expected: Vec<_> = source
        .document
        .items()
        .iter()
        .cloned()
        .map(|mut object| {
            crate::pin_text_source::remap_theme(&mut object.annotation, from_light, to_light);
            object
        })
        .collect();
    assert_eq!(restored.document.items(), expected);
    assert_eq!(restored.document.crop_mask, source.document.crop_mask);
    assert_eq!(restored.crop, source.crop);
    assert_eq!(restored.document.can_undo(), source.document.can_undo());
    assert_eq!(restored.document.can_redo(), source.document.can_redo());
}

#[test]
fn mixed_text_document_preserves_real_two_stacks_ids_groups_and_empty_current() {
    let (mut source, ids) = mixed_source(true);
    let wire = mixed_saved(&source, true);
    let encoded = serde_json::to_value(&wire).unwrap();
    assert_eq!(encoded["version"], 2);
    assert!(encoded["objects"].as_array().unwrap().is_empty());
    assert!(encoded.get("history").is_none());
    assert_eq!(encoded["mixedDocument"]["version"], 1);
    let mut restored = mixed_editor();
    mixed_roundtrip(&wire)
        .restore_into(&mut restored, false)
        .unwrap();
    assert_mixed_current(&source, &restored, true, false);
    assert_eq!(
        mixed_saved(&restored, false),
        wire,
        "双栈、组和分配器都应回到相同浅色基准"
    );

    while source.document.can_undo() {
        source.document.undo();
        restored.document.undo();
        assert_mixed_current(&source, &restored, true, false);
    }
    assert!(restored.document.items().is_empty());
    while source.document.can_redo() {
        source.document.redo();
        restored.document.redo();
        assert_mixed_current(&source, &restored, true, false);
    }
    assert!(
        source.document.get(ids[2]).is_none(),
        "真实待重做的删除必须执行"
    );
    let next_source = source
        .document
        .insert(mixed_mark(Tool::Rectangle, 480., 0x348f56));
    let next_restored = restored
        .document
        .insert(mixed_mark(Tool::Rectangle, 480., 0x348f56));
    assert_eq!(next_source, next_restored, "恢复不能重置分配时钟");

    // 两次真实编辑可回到相同外观；它们各是有效 Undo，不能被当作单个净零 Batch。
    let previous_steps = mixed_encoded(&source, true)["mixedDocument"]["history"]["undo"]
        .as_array()
        .unwrap()
        .len();
    let original = source.document.get(ids[0]).unwrap().clone();
    let mut temporary = original.clone();
    temporary.text.push_str(" 临时修改");
    source.document.replace(ids[0], temporary);
    source.document.replace(ids[0], original);
    let valid_cycle = mixed_encoded(&source, true);
    assert_eq!(
        valid_cycle["mixedDocument"]["history"]["undo"]
            .as_array()
            .unwrap()
            .len(),
        previous_steps + 2
    );

    source.document.clear();
    let pending = source
        .document
        .insert(mixed_mark(Tool::Arrow, 420., 0x554488));
    source.document.undo();
    assert!(source.document.items().is_empty());
    assert!(source.document.can_undo() && source.document.can_redo());
    let mut empty_restored = mixed_editor();
    mixed_roundtrip(&mixed_saved(&source, true))
        .restore_into(&mut empty_restored, false)
        .unwrap();
    assert_mixed_current(&source, &empty_restored, true, false);
    source.document.redo();
    empty_restored.document.redo();
    assert_eq!(empty_restored.document.items()[0].id, pending);
    assert_mixed_current(&source, &empty_restored, true, false);
    source.document.undo();
    empty_restored.document.undo();
    source.document.undo();
    empty_restored.document.undo();
    assert_mixed_current(&source, &empty_restored, true, false);

    // 当前只剩文字时，已删除的非文字历史仍须选择 V2，不能因 current 分类丢失。
    let font = mixed_fonts()[0].clone();
    let mut historical = mixed_editor();
    historical
        .document
        .insert(mixed_text("当前纯文字", true, &font));
    let deleted = historical
        .document
        .insert(mixed_mark(Tool::Rectangle, 20., 0x223344));
    historical.document.remove(deleted);
    assert_eq!(mixed_encoded(&historical, true)["version"], 2);
    let mut historical_restored = mixed_editor();
    mixed_saved(&historical, true)
        .restore_into(&mut historical_restored, true)
        .unwrap();
    historical_restored.document.undo();
    assert_eq!(historical_restored.document.items()[1].id, deleted);
}

#[test]
fn mixed_text_theme_changes_only_default_text_and_keeps_marker_rgb_and_effect_payloads() {
    for (from_light, to_light) in [(true, false), (false, true)] {
        let (mut source, ids) = mixed_source(from_light);
        let original_marks: Vec<_> = source
            .document
            .items()
            .iter()
            .filter(|o| o.annotation.tool != Tool::Text)
            .cloned()
            .collect();
        let mut restored = mixed_editor();
        mixed_roundtrip(&mixed_saved(&source, from_light))
            .restore_into(&mut restored, to_light)
            .unwrap();
        assert_mixed_current(&source, &restored, from_light, to_light);
        assert_eq!(
            restored.document.get(ids[0]).unwrap().style.color,
            crate::pin_text_source::colors(to_light).1
        );
        assert_eq!(restored.document.get(ids[1]).unwrap().style.color, 0x8a3f65);
        for object in &original_marks {
            assert_eq!(
                restored.document.get(object.id).unwrap(),
                &object.annotation
            );
        }
        let mosaic = restored.document.get(ids[5]).unwrap();
        assert_eq!(
            mosaic.effect_regions_world(),
            source.document.get(ids[5]).unwrap().effect_regions_world()
        );
        assert!(mosaic.effect_regions_managed());
        assert_eq!(mosaic.style.block_size, 19);
        let blur = restored.document.get(ids[6]).unwrap();
        assert!(!blur.effect_regions_managed());
        assert_eq!(blur.style.block_size, 19);
        source.document.undo();
        restored.document.undo();
        assert_mixed_current(&source, &restored, from_light, to_light);
        source.document.redo();
        restored.document.redo();
        assert_mixed_current(&source, &restored, from_light, to_light);
    }
}

fn rewrite_text_font_names(value: &mut serde_json::Value, old: &str, replacement: &str) {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("tool").and_then(serde_json::Value::as_str) == Some("text")
                && object["style"]["fontName"] == old
            {
                object.get_mut("style").unwrap()["fontName"] = replacement.into();
            }
            for child in object.values_mut() {
                rewrite_text_font_names(child, old, replacement);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                rewrite_text_font_names(child, old, replacement);
            }
        }
        _ => {}
    }
}

#[test]
fn mixed_text_named_fonts_restore_by_name_and_missing_fonts_preserve_real_content_change() {
    let fonts = mixed_fonts();
    let (source, ids) = mixed_source(true);
    let encoded = mixed_encoded(&source, true);
    assert_eq!(
        encoded["mixedDocument"]["objects"][0]["style"]["fontName"],
        fonts[1]
    );
    assert_eq!(
        encoded["mixedDocument"]["objects"][1]["style"]["fontName"],
        fonts[0]
    );
    assert!(!serde_json::to_string(&encoded)
        .unwrap()
        .contains("fontIndex"));
    let mut restored = mixed_editor();
    mixed_saved(&source, true)
        .restore_into(&mut restored, true)
        .unwrap();
    assert_eq!(
        restored
            .document
            .get(ids[0])
            .unwrap()
            .style
            .text
            .font
            .name(),
        fonts[1]
    );
    assert_eq!(
        restored
            .document
            .get(ids[1])
            .unwrap()
            .style
            .text
            .font
            .name(),
        fonts[0]
    );

    const MISSING_A: &str = "A RustShot mixed missing font A";
    const MISSING_B: &str = "A RustShot mixed missing font B";
    assert_eq!(FontFamily::named(MISSING_A), FontFamily::default());
    assert_eq!(FontFamily::named(MISSING_B), FontFamily::default());
    let mut cross_machine = mixed_editor();
    let text_id = cross_machine
        .document
        .insert(mixed_text("旧正文", true, &fonts[0]));
    cross_machine
        .document
        .insert(mixed_mark(Tool::Rectangle, 100., 0x4a285f));
    let mut changed = cross_machine.document.get(text_id).unwrap().clone();
    changed.style.text.font = FontFamily::named(&fonts[1]);
    changed.text = "新正文与粗体仍须撤销".into();
    changed.style.bold = true;
    cross_machine.document.replace(text_id, changed);
    let mut encoded = mixed_encoded(&cross_machine, true);
    // 模拟保存机器的名称在目标机器缺失；原始磁盘 Replace 仍含真实正文/粗体差异。
    rewrite_text_font_names(&mut encoded, &fonts[0], MISSING_A);
    rewrite_text_font_names(&mut encoded, &fonts[1], MISSING_B);
    let wire: WireTextDocument = serde_json::from_value(encoded).unwrap();
    wire.validate().unwrap();
    let mut restored = mixed_editor();
    wire.restore_into(&mut restored, false).unwrap();
    assert_eq!(
        restored.document.get(text_id).unwrap().style.text.font,
        FontFamily::default()
    );
    assert!(restored.document.get(text_id).unwrap().style.bold);
    assert_eq!(
        restored.document.get(text_id).unwrap().text,
        "新正文与粗体仍须撤销"
    );
    restored.document.undo();
    assert_eq!(restored.document.get(text_id).unwrap().text, "旧正文");
    assert!(!restored.document.get(text_id).unwrap().style.bold);
    assert_eq!(
        restored.document.get(text_id).unwrap().style.text.font,
        FontFamily::default()
    );
    assert_eq!(
        restored.document.items()[1].annotation,
        cross_machine.document.items()[1].annotation
    );
    restored.document.redo();
    assert_eq!(
        restored.document.get(text_id).unwrap().text,
        "新正文与粗体仍须撤销"
    );
    assert!(restored.document.get(text_id).unwrap().style.bold);

    // 原始复合批次因字体名称而有变化；只有显示映射后才整体归零，允许折叠。
    let mut mapped_zero = mixed_editor();
    let text_id = mapped_zero
        .document
        .insert(mixed_text("字体批次", true, &fonts[0]));
    let mark_id = mapped_zero
        .document
        .insert(mixed_mark(Tool::Rectangle, 200., 0x653a8f));
    let original_mark = mapped_zero.document.get(mark_id).unwrap().clone();
    let mut changed_text = mapped_zero.document.get(text_id).unwrap().clone();
    changed_text.style.text.font = FontFamily::named(&fonts[1]);
    mapped_zero.document.replace(text_id, changed_text);
    let mut changed_mark = original_mark.clone();
    changed_mark.style.width = 8.;
    mapped_zero.document.replace(mark_id, changed_mark);
    mapped_zero.document.replace(mark_id, original_mark.clone());
    let mut encoded = mixed_encoded(&mapped_zero, true);
    let commands = encoded["mixedDocument"]["history"]["undo"]
        .as_array_mut()
        .unwrap();
    let batch_commands = commands.split_off(2);
    assert_eq!(batch_commands.len(), 3, "复合批次节点来自三次真实编辑");
    commands.push(serde_json::json!({"kind":"batch","commands":[
        {"kind":"batch","commands":batch_commands}
    ]}));
    rewrite_text_font_names(&mut encoded, &fonts[0], MISSING_A);
    rewrite_text_font_names(&mut encoded, &fonts[1], MISSING_B);
    let wire: WireTextDocument = serde_json::from_value(encoded).unwrap();
    wire.validate().unwrap();
    let mut restored = mixed_editor();
    wire.restore_into(&mut restored, false).unwrap();
    let folded = mixed_encoded(&restored, false);
    assert_eq!(
        folded["mixedDocument"]["history"]["undo"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "只折叠映射后归零的复合批次"
    );
    assert_eq!(restored.document.get(mark_id).unwrap(), &original_mark);
    assert_eq!(
        restored.document.get(text_id).unwrap().style.text.font,
        FontFamily::default()
    );
    restored.document.undo();
    assert_eq!(
        restored.document.items().len(),
        1,
        "第一次实际 Undo 即撤销矩形插入，不留下无变化一步"
    );
    assert_eq!(restored.document.items()[0].id, text_id);
    restored.document.undo();
    assert!(restored.document.items().is_empty());
    assert!(!restored.document.can_undo());
    restored.document.redo();
    restored.document.redo();
    assert_eq!(restored.document.items()[1].id, mark_id);
    assert_eq!(restored.document.get(mark_id).unwrap(), &original_mark);
}

fn mixed_atomic_target() -> Editor {
    let mut target = mixed_editor();
    target.document = crate::document::Document::from_bounded_history_snapshot(
        crate::document::BoundedHistorySnapshot {
            items: Vec::new(),
            crop_mask: None,
            long_clip: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 17,
            next_group: 11,
        },
    )
    .unwrap();
    target.document.invalidate_crop_mask();
    target.crop = None;
    target.style.color = 0x112233;
    target
}

fn assert_mixed_disk_rejected_atomically(encoded: serde_json::Value, label: &str) {
    let mut target = mixed_atomic_target();
    let before = target.document.bounded_history_snapshot().unwrap();
    let crop = target.crop;
    let style = target.style;
    let revision = target.document.revision();
    let edit_revision = target.document.edit_revision();
    if let Ok(mut wire) = serde_json::from_value::<WireTextDocument>(encoded) {
        let original = serde_json::to_value(&wire).unwrap();
        assert!(wire.validate().is_err(), "{label} 必须拒绝原始磁盘语义");
        assert!(
            wire.restore_into(&mut target, false).is_err(),
            "{label} 必须拒绝安装"
        );
        // 与宿主合同一致：容量裁剪前先完整校验，坏底部不能被洗掉。
        let capacity_gate = (|| -> Result<bool> {
            wire.validate()?;
            Ok(wire.trim_oldest_history())
        })();
        assert!(capacity_gate.is_err(), "{label} 不得进入容量裁剪");
        assert_eq!(serde_json::to_value(&wire).unwrap(), original);
    }
    assert_eq!(
        target.document.bounded_history_snapshot().unwrap(),
        before,
        "{label}"
    );
    assert_eq!(target.crop, crop);
    assert_eq!(target.style, style);
    assert_eq!(target.document.revision(), revision);
    assert_eq!(target.document.edit_revision(), edit_revision);
}

#[test]
fn mixed_text_bad_nested_nodes_and_oldest_invalid_font_or_id_reject_before_mapping_or_trim() {
    let (source, _) = mixed_source(true);
    let valid = mixed_encoded(&source, true);
    let mut gate_probe = mixed_atomic_target();
    mixed_saved(&source, true)
        .restore_into(&mut gate_probe, false)
        .unwrap();
    assert_eq!(
        gate_probe.document.items().len(),
        source.document.items().len()
    );
    let legal = valid["mixedDocument"]["history"]["undo"][0].clone();
    let annotation = legal["commands"][0]["object"]["annotation"].clone();
    let ids = valid["mixedDocument"]["history"]["ids"].clone();
    let id = ids[0].clone();
    let noops = [
        (
            "等值替换",
            serde_json::json!({"kind":"replace","index":0,"before":annotation,"after":annotation}),
            false,
        ),
        (
            "空清空",
            serde_json::json!({"kind":"clear","objects":[]}),
            true,
        ),
        (
            "等值组",
            serde_json::json!({"kind":"groups","before":[[id,null]],"after":[[id,null]]}),
            false,
        ),
        (
            "等值层序",
            serde_json::json!({"kind":"order","before":ids,"after":ids}),
            false,
        ),
        (
            "等值遮罩",
            serde_json::json!({"kind":"cropMask","before":null,"after":null}),
            false,
        ),
        (
            "空批次",
            serde_json::json!({"kind":"batch","commands":[]}),
            false,
        ),
    ];
    for (label, noop, before_insert) in noops {
        for depth in 1..=2 {
            let mut node = noop.clone();
            for _ in 0..depth {
                node = serde_json::json!({"kind":"batch","commands":[node]});
            }
            let commands = if before_insert {
                vec![node, legal.clone()]
            } else {
                vec![legal.clone(), node]
            };
            let mut bad = valid.clone();
            bad["mixedDocument"]["history"]["undo"][0] =
                serde_json::json!({"kind":"batch","commands":commands});
            assert_mixed_disk_rejected_atomically(bad, label);
        }
    }
    let mut bad_font = valid.clone();
    bad_font["mixedDocument"]["history"]["undo"][0]["commands"][0]["object"]["annotation"]
        ["style"]["fontName"] = "损坏\n字体名".into();
    assert_mixed_disk_rejected_atomically(bad_font, "最旧底部非法字体名");
    let mut bad_id = valid.clone();
    bad_id["mixedDocument"]["history"]["undo"][0]["commands"][0]["object"]["id"] = 0.into();
    assert_mixed_disk_rejected_atomically(bad_id, "最旧底部零 ID");
    let mut duplicate_id = valid.clone();
    duplicate_id["mixedDocument"]["history"]["ids"][1] =
        duplicate_id["mixedDocument"]["history"]["ids"][0].clone();
    assert_mixed_disk_rejected_atomically(duplicate_id, "重复 current ID");
    let mut both_formats = valid.clone();
    both_formats["objects"] = serde_json::json!([WireTextObject::from_annotation(
        &source.document.items()[0].annotation
    )
    .unwrap()]);
    serde_json::from_value::<WireTextDocument>(both_formats.clone())
        .expect("两份合法载荷均应可解析，由版本合同拒绝双 current");
    assert_mixed_disk_rejected_atomically(both_formats, "V2 不得夹带第二份 current");
    let mut missing_history = valid.clone();
    missing_history["mixedDocument"]
        .as_object_mut()
        .unwrap()
        .remove("history");
    assert_mixed_disk_rejected_atomically(missing_history, "V2 必须含完整 mixed 历史");
    let mut cross_size = valid.clone();
    cross_size["mixedDocument"]["width"] = (MIXED_SIZE.0 + 1).into();
    assert_mixed_disk_rejected_atomically(cross_size, "内外尺寸不一致");
    let mut wrong_version = valid.clone();
    wrong_version["version"] = 3.into();
    assert_mixed_disk_rejected_atomically(wrong_version, "未知外层版本");
    let mut raster_version = valid.clone();
    raster_version["mixedDocument"]["version"] = 2.into();
    assert_mixed_disk_rejected_atomically(raster_version, "未知 mixed 核心版本");
    let mut v1_with_mixed = valid.clone();
    v1_with_mixed["version"] = 1.into();
    assert_mixed_disk_rejected_atomically(v1_with_mixed, "V1 不得夹带 mixedDocument");
    let mut oversized = valid;
    oversized["mixedDocument"]["objects"][0]["text"] = "\u{0001}".repeat(100 * 1024).into();
    oversized["mixedDocument"]["history"]["undo"] = serde_json::json!([]);
    oversized["mixedDocument"]["history"]["redo"] = serde_json::json!([]);
    assert!(serde_json::to_vec(&oversized).unwrap().len() > 512 * 1024);
    assert_mixed_disk_rejected_atomically(oversized, "输入 JSON 超过 512 KiB");

    // 有效兄弟 Insert 不能遮住子 Batch 内 A→B→A 的净零命令。
    let mut net_zero = mixed_encoded(&source, true);
    let mut changed = annotation.clone();
    changed["text"] = "批次中临时改变但最终归零".into();
    let zero_batch = serde_json::json!({"kind":"batch","commands":[
        {"kind":"replace","index":0,"before":annotation,"after":changed},
        {"kind":"replace","index":0,"before":changed,"after":annotation}
    ]});
    net_zero["mixedDocument"]["history"]["undo"][0] =
        serde_json::json!({"kind":"batch","commands":[legal,zero_batch]});
    assert_mixed_disk_rejected_atomically(net_zero, "最旧嵌套净零 Batch");

    // 合法磁盘也不能覆盖已有目标；保留其真实两栈、时钟与界面状态。
    let (mut occupied, _) = mixed_source(false);
    occupied.style.color = 0x215487;
    let before = occupied.document.bounded_history_snapshot().unwrap();
    let crop = occupied.crop;
    let style = occupied.style;
    let revision = occupied.document.revision();
    let edit_revision = occupied.document.edit_revision();
    assert!(mixed_saved(&source, true)
        .restore_into(&mut occupied, false)
        .is_err());
    assert_eq!(
        occupied.document.bounded_history_snapshot().unwrap(),
        before
    );
    assert_eq!(occupied.crop, crop);
    assert_eq!(occupied.style, style);
    assert_eq!(occupied.document.revision(), revision);
    assert_eq!(occupied.document.edit_revision(), edit_revision);
}

#[test]
fn mixed_text_budget_keeps_recent_real_chain_and_v1_with_or_without_history_stays_compatible() {
    let font = mixed_fonts()[0].clone();
    let mut source = mixed_editor();
    let id = source.document.insert(mixed_text("初始正文", true, &font));
    source
        .document
        .insert(mixed_mark(Tool::Rectangle, 200., 0x753a91));
    for index in 0..170 {
        let mut next = source.document.get(id).unwrap().clone();
        next.text = format!("最近版本-{index:03} 中文🙂");
        source.document.replace(id, next);
    }
    let wire = mixed_saved(&source, true);
    let encoded = serde_json::to_value(&wire).unwrap();
    let steps = encoded["mixedDocument"]["history"]["undo"]
        .as_array()
        .unwrap()
        .len();
    assert!(steps > 0 && steps <= 128);
    let mut restored = mixed_editor();
    mixed_roundtrip(&wire)
        .restore_into(&mut restored, true)
        .unwrap();
    for _ in 0..steps {
        source.document.undo();
        restored.document.undo();
        assert_eq!(restored.document.items(), source.document.items());
    }
    assert!(!restored.document.can_undo());
    for _ in 0..steps {
        source.document.redo();
        restored.document.redo();
        assert_eq!(restored.document.items(), source.document.items());
    }
    assert_eq!(mixed_saved(&restored, true), wire);

    let mut large = mixed_editor();
    let id = large.document.insert(mixed_text("大正文起点", true, &font));
    large
        .document
        .insert(mixed_mark(Tool::Arrow, 290., 0x517c94));
    for index in 0..9 {
        let mut next = large.document.get(id).unwrap().clone();
        next.text = format!("版本{index}\n{}", "\u{0001}".repeat(12 * 1024));
        large.document.replace(id, next);
    }
    let wire = mixed_saved(&large, true);
    assert!(serde_json::to_vec(&wire).unwrap().len() <= 480 * 1024);
    let encoded = serde_json::to_value(&wire).unwrap();
    let steps = encoded["mixedDocument"]["history"]["undo"]
        .as_array()
        .unwrap()
        .len();
    assert!(
        steps > 0 && steps < 9,
        "JSON 转义字节只裁最远历史，保留最新可执行链"
    );
    let mut restored = mixed_editor();
    wire.restore_into(&mut restored, true).unwrap();
    assert_eq!(
        restored.document.items(),
        large.document.items(),
        "当前正文与箭头完整保留"
    );
    for _ in 0..steps {
        large.document.undo();
        restored.document.undo();
        assert_eq!(restored.document.items(), large.document.items());
    }
    assert!(!restored.document.can_undo());

    let mut legacy = mixed_editor();
    let id = legacy.document.insert(mixed_text("兼容旧 V1", true, &font));
    let mut changed = legacy.document.get(id).unwrap().clone();
    changed.text = "旧 V1 重做正文".into();
    legacy.document.replace(id, changed);
    legacy.document.undo();
    let wire = mixed_saved(&legacy, true);
    assert_eq!(wire.version, 1);
    let mut restored = mixed_editor();
    mixed_roundtrip(&wire)
        .restore_into(&mut restored, true)
        .unwrap();
    assert_mixed_current(&legacy, &restored, true, true);
    legacy.document.redo();
    restored.document.redo();
    assert_mixed_current(&legacy, &restored, true, true);
    let mut without_history = serde_json::to_value(mixed_saved(&legacy, true)).unwrap();
    without_history.as_object_mut().unwrap().remove("history");
    let old: WireTextDocument = serde_json::from_value(without_history).unwrap();
    old.validate().unwrap();
    let mut restored = mixed_editor();
    old.restore_into(&mut restored, true).unwrap();
    assert_eq!(restored.document.items(), legacy.document.items());
    assert!(!restored.document.can_undo() && !restored.document.can_redo());
}

#[test]
fn mixed_text_live_theme_collapse_preserves_pure_compatibility_mixed_navigation_and_raw_rejection()
{
    use crate::document::{Document, HistoryCommand};

    let font = mixed_fonts()[0].clone();
    for in_redo in [false, true] {
        let mut source = mixed_editor();
        let id = source
            .document
            .insert(mixed_text("纯文字主题折叠", true, &font));
        let mut custom = source.document.get(id).unwrap().clone();
        custom.style.color = crate::pin_text_source::colors(false).1;
        source.document.replace(id, custom);
        if in_redo {
            source.document.undo();
        }
        source
            .document
            .remap_silently(|a| crate::pin_text_source::remap_theme(a, true, false));
        assert!(
            source.document.bounded_history_snapshot().is_err(),
            "真实主题切换留下的等值 Replace 不能通过原始快照门"
        );
        let revisions = (source.document.revision(), source.document.edit_revision());
        let items = source.document.items().to_vec();
        let old = WireTextDocument::try_from_text_editor(&source, MIXED_SIZE, false).unwrap();
        let saved = mixed_saved(&source, false);
        assert_eq!(saved.version, 1);
        assert_eq!(saved, old, "纯文字新宿主须保留原 V1 显示适配合同");
        let encoded = serde_json::to_value(&saved).unwrap();
        assert_eq!(encoded["history"]["undo"].as_array().unwrap().len(), 1);
        assert!(encoded["history"]["redo"].as_array().unwrap().is_empty());
        assert_eq!(source.document.items(), items);
        assert_eq!(
            (source.document.revision(), source.document.edit_revision()),
            revisions
        );
        assert!(
            source.document.bounded_history_snapshot().is_err(),
            "保存只能归一化候选，不改写活动历史"
        );
        let mut restored = mixed_editor();
        saved.restore_into(&mut restored, false).unwrap();
        assert_eq!(restored.document.items(), items);
        restored.document.undo();
        assert!(restored.document.items().is_empty());
        assert!(!restored.document.can_undo() && restored.document.can_redo());
        restored.document.redo();
        assert_eq!(restored.document.items()[0].id, id);
        assert!(!restored.document.can_redo());
        let next = mixed_text("纯文字分配器继续", false, &font);
        assert_eq!(source.document.insert(next.clone()), 2);
        assert_eq!(restored.document.insert(next), 2);
    }

    for in_redo in [false, true] {
        let mut source = mixed_editor();
        let text_id = source
            .document
            .insert(mixed_text("混合主题折叠", true, &font));
        let mark_id = source
            .document
            .insert(mixed_mark(Tool::Rectangle, 200., 0x653a8f));
        let original_mark = source.document.get(mark_id).unwrap().clone();
        let mut custom = source.document.get(text_id).unwrap().clone();
        custom.style.color = crate::pin_text_source::colors(false).1;
        source.document.replace(text_id, custom);
        let mut wider = original_mark.clone();
        wider.style.width = original_mark.style.width + 1.;
        source.document.replace(mark_id, wider);
        source.document.replace(mark_id, original_mark.clone());
        // 合并三次真实命令成一个复合手势；映射前颜色仍有净变化，宽度已往返。
        let mut raw = source.document.bounded_history_snapshot().unwrap();
        assert_eq!(raw.undo.len(), 5);
        let changes = raw.undo.split_off(2);
        raw.undo
            .push(HistoryCommand::Batch(vec![HistoryCommand::Batch(changes)]));
        raw.next_id = 17;
        raw.next_group = 11;
        crate::pin_raster_document::WireRasterDocument::from_vector_text_snapshot(
            raw.clone(),
            MIXED_SIZE,
            true,
        )
        .unwrap();
        source.document = Document::from_bounded_history_snapshot(raw).unwrap();
        if in_redo {
            source.document.undo();
        }
        source
            .document
            .remap_silently(|a| crate::pin_text_source::remap_theme(a, true, false));
        let live = source.document.bounded_history_snapshot().unwrap();
        // 受信 live 宿主与外来 typed 入口必须区分：等值子节点仍不得绕过原始 strict。
        assert!(
            crate::pin_raster_document::WireRasterDocument::from_vector_text_snapshot(
                live.clone(),
                MIXED_SIZE,
                false,
            )
            .is_err()
        );
        let revisions = (source.document.revision(), source.document.edit_revision());
        let saved = mixed_saved(&source, false);
        let encoded = serde_json::to_value(&saved).unwrap();
        assert_eq!(saved.version, 2);
        let history = &encoded["mixedDocument"]["history"];
        let undo = history["undo"].as_array().unwrap();
        assert_eq!(undo.len(), 2, "只保留两个真实 Insert，不留下净零批次");
        assert!(undo.iter().all(|command| command["kind"] == "insert"));
        assert!(history["redo"].as_array().unwrap().is_empty());
        assert_eq!(history["nextId"], 17);
        assert_eq!(history["nextGroup"], 11);
        assert_eq!(source.document.bounded_history_snapshot().unwrap(), live);
        assert_eq!(
            (source.document.revision(), source.document.edit_revision()),
            revisions
        );

        let mut restored = mixed_editor();
        saved.restore_into(&mut restored, false).unwrap();
        assert_eq!(restored.document.items(), source.document.items());
        assert_eq!(restored.document.get(mark_id).unwrap(), &original_mark);
        let restored_snapshot = restored.document.bounded_history_snapshot().unwrap();
        assert_eq!(
            (restored_snapshot.next_id, restored_snapshot.next_group),
            (17, 11)
        );
        restored.document.undo();
        assert_eq!(
            restored.document.items().len(),
            1,
            "第一次实际 Undo 即移除矩形，不先消耗空操作"
        );
        assert_eq!(restored.document.items()[0].id, text_id);
        restored.document.undo();
        assert!(restored.document.items().is_empty());
        assert!(!restored.document.can_undo());
        restored.document.redo();
        assert_eq!(restored.document.items()[0].id, text_id);
        restored.document.redo();
        assert_eq!(restored.document.items()[1].id, mark_id);
        assert_eq!(restored.document.get(mark_id).unwrap(), &original_mark);
        assert!(!restored.document.can_redo());

        // 合法 mixed 成果夹带外来等值子节点，仍须在映射或裁剪前原子拒绝。
        let mut malformed = encoded;
        let legal = malformed["mixedDocument"]["history"]["undo"][0].clone();
        let annotation = legal["object"]["annotation"].clone();
        malformed["mixedDocument"]["history"]["undo"][0] = serde_json::json!({
            "kind":"batch", "commands":[legal, {
                "kind":"batch", "commands":[{
                    "kind":"replace", "index":0, "before":annotation, "after":annotation
                }]
            }]
        });
        assert_mixed_disk_rejected_atomically(malformed, "live 主题适配不放宽外来等值节点");

        let next = mixed_mark(Tool::Arrow, 420., 0x554488);
        assert_eq!(source.document.insert(next.clone()), 18);
        assert_eq!(restored.document.insert(next), 18);
        assert!(source.document.set_group(&[text_id, mark_id, 18], true));
        assert!(restored.document.set_group(&[text_id, mark_id, 18], true));
        assert!(restored
            .document
            .items()
            .iter()
            .all(|object| object.group == Some(12)));
        assert_eq!(source.document.items(), restored.document.items());
        let clock = restored.document.bounded_history_snapshot().unwrap();
        assert_eq!((clock.next_id, clock.next_group), (18, 12));
        assert_eq!(mixed_saved(&source, false), mixed_saved(&restored, false));
    }

    // 单个外来 typed 等值 Replace 也继续拒绝，不使用 live 入口给它降级。
    let mut typed = mixed_editor();
    let id = typed
        .document
        .insert(mixed_text("外来 typed noop", true, &font));
    let annotation = typed.document.get(id).unwrap().clone();
    let mut snapshot = typed.document.bounded_history_snapshot().unwrap();
    snapshot.undo.push(HistoryCommand::Replace {
        index: 0,
        before: annotation.clone(),
        after: annotation,
    });
    assert!(
        crate::pin_raster_document::WireRasterDocument::from_vector_text_snapshot(
            snapshot, MIXED_SIZE, true,
        )
        .is_err()
    );
}
