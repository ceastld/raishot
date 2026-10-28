//! 录屏输出回执与排队关闭；只有实际当前任务可以消费自己的槽位。
use super::{volume_intent, volume_save};
use crate::{audio_capture::MixLevels, Result};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputKind {
    Copy,
    Save,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Completion {
    Preview,
    ResumeClose,
    RemoveWindow,
    CloseFailed,
}

/// 复制/保存成功后才续接用户关闭；失败解除关闭等待，保留当前文件与重试入口。
pub(super) fn complete(kind: OutputKind, succeeded: bool, closing: &mut bool) -> Completion {
    let requested_close = std::mem::take(closing);
    match (kind, succeeded, requested_close) {
        (OutputKind::Close, true, _) => Completion::RemoveWindow,
        (OutputKind::Close, false, _) => Completion::CloseFailed,
        (OutputKind::Copy | OutputKind::Save, true, true) => Completion::ResumeClose,
        _ => Completion::Preview,
    }
}

/// 先核 Arc 和动作，再清槽；旧回执不能释放或改写后继任务。
pub(super) fn take_current_receipt<J, A: PartialEq>(
    slot: &mut Option<(Arc<J>, A)>,
    job: &Arc<J>,
    action: &A,
) -> bool {
    if !slot.as_ref().is_some_and(|(current, current_action)| {
        Arc::ptr_eq(current, job) && current_action == action
    }) {
        return false;
    }
    slot.take();
    true
}

pub(super) fn take_current_job<J>(slot: &mut Option<Arc<J>>, job: &Arc<J>) -> bool {
    if !slot
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, job))
    {
        return false;
    }
    slot.take();
    true
}

/// 原生保存回调和真实输出入口共用的宿主门；回调先恢复 picking_save 再检查。
#[derive(Clone, Copy, Default)]
pub(super) struct OutputGate {
    pub preview: bool,
    pub has_output: bool,
    pub gif_busy: bool,
    pub output_busy: bool,
    pub closing: bool,
    pub picking_save: bool,
    pub gesture_busy: bool,
}

impl OutputGate {
    pub(super) fn worker_slot_available(self) -> bool {
        !self.gif_busy && !self.output_busy
    }

    pub(super) fn ready(self) -> bool {
        self.preview
            && self.has_output
            && self.worker_slot_available()
            && !self.closing
            && !self.picking_save
            && !self.gesture_busy
    }

    pub(super) fn dialog_current(
        self,
        owner: &volume_intent::SaveOwner,
        snapshot: &volume_intent::SaveSnapshot,
        source: Option<&Path>,
        output: Option<&Path>,
    ) -> bool {
        self.ready() && owner.snapshot_current(snapshot, source, output)
    }
}

pub(super) fn new_gif_request(
    allocate: impl FnOnce() -> Result<PathBuf>,
) -> Result<volume_save::SaveRequest> {
    volume_save::SaveRequest::for_new_output(&allocate()?)
}

/// 原件与额外 GIF 的会话所有权；Copy/另存成品不加入重录删除集合。
#[derive(Default)]
pub(super) struct SessionFiles {
    original: Option<volume_save::OwnedFile>,
    gif: Option<(PathBuf, volume_save::OwnedFile)>,
    auxiliary: Vec<volume_save::OwnedFile>,
    /// 原件音轨实际包含的音量；随录前设置及原件覆盖保存更新。
    baked_levels: MixLevels,
}

impl SessionFiles {
    pub(super) fn recording_published(&mut self, path: &Path) -> Result<()> {
        self.clear();
        self.original = Some(volume_save::OwnedFile::from_published(path)?);
        Ok(())
    }

    pub(super) fn recording_published_with_mix(
        &mut self,
        path: &Path,
        initial_mix: MixLevels,
    ) -> Result<()> {
        let published = self.recording_published(path);
        // 成片已提交；登记身份失败也不能把实际混音退回默认。
        self.baked_levels = initial_mix;
        published
    }

    pub(super) fn writeback_request(&self, source: &Path) -> Result<volume_save::SaveRequest> {
        volume_save::SaveRequest::for_owned_writeback(
            self.original.as_ref().ok_or_else(|| {
                tr!(
                    "原录像已被其它程序修改，未写回，现有文件已保留",
                    "The recording was changed by another program; nothing was written and the file is kept"
                )
            })?,
            source,
        )
    }

    pub(super) fn gif_published(&mut self, source: &Path, path: &Path) -> Result<()> {
        self.gif = Some((source.into(), volume_save::OwnedFile::from_published(path)?));
        Ok(())
    }

    /// 仅登记成功 RecordingResult 回执中的实际中间预览／分轨。
    /// 用户 Save 覆盖这些路径后不刷新其可删除身份。
    pub(super) fn auxiliary_published<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Result<()> {
        for path in paths {
            self.auxiliary
                .push(volume_save::OwnedFile::from_published(path)?);
        }
        Ok(())
    }

    pub(super) fn cleanup_auxiliary(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        for file in std::mem::take(&mut self.auxiliary) {
            let path = file.path().display().to_string();
            if let Err(error) = file.remove_if_current() {
                errors.push(format!("{path}：{error}"));
            }
        }
        errors
    }

    pub(super) fn mix_needed(
        &self,
        source: Option<&Path>,
        output: Option<&Path>,
        has_tracks: bool,
        levels: MixLevels,
    ) -> bool {
        has_tracks
            && source.is_some_and(|source| output == Some(source))
            && levels != self.baked_levels
    }

    pub(super) fn save_published(
        &mut self,
        request: &volume_save::SaveRequest,
        path: &Path,
        levels: MixLevels,
    ) -> Result<()> {
        if let Some(original) = &mut self.original {
            if original.refresh_after_save(request, path)? {
                self.baked_levels = levels;
            }
        }
        if let Some((_, gif)) = &mut self.gif {
            let _ = gif.refresh_after_save(request, path)?;
        }
        Ok(())
    }

    pub(super) fn reusable_gif(&self, source: &Path) -> Option<&Path> {
        self.gif.as_ref().and_then(|(original, file)| {
            (original == source && file.current(file.path()).unwrap_or(false))
                .then_some(file.path())
        })
    }

    pub(super) fn remove_gif_for_rerecord(&mut self) -> Result<bool> {
        match self.gif.take() {
            Some((_, file)) => file.remove_if_current(),
            None => Ok(false),
        }
    }

    pub(super) fn remove_original_for_rerecord(&mut self) -> Result<bool> {
        match self.original.take() {
            Some(file) => file.remove_if_current(),
            None => Ok(false),
        }
    }

    pub(super) fn clear(&mut self) {
        self.original.take();
        self.gif.take();
        self.auxiliary.clear();
        self.baked_levels = MixLevels::default();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputSnapshot {
    pub source: PathBuf,
    pub output: PathBuf,
    pub levels: MixLevels,
}

impl OutputSnapshot {
    pub(super) fn capture(source: &Path, output: &Path, levels: MixLevels) -> Self {
        Self {
            source: source.into(),
            output: output.into(),
            levels,
        }
    }

    pub(super) fn current(
        &self,
        source: Option<&Path>,
        output: Option<&Path>,
        levels: MixLevels,
    ) -> bool {
        source == Some(self.source.as_path())
            && output == Some(self.output.as_path())
            && levels == self.levels
    }
}

/// 已经原子提交的自有成品，归录屏目录所有；引用失效或 Drop 均不删除文件。
pub(super) struct PublishedCopy {
    pub snapshot: OutputSnapshot,
    pub path: PathBuf,
    owned: volume_save::OwnedFile,
}

impl PublishedCopy {
    pub(super) fn from_published(snapshot: OutputSnapshot, path: &Path) -> Result<Self> {
        Ok(Self {
            snapshot,
            path: path.into(),
            owned: volume_save::OwnedFile::from_published(path)?,
        })
    }

    pub(super) fn current(
        &self,
        source: Option<&Path>,
        output: Option<&Path>,
        levels: MixLevels,
    ) -> bool {
        self.snapshot.current(source, output, levels)
            && self.owned.current(&self.path).unwrap_or(false)
    }
}

pub(super) fn record_button_hint(stage: super::Stage) -> &'static str {
    match stage {
        super::Stage::Countdown => tr!("取消倒计时 · F9", "Cancel countdown · F9"),
        super::Stage::Recording => tr!("停止录制 · F9", "Stop recording · F9"),
        super::Stage::Idle | super::Stage::Preview => tr!("开始录制 · F9", "Start recording · F9"),
    }
}

#[cfg(test)]
#[path = "recording_output_state_tests.rs"]
mod tests;
