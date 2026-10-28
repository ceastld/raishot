//! 调整音量另存的会话与任务意图；路径和值相同不能复活已撤销的请求。
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub(super) struct SaveSnapshot {
    session: Arc<()>,
    source: PathBuf,
    output: PathBuf,
}

#[derive(Clone, Debug)]
pub(super) struct SaveIntent {
    snapshot: SaveSnapshot,
    job: Arc<()>,
    target: PathBuf,
}

#[derive(Default)]
pub(super) struct SaveOwner {
    session: Arc<()>,
    active: Option<SaveIntent>,
}

impl SaveOwner {
    pub(super) fn capture(&self, source: &Path, output: &Path) -> SaveSnapshot {
        SaveSnapshot {
            session: self.session.clone(),
            source: source.into(),
            output: output.into(),
        }
    }

    pub(super) fn snapshot_current(
        &self,
        snapshot: &SaveSnapshot,
        source: Option<&Path>,
        output: Option<&Path>,
    ) -> bool {
        Arc::ptr_eq(&self.session, &snapshot.session)
            && source == Some(snapshot.source.as_path())
            && output == Some(snapshot.output.as_path())
    }

    pub(super) fn begin(&mut self, snapshot: &SaveSnapshot, target: &Path) -> Option<SaveIntent> {
        if !Arc::ptr_eq(&self.session, &snapshot.session) {
            return None;
        }
        let intent = SaveIntent {
            snapshot: snapshot.clone(),
            job: Arc::new(()),
            target: target.into(),
        };
        self.active = Some(intent.clone());
        Some(intent)
    }

    pub(super) fn current(
        &self,
        intent: &SaveIntent,
        source: Option<&Path>,
        output: Option<&Path>,
        target: &Path,
    ) -> bool {
        self.snapshot_current(&intent.snapshot, source, output)
            && self
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.job, &intent.job))
            && target == intent.target
    }

    /// 只消费实际当前任务一次；迟到完成不能清掉后继任务。
    pub(super) fn finish(&mut self, intent: &SaveIntent) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.job, &intent.job))
        {
            self.active = None;
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.active = None;
        self.session = Arc::new(());
    }
}
