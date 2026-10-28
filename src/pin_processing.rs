//! 贴图后台处理：独占缓冲做几何交换，或借不可变原图重填绝对处理结果。
use super::{preview, PinView, MAX_PIN_WINDOW_SIDE};
use crate::{
    capture::Image,
    pin_geometry::GeometryPermutation,
    pin_model::{Adjustments, ProcessingHistory},
    pin_refill::OriginalRefillPlan,
    Result,
};
use gpui::{Context, RenderImage, Window};
use std::sync::Arc;

/// 只保存数字与静态原因，不延长任何旧图、Vec 或保存快照的生命周期。
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(crate) struct ProcessingProbe {
    pub request: u64,
    pub old_cpu_pointer: usize,
    pub new_cpu_pointer: usize,
    pub old_image_id: usize,
    pub new_image_id: usize,
    pub old_byte_length: usize,
    pub new_byte_length: usize,
    pub old_buffer_capacity: Option<usize>,
    pub new_buffer_capacity: Option<usize>,
    pub old_arc_strong_count: usize,
    pub reused: bool,
    pub completed: bool,
    pub failed: bool,
    pub choice: Option<&'static str>,
    pub image_unavailable: bool,
    pub fallback_reason: Option<&'static str>,
    pub unique_reuses: u64,
    pub original_refills: u64,
    pub borrowed_fallbacks: u64,
}

pub(super) struct ProcessingRequest {
    request: u64,
    original_id: usize,
    candidate_id: usize,
    original_size: (u32, u32),
    current_size: (u32, u32),
    presentation_style: Option<crate::selection_finish::SelectionFinishStyle>,
    previous: Adjustments,
    next: Adjustments,
    old_history: ProcessingHistory,
}
impl ProcessingRequest {
    pub(super) fn new(
        view: &PinView,
        previous: Adjustments,
        old_history: ProcessingHistory,
    ) -> Self {
        Self {
            request: view.processing_request,
            original_id: view.original.id.0,
            candidate_id: view.image.id.0,
            original_size: view.original_size,
            current_size: view.image_size,
            presentation_style: view.presentation_style,
            previous,
            next: view.state.adjustments,
            old_history,
        }
    }
    fn current(&self, view: &PinView) -> bool {
        view.processing
            && view.processing_request == self.request
            && view.original.id.0 == self.original_id
            && view.original_size == self.original_size
            && view.image_size == self.current_size
            && view.presentation_style == self.presentation_style
            && view.state.adjustments == self.next
            && view.image.id.0
                == if view.processing_image_unavailable {
                    self.original_id
                } else {
                    self.candidate_id
                }
    }
}

enum OwnedOperation {
    Geometry(GeometryPermutation),
    Refill {
        source: Arc<RenderImage>,
        original_size: (u32, u32),
        plan: Box<OriginalRefillPlan>,
    },
}
impl OwnedOperation {
    fn validate(&self, size: (u32, u32), bytes: &[u8]) -> Result<()> {
        match self {
            Self::Geometry(plan) => plan.validate(size.0, size.1, bytes),
            Self::Refill {
                source,
                original_size,
                plan,
            } => {
                if source.frame_count() != 1 {
                    return Err("贴图原始帧格式错误".into());
                }
                let actual = source.size(0);
                if (u32::from(actual.width), u32::from(actual.height)) != *original_size {
                    return Err("贴图原始帧尺寸不一致".into());
                }
                plan.validate(source.as_bytes(0).ok_or("贴图原始像素帧缺失")?, size, bytes)
            }
        }
    }
    fn refill(&self) -> bool {
        matches!(self, Self::Refill { .. })
    }
}

enum ProcessingInput {
    Owned {
        frame: image::Frame,
        operation: OwnedOperation,
        size: (u32, u32),
    },
    Borrowed {
        source: Arc<RenderImage>,
        size: (u32, u32),
        adjustments: Adjustments,
    },
}
enum ProcessingResult {
    Owned(image::Frame),
    Borrowed(Image),
    FailedOwned(image::Frame, String),
    FailedBorrowed(String),
}
impl ProcessingInput {
    fn run(self) -> ProcessingResult {
        match self {
            Self::Owned {
                mut frame,
                operation,
                size,
            } => {
                if frame.buffer().dimensions() != size {
                    return ProcessingResult::FailedOwned(frame, "贴图处理帧尺寸不一致".into());
                }
                if let Err(error) = operation.validate(size, frame.buffer().as_raw()) {
                    return ProcessingResult::FailedOwned(frame, error.to_string());
                }
                match operation {
                    OwnedOperation::Geometry(plan) => {
                        match plan.apply(size.0, size.1, frame.buffer_mut().as_mut()) {
                            Ok(()) => ProcessingResult::Owned(frame),
                            Err(error) => ProcessingResult::FailedOwned(frame, error.to_string()),
                        }
                    }
                    OwnedOperation::Refill { source, plan, .. } => {
                        let bytes = source.as_bytes(0).expect("原帧已预检");
                        let output_size = plan.output_size();
                        let metadata = (
                            frame.left(),
                            frame.top(),
                            frame.delay(),
                            frame.buffer().color_space(),
                        );
                        // apply 的全部恢复性拒绝检查在首次写入前；之后只移动 Vec 和帧头。
                        if let Err(error) = plan.apply(bytes, size, frame.buffer_mut().as_mut()) {
                            return ProcessingResult::FailedOwned(frame, error.to_string());
                        }
                        let pixels = frame.into_buffer().into_raw();
                        // validate 已核对 4×width×height 的 checked 长度全等；对应锁定版
                        // ImageBuffer::check_image_fits，因此 from_raw 不存在可恢复失败分支。
                        let mut buffer =
                            image::RgbaImage::from_raw(output_size.0, output_size.1, pixels)
                                .expect("重填计划已验证输出帧头与完整缓冲长度");
                        buffer.set_rgb_primaries(metadata.3.primaries);
                        buffer.set_transfer_function(metadata.3.transfer);
                        ProcessingResult::Owned(image::Frame::from_parts(
                            buffer, metadata.0, metadata.1, metadata.2,
                        ))
                    }
                }
            }
            Self::Borrowed {
                source,
                size,
                adjustments,
            } => match source
                .as_bytes(0)
                .ok_or_else(|| "贴图原始像素帧缺失".to_string())
                .and_then(|bytes| {
                    adjustments
                        .apply_bgra(size.0, size.1, bytes)
                        .map_err(|error| error.to_string())
                }) {
                Ok(image) => ProcessingResult::Borrowed(image),
                Err(error) => ProcessingResult::FailedBorrowed(error),
            },
        }
    }
}

enum InputSelection {
    Owned {
        input: ProcessingInput,
        capacity: usize,
        refill: bool,
    },
    Borrowed(&'static str),
}

/// 宿主和核心测试共用实际 Arc/Frame 所有权边界；不保留 current 的第二个强引用。
fn take_unique_input(
    image: &mut Arc<RenderImage>,
    original: &Arc<RenderImage>,
    size: (u32, u32),
    operation: OwnedOperation,
) -> Result<InputSelection> {
    if Arc::ptr_eq(image, original) {
        return Ok(InputSelection::Borrowed("original-image"));
    }
    if image.frame_count() != 1 {
        return Ok(InputSelection::Borrowed("render-image-format"));
    }
    let actual = image.size(0);
    if (u32::from(actual.width), u32::from(actual.height)) != size {
        return Err("贴图当前帧尺寸不一致".into());
    }
    operation.validate(size, image.as_bytes(0).ok_or("贴图当前像素帧缺失")?)?;
    let candidate = std::mem::replace(image, original.clone());
    match Arc::try_unwrap(candidate) {
        Ok(candidate) => match candidate.try_into_single_frame() {
            Ok(frame) => {
                let capacity = frame.buffer().as_raw().capacity();
                let refill = operation.refill();
                Ok(InputSelection::Owned {
                    input: ProcessingInput::Owned {
                        frame,
                        operation,
                        size,
                    },
                    capacity,
                    refill,
                })
            }
            Err(candidate) => {
                *image = Arc::new(candidate);
                Ok(InputSelection::Borrowed("render-image-format"))
            }
        },
        Err(candidate) => {
            *image = candidate;
            Ok(InputSelection::Borrowed("shared-image"))
        }
    }
}

impl PinView {
    pub(crate) fn require_processing_image_available(&self) -> Result<()> {
        if self.processing_image_unavailable {
            Err("贴图图像正在处理，请稍后再试".into())
        } else {
            Ok(())
        }
    }
    pub(crate) fn processing_probe(&self) -> ProcessingProbe {
        ProcessingProbe {
            image_unavailable: self.processing_image_unavailable,
            ..self.processing_probe
        }
    }
    /// 仅本进程 crate 内受控 QA 的对照，不读取环境变量或用户偏好。
    pub(crate) fn force_borrowed_geometry_for_qa(&mut self, force: bool) {
        self.processing_force_borrowed_geometry = force;
    }
    pub(super) fn begin_processing_probe(&mut self) {
        let old_bytes = self.image.as_bytes(0);
        let old_counts = self.processing_probe;
        self.processing_probe = ProcessingProbe {
            request: self.processing_request,
            old_cpu_pointer: old_bytes.map_or(0, |bytes| bytes.as_ptr() as usize),
            old_image_id: self.image.id.0,
            old_byte_length: old_bytes.map_or(0, <[u8]>::len),
            old_arc_strong_count: Arc::strong_count(&self.image),
            unique_reuses: old_counts.unique_reuses,
            original_refills: old_counts.original_refills,
            borrowed_fallbacks: old_counts.borrowed_fallbacks,
            ..Default::default()
        };
    }
    pub(super) fn finish_reset_processing_probe(&mut self) {
        self.processing_probe.choice = Some("reset-original");
        self.processing_probe.fallback_reason = Some("reset-shared-original");
        self.processing_probe.new_image_id = self.image.id.0;
        self.processing_probe.new_cpu_pointer = self
            .image
            .as_bytes(0)
            .map_or(0, |bytes| bytes.as_ptr() as usize);
        self.processing_probe.new_byte_length = self.image.as_bytes(0).map_or(0, <[u8]>::len);
        self.processing_probe.completed = true;
    }
    pub(super) fn start_processing(
        &mut self,
        request: ProcessingRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // defer 之前已 seal 掉 canvas；GPUI 在该 effect 边界释放失去所有者的实体。
        // 延迟到此处只为实际引用退休，不能清除历史或等待/中断后台保存。
        if !request.current(self) {
            return;
        }
        self.begin_processing_probe();
        let input = match self
            .validate_presentation_content_size(
                request
                    .next
                    .dimensions(request.original_size.0, request.original_size.1),
                window,
            )
            .and_then(|_| self.processing_input(&request))
        {
            Ok(input) => input,
            Err(error) => {
                self.state.adjustments = request.previous;
                self.history = request.old_history;
                self.processing_image_unavailable = false;
                self.processing = false;
                self.processing_probe.completed = true;
                self.processing_probe.failed = true;
                self.processing_probe.new_image_id = self.image.id.0;
                self.processing_probe.new_cpu_pointer = self
                    .image
                    .as_bytes(0)
                    .map_or(0, |bytes| bytes.as_ptr() as usize);
                self.processing_probe.new_byte_length =
                    self.image.as_bytes(0).map_or(0, <[u8]>::len);
                self.status = error.to_string();
                if let Err(geometry_error) = self.sync_presentation_geometry(window, cx) {
                    self.status = geometry_error.to_string();
                }
                self.sync_toolbar(window, cx);
                cx.notify();
                return;
            }
        };
        let task = cx.background_executor().spawn(async move { input.run() });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, window, cx| {
                if !request.current(v) {
                    // 宿主关闭/旧请求失效后，result 连同 owned Frame 正常 Drop。
                    return;
                }
                let error = match result {
                    ProcessingResult::Owned(frame) => {
                        v.image_size = frame.buffer().dimensions();
                        v.processing_probe.new_buffer_capacity =
                            Some(frame.buffer().as_raw().capacity());
                        v.image = Arc::new(RenderImage::new(vec![frame]));
                        None
                    }
                    ProcessingResult::Borrowed(image) => {
                        let size = (image.width, image.height);
                        match preview(image) {
                            Ok(image) => {
                                v.image = image;
                                v.image_size = size;
                                None
                            }
                            Err(error) => Some(error.to_string()),
                        }
                    }
                    ProcessingResult::FailedOwned(frame, error) => {
                        v.processing_probe.new_buffer_capacity =
                            Some(frame.buffer().as_raw().capacity());
                        v.image = Arc::new(RenderImage::new(vec![frame]));
                        v.image_size = request.current_size;
                        Some(error)
                    }
                    ProcessingResult::FailedBorrowed(error) => Some(error),
                };
                // 先安装结果/恢复旧帧，再撤销不可用位，输出不能看见占位原图。
                v.processing_image_unavailable = false;
                v.processing = false;
                v.processing_probe.new_image_id = v.image.id.0;
                v.processing_probe.new_cpu_pointer = v
                    .image
                    .as_bytes(0)
                    .map_or(0, |bytes| bytes.as_ptr() as usize);
                v.processing_probe.new_byte_length = v.image.as_bytes(0).map_or(0, <[u8]>::len);
                v.processing_probe.completed = true;
                v.processing_probe.failed = error.is_some();
                if let Some(error) = error {
                    v.state.adjustments = request.previous;
                    v.history = request.old_history;
                    v.status = error;
                } else {
                    v.status.clear();
                }
                if let Err(error) = v.sync_presentation_geometry(window, cx) {
                    v.status = error.to_string();
                }
                if v.chrome {
                    let _ = v.ensure_canvas(window, cx);
                }
                v.sync_toolbar(window, cx);
                cx.notify();
            });
        })
        .detach();
    }
    fn processing_input(&mut self, request: &ProcessingRequest) -> Result<ProcessingInput> {
        let plan =
            GeometryPermutation::between(request.previous, request.next, request.original_size)?;
        let reason = if self.image_size.0 <= MAX_PIN_WINDOW_SIDE
            && self.image_size.1 <= MAX_PIN_WINDOW_SIDE
        {
            "small-image"
        } else if Arc::ptr_eq(&self.image, &self.original) {
            "original-image"
        } else if self.image.frame_count() != 1 {
            "render-image-format"
        } else if self.processing_force_borrowed_geometry {
            "qa-forced-borrowed"
        } else {
            let operation = match plan {
                Some(plan) => OwnedOperation::Geometry(plan),
                None => OwnedOperation::Refill {
                    source: self.original.clone(),
                    original_size: request.original_size,
                    plan: Box::new(OriginalRefillPlan::new(
                        request.original_size,
                        request.previous,
                        request.next,
                    )?),
                },
            };
            match take_unique_input(
                &mut self.image,
                &self.original,
                request.current_size,
                operation,
            )? {
                InputSelection::Owned {
                    input,
                    capacity,
                    refill,
                } => {
                    self.processing_image_unavailable = true;
                    self.processing_probe.choice = Some(if refill {
                        "unique-refill-buffer"
                    } else {
                        "unique-buffer"
                    });
                    self.processing_probe.old_buffer_capacity = Some(capacity);
                    self.processing_probe.reused = true;
                    self.processing_probe.unique_reuses =
                        self.processing_probe.unique_reuses.saturating_add(1);
                    if refill {
                        self.processing_probe.original_refills =
                            self.processing_probe.original_refills.saturating_add(1);
                    }
                    return Ok(input);
                }
                InputSelection::Borrowed(reason) => reason,
            }
        };
        self.processing_probe.fallback_reason = Some(reason);
        self.processing_probe.choice = Some("borrowed-original");
        self.processing_probe.borrowed_fallbacks =
            self.processing_probe.borrowed_fallbacks.saturating_add(1);
        Ok(ProcessingInput::Borrowed {
            source: self.original.clone(),
            size: request.original_size,
            adjustments: request.next,
        })
    }
}

#[cfg(test)]
mod refill_integration_tests {
    use super::*;

    fn original() -> Arc<RenderImage> {
        let bytes: Vec<_> = (0..15_u8).flat_map(|i| [9, 100, 203, i * 17]).collect();
        Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::from_raw(3, 5, bytes).unwrap(),
        )]))
    }
    fn operation(original: &Arc<RenderImage>) -> OwnedOperation {
        OwnedOperation::Refill {
            source: original.clone(),
            original_size: (3, 5),
            plan: Box::new(
                OriginalRefillPlan::new(
                    (3, 5),
                    Adjustments::default(),
                    Adjustments {
                        turns: 1,
                        grayscale: true,
                        brightness: 100,
                        ..Default::default()
                    },
                )
                .unwrap(),
            ),
        }
    }
    fn candidate() -> Arc<RenderImage> {
        let mut bytes = Vec::with_capacity(128);
        bytes.resize(60, 42);
        let mut buffer = image::RgbaImage::from_raw(3, 5, bytes).unwrap();
        buffer.set_rgb_primaries(image::metadata::Cicp::DISPLAY_P3.primaries);
        Arc::new(RenderImage::new(vec![image::Frame::from_parts(
            buffer,
            7,
            8,
            image::Delay::from_numer_denom_ms(11, 2),
        )]))
    }
    #[test]
    fn pin_original_refill_frame_ownership_metadata_and_failure_recovery() {
        let source = original();
        let original_bytes = source.as_bytes(0).unwrap().to_vec();
        let mut original_alias = source.clone();
        assert!(matches!(
            take_unique_input(&mut original_alias, &source, (3, 5), operation(&source)).unwrap(),
            InputSelection::Borrowed("original-image")
        ));
        assert!(Arc::ptr_eq(&original_alias, &source));

        let mut shared = candidate();
        let checkpoint = shared.clone();
        let shared_id = shared.id;
        let shared_pointer = shared.as_bytes(0).unwrap().as_ptr();
        assert!(matches!(
            take_unique_input(&mut shared, &source, (3, 5), operation(&source)).unwrap(),
            InputSelection::Borrowed("shared-image")
        ));
        assert!(Arc::ptr_eq(&shared, &checkpoint));
        assert_eq!(shared.id, shared_id);
        assert_eq!(shared.as_bytes(0).unwrap().as_ptr(), shared_pointer);
        assert!(checkpoint.as_bytes(0).unwrap().iter().all(|b| *b == 42));

        let mut unique = candidate();
        let old_id = unique.id;
        let old_pointer = unique.as_bytes(0).unwrap().as_ptr();
        let old_weak = Arc::downgrade(&unique);
        let InputSelection::Owned {
            input,
            capacity,
            refill,
        } = take_unique_input(&mut unique, &source, (3, 5), operation(&source)).unwrap()
        else {
            panic!("独占处理帧未被消费")
        };
        assert!(refill && capacity >= 128);
        assert!(old_weak.upgrade().is_none());
        assert!(Arc::ptr_eq(&unique, &source));
        let ProcessingResult::Owned(frame) = input.run() else {
            panic!("完整预检后重填失败")
        };
        assert_eq!(frame.buffer().dimensions(), (5, 3));
        assert_eq!(
            (frame.left(), frame.top(), frame.delay()),
            (7, 8, image::Delay::from_numer_denom_ms(11, 2))
        );
        assert_eq!(
            frame.buffer().color_space(),
            image::metadata::Cicp::DISPLAY_P3
        );
        assert_eq!(frame.buffer().as_raw().as_ptr(), old_pointer);
        assert_eq!(frame.buffer().as_raw().len(), 60);
        assert_eq!(frame.buffer().as_raw().capacity(), capacity);
        // 明确的顺时针列排列；颜色饱和到255，仍必须保留每一原像素的alpha。
        for (pixel, index) in frame
            .buffer()
            .as_raw()
            .chunks_exact(4)
            .zip([12, 9, 6, 3, 0, 13, 10, 7, 4, 1, 14, 11, 8, 5, 2])
        {
            assert_eq!(pixel, &[255, 255, 255, index * 17]);
        }
        let result = RenderImage::new(vec![frame]);
        assert_ne!(result.id, old_id);
        assert_eq!(source.as_bytes(0).unwrap(), original_bytes);

        let mut invalid = candidate();
        let invalid_id = invalid.id;
        let invalid_pointer = invalid.as_bytes(0).unwrap().as_ptr();
        assert!(take_unique_input(&mut invalid, &source, (5, 3), operation(&source)).is_err());
        assert_eq!(invalid.id, invalid_id);
        assert_eq!(invalid.as_bytes(0).unwrap().as_ptr(), invalid_pointer);
        let frame = Arc::try_unwrap(invalid)
            .unwrap()
            .try_into_single_frame()
            .unwrap();
        let bytes = frame.buffer().as_raw().clone();
        let before_pointer = frame.buffer().as_raw().as_ptr();
        let before_capacity = frame.buffer().as_raw().capacity();
        let ProcessingResult::FailedOwned(restored, error) = (ProcessingInput::Owned {
            frame,
            operation: operation(&source),
            size: (5, 3),
        })
        .run() else {
            panic!("尺寸错误未携完整旧帧返回")
        };
        assert!(!error.is_empty());
        assert_eq!(restored.buffer().dimensions(), (3, 5));
        assert_eq!(restored.buffer().as_raw(), &bytes);
        assert_eq!(restored.buffer().as_raw().as_ptr(), before_pointer);
        assert_eq!(restored.buffer().as_raw().capacity(), before_capacity);
        assert_eq!(
            (restored.left(), restored.top(), restored.delay()),
            (7, 8, image::Delay::from_numer_denom_ms(11, 2))
        );
        assert_eq!(
            restored.buffer().color_space(),
            image::metadata::Cicp::DISPLAY_P3
        );
        assert_eq!(source.as_bytes(0).unwrap(), original_bytes);
    }
}
