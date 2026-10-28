//! 设置页的全局组合录制。录制期间拦截全部物理键盘与鼠标输入：按住的键实时显示，
//! 全部松开时完成；Esc 或前台窗口切换即取消。注入的输入不进入录制，仍交给窗口内的
//! 按键录制适配。监听线程在首次录制时创建，设置页关闭时随之卸载。
use crate::{
    trigger::{Activation, Button},
    Result,
};
use omnitrigger_windows::{Backend, BackendCommand, BackendEvent};
use std::sync::mpsc::Receiver;

pub(crate) enum RecorderEvent {
    /// 当前按住的键（不含开始录制前已按住的键）。
    Progress(Vec<Button>),
    Recorded {
        held: Vec<Button>,
        activation: Activation,
    },
    Cancelled,
    Error(String),
}

pub(crate) struct TriggerRecorder {
    backend: Backend,
    events: Receiver<BackendEvent>,
}

impl TriggerRecorder {
    pub(crate) fn start() -> Result<Self> {
        let (backend, events) = Backend::start()?;
        Ok(Self { backend, events })
    }

    pub(crate) fn begin(&self) -> Result<()> {
        // 丢弃上一轮遗留的事件，避免旧结果被当成本次录制。
        while self.events.try_recv().is_ok() {}
        Ok(self.backend.send(BackendCommand::StartRecording)?)
    }

    pub(crate) fn cancel(&self) {
        let _ = self.backend.send(BackendCommand::CancelRecording);
    }

    pub(crate) fn poll(&self) -> Vec<RecorderEvent> {
        self.events
            .try_iter()
            .filter_map(|event| match event {
                BackendEvent::RecordingProgress(held) => Some(RecorderEvent::Progress(held)),
                BackendEvent::Recorded(spec) => Some(RecorderEvent::Recorded {
                    held: spec.held,
                    activation: spec.activation,
                }),
                BackendEvent::RecordingCancelled => Some(RecorderEvent::Cancelled),
                BackendEvent::Error(message) => Some(RecorderEvent::Error(message)),
                _ => None,
            })
            .collect()
    }
}
