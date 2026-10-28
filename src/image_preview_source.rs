//! 同次冻结输入的预览来源租约；像素采样与完整原图的编辑/导出资格分开。
use crate::{
    image_container_metadata::Summary,
    image_preview::{
        png::{PreviewDecoded, PreviewLimits},
        PreviewGeometry,
    },
    Result,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// 覆盖Arc、自身及小型控制结构的保守预留，不代表分配器或进程RSS。
const CONTROL_SLOT: u64 = 64 * 1024;
const MAX_BYTES: u64 = crate::image_decode::MAX_DECODE_BYTES;

/// 只能从成功完成的同源总览解码取得；不重新读路径，不保存自引用分段视图。
/// 不实现Clone，持有者只克隆Arc，避免复制完整压缩输入。
pub(crate) struct FrozenPreviewSource {
    input: Vec<u8>,
    geometry: PreviewGeometry,
    source: Summary,
}

impl FrozenPreviewSource {
    /// external_resident_bytes排除本次输入及其尚未创建的租约控制槽。
    /// PNG解码工作区先释放，再独立检查输入、预览实际capacity与控制槽同驻阶段。
    pub(crate) fn open(
        bytes: Vec<u8>,
        external_resident_bytes: u64,
        limits: PreviewLimits,
        cancel: &AtomicBool,
    ) -> Result<(Arc<Self>, PreviewDecoded)> {
        check_cancel(cancel)?;
        let result = (|| {
            let input_reserved = bytes.capacity() as u64;
            let decoded = {
                let plan = crate::image_preview::png::inspect(
                    &bytes,
                    input_reserved,
                    external_resident_bytes,
                    limits,
                    cancel,
                )?;
                crate::image_preview::png::decode(plan, cancel)?
            };
            // 此时PNG后端、行和借用分段目录均已销毁。
            check_retained_budget(
                external_resident_bytes,
                input_reserved,
                decoded.image.bgra.capacity() as u64,
            )?;
            check_cancel(cancel)?;
            let source = Arc::new(Self {
                input: bytes,
                geometry: decoded.geometry,
                source: decoded.source,
            });
            Ok((source, decoded))
        })();
        // 取消优先于解析、预算或分配错误；不交付刚完成的过期租约。
        check_cancel(cancel)?;
        result
    }

    pub(crate) fn input(&self) -> &[u8] {
        &self.input
    }

    pub(crate) fn input_reserved_bytes(&self) -> u64 {
        self.input.capacity() as u64
    }

    pub(crate) fn geometry(&self) -> PreviewGeometry {
        self.geometry
    }

    pub(crate) fn source(&self) -> Summary {
        self.source
    }

    pub(crate) fn encoded_bytes(&self) -> u64 {
        self.input.len() as u64
    }

    pub(crate) fn retained_bytes(&self) -> u64 {
        // 私有构造器已在Arc分配前证明C+CONTROL_SLOT<=MAX_BYTES。
        self.input_reserved_bytes() + CONTROL_SLOT
    }
}

/// 来源集合按Arc身份计费，显式排除目标来源；不从调用方的任意总R中减去C。
/// 只返回checked数学和；调用方再与渲染和其它驻留一起核总预算。
/// 双循环不创建HashSet或其它临时堆分配。
pub(crate) fn source_resident_bytes(
    sources: &[&Arc<FrozenPreviewSource>],
    exclude: Option<&Arc<FrozenPreviewSource>>,
) -> Result<u64> {
    let mut total = 0;
    for (index, source) in sources.iter().enumerate() {
        if exclude.is_some_and(|excluded| Arc::ptr_eq(*source, excluded))
            || sources[..index]
                .iter()
                .any(|previous| Arc::ptr_eq(*previous, *source))
        {
            continue;
        }
        total = add(total, source.retained_bytes())?;
    }
    Ok(total)
}

fn check_retained_budget(resident: u64, input: u64, pixels: u64) -> Result<()> {
    let total = add(add(add(resident, input)?, pixels)?, CONTROL_SLOT)?;
    if total > MAX_BYTES {
        Err(budget())
    } else {
        Ok(())
    }
}

fn add(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right).ok_or_else(budget)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(crate::tr!("PNG预览已取消", "PNG preview canceled").into())
    } else {
        Ok(())
    }
}

fn budget() -> Box<dyn std::error::Error> {
    crate::tr!(
        "冻结预览来源超过256MiB预算",
        "Frozen preview source exceeds the 256 MiB budget"
    )
    .into()
}

#[cfg(test)]
#[path = "image_preview_source_tests.rs"]
mod tests;
