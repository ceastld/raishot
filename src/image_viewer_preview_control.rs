//! 局部预览沿查看器唯一工作线程执行，窗口只计划请求并交付同源结果。
use super::*;
use crate::image_preview::source::FrozenPreviewSource;

impl Loader {
    pub(super) fn revoke_preview_region_locked(mailbox: &mut Mailbox) {
        if let Some(cancel) = mailbox.preview_region_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        mailbox.preview_region_request = None;
        mailbox.preview_region_completed = None;
    }

    pub(super) fn revoke_preview_region(&self) {
        let mut mailbox = self.shared.0.lock().unwrap();
        Self::revoke_preview_region_locked(&mut mailbox);
        self.shared.1.notify_one();
    }

    pub(super) fn request_preview_region(&self, request: preview_region::Request) {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.stopped || mailbox.serial != request.serial {
            return;
        }
        Self::revoke_prefetch_locked(&mut mailbox);
        Self::revoke_thumbnails_locked(&mut mailbox);
        Self::revoke_preview_region_locked(&mut mailbox);
        mailbox.preview_region_cancel = Some(request.cancel.clone());
        mailbox.preview_region_request = Some(request);
        self.shared.1.notify_one();
    }

    pub(super) fn take_preview_region(&self) -> Option<preview_region::Completion> {
        let mut mailbox = self.shared.0.lock().unwrap();
        let result = mailbox.preview_region_completed.take();
        if result.is_some() {
            self.shared.1.notify_one();
        }
        result
    }
}

impl ImageViewer {
    pub(super) fn clear_preview_region(&mut self) {
        if self.preview_region.pending()
            || self.preview_region.surface().is_some()
            || self.preview_region.error().is_some()
        {
            self.loader.revoke_preview_region();
            if let Some(image) = self.preview_region.clear() {
                self.retiring.push(image);
            }
        }
    }

    pub(super) fn prepare_preview_region(&mut self, window: &Window) {
        if self.loading || self.modal_is_open() || self.choosing || self.snapshot_pending {
            self.clear_preview_region();
            return;
        }
        let Some((source, geometry, viewport)) = self.image.as_ref().and_then(|image| {
            Some((
                image.preview_source.clone()?,
                image.preview_geometry?,
                self.preview_viewport?,
            ))
        }) else {
            self.clear_preview_region();
            return;
        };
        let source_dimensions = geometry.normalized_source_dimensions();
        let raster_dimensions = geometry.raster_dimensions();
        let density = (f64::from(raster_dimensions.0) / f64::from(source_dimensions.0))
            .min(f64::from(raster_dimensions.1) / f64::from(source_dimensions.1));
        // 总览已经足以覆盖窗口像素时不进行重复行解码。
        if viewport.zoom() <= density {
            self.clear_preview_region();
            return;
        }
        match viewport.visible_region(self.image_area(window), 2) {
            Ok(Some(region)) => {
                match self
                    .preview_region
                    .set_target(self.serial, &source, region, Instant::now())
                {
                    Ok(change) => {
                        if change.changed {
                            self.loader.revoke_preview_region();
                        }
                        if let Some(image) = change.retired {
                            self.retiring.push(image);
                        }
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            Ok(None) => self.clear_preview_region(),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    pub(super) fn poll_preview_region(&mut self, cx: &mut Context<Self>) {
        if let Some(completion) = self.loader.take_preview_region() {
            if let Some(source) = self
                .image
                .as_ref()
                .and_then(|image| image.preview_source.as_ref())
            {
                let (accepted, retired) =
                    self.preview_region.accept(completion, self.serial, source);
                if let Some(image) = retired {
                    self.retiring.push(image);
                }
                if accepted {
                    cx.notify();
                }
            }
        }
        if self.loading || self.modal_is_open() || self.choosing || self.snapshot_pending {
            return;
        }
        let Some(source) = self
            .image
            .as_ref()
            .and_then(|image| image.preview_source.clone())
        else {
            return;
        };
        let resident = self.preview_other_resident(&source);
        if let Some(request) = self.preview_region.due_request(Instant::now(), resident) {
            self.thumbnail_plan = None;
            self.loader.request_preview_region(request);
            cx.notify();
        }
    }

    fn preview_other_resident(&self, source: &Arc<FrozenPreviewSource>) -> u64 {
        // 来源集合显式排除目标Arc；从不对任意总R减去一个猜测的来源容量。
        presentation_resident_bytes_for_excluding_preview_source(
            self.image.as_ref(),
            self.original_tiles
                .iter()
                .map(|(_, image)| image)
                .chain(self.retiring.iter())
                .chain(self.retired.iter())
                .chain(
                    self.preview_region
                        .surface()
                        .into_iter()
                        .map(|surface| &surface.image),
                )
                .chain(
                    self.thumbnails
                        .iter()
                        .filter_map(|slot| match &slot.thumbnail {
                            Some(Thumbnail::Ready(image)) => Some(image),
                            _ => None,
                        }),
                ),
            self.retiring_frames
                .iter()
                .chain(self.retired_frames.iter()),
            Some(source),
        )
        .saturating_add(self.pages.as_ref().map_or(0, pages::Info::retained_bytes))
    }
}
