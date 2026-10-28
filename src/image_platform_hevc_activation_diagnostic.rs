//! 测试构建只观察activation的有界属性键与类型，不读取属性值或激活解码器。
#![cfg(test)]

use std::sync::atomic::{AtomicBool, Ordering};
use windows::{core::GUID, Win32::Media::MediaFoundation::IMFActivate};

const MAX_ATTRIBUTE_KEYS: u32 = 32;

fn stopped(cancel: &AtomicBool, index: usize, stage: &str) -> bool {
    let cancelled = cancel.load(Ordering::Acquire);
    if cancelled {
        eprintln!("HEVC activation index={index} stage={stage} cancelled=1");
    }
    cancelled
}

struct StoreLock<'a> {
    activation: &'a IMFActivate,
    index: usize,
}

impl Drop for StoreLock<'_> {
    fn drop(&mut self) {
        // 取消也必须归还成功取得的锁；辅助失败只记录原始HRESULT。
        if let Err(error) = unsafe { self.activation.UnlockStore() } {
            eprintln!(
                "HEVC activation index={} stage=UnlockStore HRESULT=0x{:08X}",
                self.index,
                error.code().0 as u32,
            );
        }
    }
}

pub(super) fn inspect(activation: &IMFActivate, index: usize, cancel: &AtomicBool) {
    if stopped(cancel, index, "before LockStore") {
        return;
    }
    let result = unsafe { activation.LockStore() };
    let _lock = match result {
        // 先建立拥有者，再检查调用期间到达的取消，避免提前返回遗留属性锁。
        Ok(()) => StoreLock { activation, index },
        Err(error) => {
            if !stopped(cancel, index, "after LockStore") {
                eprintln!(
                    "HEVC activation index={index} stage=LockStore HRESULT=0x{:08X}",
                    error.code().0 as u32,
                );
            }
            return;
        }
    };
    if stopped(cancel, index, "after LockStore") {
        return;
    }

    let result = unsafe { activation.GetCount() };
    if stopped(cancel, index, "after GetCount") {
        return;
    }
    let count = match result {
        Ok(count) => count,
        Err(error) => {
            eprintln!(
                "HEVC activation index={index} stage=GetCount HRESULT=0x{:08X}",
                error.code().0 as u32,
            );
            return;
        }
    };
    let limit = count.min(MAX_ATTRIBUTE_KEYS);
    eprintln!(
        "HEVC activation index={index} attribute_count={count} inspected={limit} truncated={}",
        u8::from(count > limit),
    );
    for item in 0..limit {
        if stopped(cancel, index, "before GetItemByIndex") {
            return;
        }
        let mut key = GUID::zeroed();
        // 空value指针只取得key，不创建PROPVARIANT，也不读取字符串或blob。
        let result = unsafe { activation.GetItemByIndex(item, &mut key, None) };
        if stopped(cancel, index, "after GetItemByIndex") {
            return;
        }
        if let Err(error) = result {
            eprintln!(
                "HEVC activation index={index} item={item} stage=GetItemByIndex HRESULT=0x{:08X}",
                error.code().0 as u32,
            );
            continue;
        }
        if stopped(cancel, index, "before GetItemType") {
            return;
        }
        let result = unsafe { activation.GetItemType(&key) };
        if stopped(cancel, index, "after GetItemType") {
            return;
        }
        match result {
            Ok(kind) => eprintln!(
                "HEVC activation index={index} item={item} key={key:?} type={}",
                kind.0,
            ),
            Err(error) => eprintln!(
                "HEVC activation index={index} item={item} key={key:?} stage=GetItemType HRESULT=0x{:08X}",
                error.code().0 as u32,
            ),
        }
    }
}
