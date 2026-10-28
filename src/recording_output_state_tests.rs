use super::super::{volume_intent::SaveOwner, volume_save};
use super::*;
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

fn fixture_directory() -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let root = std::env::current_dir()
        .expect("项目目录")
        .join("target/qa/recording-output-close-models");
    fs::create_dir_all(&root).expect("自有QA目录");
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时钟")
        .as_nanos();
    let path = root.join(format!(
        "{}-{time}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("唯一自有QA子目录");
    path
}

#[test]
fn copy_and_save_success_resume_queued_close_once_and_failure_keeps_retry() {
    for kind in [OutputKind::Copy, OutputKind::Save] {
        let mut closing = true;
        assert_eq!(complete(kind, true, &mut closing), Completion::ResumeClose);
        assert!(!closing, "交给真实关闭路线前必须消费旧关闭等待");
        assert_eq!(complete(kind, true, &mut closing), Completion::Preview);
        closing = true;
        assert_eq!(complete(kind, false, &mut closing), Completion::Preview);
        assert!(!closing, "失败保留预览和输出重试，不能永久busy");
    }
    let mut closing = true;
    assert_eq!(
        complete(OutputKind::Close, true, &mut closing),
        Completion::RemoveWindow
    );
    closing = true;
    assert_eq!(
        complete(OutputKind::Close, false, &mut closing),
        Completion::CloseFailed
    );
    assert!(!closing);
}

#[test]
fn late_output_receipts_cannot_clear_successor_or_repeat_owned_completion() {
    let old = Arc::new(());
    let successor = Arc::new(());
    let mut slot = Some((successor.clone(), OutputKind::Copy));
    assert!(!take_current_receipt(&mut slot, &old, &OutputKind::Copy));
    assert!(slot
        .as_ref()
        .is_some_and(|(job, _)| Arc::ptr_eq(job, &successor)));
    assert!(!take_current_receipt(
        &mut slot,
        &successor,
        &OutputKind::Save
    ));
    assert!(take_current_receipt(
        &mut slot,
        &successor,
        &OutputKind::Copy
    ));
    assert!(slot.is_none());
    assert!(!take_current_receipt(
        &mut slot,
        &successor,
        &OutputKind::Copy
    ));
}

#[test]
fn output_snapshot_and_save_intent_reject_level_source_and_session_aba() {
    let source = Path::new("original.mp4");
    let output = Path::new("preview.gif");
    let target = Path::new("published.gif");
    let levels = MixLevels::default();
    let snapshot = OutputSnapshot::capture(source, output, levels);
    assert!(snapshot.current(Some(source), Some(output), levels));
    assert!(!snapshot.current(Some(Path::new("new.mp4")), Some(output), levels));
    assert!(!snapshot.current(Some(source), Some(source), levels));
    assert!(!snapshot.current(
        Some(source),
        Some(output),
        MixLevels {
            system: 20,
            ..levels
        }
    ));
    let mut owner = SaveOwner::default();
    let old_snapshot = owner.capture(source, output);
    let old = owner.begin(&old_snapshot, target).expect("旧输出意图");
    owner.invalidate();
    let new_snapshot = owner.capture(source, output);
    let current = owner.begin(&new_snapshot, target).expect("同值后继意图");
    assert!(!owner.current(&old, Some(source), Some(output), target));
    owner.finish(&old);
    assert!(
        owner.current(&current, Some(source), Some(output), target),
        "旧完成不能清新任务"
    );
}

#[test]
fn plain_mp4_and_gif_copy_commit_snapshots_survive_source_and_cache_removal() {
    let directory = fixture_directory();
    for extension in ["mp4", "gif"] {
        let source = directory.join(format!("original.{extension}"));
        let target = directory.join(format!("published.{extension}"));
        let bytes = format!("自有{extension}固定文件字节").into_bytes();
        fs::write(&source, &bytes).expect("源文件");
        let request = volume_save::SaveRequest::for_new_output(&target).expect("仅新增目标");
        let staged = volume_save::prepare_copy(&request, &source, || true).expect("后台复制暂存");
        assert!(!target.exists(), "暂存阶段不得发布");
        let path = staged.commit(|| true).expect("当前意图无覆盖发布");
        let cache = PublishedCopy::from_published(
            OutputSnapshot::capture(&source, &source, MixLevels::default()),
            &path,
        )
        .expect("成功发布取得自有文件身份");
        assert!(cache.current(Some(&source), Some(&source), MixLevels::default()));
        assert!(!cache.current(
            Some(&source),
            Some(&source),
            MixLevels {
                system: 20,
                ..MixLevels::default()
            }
        ));
        assert!(!cache.current(Some(&source), Some(&path), MixLevels::default()));
        assert!(path.exists(), "音量或来源变化只能失效引用，不能删历史成品");
        fs::remove_file(&source).expect("模拟重录删除会话源");
        drop(cache);
        assert_eq!(fs::read(&path).expect("已提交成品不随源或引用消失"), bytes);
    }
}

#[test]
fn new_copy_target_rejects_existing_and_commit_race_never_overwrites() {
    let directory = fixture_directory();
    let source = directory.join("original.mp4");
    let target = directory.join("published.mp4");
    fs::write(&source, b"original-source").expect("原录像");
    fs::write(&target, b"already-existing").expect("已有文件");
    assert!(volume_save::SaveRequest::for_new_output(&target).is_err());
    assert_eq!(
        fs::read(&target).expect("已有文件保持"),
        b"already-existing"
    );
    assert!(volume_save::SaveRequest::for_new_output(Path::new("relative.mp4")).is_err());
    let existing_directory = directory.join("existing-directory");
    fs::create_dir(&existing_directory).expect("自有目录");
    assert!(volume_save::SaveRequest::for_new_output(&existing_directory).is_err());
    fs::remove_file(&target).expect("仅本模型自有文件");
    let request = volume_save::SaveRequest::for_new_output(&target).expect("新增目标");
    let staged = volume_save::prepare_copy(&request, &source, || true).expect("暂存");
    fs::write(&target, b"external-successor").expect("模拟准备期间目标出现");
    assert!(staged.commit(|| true).is_err());
    assert_eq!(
        fs::read(&target).expect("竞态目标保持"),
        b"external-successor"
    );
    assert_eq!(fs::read(&source).expect("原录像保持"), b"original-source");
}

#[test]
fn canceled_and_late_copy_staging_never_publishes_or_modifies_originals() {
    let directory = fixture_directory();
    let source = directory.join("original.gif");
    let target = directory.join("published.gif");
    fs::write(&source, b"original-gif").expect("原成品");
    let request = volume_save::SaveRequest::for_new_output(&target).expect("新增目标");
    assert!(volume_save::prepare_copy(&request, &source, || false).is_err());
    assert!(!target.exists());
    let staged = volume_save::prepare_copy(&request, &source, || true).expect("迟到暂存");
    drop(staged);
    assert!(!target.exists(), "撤销回执Drop只清自有暂存");
    let staged = volume_save::prepare_copy(&request, &source, || true).expect("重试暂存");
    assert!(staged.commit(|| false).is_err());
    assert!(!target.exists());
    assert_eq!(
        fs::read(&source).expect("失败原文件可重试"),
        b"original-gif"
    );
}

#[test]
fn countdown_record_hint_matches_cancel_route_and_stays_distinct_from_stop() {
    assert_eq!(
        record_button_hint(super::super::Stage::Countdown),
        "取消倒计时 · F9"
    );
    assert_eq!(
        record_button_hint(super::super::Stage::Recording),
        "停止录制 · F9"
    );
    assert_eq!(
        record_button_hint(super::super::Stage::Idle),
        "开始录制 · F9"
    );
    use super::super::shortcut_keys::{self, Action, Context, Modifiers, Route, Stage};
    let context = Context {
        stage: Stage::Countdown,
        keyboard_owned: true,
        ime_composing: false,
        native_dialog: false,
        flyout_open: false,
        drawing: false,
        output_ready: false,
    };
    assert_eq!(
        shortcut_keys::route("f9", Modifiers::default(), false, context),
        Route::Run(Action::ToggleRecord)
    );
    assert_eq!(
        shortcut_keys::route("escape", Modifiers::default(), false, context),
        Route::Run(Action::ToggleRecord)
    );
}

#[test]
fn copy_cancellation_after_bytes_written_keeps_unconfirmed_staging_and_allows_retry() {
    let directory = fixture_directory();
    let source = directory.join("original.mp4");
    let target = directory.join("published.mp4");
    fs::write(&source, b"source-remains-retryable").expect("原成品");
    let request = volume_save::SaveRequest::for_new_output(&target).expect("仅新增目标");
    let mut checkpoints = 0;
    assert!(volume_save::prepare_copy(&request, &source, || {
        checkpoints += 1;
        checkpoints == 1
    })
    .is_err());
    assert_eq!(checkpoints, 2, "真实事务在写入后收取取消，不发布");
    assert!(!target.exists());
    let unconfirmed = fs::read_dir(&directory)
        .expect("自有测试目录")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("取消前未核验身份的暂存目录应保留");
    assert_eq!(
        fs::read(unconfirmed.join("audio.mp4")).expect("不认领或删除未核验的暂存"),
        b"source-remains-retryable"
    );
    assert_eq!(
        fs::read(&source).expect("原成品不变"),
        b"source-remains-retryable"
    );
    let path = volume_save::prepare_copy(&request, &source, || true)
        .expect("重试准备")
        .commit(|| true)
        .expect("重试提交");
    assert_eq!(
        fs::read(path).expect("重试成品"),
        b"source-remains-retryable"
    );
}

#[test]
fn published_copy_commit_with_unknown_staging_content_keeps_success_and_file() {
    let directory = fixture_directory();
    let source = directory.join("original.gif");
    let target = directory.join("published.gif");
    fs::write(&source, b"published-gif-copy").expect("原成品");
    let request = volume_save::SaveRequest::for_new_output(&target).expect("仅新增目标");
    let staged = volume_save::prepare_copy(&request, &source, || true).expect("暂存");
    let staging_directory = fs::read_dir(&directory)
        .expect("自有QA目录")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("实际事务自有暂存目录");
    let unknown = staging_directory.join("unknown.txt");
    fs::write(&unknown, b"preserve-unknown").expect("模拟未知内容");
    let published = staged
        .commit(|| true)
        .expect("提交后清理提示不能撤回已发布成品");
    assert_eq!(
        fs::read(&published).expect("复制引用仍可用"),
        b"published-gif-copy"
    );
    assert_eq!(
        fs::read(&unknown).expect("不递归清理未知内容"),
        b"preserve-unknown"
    );
    let cache = PublishedCopy::from_published(
        OutputSnapshot::capture(&source, &source, MixLevels::default()),
        &published,
    )
    .expect("成功发布取得自有文件身份");
    fs::remove_file(&published).expect("模拟用户明确删除自有成品");
    assert!(
        !cache.current(Some(&source), Some(&source), MixLevels::default()),
        "缺失成品不能复用为剪贴板路径"
    );
}

#[test]
fn gif_publication_reuses_only_owned_session_path_and_rerecord_preserves_unknown_gif() {
    let directory = fixture_directory();
    let source = directory.join("X.mp4");
    let foreign = source.with_extension("gif");
    let target = directory.join("session-new.gif");
    fs::write(&source, b"session-video").unwrap();
    fs::write(&foreign, b"other-session-copy").unwrap();
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    assert!(files.reusable_gif(&source).is_none());
    assert!(new_gif_request(|| Ok(foreign.clone())).is_err());
    let request = new_gif_request(|| Ok(target.clone())).unwrap();
    volume_save::prepare_with(
        &request,
        |staged| {
            fs::write(staged, b"this-session-gif")?;
            Ok(())
        },
        || true,
    )
    .unwrap()
    .commit(|| true)
    .unwrap();
    files.gif_published(&source, &target).unwrap();
    assert_eq!(files.reusable_gif(&source), Some(target.as_path()));
    assert!(files.reusable_gif(&directory.join("other.mp4")).is_none());
    assert!(files.remove_gif_for_rerecord().unwrap());
    assert!(files.remove_original_for_rerecord().unwrap());
    assert!(!target.exists() && !source.exists());
    assert_eq!(fs::read(&foreign).unwrap(), b"other-session-copy");
    assert!(!files.remove_gif_for_rerecord().unwrap());
}

#[test]
fn replaced_session_gif_and_recording_keep_foreign_successors_and_copy_cache_expires() {
    let directory = fixture_directory();
    let source = directory.join("owned.mp4");
    let gif = directory.join("owned.gif");
    let copy = directory.join("persistent.mp4");
    fs::write(&source, b"original-video").unwrap();
    fs::write(&gif, b"original-gif").unwrap();
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    files.gif_published(&source, &gif).unwrap();
    let writeback = files.writeback_request(&source).unwrap();
    let staged = volume_save::prepare_with(
        &writeback,
        |staged| {
            fs::write(staged, b"prepared-close")?;
            Ok(())
        },
        || true,
    )
    .unwrap();
    fs::rename(&source, directory.join("original-backup.mp4")).unwrap();
    fs::write(&source, b"foreign-video").unwrap();
    fs::rename(&gif, directory.join("original-backup.gif")).unwrap();
    fs::write(&gif, b"foreign-gif").unwrap();
    assert!(files.writeback_request(&source).is_err());
    assert!(staged.commit(|| true).is_err());
    assert!(files.reusable_gif(&source).is_none());
    assert!(!files.remove_gif_for_rerecord().unwrap());
    assert!(!files.remove_original_for_rerecord().unwrap());
    assert_eq!(fs::read(&source).unwrap(), b"foreign-video");
    assert_eq!(fs::read(&gif).unwrap(), b"foreign-gif");
    let request = volume_save::SaveRequest::for_new_output(&copy).unwrap();
    volume_save::prepare_copy(&request, &source, || true)
        .unwrap()
        .commit(|| true)
        .unwrap();
    let cache = PublishedCopy::from_published(
        OutputSnapshot::capture(&source, &source, MixLevels::default()),
        &copy,
    )
    .unwrap();
    assert!(cache.current(Some(&source), Some(&source), MixLevels::default()));
    fs::rename(&copy, directory.join("copy-backup.mp4")).unwrap();
    fs::write(&copy, b"foreign-copy").unwrap();
    assert!(!cache.current(Some(&source), Some(&source), MixLevels::default()));
    drop(cache);
    assert_eq!(fs::read(&copy).unwrap(), b"foreign-copy");
}

#[test]
fn close_writeback_uses_owned_staging_and_keeps_mix_save_after_ordinary_or_queued_close() {
    for queued in [false, true] {
        let directory = fixture_directory();
        let source = directory.join("X.mp4");
        let saved = source.with_extension("mix.mp4");
        fs::write(&source, b"original-video").unwrap();
        let mut files = SessionFiles::default();
        files.recording_published(&source).unwrap();
        let mut owner = SaveOwner::default();
        let capture = owner.capture(&source, &source);
        let save = volume_save::SaveRequest::from_dialog(&saved, &saved).unwrap();
        let intent = owner.begin(&capture, &saved).unwrap();
        let published = volume_save::prepare_with(
            &save,
            |staged| {
                fs::write(staged, b"approved-mix-save")?;
                Ok(())
            },
            || true,
        )
        .unwrap()
        .commit(|| owner.current(&intent, Some(&source), Some(&source), &saved))
        .unwrap();
        owner.finish(&intent);
        files
            .save_published(&save, &published, MixLevels::default())
            .unwrap();
        let mut closing = queued;
        assert_eq!(
            complete(OutputKind::Save, true, &mut closing),
            if queued {
                Completion::ResumeClose
            } else {
                Completion::Preview
            }
        );
        assert!(!closing);
        let close = files.writeback_request(&source).unwrap();
        assert_eq!(close.target(), source.as_path());
        assert!(volume_save::prepare_with(
            &close,
            |staged| {
                assert_ne!(staged, saved.as_path());
                fs::write(staged, b"partial-close")?;
                Err("模型编码失败".into())
            },
            || true
        )
        .is_err());
        assert_eq!(fs::read(&source).unwrap(), b"original-video");
        assert_eq!(fs::read(&saved).unwrap(), b"approved-mix-save");
        let canceled_intent = owner.begin(&capture, &source).unwrap();
        let canceled = volume_save::prepare_with(
            &close,
            |staged| {
                fs::write(staged, b"late-close")?;
                Ok(())
            },
            || true,
        )
        .unwrap();
        owner.invalidate();
        assert!(canceled
            .commit(|| owner.current(&canceled_intent, Some(&source), Some(&source), &source))
            .is_err());
        assert_eq!(fs::read(&source).unwrap(), b"original-video");
        assert_eq!(fs::read(&saved).unwrap(), b"approved-mix-save");
        let current_capture = owner.capture(&source, &source);
        let close_intent = owner.begin(&current_capture, &source).unwrap();
        let staged = volume_save::prepare_with(
            &close,
            |staged| {
                assert_ne!(staged, saved.as_path());
                fs::write(staged, b"finished-close")?;
                Ok(())
            },
            || true,
        )
        .unwrap();
        assert_eq!(
            fs::read(&source).unwrap(),
            b"original-video",
            "worker暂存不得提前写回"
        );
        staged
            .commit(|| {
                owner.current(&close_intent, Some(&source), Some(&source), &source)
                    && files.writeback_request(&source).as_ref().ok() == Some(&close)
            })
            .unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"finished-close");
        assert_eq!(fs::read(&saved).unwrap(), b"approved-mix-save");
    }
}

#[test]
fn final_native_publish_rejects_new_target_after_last_inspection_and_all_cancel_gates() {
    for cancel_at in 1..=3 {
        let directory = fixture_directory();
        let source = directory.join("source.mp4");
        let target = directory.join("new.mp4");
        fs::write(&source, b"source-kept").unwrap();
        let request = volume_save::SaveRequest::for_new_output(&target).unwrap();
        let staged = volume_save::prepare_copy(&request, &source, || true).unwrap();
        let mut calls = 0;
        assert!(staged
            .commit(|| {
                calls += 1;
                calls != cancel_at
            })
            .is_err());
        assert_eq!(calls, cancel_at);
        assert!(!target.exists());
        assert_eq!(fs::read(&source).unwrap(), b"source-kept");
    }
    let directory = fixture_directory();
    let source = directory.join("source.gif");
    let target = directory.join("new.gif");
    fs::write(&source, b"source-kept").unwrap();
    let request = volume_save::SaveRequest::for_new_output(&target).unwrap();
    let staged = volume_save::prepare_copy(&request, &source, || true).unwrap();
    let mut calls = 0;
    let result = staged.commit(|| {
        calls += 1;
        if calls == 3 {
            fs::write(&target, b"appeared-after-inspection").unwrap();
        }
        true
    });
    assert_eq!(calls, 3, "第三回调在inspect之后、真正native publish之前");
    assert!(result.is_err());
    assert_eq!(fs::read(&target).unwrap(), b"appeared-after-inspection");
    assert_eq!(fs::read(&source).unwrap(), b"source-kept");
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 2, "只清自有暂存");
}

#[test]
fn plain_save_missing_extension_and_prepare_failure_preserve_unapproved_existing_targets() {
    for extension in ["mp4", "gif"] {
        let directory = fixture_directory();
        let chosen = directory.join("clip");
        let target = chosen.with_extension(extension);
        let source = directory.join(format!("source.{extension}"));
        fs::write(&source, b"session-original").unwrap();
        fs::write(&target, b"unapproved-existing").unwrap();
        assert!(volume_save::SaveRequest::from_dialog(&chosen, &target).is_err());
        assert!(volume_save::SaveRequest::for_new_output(&source).is_err());
        let approved = volume_save::SaveRequest::from_dialog(&target, &target).unwrap();
        assert!(volume_save::prepare_copy(&approved, &directory.join("missing"), || true).is_err());
        assert!(volume_save::prepare_with(
            &approved,
            |staged| {
                fs::write(staged, b"partial")?;
                Err("模型复制失败".into())
            },
            || true
        )
        .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unapproved-existing");
        assert_eq!(fs::read(&source).unwrap(), b"session-original");
    }
}

#[test]
fn approved_source_and_hardlink_save_refresh_only_the_actual_session_publication() {
    for hardlink in [false, true] {
        let directory = fixture_directory();
        let source = directory.join("source.mp4");
        fs::write(&source, b"session-original").unwrap();
        let target = if hardlink {
            let alias = directory.join("source-alias.mp4");
            fs::hard_link(&source, &alias).unwrap();
            alias
        } else {
            source.clone()
        };
        let mut files = SessionFiles::default();
        files.recording_published(&source).unwrap();
        let before = files.writeback_request(&source).unwrap();
        let approved = volume_save::SaveRequest::from_dialog(&target, &target).unwrap();
        let mut owner = SaveOwner::default();
        let capture = owner.capture(&source, &source);
        let intent = owner.begin(&capture, &target).unwrap();
        let published = volume_save::prepare_copy(&approved, &source, || true)
            .unwrap()
            .commit(|| owner.current(&intent, Some(&source), Some(&source), &target))
            .unwrap();
        owner.finish(&intent);
        files
            .save_published(&approved, &published, MixLevels::default())
            .unwrap();
        let after = files.writeback_request(&source).unwrap();
        if hardlink {
            assert_eq!(before, after, "Replace别名不改原目录项资格");
        } else {
            assert_ne!(before, after, "获准原件提交后更新真实FileID");
        }
        assert_eq!(fs::read(&source).unwrap(), b"session-original");
        assert_eq!(fs::read(&target).unwrap(), b"session-original");
        assert!(files.remove_original_for_rerecord().unwrap());
        if hardlink {
            assert_eq!(fs::read(&target).unwrap(), b"session-original");
        }
    }
    let directory = fixture_directory();
    let source = directory.join("source.mp4");
    let alias = directory.join("alias.mp4");
    fs::write(&source, b"session-original").unwrap();
    fs::hard_link(&source, &alias).unwrap();
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    let approved = volume_save::SaveRequest::from_dialog(&alias, &alias).unwrap();
    let staged = volume_save::prepare_copy(&approved, &source, || true).unwrap();
    fs::rename(&source, directory.join("source-backup.mp4")).unwrap();
    fs::write(&source, b"foreign-original").unwrap();
    let published = staged.commit(|| true).unwrap();
    files
        .save_published(&approved, &published, MixLevels::default())
        .unwrap();
    assert!(files.writeback_request(&source).is_err());
    assert!(!files.remove_original_for_rerecord().unwrap());
    assert_eq!(fs::read(&source).unwrap(), b"foreign-original");
    let directory = fixture_directory();
    let source = directory.join("source.mp4");
    let gif = directory.join("owned.gif");
    fs::write(&source, b"session-original").unwrap();
    fs::write(&gif, b"session-gif").unwrap();
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    files.gif_published(&source, &gif).unwrap();
    let approved = volume_save::SaveRequest::from_dialog(&gif, &gif).unwrap();
    let published = volume_save::prepare_copy(&approved, &gif, || true)
        .unwrap()
        .commit(|| true)
        .unwrap();
    files
        .save_published(&approved, &published, MixLevels::default())
        .unwrap();
    assert_eq!(files.reusable_gif(&source), Some(gif.as_path()));
    assert!(
        files.remove_gif_for_rerecord().unwrap(),
        "获准GIF另存同路径更新身份后仍可按会话清理"
    );
    assert_eq!(fs::read(&source).unwrap(), b"session-original");
}

#[test]
fn save_dialog_return_and_output_worker_gate_preserve_active_close_and_reject_busy_states() {
    let source = Path::new("session.mp4");
    let mut owner = SaveOwner::default();
    let snapshot = owner.capture(source, source);
    let close = owner.begin(&snapshot, source).unwrap();
    let ready = OutputGate {
        preview: true,
        has_output: true,
        ..Default::default()
    };
    assert!(ready.dialog_current(&owner, &snapshot, Some(source), Some(source)));
    for blocked in [
        OutputGate {
            closing: true,
            ..ready
        },
        OutputGate {
            output_busy: true,
            closing: true,
            ..ready
        },
        OutputGate {
            output_busy: true,
            ..ready
        },
        OutputGate {
            gif_busy: true,
            ..ready
        },
        OutputGate {
            picking_save: true,
            ..ready
        },
        OutputGate {
            gesture_busy: true,
            ..ready
        },
        OutputGate {
            preview: false,
            ..ready
        },
        OutputGate {
            has_output: false,
            ..ready
        },
    ] {
        assert!(!blocked.dialog_current(&owner, &snapshot, Some(source), Some(source)));
        assert!(
            owner.current(&close, Some(source), Some(source), source),
            "迟到对话框门不能begin新意图替换Close"
        );
    }
    assert!(!OutputGate {
        output_busy: true,
        ..ready
    }
    .worker_slot_available());
    assert!(!OutputGate {
        gif_busy: true,
        ..ready
    }
    .worker_slot_available());
    owner.invalidate();
    assert!(!ready.dialog_current(&owner, &snapshot, Some(source), Some(source)));
}

#[test]
fn gif_late_or_canceled_staging_cannot_publish_or_clear_successor() {
    let directory = fixture_directory();
    let source = directory.join("source.mp4");
    let target = directory.join("late.gif");
    fs::write(&source, b"source-kept").unwrap();
    let mut owner = SaveOwner::default();
    let capture = owner.capture(&source, &source);
    let old = owner.begin(&capture, &target).unwrap();
    let request = new_gif_request(|| Ok(target.clone())).unwrap();
    let staged = volume_save::prepare_with(
        &request,
        |path| {
            fs::write(path, b"finished-gif")?;
            Ok(())
        },
        || true,
    )
    .unwrap();
    let old_job = Arc::new(());
    let next_job = Arc::new(());
    let mut slot = Some(next_job.clone());
    assert!(!take_current_job(&mut slot, &old_job));
    assert!(Arc::ptr_eq(slot.as_ref().unwrap(), &next_job));
    owner.invalidate();
    let next_capture = owner.capture(&source, &source);
    let next = owner.begin(&next_capture, &target).unwrap();
    assert!(staged
        .commit(|| owner.current(&old, Some(&source), Some(&source), &target))
        .is_err());
    owner.finish(&old);
    assert!(owner.current(&next, Some(&source), Some(&source), &target));
    assert!(!target.exists());
    assert_eq!(fs::read(&source).unwrap(), b"source-kept");
}

#[test]
fn real_allocator_relative_directory_produces_absolute_recording_gif_and_copy_targets() {
    use crate::{recording, recording_options::Preset};
    let directory = fixture_directory();
    let current = std::env::current_dir().unwrap();
    let relative = directory.strip_prefix(&current).unwrap();
    assert!(!relative.is_absolute());
    let source = recording::new_output_path_in(relative, Preset::CompactMp4).unwrap();
    assert!(source.is_absolute());
    assert_eq!(
        source.parent(),
        Some(std::path::absolute(&directory).unwrap().as_path())
    );
    fs::write(&source, b"session-video").unwrap();
    let foreign = source.with_extension("gif");
    fs::write(&foreign, b"unknown-same-stem-gif").unwrap();
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    assert_eq!(
        files.writeback_request(&source).unwrap().target(),
        source.as_path()
    );
    let gif = new_gif_request(|| recording::new_output_path_in(relative, Preset::Gif)).unwrap();
    assert!(gif.target().is_absolute());
    assert_ne!(gif.target(), foreign.as_path());
    volume_save::prepare_with(
        &gif,
        |path| {
            fs::write(path, b"session-gif")?;
            Ok(())
        },
        || true,
    )
    .unwrap()
    .commit(|| true)
    .unwrap();
    files.gif_published(&source, gif.target()).unwrap();
    let copy_path = recording::new_output_path_in(relative, Preset::CompactMp4).unwrap();
    assert!(copy_path.is_absolute());
    assert_ne!(copy_path, source);
    let copy_request = volume_save::SaveRequest::for_new_output(&copy_path).unwrap();
    volume_save::prepare_copy(&copy_request, &source, || true)
        .unwrap()
        .commit(|| true)
        .unwrap();
    let cache = PublishedCopy::from_published(
        OutputSnapshot::capture(&source, &source, MixLevels::default()),
        &copy_path,
    )
    .unwrap();
    assert!(cache.current(Some(&source), Some(&source), MixLevels::default()));
    assert!(volume_save::SaveRequest::for_new_output(&relative.join("rejected.gif")).is_err());
    assert!(files.remove_gif_for_rerecord().unwrap());
    assert!(files.remove_original_for_rerecord().unwrap());
    drop(cache);
    assert_eq!(fs::read(&copy_path).unwrap(), b"session-video");
    assert_eq!(fs::read(&foreign).unwrap(), b"unknown-same-stem-gif");
}

#[test]
fn auxiliary_cleanup_keeps_approved_track_save_across_close_queue_rerecord_and_recording_finish() {
    for mode in [
        "close",
        "queued-close",
        "rerecord",
        "recording-finish-close",
    ] {
        let directory = fixture_directory();
        let source = directory.join("session.mp4");
        let preview = directory.join("session.gif.mp4");
        let track = directory.join("session.mp4.system.wav");
        let other_track = directory.join("session.mp4.microphone.wav");
        fs::write(&source, b"original-video").unwrap();
        fs::write(&preview, b"owned-preview").unwrap();
        fs::write(&track, b"owned-system-track").unwrap();
        fs::write(&other_track, b"owned-microphone-track").unwrap();
        let mut files = SessionFiles::default();
        files.recording_published(&source).unwrap();
        files
            .auxiliary_published([preview.as_path(), track.as_path(), other_track.as_path()])
            .unwrap();
        let saved = mode != "recording-finish-close";
        if saved {
            let approved = volume_save::SaveRequest::from_dialog(&track, &track).unwrap();
            let mut owner = SaveOwner::default();
            let capture = owner.capture(&source, &source);
            let intent = owner.begin(&capture, &track).unwrap();
            let published = volume_save::prepare_copy(&approved, &source, || true)
                .unwrap()
                .commit(|| owner.current(&intent, Some(&source), Some(&source), &track))
                .unwrap();
            owner.finish(&intent);
            files
                .save_published(&approved, &published, MixLevels::default())
                .unwrap();
            let mut closing = mode == "queued-close";
            assert_eq!(
                complete(OutputKind::Save, true, &mut closing),
                if mode == "queued-close" {
                    Completion::ResumeClose
                } else {
                    Completion::Preview
                }
            );
        }
        assert!(
            files.cleanup_auxiliary().is_empty(),
            "真实三处清理共用此所有权决策：{mode}"
        );
        assert!(!preview.exists() && !other_track.exists());
        if saved {
            assert_eq!(fs::read(&track).unwrap(), b"original-video");
        } else {
            assert!(
                !track.exists(),
                "录制中关闭成功回执也清所有实际发布辅助文件"
            );
        }
        assert_eq!(fs::read(&source).unwrap(), b"original-video");
        if mode == "rerecord" {
            assert!(files.remove_original_for_rerecord().unwrap());
        } else {
            files.clear();
        }
        assert!(files.cleanup_auxiliary().is_empty());
        if saved {
            assert_eq!(fs::read(&track).unwrap(), b"original-video");
        }
    }
}

#[test]
fn recording_initial_mix_baseline_follows_the_recorded_and_overwritten_audio() {
    let directory = fixture_directory();
    let source = directory.join("session.mp4");
    fs::write(&source, b"recorded-custom-mix").unwrap();
    let initial_mix = MixLevels {
        system: 25,
        microphone: 0,
    };
    let default = MixLevels::default();
    let mut files = SessionFiles::default();
    files
        .recording_published_with_mix(&source, initial_mix)
        .unwrap();
    assert!(!files.mix_needed(Some(&source), Some(&source), true, initial_mix));
    assert!(files.mix_needed(Some(&source), Some(&source), true, default));

    let request = files.writeback_request(&source).unwrap();
    let published = volume_save::prepare_with(
        &request,
        |path| {
            fs::write(path, b"remixed-default")?;
            Ok(())
        },
        || true,
    )
    .unwrap()
    .commit(|| true)
    .unwrap();
    files.save_published(&request, &published, default).unwrap();
    assert!(!files.mix_needed(Some(&source), Some(&source), true, default));
    assert!(files.mix_needed(Some(&source), Some(&source), true, initial_mix));
    assert_eq!(fs::read(&source).unwrap(), b"remixed-default");
}

#[test]
fn source_save_baked_levels_restore_default_for_copy_save_close_and_keep_alias_baseline() {
    let directory = fixture_directory();
    let source = directory.join("session.mp4");
    fs::write(&source, b"baked-default").unwrap();
    let default = MixLevels::default();
    let quieter = MixLevels {
        system: 20,
        ..default
    };
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    assert!(!files.mix_needed(Some(&source), Some(&source), true, default));
    assert!(files.mix_needed(Some(&source), Some(&source), true, quieter));
    let approved = volume_save::SaveRequest::from_dialog(&source, &source).unwrap();
    let published = volume_save::prepare_with(
        &approved,
        |path| {
            fs::write(path, b"baked-system-20")?;
            Ok(())
        },
        || true,
    )
    .unwrap()
    .commit(|| true)
    .unwrap();
    files
        .save_published(&approved, &published, quieter)
        .unwrap();
    assert!(!files.mix_needed(Some(&source), Some(&source), true, quieter));
    assert!(files.mix_needed(Some(&source), Some(&source), true, default));
    assert!(!files.mix_needed(Some(&source), Some(&source), false, default));
    assert!(!files.mix_needed(
        Some(&source),
        Some(&directory.join("current.gif")),
        true,
        default
    ));
    for kind in [OutputKind::Copy, OutputKind::Save, OutputKind::Close] {
        assert!(
            files.mix_needed(Some(&source), Some(&source), true, default),
            "真实Copy/Save worker与Close入口共同调用此判定：{kind:?}"
        );
        let request = match kind {
            OutputKind::Copy => {
                volume_save::SaveRequest::for_new_output(&directory.join("restored-copy.mp4"))
                    .unwrap()
            }
            OutputKind::Save => {
                let path = directory.join("restored-save.mp4");
                volume_save::SaveRequest::from_dialog(&path, &path).unwrap()
            }
            OutputKind::Close => files.writeback_request(&source).unwrap(),
        };
        let restored = volume_save::prepare_with(
            &request,
            |path| {
                fs::write(path, b"baked-restored-default")?;
                Ok(())
            },
            || true,
        )
        .unwrap()
        .commit(|| true)
        .unwrap();
        assert_eq!(fs::read(&restored).unwrap(), b"baked-restored-default");
    }
    let directory = fixture_directory();
    let source = directory.join("session.mp4");
    let alias = directory.join("approved-alias.mp4");
    fs::write(&source, b"original-default").unwrap();
    fs::hard_link(&source, &alias).unwrap();
    let mut files = SessionFiles::default();
    files.recording_published(&source).unwrap();
    let approved = volume_save::SaveRequest::from_dialog(&alias, &alias).unwrap();
    let published = volume_save::prepare_with(
        &approved,
        |path| {
            fs::write(path, b"alias-system-20")?;
            Ok(())
        },
        || true,
    )
    .unwrap()
    .commit(|| true)
    .unwrap();
    files
        .save_published(&approved, &published, quieter)
        .unwrap();
    assert!(!files.mix_needed(Some(&source), Some(&source), true, default));
    assert!(files.mix_needed(Some(&source), Some(&source), true, quieter));
    assert!(files.writeback_request(&source).is_ok());
    assert_eq!(fs::read(&source).unwrap(), b"original-default");
    assert!(files.remove_original_for_rerecord().unwrap());
    assert_eq!(fs::read(&alias).unwrap(), b"alias-system-20");
}
