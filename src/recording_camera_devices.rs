//! 摄像头目录元数据；只供后台工作线程调用，不激活设备或读取摄像头帧。
//! 目录项不证明摄像头权限、当前可用性、格式协商或首帧成功。

use crate::{media_foundation::MfRuntime, Result};
use windows::{
    core::GUID,
    Win32::{Media::MediaFoundation::*, System::Com::CoTaskMemFree},
};

const MAX_DEVICES: u32 = 32;
const MAX_ID_UNITS: usize = 4096;
const MAX_FRIENDLY_UNITS: usize = 512;
const MAX_DIAGNOSTICS: usize = 96;
const MAX_DIAGNOSTIC_CHARS: usize = 512;
const CATALOG_SCOPE: &str = "Video-capture activation metadata only; no device is activated and no frame is read. The first 32 OS-reported entries are inspected, invalid or duplicate IDs are omitted, and count retains the original OS count. Symbolic links retain raw UTF-16 and are opaque device identities; friendly names and list positions are not selection identities. Enumeration does not prove permission, availability, negotiated format or first-frame success. Native enumeration may allocate more than 32 entries; all returned activation references are released before the array is freed. Native calls and that complete cleanup have no hard time or allocation budget.";

/// 不含结尾 NUL 的 opaque symbolic link；不能经 lossy 字符串转换后用于设备路由。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CameraDeviceId {
    units: Vec<u16>,
}

impl CameraDeviceId {
    pub(crate) fn from_utf16(units: Vec<u16>) -> Result<Self> {
        if units.is_empty() {
            return Err("Camera device ID is empty".into());
        }
        if units.len() > MAX_ID_UNITS {
            return Err("Camera device ID exceeds 4096 UTF-16 code units".into());
        }
        if units.contains(&0) {
            return Err("Camera device ID contains a NUL code unit".into());
        }
        Ok(Self { units })
    }

    pub(crate) fn as_units(&self) -> &[u16] {
        &self.units
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CameraDevice {
    pub id: CameraDeviceId,
    /// 仅供显示，可能重复；不会参与设备身份匹配。
    pub friendly_name: Option<String>,
    /// activation 元数据中的硬件源标志；缺失/读取失败保持未知。
    pub hardware_source: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct Catalog {
    pub devices: Vec<CameraDevice>,
    /// MF 返回的原始条目数，包含超过扫描上限、无效和重复的条目。
    pub count: u32,
    /// 仅表示原始条目数超过 32；被拒绝的 ID 另记 diagnostics。
    pub list_truncated: bool,
    pub diagnostics: Vec<String>,
    pub scope: &'static str,
}

impl Catalog {
    fn diagnostic(&mut self, message: String) {
        if self.diagnostics.len() < MAX_DIAGNOSTICS {
            self.diagnostics
                .push(message.chars().take(MAX_DIAGNOSTIC_CHARS).collect());
        }
    }
}

/// 数组内的每个 Option 拥有一个 COM 引用；清理不能只释放已列出的 32 项。
struct DeviceActivations {
    ptr: *mut Option<IMFActivate>,
    count: u32,
}

impl Drop for DeviceActivations {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                for index in 0..self.count as usize {
                    std::ptr::drop_in_place(self.ptr.add(index));
                }
                CoTaskMemFree(Some(self.ptr.cast()));
            }
        }
    }
}

/// 内部初始化当前线程的 COM/MF；返回值只含普通数据，不持有任何 COM 引用。
/// 必须在后台工作线程调用；MFEnumDeviceSources 和全部引用清理不能被硬预算中断。
pub(crate) fn enumerate() -> Result<Catalog> {
    let _runtime = MfRuntime::new()?;
    let mut attributes = None;
    unsafe {
        MFCreateAttributes(&mut attributes, 1)?;
    }
    let attributes = attributes.ok_or("MFCreateAttributes returned no camera attributes")?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
    }
    let mut array = DeviceActivations {
        ptr: std::ptr::null_mut(),
        count: 0,
    };
    unsafe {
        MFEnumDeviceSources(&attributes, &mut array.ptr, &mut array.count)?;
    }
    if array.count > 0 && array.ptr.is_null() {
        return Err("MFEnumDeviceSources returned a nonzero count without an array".into());
    }
    let mut catalog = Catalog {
        devices: Vec::new(),
        count: array.count,
        list_truncated: array.count > MAX_DEVICES,
        diagnostics: Vec::new(),
        scope: CATALOG_SCOPE,
    };
    if catalog.list_truncated {
        catalog.diagnostic(format!(
            "Camera catalog is limited to the first {MAX_DEVICES} of {} OS-reported entries",
            array.count
        ));
    }
    for index in 0..array.count.min(MAX_DEVICES) as usize {
        let activation = unsafe { &*array.ptr.add(index) };
        let Some(activation) = activation.as_ref() else {
            catalog.diagnostic(format!(
                "Camera catalog entry {index} has no activation object"
            ));
            continue;
        };
        let id = match read_string(
            activation,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            MAX_ID_UNITS,
        )
        .and_then(CameraDeviceId::from_utf16)
        {
            Ok(id) => id,
            Err(error) => {
                catalog.diagnostic(format!("Camera catalog entry {index} ID: {error}"));
                continue;
            }
        };
        if catalog.devices.iter().any(|device| device.id == id) {
            catalog.diagnostic(format!(
                "Camera catalog entry {index} has a duplicate device ID"
            ));
            continue;
        }
        let friendly_name = match read_string(
            activation,
            &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
            MAX_FRIENDLY_UNITS,
        ) {
            Ok(units) if units.is_empty() => None,
            // lossy 转换只用于显示名称；设备路由始终使用上面的原始 UTF-16 ID。
            Ok(units) => Some(String::from_utf16_lossy(&units)),
            Err(error) => {
                catalog.diagnostic(format!(
                    "Camera catalog entry {index} friendly name: {error}"
                ));
                None
            }
        };
        let hardware_source = match unsafe {
            activation.GetUINT32(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_HW_SOURCE)
        } {
            Ok(value) => Some(value != 0),
            Err(error) => {
                catalog.diagnostic(format!(
                    "Camera catalog entry {index} hardware-source flag: {error}"
                ));
                None
            }
        };
        catalog.devices.push(CameraDevice {
            id,
            friendly_name,
            hardware_source,
        });
    }
    Ok(catalog)
}

fn read_string(attributes: &IMFAttributes, key: &GUID, max_units: usize) -> Result<Vec<u16>> {
    let declared = unsafe { attributes.GetStringLength(key)? } as usize;
    if declared > max_units {
        return Err(format!("Camera attribute exceeds {max_units} UTF-16 code units").into());
    }
    // 非零哨兵可检测原生调用未写终止符；GetString 的 returned 不包含 NUL。
    let mut buffer = vec![u16::MAX; declared + 1];
    let mut returned = u32::MAX;
    unsafe {
        attributes.GetString(key, &mut buffer, Some(&mut returned))?;
    }
    validate_string_result(declared, returned, &buffer)?;
    buffer.truncate(declared);
    Ok(buffer)
}

fn validate_string_result(declared: usize, returned: u32, buffer: &[u16]) -> Result<()> {
    let expected_buffer = declared
        .checked_add(1)
        .ok_or("Camera attribute string length overflow")?;
    if returned as usize != declared || buffer.len() != expected_buffer {
        return Err("Camera attribute string declared/returned/buffer lengths disagree".into());
    }
    if buffer.last() != Some(&0) {
        return Err("Camera attribute string has no terminating NUL".into());
    }
    if buffer[..declared].contains(&0) {
        return Err("Camera attribute string contains an embedded NUL".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_camera_devices_id_preserves_raw_utf16_and_enforces_bounds() {
        let units = vec![0x0061, 0xd800, 0x0062, 0xdc00];
        let id = CameraDeviceId::from_utf16(units.clone()).unwrap();
        assert_eq!(id.as_units(), units.as_slice());
        assert_eq!(id, CameraDeviceId::from_utf16(units).unwrap());
        assert!(CameraDeviceId::from_utf16(vec![]).is_err());
        assert!(CameraDeviceId::from_utf16(vec![0]).is_err());
        assert!(CameraDeviceId::from_utf16(vec![1, 0, 2]).is_err());
        assert!(CameraDeviceId::from_utf16(vec![1; MAX_ID_UNITS]).is_ok());
        assert!(CameraDeviceId::from_utf16(vec![1; MAX_ID_UNITS + 1]).is_err());
    }

    #[test]
    fn recording_camera_devices_string_result_requires_exact_length_and_terminator() {
        let buffer = [0x0061, 0xd800, 0x0062, 0];
        assert!(validate_string_result(3, 3, &buffer).is_ok());
        assert!(validate_string_result(0, 0, &[0]).is_ok());
        for returned in [0, 2, 4, u32::MAX] {
            assert!(validate_string_result(3, returned, &buffer).is_err());
        }
        assert!(validate_string_result(3, 3, &buffer[..3]).is_err());
        assert!(validate_string_result(3, 3, &[1, 2, 3, 0, 0]).is_err());
        assert!(validate_string_result(3, 3, &[1, 2, 3, u16::MAX]).is_err());
        assert!(validate_string_result(3, 3, &[1, 0, 3, 0]).is_err());
        assert!(validate_string_result(usize::MAX, u32::MAX, &[]).is_err());
    }
}
