//! EXR构造、目录过滤和串行解块的分阶段许可，冻结来源只计一次。
use std::sync::atomic::AtomicBool;

use super::{exr_adapter, exr_budget, exr_compression, CodecWorkspace};

const OBSERVER_BYTES: u64 = 256 * 1024;
const CONTROL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Prepared {
    pub workspace: CodecWorkspace,
    pub selection: exr_adapter::Selection,
}

fn sum(parts: &[u64]) -> crate::Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or_else(super::shared_budget_error)
}

fn times(value: u64, multiplier: u64) -> crate::Result<u64> {
    value
        .checked_mul(multiplier)
        .ok_or_else(super::shared_budget_error)
}

fn window(value: exr_budget::Window) -> exr_adapter::Window {
    exr_adapter::Window {
        x: value.x,
        y: value.y,
        width: value.width,
        height: value.height,
    }
}

/// resident包含来源actual capacity及宿主R；观察器只借用同一份冻结输入。
pub(super) fn inspect(bytes: &[u8], resident: u64, cancel: &AtomicBool) -> crate::Result<Prepared> {
    let result = (|| {
        super::check_cancel(cancel)?;
        super::check_shared_budget(&[resident, OBSERVER_BYTES])?;
        let directory = exr_budget::inspect(bytes, resident, cancel)?;
        let selection = directory.selection;
        let thread_bytes = exr_adapter::DECODER_THREAD_BYTES;
        let filtered_initial = times(
            (directory.header_count as u64).saturating_mul(32).min(4096),
            8,
        )?;
        let filtered_retained =
            filtered_initial.max(times(selection.selected_chunk_count as u64, 16)?);
        // 解块时全offset表和tile目录已释放；metadata、两个属性副本及filtered Vec仍在。
        // 后端自有浮点图与caller native共存；这里另计一份，caller仍由外层单计。
        let runtime_base = sum(&[
            times(directory.retained_metadata_bytes, 3)?,
            directory.native_bytes,
            filtered_retained,
            thread_bytes,
            CONTROL_BYTES,
        ])?;
        let filter_extra = sum(&[
            directory.retained_metadata_bytes,
            directory.native_bytes,
            directory.filter_extra_bytes,
            thread_bytes,
        ])?;
        let mut maximum_temporary = 0;
        let mut maximum_scratch = 0;
        let mut maximum_pixel_row = 0;
        directory.for_each_selected_chunk(cancel, |chunk| {
            let workspace = exr_compression::inspect(
                chunk.compression,
                chunk.payload,
                chunk.raw_block_bytes,
                exr_compression::Geometry {
                    width: chunk.dimensions.0,
                    height: chunk.dimensions.1,
                    origin_y: chunk.origin_y,
                },
                directory
                    .channels()
                    .iter()
                    .map(|channel| exr_compression::Channel {
                        name: channel.name,
                        sample_type: channel.sample_type,
                        sampling_x: channel.sampling_x,
                        sampling_y: channel.sampling_y,
                    }),
                cancel,
            )?;
            maximum_temporary = maximum_temporary.max(workspace.temporary_bytes);
            maximum_scratch = maximum_scratch.max(workspace.interleave_scratch_bytes);
            maximum_pixel_row = maximum_pixel_row.max(times(u64::from(chunk.dimensions.0), 16)?);
            // 后端按物理offset排序。T+2S覆盖任意排列中的旧TLS与增长新TLS，
            // 不把逻辑目录遍历顺序误认为实际解码顺序，也不为排序另分配大目录。
            let runtime = sum(&[
                runtime_base,
                maximum_pixel_row,
                maximum_temporary,
                times(maximum_scratch, 2)?,
            ])?;
            super::check_shared_budget(&[resident, directory.native_bytes, runtime])
        })?;
        let runtime_extra = sum(&[
            runtime_base,
            maximum_pixel_row,
            maximum_temporary,
            times(maximum_scratch, 2)?,
        ])?;
        let workspace = CodecWorkspace {
            dimensions: (
                selection.display_window.width,
                selection.display_window.height,
            ),
            native_bytes: directory.native_bytes,
            constructor_bytes: directory.constructor_bytes.max(OBSERVER_BYTES),
            decode_extra_bytes: filter_extra.max(runtime_extra),
            backend_limit_floor: 0,
        };
        super::check_shared_budget(&[resident, workspace.constructor_bytes])?;
        super::check_shared_budget(&[
            resident,
            workspace.native_bytes,
            workspace.decode_extra_bytes,
        ])?;
        Ok(Prepared {
            workspace,
            selection: exr_adapter::Selection {
                part_index: selection.part_index,
                display_window: window(selection.display_window),
                data_window: window(selection.data_window),
                has_alpha: selection.has_alpha,
            },
        })
    })();
    super::check_cancel(cancel)?;
    result
}

#[cfg(test)]
#[path = "image_decode_exr_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "image_decode_exr_matrix_tests.rs"]
mod matrix_tests;
