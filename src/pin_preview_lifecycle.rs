//! 贴图宿主按当前画面退休 GPU 图像键；CPU 像素仍由正文、原图和撤销快照持有。
use gpui::{ImageId, RenderImage, Window};
use std::{collections::HashSet, sync::Arc};

#[derive(Default)]
pub(crate) struct PreviewLifecycle {
    current: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    enqueued: u64,
    released: u64,
}

impl PreviewLifecycle {
    pub(crate) fn render(
        &mut self,
        mut current: Vec<Arc<RenderImage>>,
        nested_retired: Vec<Arc<RenderImage>>,
        window: &mut Window,
    ) -> Option<String> {
        let mut active = HashSet::<ImageId>::new();
        current.retain(|image| active.insert(image.id));
        let previous = std::mem::replace(&mut self.current, current);

        // 同一图像可能被底图/效果或原退休队列共同引用；一次退休只移除同一个键一次。
        // Undo 在这一帧重新使用的图像必须保留，不能因为上一帧排队而提前移除。
        self.retired.retain(|image| !active.contains(&image.id));
        let mut queued: HashSet<_> = self.retired.iter().map(|image| image.id).collect();
        for image in previous.into_iter().chain(nested_retired) {
            if !active.contains(&image.id) && queued.insert(image.id) {
                self.retired.push(image);
                self.enqueued = self.enqueued.saturating_add(1);
            }
        }

        let mut error = None;
        for image in std::mem::take(&mut self.retired) {
            match window.drop_image(image.clone()) {
                Ok(()) => self.released = self.released.saturating_add(1),
                Err(reason) => {
                    error = Some(format!("释放旧贴图预览纹理失败：{reason}"));
                    self.retired.push(image);
                }
            }
        }
        error
    }

    /// 当前候选键、待退休数量、累计入队/成功 drop_image 调用；不直接查询 atlas 驻留。
    pub(crate) fn counts(&self) -> (usize, usize, u64, u64) {
        (
            self.current.len(),
            self.retired.len(),
            self.enqueued,
            self.released,
        )
    }

    /// 仅观察宿主追踪的图像身份，不持有像素，也不代表直接查询 GPU atlas。
    pub(crate) fn current_image_ids(&self) -> Vec<usize> {
        self.current.iter().map(|image| image.id.0).collect()
    }
}
