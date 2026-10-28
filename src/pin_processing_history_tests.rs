//! 处理参数历史的组合模型；使用真实 change / undo / redo，不生成图片像素。
use super::*;
use serde_json::{json, Value};

const RAW: (u32, u32) = (360, 240);

fn processing_fixture() -> (ProcessingHistory, Adjustments) {
    let mut history = ProcessingHistory::default();
    let mut current = Adjustments::default();
    let mut next = current.rotated(1);
    assert!(history.change(&mut current, next));
    next.flip_x = true;
    assert!(history.change(&mut current, next));
    next.flip_y = true;
    assert!(history.change(&mut current, next));
    next.grayscale = true;
    assert!(history.change(&mut current, next));
    next.invert = true;
    assert!(history.change(&mut current, next));
    next.brightness = -37;
    assert!(history.change(&mut current, next));
    next = current.rotated(2);
    assert!(history.change(&mut current, next));
    next.brightness = 64;
    assert!(history.change(&mut current, next));
    assert!(history.undo(&mut current));
    assert!(history.undo(&mut current));
    assert!(!history.undo.is_empty() && !history.redo.is_empty());
    assert_eq!(
        current,
        Adjustments {
            turns: 1,
            flip_x: true,
            flip_y: true,
            grayscale: true,
            invert: true,
            brightness: -37,
        }
    );
    (history, current)
}

fn processing_restore_target() -> (ProcessingHistory, Adjustments) {
    let mut history = ProcessingHistory::default();
    let mut current = Adjustments::default();
    let mut next = current.rotated(2);
    assert!(history.change(&mut current, next));
    next.brightness = 22;
    assert!(history.change(&mut current, next));
    next.invert = true;
    assert!(history.change(&mut current, next));
    assert!(history.undo(&mut current));
    (history, current)
}

fn processing_wire(history: &ProcessingHistory, current: Adjustments) -> WireProcessingHistory {
    WireProcessingHistory::from_history(history, current, RAW, current.dimensions(RAW.0, RAW.1))
        .unwrap()
}

fn encoded_history(history: &ProcessingHistory, current: Adjustments) -> Value {
    serde_json::to_value(processing_wire(history, current)).unwrap()
}

fn copied_snapshot(snapshot: &ProcessingHistorySnapshot) -> ProcessingHistorySnapshot {
    ProcessingHistorySnapshot {
        current: snapshot.current,
        undo: snapshot.undo.clone(),
        redo: snapshot.redo.clone(),
    }
}

fn assert_same_processing_history(
    source: &ProcessingHistory,
    source_current: Adjustments,
    restored: &ProcessingHistory,
    restored_current: Adjustments,
) {
    assert_eq!(source_current, restored_current);
    assert_eq!(source.undo, restored.undo);
    assert_eq!(source.redo, restored.redo);
    assert_eq!(
        source_current.dimensions(RAW.0, RAW.1),
        restored_current.dimensions(RAW.0, RAW.1),
    );
    assert_eq!(
        encoded_history(source, source_current),
        encoded_history(restored, restored_current),
    );
}

#[test]
fn processing_wire_roundtrip_preserves_six_parameters_both_stacks_and_latest_64_step_chain() {
    let (mut source, mut current) = processing_fixture();
    let encoded = encoded_history(&source, current);
    assert_eq!(encoded["version"], 1);
    assert_eq!(encoded["originalWidth"], RAW.0);
    assert_eq!(encoded["originalHeight"], RAW.1);
    assert_eq!(
        (encoded["width"].as_u64(), encoded["height"].as_u64()),
        (Some(240), Some(360))
    );
    let fields: std::collections::BTreeSet<_> = encoded["current"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        [
            "turns",
            "flipX",
            "flipY",
            "grayscale",
            "invert",
            "brightness"
        ]
        .into_iter()
        .collect()
    );
    let wire: WireProcessingHistory =
        serde_json::from_slice(&serde_json::to_vec(&processing_wire(&source, current)).unwrap())
            .unwrap();
    let (mut restored, _) = processing_restore_target();
    let mut restored_current = current;
    wire.restore_into(current, RAW, (240, 360), &mut restored)
        .unwrap();
    assert_same_processing_history(&source, current, &restored, restored_current);

    // 两次 Redo 恢复270°与最终亮度，再退回原始参数，最后沿原链全部重做。
    for undo in [
        false, false, true, true, true, true, true, true, true, true, false, false, false, false,
        false, false, false, false,
    ] {
        let source_changed = if undo {
            source.undo(&mut current)
        } else {
            source.redo(&mut current)
        };
        let restored_changed = if undo {
            restored.undo(&mut restored_current)
        } else {
            restored.redo(&mut restored_current)
        };
        assert!(source_changed && restored_changed);
        assert_same_processing_history(&source, current, &restored, restored_current);
        assert_eq!(
            current.dimensions(RAW.0, RAW.1),
            match current.turns {
                1 | 3 => (240, 360),
                0 | 2 => RAW,
                _ => panic!("真实有效链不应产生非法turns"),
            }
        );
    }

    // 不等翻转轴连续四次90°旋转：每步保存恢复，再用真实Undo回到上一参数。
    let mut source = ProcessingHistory::default();
    let mut current = Adjustments {
        flip_x: true,
        flip_y: false,
        ..Default::default()
    };
    for expected in [
        (1, false, true),
        (2, true, false),
        (3, false, true),
        (0, true, false),
    ] {
        let previous = current;
        let next = current.rotated(1);
        assert_eq!((next.turns, next.flip_x, next.flip_y), expected);
        assert!(source.change(&mut current, next));
        let wire: WireProcessingHistory =
            serde_json::from_value(encoded_history(&source, current)).unwrap();
        let (mut restored, _) = processing_restore_target();
        let mut restored_current = current;
        wire.restore_into(
            current,
            RAW,
            current.dimensions(RAW.0, RAW.1),
            &mut restored,
        )
        .unwrap();
        assert_same_processing_history(&source, current, &restored, restored_current);
        let mut undo_source = source.clone();
        let mut undo_current = current;
        assert!(undo_source.undo(&mut undo_current));
        assert!(restored.undo(&mut restored_current));
        assert_eq!(restored_current, previous);
        assert_same_processing_history(&undo_source, undo_current, &restored, restored_current);
    }

    // 达到上限的来源由真实change构造，visited只记录每次实际当前参数。
    let mut source = ProcessingHistory::default();
    let mut current = Adjustments::default();
    let mut visited = vec![current];
    for brightness in 1..=96 {
        let next = Adjustments {
            brightness,
            ..current
        };
        assert!(source.change(&mut current, next));
        visited.push(current);
    }
    for _ in 0..17 {
        assert!(source.undo(&mut current));
    }
    let snapshot = source.bounded_snapshot(current).unwrap();
    assert_eq!(
        snapshot.undo.len() + snapshot.redo.len(),
        MAX_PROCESSING_HISTORY_STEPS
    );
    let wire: WireProcessingHistory =
        serde_json::from_value(encoded_history(&source, current)).unwrap();
    let mut restored = ProcessingHistory::default();
    let mut restored_current = current;
    wire.restore_into(current, RAW, RAW, &mut restored).unwrap();
    while source.undo(&mut current) {
        assert!(restored.undo(&mut restored_current));
        assert_same_processing_history(&source, current, &restored, restored_current);
    }
    assert!(!restored.undo(&mut restored_current));
    assert_eq!(
        current,
        visited[visited.len() - 1 - MAX_PROCESSING_HISTORY_STEPS]
    );
    let mut redone = 0;
    while source.redo(&mut current) {
        assert!(restored.redo(&mut restored_current));
        redone += 1;
        assert_same_processing_history(&source, current, &restored, restored_current);
    }
    assert_eq!(redone, MAX_PROCESSING_HISTORY_STEPS);
    assert!(!restored.redo(&mut restored_current));
    assert_eq!(current, *visited.last().unwrap());
}

#[test]
fn processing_noop_and_host_clone_rollback_preserve_saved_stacks_and_pending_redo() {
    let (mut history, mut current) = processing_fixture();
    let before = encoded_history(&history, current);
    let same = current;
    assert!(!history.change(&mut current, same));
    assert_eq!(encoded_history(&history, current), before);

    // 宿主ProcessingRequest保存old_history和previous；这里只代理实际参数回退合同。
    let old_history = history.clone();
    let previous = current;
    let next = current.rotated(1);
    assert!(history.change(&mut current, next));
    assert!(history.redo.is_empty());
    history = old_history;
    current = previous;
    assert_eq!(encoded_history(&history, current), before);
    let wire: WireProcessingHistory = serde_json::from_value(before).unwrap();
    let mut restored = ProcessingHistory::default();
    let mut restored_current = current;
    wire.restore_into(
        current,
        RAW,
        current.dimensions(RAW.0, RAW.1),
        &mut restored,
    )
    .unwrap();
    assert!(history.redo(&mut current));
    assert!(restored.redo(&mut restored_current));
    assert_same_processing_history(&history, current, &restored, restored_current);
}

#[test]
fn processing_typed_invalid_values_noops_identity_and_over_capacity_reject_atomically() {
    let (source, current) = processing_fixture();
    let valid = source.bounded_snapshot(current).unwrap();
    let mut cases = Vec::new();
    let mut invalid = copied_snapshot(&valid);
    invalid.current.turns = 4;
    cases.push(("非法current turns", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.current.brightness = 101;
    cases.push(("非法current亮度", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.undo[0].turns = 7;
    cases.push(("非法Undo turns", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.redo[0].brightness = -101;
    cases.push(("非法Redo亮度", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.undo = vec![Adjustments::default(); 2];
    cases.push(("Undo相邻相等", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.redo = vec![
        Adjustments {
            turns: 2,
            ..Default::default()
        };
        2
    ];
    cases.push(("Redo相邻相等", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.undo.push(current);
    cases.push(("Undo尾等于current", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.redo.push(current);
    cases.push(("Redo尾等于current", invalid));
    let mut invalid = copied_snapshot(&valid);
    invalid.undo = (0..=MAX_PROCESSING_HISTORY_STEPS)
        .map(|index| Adjustments {
            brightness: (index % 2) as i16,
            ..Default::default()
        })
        .collect();
    invalid.redo.clear();
    cases.push(("总容量65步不能静默裁剪", invalid));

    for (label, invalid) in cases {
        let live_invalid = ProcessingHistory {
            undo: invalid.undo.clone(),
            redo: invalid.redo.clone(),
        };
        assert!(
            live_invalid.bounded_snapshot(invalid.current).is_err(),
            "{label}导出也须拒绝"
        );
        assert!(
            ProcessingHistory::from_bounded_snapshot(copied_snapshot(&invalid), current).is_err(),
            "{label}"
        );
        let (mut target, target_current) = processing_restore_target();
        let before = encoded_history(&target, target_current);
        assert!(
            target.restore_bounded_snapshot(current, invalid).is_err(),
            "{label}"
        );
        assert_eq!(
            encoded_history(&target, target_current),
            before,
            "{label}不应部分安装"
        );
    }
    let wrong_current = Adjustments {
        brightness: current.brightness + 1,
        ..current
    };
    assert!(
        ProcessingHistory::from_bounded_snapshot(copied_snapshot(&valid), wrong_current).is_err()
    );
    let (mut target, _) = processing_restore_target();
    let old_undo = target.undo.clone();
    let old_redo = target.redo.clone();
    assert!(target
        .restore_bounded_snapshot(wrong_current, valid)
        .is_err());
    assert_eq!(target.undo, old_undo);
    assert_eq!(target.redo, old_redo);
}

fn rejected_wire_preserves_target(
    encoded: Value,
    expected_current: Adjustments,
    raw: (u32, u32),
    processed: (u32, u32),
    label: &str,
) {
    let (mut target, target_current) = processing_restore_target();
    let before = encoded_history(&target, target_current);
    if let Ok(wire) = serde_json::from_value::<WireProcessingHistory>(encoded) {
        assert!(
            wire.validate(expected_current, raw, processed).is_err(),
            "{label}校验须拒绝"
        );
        assert!(
            wire.restore_into(expected_current, raw, processed, &mut target)
                .is_err(),
            "{label}恢复须拒绝"
        );
    }
    assert_eq!(
        encoded_history(&target, target_current),
        before,
        "{label}不能改变目标"
    );
}

#[test]
fn processing_wire_bad_versions_fields_null_values_dimensions_and_capacity_reject_atomically() {
    let (source, current) = processing_fixture();
    let valid = encoded_history(&source, current);
    let processed = current.dimensions(RAW.0, RAW.1);
    let mut cases = Vec::new();
    let mut invalid = valid.clone();
    invalid["version"] = json!(2);
    cases.push(("外层版本", invalid));
    let mut invalid = valid.clone();
    invalid["history"]["version"] = json!(2);
    cases.push(("历史版本", invalid));
    let mut invalid = valid.clone();
    invalid["history"] = Value::Null;
    cases.push(("显式null历史不是旧缺失", invalid));
    let mut invalid = valid.clone();
    invalid["current"]["turns"] = json!(4);
    cases.push(("非法current turns不可正规化", invalid));
    let mut invalid = valid.clone();
    invalid["current"]["brightness"] = json!(-101);
    cases.push(("非法负亮度", invalid));
    let mut invalid = valid.clone();
    invalid["history"]["undo"][0]["brightness"] = json!(101);
    cases.push(("非法历史亮度", invalid));
    let mut invalid = valid.clone();
    invalid["current"].as_object_mut().unwrap().remove("flipX");
    cases.push(("current缺六字段之一", invalid));
    let mut invalid = valid.clone();
    invalid.as_object_mut().unwrap().remove("current");
    cases.push(("缺current", invalid));
    let mut invalid = valid.clone();
    invalid["current"]["contrast"] = json!(1);
    cases.push(("current未知字段", invalid));
    let mut invalid = valid.clone();
    invalid["extra"] = json!(true);
    cases.push(("外层未知字段", invalid));
    let mut invalid = valid.clone();
    invalid["history"]["extra"] = json!(true);
    cases.push(("历史未知字段", invalid));
    let mut invalid = valid.clone();
    invalid["history"].as_object_mut().unwrap().remove("redo");
    cases.push(("历史缺Redo数组", invalid));
    let mut invalid = valid.clone();
    invalid["originalWidth"] = json!(RAW.0 + 1);
    cases.push(("原始尺寸身份", invalid));
    let mut invalid = valid.clone();
    invalid["width"] = json!(RAW.0);
    cases.push(("90度当前尺寸不匹配", invalid));
    let mut invalid = valid.clone();
    invalid["originalHeight"] = json!(0);
    cases.push(("零原始尺寸", invalid));
    let mut invalid = valid.clone();
    invalid["history"]["undo"]
        .as_array_mut()
        .unwrap()
        .push(valid["current"].clone());
    cases.push(("Undo尾等于current", invalid));
    let mut invalid = valid.clone();
    let same = invalid["history"]["redo"][0].clone();
    invalid["history"]["redo"]
        .as_array_mut()
        .unwrap()
        .insert(0, same);
    cases.push(("Redo相邻noop", invalid));
    let mut invalid = valid.clone();
    invalid["history"]["undo"] = Value::Array(
        (0..=MAX_PROCESSING_HISTORY_STEPS)
            .map(|index| valid["history"]["undo"][index % 2].clone())
            .collect(),
    );
    invalid["history"]["redo"] = json!([]);
    cases.push(("历史65步不允许导入时裁剪", invalid));
    for (label, invalid) in cases {
        rejected_wire_preserves_target(invalid, current, RAW, processed, label);
    }
    let wrong_current = Adjustments {
        invert: !current.invert,
        ..current
    };
    rejected_wire_preserves_target(
        valid.clone(),
        wrong_current,
        RAW,
        processed,
        "宿主current精确匹配",
    );
    rejected_wire_preserves_target(
        valid.clone(),
        current,
        (RAW.0 + 1, RAW.1),
        processed,
        "宿主原图身份",
    );
    rejected_wire_preserves_target(valid, current, RAW, RAW, "宿主processed身份");
    for raw in [(0, 1), (1, 0), (65_536, 1), (1, 65_536), (8_000, 8_000)] {
        assert!(
            WireProcessingHistory::from_history(
                &source,
                current,
                raw,
                current.dimensions(raw.0, raw.1)
            )
            .is_err(),
            "无效原图尺寸{raw:?}"
        );
    }
}

#[test]
fn processing_legacy_missing_history_restores_nondefault_current_with_empty_navigation() {
    let (source, current) = processing_fixture();
    assert_ne!(current, Adjustments::default());
    let mut encoded = encoded_history(&source, current);
    assert!(encoded.as_object_mut().unwrap().remove("history").is_some());
    let legacy: WireProcessingHistory = serde_json::from_value(encoded).unwrap();
    let processed = current.dimensions(RAW.0, RAW.1);
    legacy.validate(current, RAW, processed).unwrap();
    let (mut restored, _) = processing_fixture();
    legacy
        .restore_into(current, RAW, processed, &mut restored)
        .unwrap();
    assert!(restored.undo.is_empty() && restored.redo.is_empty());
    let mut restored_current = current;
    assert!(!restored.undo(&mut restored_current));
    assert!(!restored.redo(&mut restored_current));
    assert_eq!(restored_current, current);
    let saved = encoded_history(&restored, restored_current);
    assert_eq!(
        saved["current"],
        encoded_history(&source, current)["current"]
    );
    assert_eq!(saved["history"]["undo"], json!([]));
    assert_eq!(saved["history"]["redo"], json!([]));
}

#[test]
fn processing_capacity_trim_preserves_declared_dimensions_current_and_recent_real_chain() {
    let (source, current) = processing_fixture();
    let processed = current.dimensions(RAW.0, RAW.1);
    let mut wire = processing_wire(&source, current);
    assert_eq!(wire.dimensions(), (RAW, processed));
    wire.validate(current, RAW, processed).unwrap();
    for (stack, undo_count, redo_count) in [
        ("undo", 5, 2),
        ("undo", 4, 2),
        ("undo", 3, 2),
        ("undo", 2, 2),
        ("undo", 1, 2),
        ("redo", 1, 1),
        ("undo", 0, 1),
        ("redo", 0, 0),
    ] {
        let mut expected = serde_json::to_value(&wire).unwrap();
        expected["history"][stack].as_array_mut().unwrap().remove(0);
        assert!(wire.trim_oldest_history());
        assert_eq!(serde_json::to_value(&wire).unwrap(), expected);
        assert_eq!(wire.dimensions(), (RAW, processed));
        wire.validate(current, RAW, processed).unwrap();
        assert_eq!(
            expected["history"]["undo"].as_array().unwrap().len(),
            undo_count
        );
        assert_eq!(
            expected["history"]["redo"].as_array().unwrap().len(),
            redo_count
        );

        let mut restored = ProcessingHistory::default();
        let mut restored_current = current;
        wire.restore_into(current, RAW, processed, &mut restored)
            .unwrap();
        let mut original = source.clone();
        let mut original_current = current;
        for _ in 0..undo_count {
            assert!(restored.undo(&mut restored_current));
            assert!(original.undo(&mut original_current));
            assert_eq!(restored_current, original_current);
        }
        assert!(!restored.undo(&mut restored_current));
        for _ in 0..undo_count + redo_count {
            assert!(restored.redo(&mut restored_current));
            assert!(original.redo(&mut original_current));
            assert_eq!(restored_current, original_current);
        }
        assert!(!restored.redo(&mut restored_current));
    }
    let empty = serde_json::to_value(&wire).unwrap();
    assert!(!wire.trim_oldest_history());
    assert_eq!(serde_json::to_value(&wire).unwrap(), empty);
    let mut legacy = encoded_history(&source, current);
    legacy.as_object_mut().unwrap().remove("history");
    let mut legacy: WireProcessingHistory = serde_json::from_value(legacy).unwrap();
    let before = serde_json::to_value(&legacy).unwrap();
    assert_eq!(legacy.dimensions(), (RAW, processed));
    assert!(!legacy.trim_oldest_history());
    assert_eq!(serde_json::to_value(&legacy).unwrap(), before);

    // 声明尺寸可以读取，但不是证明；坏底部先校验失败，不能借裁剪洗掉错误。
    let mut bad = encoded_history(&source, current);
    bad["history"]["undo"][0]["turns"] = json!(4);
    let mut bad: WireProcessingHistory = serde_json::from_value(bad).unwrap();
    let before = serde_json::to_value(&bad).unwrap();
    assert_eq!(bad.dimensions(), (RAW, processed));
    let guarded_trim: Result<bool> = (|| {
        bad.validate(current, RAW, processed)?;
        Ok(bad.trim_oldest_history())
    })();
    assert!(guarded_trim.is_err());
    assert_eq!(serde_json::to_value(&bad).unwrap(), before);
}
