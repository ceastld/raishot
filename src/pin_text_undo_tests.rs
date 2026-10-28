//! 纯文字贴图恢复模型；使用真实 Editor 命令，不生成底图或另写历史解释器。
use super::*;

const SIZE: (u32, u32) = (180, 120);

fn empty_editor() -> Editor {
    let full = Rect::full(SIZE.0, SIZE.1);
    Editor::new(full, Some(full))
}

fn named_font() -> String {
    crate::text_format::fonts()
        .iter()
        .find(|name| {
            !name.eq_ignore_ascii_case("Microsoft YaHei UI")
                && !name.is_empty()
                && name.len() <= 256
                && !name.chars().any(char::is_control)
        })
        .expect("模型需要一个已安装的命名字体")
        .clone()
}

fn text(body: &str, light: bool, color: Option<u32>, font: &str) -> Annotation {
    let mut annotation = Annotation::new(
        Tool::Text,
        Point { x: 2., y: 3. },
        Style {
            color: color.unwrap_or(crate::pin_text_source::colors(light).1),
            block_size: 37,
            filled: true,
            radius: 11.,
            arrow_head: crate::arrow_geometry::ArrowHead::Dart,
            arrow_start: true,
            arrow_end: false,
            dashed: true,
            pencil_smooth: false,
            polyline_smooth: false,
            highlighter_blend: crate::annotation::HighlighterBlend::Multiply,
            highlighter_shape: crate::annotation::HighlighterShape::Line,
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
    annotation.update(Point { x: 170., y: 110. });
    annotation.text = body.into();
    annotation.rotation = 15.;
    annotation
}

fn source_editor(light: bool, font: &str) -> (Editor, Vec<u64>) {
    let mut editor = empty_editor();
    let ids = editor.document.insert_many(vec![
        text("默认色  a\t🙂\r\n第二行", light, None, font),
        text("自选颜色", light, Some(0x7a3159), font),
    ]);
    assert!(editor.document.set_group(&ids, true));
    let mut changed = editor.document.get(ids[0]).unwrap().clone();
    changed.text = "最新版本 🙂\n中文".into();
    changed.style.bold = true;
    editor.document.replace(ids[0], changed);
    editor.document.remove(ids[1]);
    editor.document.undo();
    assert!(editor.document.can_undo() && editor.document.can_redo());
    (editor, ids)
}

fn snapshot(editor: &Editor, light: bool) -> WireTextDocument {
    WireTextDocument::try_from_text_editor(editor, SIZE, light).unwrap()
}

fn roundtrip(document: &WireTextDocument) -> WireTextDocument {
    serde_json::from_slice(&serde_json::to_vec(document).unwrap()).unwrap()
}

fn assert_themed_current(source: &Editor, restored: &Editor, from_light: bool, to_light: bool) {
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
    assert_eq!(restored.crop, source.crop);
}

#[test]
fn text_history_preserves_both_stacks_ids_groups_named_fonts_and_theme_in_both_directions() {
    let font = named_font();
    for (from_light, to_light) in [(false, true), (true, false)] {
        let (mut source, ids) = source_editor(from_light, &font);
        let document = snapshot(&source, from_light);
        let encoded = serde_json::to_value(&document).unwrap();
        assert_eq!(encoded["objects"][0]["style"]["fontName"], font);
        assert!(!serde_json::to_string(&document)
            .unwrap()
            .contains("\"fontIndex\""));
        let mut restored = empty_editor();
        roundtrip(&document)
            .restore_into(&mut restored, to_light)
            .unwrap();
        assert!(restored.document.can_undo() && restored.document.can_redo());
        assert_eq!(
            restored
                .document
                .items()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert!(restored.document.items().iter().all(|o| o.group.is_some()));
        assert_themed_current(&source, &restored, from_light, to_light);
        assert_eq!(
            snapshot(&restored, to_light),
            document,
            "主题基准保存须包含相同双栈和分配器"
        );

        // 历史应用由两份真实 Document 完成；只对比当前对象的主题映射。
        for undo in [true, true, false, false, false, true] {
            if undo {
                source.document.undo();
                restored.document.undo();
            } else {
                source.document.redo();
                restored.document.redo();
            }
            assert_themed_current(&source, &restored, from_light, to_light);
            assert_eq!(source.document.can_undo(), restored.document.can_undo());
            assert_eq!(source.document.can_redo(), restored.document.can_redo());
            assert!(restored
                .document
                .items()
                .iter()
                .all(|o| o.annotation.style.text.font.name() == font));
        }
    }

    // 空的当前内容仍可能有可执行的两栈，不能因 objects.is_empty 丢弃文字来源。
    let mut empty = empty_editor();
    let first = empty
        .document
        .insert(text("已删除文字", false, None, &font));
    empty.document.remove(first);
    let second = empty
        .document
        .insert(text("等待重做文字", false, None, &font));
    empty.document.undo();
    assert!(empty.document.items().is_empty());
    assert!(empty.document.can_undo() && empty.document.can_redo());
    let mut restored = empty_editor();
    roundtrip(&snapshot(&empty, false))
        .restore_into(&mut restored, true)
        .unwrap();
    assert!(restored.document.items().is_empty());
    assert!(restored.document.can_undo() && restored.document.can_redo());
    restored.document.redo();
    assert_eq!(restored.document.items()[0].id, second);
    assert_eq!(
        restored.document.items()[0].annotation.style.color,
        crate::pin_text_source::colors(true).1
    );
    restored.document.undo();
    restored.document.undo();
    assert_eq!(restored.document.items()[0].id, first);
}

#[test]
fn legacy_v1_text_without_history_restores_current_content_with_empty_stacks() {
    let font = named_font();
    let mut source = empty_editor();
    let mut annotation = text("旧 V1 文字🙂", true, None, &font);
    // 旧 V1 只保存文字样式，额外绘图参数按当时默认值恢复。
    annotation.style = Style {
        color: annotation.style.color,
        width: annotation.style.width,
        opacity: annotation.style.opacity,
        font_size: annotation.style.font_size,
        bold: annotation.style.bold,
        text: annotation.style.text,
        ..Default::default()
    };
    source.document.insert(annotation);
    let mut encoded = serde_json::to_value(snapshot(&source, true)).unwrap();
    assert_eq!(encoded["version"], 1);
    assert!(encoded.as_object_mut().unwrap().remove("history").is_some());
    for object in encoded["objects"].as_array_mut().unwrap() {
        object.as_object_mut().unwrap().remove("group");
        object["style"].as_object_mut().unwrap().remove("extras");
    }
    let old: WireTextDocument = serde_json::from_value(encoded).unwrap();
    let mut restored = empty_editor();
    old.restore_into(&mut restored, false).unwrap();
    assert_themed_current(&source, &restored, true, false);
    assert!(!restored.document.can_undo() && !restored.document.can_redo());
    let before = restored.document.items().to_vec();
    restored.document.undo();
    restored.document.redo();
    assert_eq!(restored.document.items(), before);
}

#[test]
fn text_cancel_and_noop_preserve_current_and_both_saved_stacks() {
    let font = named_font();
    let (mut editor, ids) = source_editor(true, &font);
    let before = snapshot(&editor, true);
    let revision = editor.document.revision();
    let edit_revision = editor.document.edit_revision();
    editor.begin_text(Some(ids[0]), Point { x: 2., y: 3. });
    editor.text_draft.as_mut().unwrap().annotation.text = "取消的文字草稿".into();
    assert!(editor.cancel_text());
    let unchanged = editor.document.get(ids[0]).unwrap().clone();
    editor.document.replace(ids[0], unchanged);
    editor.document.remove(u64::MAX);
    editor.document.remove_many(&[]);
    editor.document.insert_many(Vec::new());
    assert!(!editor.document.set_group(&ids, true));
    editor.begin_text(Some(ids[0]), Point { x: 2., y: 3. });
    editor.finish_text();
    assert_eq!(snapshot(&editor, true), before);
    assert_eq!(editor.document.revision(), revision);
    assert_eq!(editor.document.edit_revision(), edit_revision);
}

#[test]
fn bounded_text_history_keeps_latest_real_chain_and_all_current_content() {
    let font = named_font();
    let mut source = empty_editor();
    let id = source
        .document
        .insert(text("revision-start", true, None, &font));
    for index in 0..160 {
        let mut next = source.document.get(id).unwrap().clone();
        next.text = format!("revision-{index:03} 中文🙂");
        source.document.replace(id, next);
    }
    let document = snapshot(&source, true);
    let encoded = serde_json::to_value(&document).unwrap();
    let steps = encoded["history"]["undo"].as_array().unwrap().len();
    assert!(steps > 0 && steps <= 128, "裁掉最远端历史，不丢最新链");
    let mut restored = empty_editor();
    roundtrip(&document)
        .restore_into(&mut restored, true)
        .unwrap();
    assert_eq!(restored.document.items(), source.document.items());
    assert_eq!(restored.document.items()[0].id, id);
    for index in (160 - steps..160).rev() {
        restored.document.undo();
        source.document.undo();
        assert_eq!(restored.document.items(), source.document.items());
        assert_eq!(
            restored.document.get(id).unwrap().text,
            format!("revision-{:03} 中文🙂", index - 1)
        );
    }
    assert!(!restored.document.can_undo());
    let oldest = restored.document.items().to_vec();
    restored.document.undo();
    assert_eq!(restored.document.items(), oldest);
    for _ in 0..steps {
        restored.document.redo();
    }
    assert_eq!(
        restored.document.get(id).unwrap().text,
        "revision-159 中文🙂"
    );
    assert_eq!(snapshot(&restored, true), document);

    // 字节预算也必须只裁最远历史；重复换行真实增加 JSON 字节而非伪造命令。
    let mut large = empty_editor();
    let id = large
        .document
        .insert(text("large-start", true, None, &font));
    for index in 0..8 {
        let mut next = large.document.get(id).unwrap().clone();
        next.text = format!("revision-{index}\n{}", "A\n".repeat(16 * 1024));
        large.document.replace(id, next);
    }
    let document = snapshot(&large, true);
    assert!(serde_json::to_vec(&document).unwrap().len() <= 240 * 1024);
    let encoded = serde_json::to_value(&document).unwrap();
    let steps = encoded["history"]["undo"].as_array().unwrap().len();
    assert!(steps > 0 && steps < 8, "字节容量保留最近可执行历史");
    let mut restored = empty_editor();
    roundtrip(&document)
        .restore_into(&mut restored, true)
        .unwrap();
    assert_eq!(
        restored.document.items(),
        large.document.items(),
        "当前大文字完整保留"
    );
    for _ in 0..steps {
        large.document.undo();
        restored.document.undo();
        assert_eq!(restored.document.items(), large.document.items());
    }
    assert!(!restored.document.can_undo());

    // 合法当前正文被 JSON 转义放大时，只能裁历史，不能连当前对象也丢掉。
    let mut escaped = empty_editor();
    let escaped_id =
        escaped
            .document
            .insert(text(&"\u{0001}".repeat(64 * 1024), true, None, &font));
    let document = snapshot(&escaped, true);
    let bytes = serde_json::to_vec(&document).unwrap().len();
    assert!(bytes > 240 * 1024 && bytes <= 480 * 1024);
    let encoded = serde_json::to_value(&document).unwrap();
    assert!(encoded["history"]["undo"].as_array().unwrap().is_empty());
    assert!(encoded["history"]["redo"].as_array().unwrap().is_empty());
    let mut restored = empty_editor();
    roundtrip(&document)
        .restore_into(&mut restored, true)
        .unwrap();
    assert_eq!(restored.document.items(), escaped.document.items());
    assert!(!restored.document.can_undo() && !restored.document.can_redo());
    let next_id = restored
        .document
        .insert(text("new object", true, None, &font));
    assert!(next_id > escaped_id, "裁空历史仍保留对象分配时钟");
}

#[test]
fn bad_text_history_command_ids_style_and_nontext_history_are_rejected_before_install() {
    fn rejected(encoded: serde_json::Value, label: &str) {
        let mut target = empty_editor();
        target.style.color = 0x112233;
        let revision = target.document.revision();
        let edit_revision = target.document.edit_revision();
        match serde_json::from_value::<WireTextDocument>(encoded) {
            Ok(document) => {
                assert!(document.validate().is_err(), "{label} 必须先拒绝磁盘语义");
                assert!(
                    document.restore_into(&mut target, false).is_err(),
                    "{label} 不得安装"
                );
            }
            Err(_) => {
                // 不支持的命令种类在读取磁盘元数据时就被拒绝。
            }
        }
        assert!(
            target.document.items().is_empty(),
            "{label} 不得留下部分对象"
        );
        assert!(target.document.crop_mask.is_none());
        assert!(!target.document.can_undo() && !target.document.can_redo());
        assert_eq!(target.document.revision(), revision);
        assert_eq!(target.document.edit_revision(), edit_revision);
        assert_eq!(target.crop, Some(Rect::full(SIZE.0, SIZE.1)));
        assert_eq!(target.style.color, 0x112233);
    }
    let font = named_font();
    let (mut source, _) = source_editor(true, &font);
    let valid = serde_json::to_value(snapshot(&source, true)).unwrap();
    assert_eq!(valid["history"]["undo"][0]["kind"], "batch");
    let mut bad_index = valid.clone();
    bad_index["history"]["undo"][0]["commands"][0]["index"] = 999999.into();
    rejected(bad_index, "不可达的插入序位");
    let mut duplicate_id = valid.clone();
    duplicate_id["history"]["ids"][1] = duplicate_id["history"]["ids"][0].clone();
    rejected(duplicate_id, "重复的当前对象 ID");
    let mut bad_style = valid.clone();
    bad_style["history"]["undo"][0]["commands"][0]["object"]["annotation"]["style"]["opacity"] =
        serde_json::json!(-1.);
    rejected(bad_style, "历史对象的非法透明度");
    let mut bad_extras = valid.clone();
    bad_extras["history"]["undo"][0]["commands"][0]["object"]["annotation"]["style"]["extras"]
        ["radius"] = serde_json::json!(-1.);
    rejected(bad_extras, "历史对象的非法完整样式字段");
    let mut nontext_command = valid;
    nontext_command["history"]["undo"][0] =
        serde_json::json!({"kind": "cropMask", "before": null, "after": null});
    rejected(nontext_command, "纯文字历史不支持的非文字命令");

    // 非文字 Annotation 没有可伪装的磁盘 tool 字段，必须在真实导出入口拒绝。
    let mut rectangle = Annotation::new(Tool::Rectangle, Point { x: 3., y: 4. }, Style::default());
    rectangle.update(Point { x: 30., y: 40. });
    let nontext = source.document.insert(rectangle);
    source.document.remove(nontext);
    assert!(source
        .document
        .items()
        .iter()
        .all(|o| o.annotation.tool == Tool::Text));
    assert!(WireTextDocument::try_from_text_editor(&source, SIZE, true).is_err());
    assert!(WireTextDocument::from_text_editor(&source, SIZE, true).is_none());
}

#[test]
fn theme_remap_collapses_only_equal_color_replace_and_preserves_insert_history_and_id() {
    let mut source = empty_editor();
    let id = source
        .document
        .insert(text("颜色替换折叠", true, None, "Microsoft YaHei UI"));
    let mut custom = source.document.get(id).unwrap().clone();
    custom.style.color = crate::pin_text_source::colors(false).1;
    source.document.replace(id, custom);
    let before = serde_json::to_value(snapshot(&source, true)).unwrap();
    assert_eq!(before["history"]["undo"].as_array().unwrap().len(), 2);
    assert_eq!(before["history"]["undo"][1]["kind"], "replace");

    // 当前自选色已经等于深色默认色；静默换主题仍会改写旧 Insert 和 Replace.before。
    source
        .document
        .remap_silently(|annotation| crate::pin_text_source::remap_theme(annotation, true, false));
    let revision = source.document.revision();
    let edit_revision = source.document.edit_revision();
    let document = WireTextDocument::try_from_text_editor(&source, SIZE, false).unwrap();
    let encoded = serde_json::to_value(&document).unwrap();
    assert_eq!(
        encoded["history"]["undo"].as_array().unwrap().len(),
        1,
        "只去掉映射后的等值 Replace"
    );
    assert_eq!(encoded["history"]["undo"][0]["kind"], "insert");
    assert!(encoded["history"]["redo"].as_array().unwrap().is_empty());
    assert_eq!(source.document.revision(), revision, "导出不提交新命令");
    assert_eq!(source.document.edit_revision(), edit_revision);

    let mut restored = empty_editor();
    roundtrip(&document)
        .restore_into(&mut restored, true)
        .unwrap();
    assert_eq!(restored.document.items()[0].id, id);
    assert_eq!(
        restored.document.get(id).unwrap().style.color,
        crate::pin_text_source::colors(true).1
    );
    restored.document.undo();
    assert!(restored.document.items().is_empty());
    assert!(!restored.document.can_undo() && restored.document.can_redo());
    restored.document.redo();
    assert_eq!(restored.document.items()[0].id, id);
    assert_eq!(
        restored.document.get(id).unwrap().style.color,
        crate::pin_text_source::colors(true).1
    );
    assert!(!restored.document.can_redo());
    assert_eq!(snapshot(&restored, true), document);
}

#[test]
fn missing_font_mapping_collapses_valid_font_only_replace_but_rejects_original_wire_noop() {
    const MISSING_A: &str = "A RustShot missing font A";
    const MISSING_B: &str = "A RustShot missing font B";
    assert_eq!(FontFamily::named(MISSING_A), FontFamily::default());
    assert_eq!(FontFamily::named(MISSING_B), FontFamily::default());
    let mut source = empty_editor();
    let id = source
        .document
        .insert(text("缺字体恢复折叠", true, None, "Microsoft YaHei UI"));
    let mut changed = source.document.get(id).unwrap().clone();
    assert!(!changed.style.bold);
    changed.style.bold = true;
    source.document.replace(id, changed);
    let mut encoded = serde_json::to_value(snapshot(&source, true)).unwrap();
    assert_eq!(encoded["history"]["undo"][0]["kind"], "insert");
    assert_eq!(encoded["history"]["undo"][1]["kind"], "replace");
    encoded["objects"][0]["style"]["fontName"] = MISSING_B.into();
    encoded["objects"][0]["style"]["bold"] = false.into();
    encoded["history"]["undo"][0]["object"]["annotation"]["style"]["fontName"] = MISSING_A.into();
    encoded["history"]["undo"][1]["before"]["style"]["fontName"] = MISSING_A.into();
    encoded["history"]["undo"][1]["before"]["style"]["bold"] = false.into();
    encoded["history"]["undo"][1]["after"]["style"]["fontName"] = MISSING_B.into();
    encoded["history"]["undo"][1]["after"]["style"]["bold"] = false.into();
    assert_ne!(
        encoded["history"]["undo"][1]["before"], encoded["history"]["undo"][1]["after"],
        "原始字体名称差异是合法替换"
    );
    let document: WireTextDocument = serde_json::from_value(encoded.clone()).unwrap();
    document.validate().unwrap();
    let mut restored = empty_editor();
    document.restore_into(&mut restored, true).unwrap();
    assert_eq!(restored.document.items()[0].id, id);
    assert_eq!(
        restored.document.get(id).unwrap().style.text.font,
        FontFamily::default()
    );
    assert!(!restored.document.get(id).unwrap().style.bold);
    let mapped = serde_json::to_value(snapshot(&restored, true)).unwrap();
    assert_eq!(
        mapped["history"]["undo"].as_array().unwrap().len(),
        1,
        "字体回退后只保留 Insert"
    );
    assert_eq!(mapped["history"]["undo"][0]["kind"], "insert");
    restored.document.undo();
    assert!(restored.document.items().is_empty());
    assert!(!restored.document.can_undo() && restored.document.can_redo());
    restored.document.redo();
    assert_eq!(restored.document.items()[0].id, id);
    assert_eq!(
        restored.document.get(id).unwrap().style.text.font,
        FontFamily::default()
    );
    assert!(!restored.document.can_redo());

    // 同名 before/after 原本就是非法 noop，必须在运行时字体映射前拒绝。
    encoded["objects"][0]["style"]["fontName"] = MISSING_A.into();
    encoded["history"]["undo"][1]["after"]["style"]["fontName"] = MISSING_A.into();
    assert_eq!(
        encoded["history"]["undo"][1]["before"],
        encoded["history"]["undo"][1]["after"]
    );
    let malformed: WireTextDocument = serde_json::from_value(encoded).unwrap();
    assert!(malformed.validate().is_err());
    let mut target = empty_editor();
    let revision = target.document.revision();
    assert!(malformed.restore_into(&mut target, false).is_err());
    assert!(target.document.items().is_empty());
    assert!(!target.document.can_undo() && !target.document.can_redo());
    assert_eq!(target.document.revision(), revision);
    assert_eq!(target.crop, Some(Rect::full(SIZE.0, SIZE.1)));
}

#[test]
fn text_raw_mixed_batch_noops_reject_before_mapping_and_keep_target_atomic() {
    fn target() -> Editor {
        let mut target = empty_editor();
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
        target.document.invalidate_crop_mask(); // 非零修订仍满足空文档恢复门。
        target.crop = None;
        target.style.color = 0x112233;
        target
    }

    let mut source = empty_editor();
    source
        .document
        .insert(text("原始批次校验", true, None, "Microsoft YaHei UI"));
    let valid = serde_json::to_value(snapshot(&source, true)).unwrap();
    let legal = valid["history"]["undo"][0].clone();
    assert_eq!(legal["kind"], "insert");
    let id = valid["history"]["ids"][0].clone();
    let annotation = valid["objects"][0].clone();
    // 使用同一目标可成功恢复合法来源，坏节点的失败不能冒充目标前置保护。
    let mut gate_probe = target();
    roundtrip(&snapshot(&source, true))
        .restore_into(&mut gate_probe, false)
        .unwrap();
    assert_eq!(gate_probe.document.items().len(), 1);

    let cases = [
        (
            "等值替换",
            serde_json::json!({"kind": "replace", "index": 0, "before": annotation, "after": annotation}),
            false,
        ),
        (
            "等值分组",
            serde_json::json!({"kind": "groups", "before": [[id, null]], "after": [[id, null]]}),
            false,
        ),
        (
            "等值层序",
            serde_json::json!({"kind": "order", "before": [id], "after": [id]}),
            false,
        ),
        (
            "空清空",
            serde_json::json!({"kind": "clear", "objects": []}),
            true, // 必须在 Insert 前，才能构成旧核心接受的连续命令链。
        ),
        (
            "空批次",
            serde_json::json!({"kind": "batch", "commands": []}),
            false, // 旧核心已拒绝空 Batch；本入口也必须递归拒绝。
        ),
    ];
    for (label, noop, clear_first) in cases {
        for depth in 0..=2 {
            let mut node = noop.clone();
            for _ in 0..depth {
                node = serde_json::json!({"kind": "batch", "commands": [node]});
            }
            let commands = if clear_first {
                vec![node, legal.clone()]
            } else {
                vec![legal.clone(), node]
            };
            let mut encoded = valid.clone();
            encoded["history"]["undo"] =
                serde_json::json!([{"kind": "batch", "commands": commands}]);
            let malformed: WireTextDocument = serde_json::from_value(encoded).unwrap();
            assert!(
                malformed.validate().is_err(),
                "{label} 深度{depth}必须在显示映射前拒绝原始节点"
            );
            let mut target = target();
            let before = target.document.bounded_history_snapshot().unwrap();
            let crop = target.crop;
            let style = target.style;
            let revision = target.document.revision();
            let edit_revision = target.document.edit_revision();
            assert!(malformed.restore_into(&mut target, false).is_err());
            assert_eq!(target.document.bounded_history_snapshot().unwrap(), before);
            assert_eq!(target.crop, crop);
            assert_eq!(target.style, style);
            assert_eq!(target.document.revision(), revision);
            assert_eq!(target.document.edit_revision(), edit_revision);
        }
    }

    // 原始不同名称是有效替换；本机缺字体造成映射后等值，仍可折叠。
    const FONT_A: &str = "A RustShot missing font A";
    const FONT_B: &str = "A RustShot missing font B";
    assert_eq!(FontFamily::named(FONT_A), FontFamily::default());
    assert_eq!(FontFamily::named(FONT_B), FontFamily::default());
    let mut mapped = valid;
    mapped["objects"][0]["style"]["fontName"] = FONT_B.into();
    let mut insert = legal;
    insert["object"]["annotation"]["style"]["fontName"] = FONT_A.into();
    let mut before = mapped["objects"][0].clone();
    before["style"]["fontName"] = FONT_A.into();
    let after = mapped["objects"][0].clone();
    mapped["history"]["undo"] = serde_json::json!([{
        "kind": "batch",
        "commands": [insert, {"kind": "replace", "index": 0, "before": before, "after": after}]
    }]);
    let mapped: WireTextDocument = serde_json::from_value(mapped).unwrap();
    mapped.validate().unwrap();
    let mut restored = target();
    mapped.restore_into(&mut restored, false).unwrap();
    assert_eq!(restored.document.items().len(), 1);
    assert_eq!(restored.document.items()[0].id, id.as_u64().unwrap());
    assert_eq!(
        restored.document.items()[0].annotation.style.text.font,
        FontFamily::default()
    );
    let folded = serde_json::to_value(snapshot(&restored, false)).unwrap();
    assert_eq!(folded["history"]["undo"].as_array().unwrap().len(), 1);
    assert_eq!(folded["history"]["undo"][0]["kind"], "batch");
    assert_eq!(
        folded["history"]["undo"][0]["commands"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "映射后的等值 Replace 必须实际折叠，不能仅凭一次 Undo 推断"
    );
    assert_eq!(
        folded["history"]["undo"][0]["commands"][0]["kind"],
        "insert"
    );
    restored.document.undo();
    assert!(restored.document.items().is_empty());
    assert!(!restored.document.can_undo() && restored.document.can_redo());
    restored.document.redo();
    assert_eq!(restored.document.items().len(), 1);
    assert!(!restored.document.can_redo());
}
