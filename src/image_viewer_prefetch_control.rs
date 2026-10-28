//! 邻图预取只使用宿主当前授予的空闲驻留额度，前台工作可立即撤销。
use super::*;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Plan {
    serial: u64,
    thumbnail_epoch: u64,
    rotation_epoch: u64,
    path: PathBuf,
    resident_bytes: u64,
}

impl Loader {
    pub(super) fn revoke_prefetch_locked(mailbox: &mut Mailbox) {
        if let Some(cancel) = mailbox.prefetch_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        mailbox.prefetch_request = None;
    }

    pub(super) fn revoke_prefetch(&self) {
        let mut mailbox = self.shared.0.lock().unwrap();
        Self::revoke_prefetch_locked(&mut mailbox);
        self.shared.1.notify_one();
    }

    pub(super) fn prefetch(&self, grant: prefetch::Grant) {
        let mut mailbox = self.shared.0.lock().unwrap();
        if mailbox.stopped || mailbox.serial != grant.serial {
            return;
        }
        Self::revoke_prefetch_locked(&mut mailbox);
        mailbox.prefetch_epoch = grant.epoch;
        mailbox.prefetch_cancel = Some(grant.cancel.clone());
        mailbox.prefetch_request = Some(grant);
        self.shared.1.notify_one();
    }
}

impl ImageViewer {
    pub(super) fn invalidate_prefetch(&mut self) {
        self.prefetch_plan = None;
        self.loader.revoke_prefetch();
    }

    pub(super) fn foreground_resident_bytes(&self) -> u64 {
        presentation_resident_bytes_for(
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
        )
        .saturating_add(self.pages.as_ref().map_or(0, pages::Info::retained_bytes))
    }

    pub(super) fn refresh_prefetch(&mut self) {
        let eligible = self.image.as_ref().is_some_and(|image| {
            matches!(
                image.metadata.format,
                crate::image_metadata::SourceFormat::Raster(
                    image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Bmp
                )
            )
        });
        let idle =
            eligible
                && self.animation.is_none()
                && self.pages.is_none()
                && self.source_kind.is_none()
                && self.source_file.is_some()
                && self.file_drag_gesture.is_none()
                && self.file_drag_request.is_none()
                && !self.loading
                && !self.snapshot_pending
                && !self.modal_is_open()
                && self.retiring.is_empty()
                && self.retired.is_empty()
                && self.retiring_frames.is_empty()
                && self.retired_frames.is_empty()
                && self.desired_projection.zip(self.projection).is_some_and(
                    |(desired, presented)| desired.rotation() as u8 == presented.rotation() as u8,
                );
        let neighbor = idle
            .then(|| {
                self.collection
                    .as_ref()
                    .and_then(|collection| {
                        collection.neighbor(1).or_else(|| collection.neighbor(-1))
                    })
                    .map(Path::to_path_buf)
            })
            .flatten();
        let Some(path) = neighbor else {
            if self.prefetch_plan.take().is_some() {
                self.loader.revoke_prefetch();
            }
            return;
        };
        let resident_bytes = self.foreground_resident_bytes();
        let plan = Plan {
            serial: self.serial,
            thumbnail_epoch: self.thumbnail_epoch,
            rotation_epoch: self.rotation_epoch,
            path: path.clone(),
            resident_bytes,
        };
        if self.prefetch_plan.as_ref() == Some(&plan) {
            return;
        }
        self.prefetch_epoch = self.prefetch_epoch.wrapping_add(1);
        self.loader.prefetch(prefetch::Grant {
            serial: self.serial,
            epoch: self.prefetch_epoch,
            path,
            resident_bytes,
            max_entry_bytes: prefetch::MAX_ENTRY_BYTES,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.prefetch_plan = Some(plan);
    }
}
