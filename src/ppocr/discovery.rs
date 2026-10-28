//! ORT 1.24 创建全局环境时做 Windows 设备发现：SetupAPI 枚举显示／处理器／计算加速器
//! 三类设备并读驱动版本，本机实测约 1.3 秒纯 CPU、占 CreateEnv 九成，每个新进程付一次。
//! RustShot 只用 CPU 推理，用不上这份清单：把 onnxruntime.dll 导入表里的
//! `SetupDiGetClassDevsA` 指向返回空集合的桩，ORT 按枚举失败跳过该段，CPU 设备改由 CPUID
//! 得出；DXGI／DXCore 部分照常。只改这一个 DLL 的一项导入，进程内其它模块不受影响。
use std::ffi::c_void;
use windows::Win32::{
    Foundation::{SetLastError, ERROR_NOT_SUPPORTED, HMODULE},
    System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE},
};

const SETUPAPI: &[u8] = b"setupapi.dll";
const CLASS_DEVICES: &[u8] = b"SetupDiGetClassDevsA";
const INVALID_HANDLE_VALUE: isize = -1;

unsafe extern "system" fn empty_device_set(
    _class: *const c_void,
    _enumerator: *const u8,
    _parent: *mut c_void,
    _flags: u32,
) -> isize {
    unsafe { SetLastError(ERROR_NOT_SUPPORTED) };
    INVALID_HANDLE_VALUE
}

/// 已映射 PE 映像的只读视图；每次读取都核对不越过 SizeOfImage。
struct MappedImage {
    base: *const u8,
    size: usize,
}

impl MappedImage {
    /// `module` 必须是当前进程已加载、调用期间不会卸载的 PE32+ 模块。
    unsafe fn new(module: HMODULE) -> Option<Self> {
        let base = module.0 as *const u8;
        if base.is_null() {
            return None;
        }
        let header = Self { base, size: 0x40 };
        if header.u16(0)? != 0x5A4D {
            return None;
        }
        let nt = header.u32(0x3C)? as usize;
        // 先只放开到可选头末尾，读出真实 SizeOfImage 后再放开全部映像。
        let header = Self {
            base,
            size: nt.checked_add(24 + 240)?,
        };
        if header.u32(nt)? != 0x0000_4550 || header.u16(nt + 24)? != 0x20B {
            return None;
        }
        let size = header.u32(nt + 24 + 56)? as usize;
        (size >= header.size).then_some(Self { base, size })
    }

    fn bytes(&self, offset: usize, len: usize) -> Option<&[u8]> {
        (offset.checked_add(len)? <= self.size)
            .then(|| unsafe { std::slice::from_raw_parts(self.base.add(offset), len) })
    }

    fn u16(&self, offset: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(offset, 2)?.try_into().ok()?))
    }

    fn u32(&self, offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(offset, 4)?.try_into().ok()?))
    }

    fn u64(&self, offset: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(offset, 8)?.try_into().ok()?))
    }

    /// 比较以 NUL 结尾的名字；`ignore_case` 用于 DLL 名。
    fn name_is(&self, offset: usize, expected: &[u8], ignore_case: bool) -> bool {
        let Some(name) = self.bytes(offset, expected.len() + 1) else {
            return false;
        };
        name[expected.len()] == 0
            && if ignore_case {
                name[..expected.len()].eq_ignore_ascii_case(expected)
            } else {
                &name[..expected.len()] == expected
            }
    }

    /// 按名导入的 IAT 槽相对偏移；按序号导入或不存在时返回 None。
    fn import_slot(&self, dll: &[u8], function: &[u8]) -> Option<usize> {
        let nt = self.u32(0x3C)? as usize;
        let imports = self.u32(nt + 24 + 120)? as usize;
        if imports == 0 {
            return None;
        }
        for descriptor in (imports..).step_by(20) {
            let names = self.u32(descriptor)? as usize;
            let dll_name = self.u32(descriptor + 12)? as usize;
            let slots = self.u32(descriptor + 16)? as usize;
            if dll_name == 0 {
                return None;
            }
            if names == 0 || !self.name_is(dll_name, dll, true) {
                continue;
            }
            for index in 0.. {
                let thunk = self.u64(names + index * 8)?;
                if thunk == 0 {
                    return None;
                }
                if thunk >> 63 == 0
                    && self.name_is((thunk as usize & 0x7FFF_FFFF) + 2, function, false)
                {
                    return slots.checked_add(index * 8);
                }
            }
        }
        None
    }
}

/// 须在首次创建 ORT 环境之前、由唯一的初始化路径调用；找不到该导入（运行库版本
/// 变化）或改写失败时保持原样并返回 false，ORT 照常做完整设备发现。
pub(super) unsafe fn skip_setupapi_scan(module: HMODULE) -> bool {
    let Some(image) = (unsafe { MappedImage::new(module) }) else {
        return false;
    };
    let Some(offset) = image.import_slot(SETUPAPI, CLASS_DEVICES) else {
        return false;
    };
    if image.bytes(offset, 8).is_none() {
        return false;
    }
    let slot = unsafe { image.base.add(offset) } as *mut usize;
    let mut previous = PAGE_PROTECTION_FLAGS(0);
    if unsafe { VirtualProtect(slot as *const c_void, 8, PAGE_READWRITE, &mut previous) }.is_err() {
        return false;
    }
    unsafe { slot.write_volatile(empty_device_set as usize) };
    let mut ignored = PAGE_PROTECTION_FLAGS(0);
    let _ = unsafe { VirtualProtect(slot as *const c_void, 8, previous, &mut ignored) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::{core::w, Win32::System::LibraryLoader::GetModuleHandleW};

    #[test]
    fn finds_named_imports_of_a_loaded_system_module() {
        // kernelbase 按名导入 ntdll；只读解析，不改写系统模块。
        let module = unsafe { GetModuleHandleW(w!("kernelbase.dll")) }.unwrap();
        let image = unsafe { MappedImage::new(module) }.unwrap();
        assert!(image.import_slot(b"NTDLL.DLL", b"NtClose").is_some());
        assert!(image.import_slot(b"ntdll.dll", b"NoSuchExport").is_none());
        assert!(image.import_slot(SETUPAPI, CLASS_DEVICES).is_none());
    }

    #[test]
    fn stub_reports_an_empty_invalid_device_set() {
        let handle = unsafe {
            empty_device_set(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), 0)
        };
        assert_eq!(handle, INVALID_HANDLE_VALUE);
    }
}
