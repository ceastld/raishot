//! 音频端点目录与选择解析；只枚举元数据，不激活音频客户端或开始采音。
//! “默认”在每次解析时查系统；固定 ID 的临时回退不改写用户保存的选择。
use crate::Result;
use serde::{Deserialize, Serialize};
use std::{fmt, marker::PhantomData, rc::Rc};
use windows::{
    core::{Error, Interface, PCWSTR, PWSTR},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{ERROR_NOT_FOUND, E_INVALIDARG, E_POINTER, RPC_E_CHANGED_MODE},
        Media::Audio::{
            eCapture, eConsole, eRender, EDataFlow, IMMDevice, IMMDeviceEnumerator, IMMEndpoint,
            MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            StructuredStorage::PropVariantToStringAlloc, CLSCTX_ALL, COINIT_MULTITHREADED,
            STGM_READ,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndpointKind {
    Render,
    Capture,
}
impl EndpointKind {
    fn flow(self) -> EDataFlow {
        match self {
            Self::Render => eRender,
            Self::Capture => eCapture,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Render => tr!("输出", "output"),
            Self::Capture => tr!("输入", "input"),
        }
    }
}

/// ID 是不透明标识，原样保存；名称相同的端点也不能互相替代。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum EndpointSelection {
    #[default]
    Default,
    Fixed {
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndpointInfo {
    pub kind: EndpointKind,
    pub id: String,
    /// 名称读取失败时使用完整 ID；界面不能仅凭此字段识别设备。
    pub name: String,
    /// 本次成功查询观察到的 eConsole 默认端点；后续系统变化需要重新查询。
    /// 固定端点解析时，默认查询失败另见 ResolvedEndpoint::default_query_error。
    pub is_default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointCheckStage {
    Lookup,
    Direction,
    State,
    Identity,
}
impl EndpointCheckStage {
    fn label(self) -> &'static str {
        match self {
            Self::Lookup => tr!("查找", "lookup"),
            Self::Direction => tr!("方向检查", "direction check"),
            Self::State => tr!("状态检查", "state check"),
            Self::Identity => tr!("标识读取", "identity read"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointFallbackReason {
    InvalidId,
    NotFound,
    WrongKind {
        actual_flow: i32,
    },
    Inactive {
        state: u32,
    },
    Unavailable {
        stage: EndpointCheckStage,
        hresult: i32,
    },
}
impl fmt::Display for EndpointFallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::InvalidId => f.write_str(tr!(
                "设备标识为空或包含空字符",
                "The endpoint ID is empty or contains a null character"
            )),
            Self::NotFound => f.write_str(tr!("设备已不存在", "The endpoint no longer exists")),
            Self::WrongKind { .. } => f.write_str(tr!(
                "设备的输入输出方向不匹配",
                "The endpoint has the wrong audio direction"
            )),
            Self::Inactive { .. } => f.write_str(tr!(
                "设备未连接或未启用",
                "The endpoint is disconnected or disabled"
            )),
            Self::Unavailable { stage, hresult } => {
                let code = hresult as u32;
                f.write_str(&trf!(
                    "设备{}失败（0x{code:08X}）",
                    "Endpoint {} failed (0x{code:08X})",
                    stage.label()
                ))
            }
        }
    }
}

/// device 必须在调用方的 COM apartment 内使用、释放；本函数不替调用方初始化 COM。
pub(crate) struct ResolvedEndpoint {
    pub device: IMMDevice,
    pub info: EndpointInfo,
    pub requested: EndpointSelection,
    pub fallback: Option<EndpointFallbackReason>,
    /// 只影响默认标记的附带查询；不会取消已经验证成功的固定选择。
    pub default_query_error: Option<EndpointFallbackReason>,
}

/// 可以在已初始化 COM 的线程调用；只返回拥有的字符串，所有 COM 对象先于 guard 释放。
pub(crate) fn enumerate_active(kind: EndpointKind) -> Result<Vec<EndpointInfo>> {
    let _com = ComApartment::enter()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let default_id = default_id(&enumerator, kind).map_err(|reason| {
        trf!(
            "无法查询默认音频{}设备：{reason}",
            "Cannot query the default audio {} endpoint: {reason}",
            kind.label()
        )
    })?;
    let collection = unsafe { enumerator.EnumAudioEndpoints(kind.flow(), DEVICE_STATE_ACTIVE) }
        .map_err(|error| {
            trf!(
                "无法枚举音频{}设备：{error}",
                "Cannot enumerate audio {} endpoints: {error}",
                kind.label()
            )
        })?;
    let mut endpoints = Vec::new();
    for index in 0..unsafe { collection.GetCount()? } {
        let device = unsafe { collection.Item(index)? };
        let candidate = match checked_device(device, kind) {
            Ok(candidate) => candidate,
            // 枚举快照之后端点可能已被拔出；不把它作为仍可选择的活动设备。
            Err(EndpointFallbackReason::Inactive { .. }) => continue,
            Err(reason) => {
                return Err(trf!(
                    "无法读取音频{}设备：{reason}",
                    "Cannot read an audio {} endpoint: {reason}",
                    kind.label()
                )
                .into());
            }
        };
        endpoints.push(endpoint_info(&candidate, kind, default_id.as_deref()));
    }
    sort_endpoints(&mut endpoints);
    Ok(endpoints)
}

/// 调用方负责 enumerator 所在的 COM 生命周期；解析不会创建短命 guard 后返回服务接口。
/// 固定端点不可用时临时取当前 eConsole 默认端点，requested 始终保留原选择。
pub(crate) fn resolve_endpoint(
    enumerator: &IMMDeviceEnumerator,
    kind: EndpointKind,
    selection: &EndpointSelection,
) -> Result<ResolvedEndpoint> {
    let selected = choose_endpoint(
        selection,
        |id| {
            let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            let device = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
                .map_err(|error| lookup_failure(error.code().0))?;
            checked_device(device, kind)
        },
        || {
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(kind.flow(), eConsole) }
                .map_err(|error| {
                    trf!(
                        "默认音频{}设备不可用：{error}",
                        "The default audio {} endpoint is unavailable: {error}",
                        kind.label()
                    )
                })?;
            checked_device(device, kind).map_err(|reason| {
                trf!(
                    "默认音频{}设备不可用：{reason}",
                    "The default audio {} endpoint is unavailable: {reason}",
                    kind.label()
                )
                .into()
            })
        },
    )?;
    let (is_default, default_query_error) =
        default_annotation(&selected.endpoint.id, selected.used_default, || {
            default_id(enumerator, kind)
        });
    let mut info = endpoint_info(&selected.endpoint, kind, None);
    info.is_default = is_default;
    Ok(ResolvedEndpoint {
        device: selected.endpoint.device,
        info,
        requested: selected.requested,
        fallback: selected.fallback,
        default_query_error,
    })
}

struct EndpointCandidate {
    device: IMMDevice,
    id: String,
}

fn checked_device(
    device: IMMDevice,
    kind: EndpointKind,
) -> std::result::Result<EndpointCandidate, EndpointFallbackReason> {
    let endpoint: IMMEndpoint = device
        .cast()
        .map_err(|error| unavailable(EndpointCheckStage::Direction, error))?;
    let flow = unsafe { endpoint.GetDataFlow() }
        .map_err(|error| unavailable(EndpointCheckStage::Direction, error))?;
    check_direction(kind, flow)?;
    let state = unsafe { device.GetState() }
        .map_err(|error| unavailable(EndpointCheckStage::State, error))?;
    check_active(state)?;
    let id =
        device_id(&device).map_err(|error| unavailable(EndpointCheckStage::Identity, error))?;
    Ok(EndpointCandidate { device, id })
}

fn check_direction(
    expected: EndpointKind,
    actual: EDataFlow,
) -> std::result::Result<(), EndpointFallbackReason> {
    if expected.flow() == actual {
        Ok(())
    } else {
        Err(EndpointFallbackReason::WrongKind {
            actual_flow: actual.0,
        })
    }
}

fn check_active(state: DEVICE_STATE) -> std::result::Result<(), EndpointFallbackReason> {
    if state.0 & DEVICE_STATE_ACTIVE.0 != 0 {
        Ok(())
    } else {
        Err(EndpointFallbackReason::Inactive { state: state.0 })
    }
}

fn lookup_failure(hresult: i32) -> EndpointFallbackReason {
    if hresult == windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0).0 {
        EndpointFallbackReason::NotFound
    } else {
        EndpointFallbackReason::Unavailable {
            stage: EndpointCheckStage::Lookup,
            hresult,
        }
    }
}

fn unavailable(stage: EndpointCheckStage, error: Error) -> EndpointFallbackReason {
    EndpointFallbackReason::Unavailable {
        stage,
        hresult: error.code().0,
    }
}

/// 没有默认端点不妨碍列出或使用有效的固定端点；其它查询失败保留原始步骤与错误码。
pub(crate) fn default_id(
    enumerator: &IMMDeviceEnumerator,
    kind: EndpointKind,
) -> std::result::Result<Option<String>, EndpointFallbackReason> {
    match unsafe { enumerator.GetDefaultAudioEndpoint(kind.flow(), eConsole) } {
        Ok(device) => device_id(&device)
            .map(Some)
            .map_err(|error| unavailable(EndpointCheckStage::Identity, error)),
        Err(error) if lookup_failure(error.code().0) == EndpointFallbackReason::NotFound => {
            Ok(None)
        }
        Err(error) => Err(lookup_failure(error.code().0)),
    }
}

fn default_annotation(
    selected_id: &str,
    used_default: bool,
    query: impl FnOnce() -> std::result::Result<Option<String>, EndpointFallbackReason>,
) -> (bool, Option<EndpointFallbackReason>) {
    if used_default {
        return (true, None);
    }
    match query() {
        Ok(id) => (id.as_deref() == Some(selected_id), None),
        Err(reason) => (false, Some(reason)),
    }
}

fn endpoint_info(
    candidate: &EndpointCandidate,
    kind: EndpointKind,
    default_id: Option<&str>,
) -> EndpointInfo {
    EndpointInfo {
        kind,
        name: friendly_name(&candidate.device).unwrap_or_else(|| candidate.id.clone()),
        is_default: default_id == Some(candidate.id.as_str()),
        id: candidate.id.clone(),
    }
}

fn sort_endpoints(endpoints: &mut [EndpointInfo]) {
    endpoints.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn device_id(device: &IMMDevice) -> windows::core::Result<String> {
    let id = TaskString(unsafe { device.GetId()? });
    let text = id.read()?;
    if text.is_empty() || text.contains('\0') {
        Err(Error::from_hresult(E_INVALIDARG))
    } else {
        Ok(text)
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    // windows 的 PROPVARIANT 自带 Drop；不再手动 PropVariantClear。
    let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.ok()?;
    let text = TaskString(unsafe { PropVariantToStringAlloc(&value) }.ok()?)
        .read()
        .ok()?;
    (!text.trim().is_empty()).then_some(text)
}

struct TaskString(PWSTR);
impl TaskString {
    fn read(&self) -> windows::core::Result<String> {
        if self.0.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        unsafe { self.0.to_string() }.map_err(|_| Error::from_hresult(E_INVALIDARG))
    }
}
impl Drop for TaskString {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 .0.cast())) };
    }
}

struct ComApartment {
    owned: bool,
    _thread: PhantomData<Rc<()>>,
}
impl ComApartment {
    fn enter() -> Result<Self> {
        let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK 与 S_FALSE 都须平衡；已有 STA 的 changed-mode 不增加计数，也不释放它。
        if status.is_err() && status != RPC_E_CHANGED_MODE {
            return Err(Error::from_hresult(status).into());
        }
        Ok(Self {
            owned: status.is_ok(),
            _thread: PhantomData,
        })
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

#[derive(Debug)]
struct SelectedEndpoint<T> {
    endpoint: T,
    requested: EndpointSelection,
    fallback: Option<EndpointFallbackReason>,
    used_default: bool,
}

/// 选择事务与 COM 分离；纯测试也经过实际的默认/固定回退路径。
fn choose_endpoint<T>(
    selection: &EndpointSelection,
    fixed: impl FnOnce(&str) -> std::result::Result<T, EndpointFallbackReason>,
    default: impl FnOnce() -> Result<T>,
) -> Result<SelectedEndpoint<T>> {
    let (endpoint, fallback, used_default) = match selection {
        EndpointSelection::Default => (default()?, None, true),
        EndpointSelection::Fixed { id } => {
            let found = if id.is_empty() || id.contains('\0') {
                Err(EndpointFallbackReason::InvalidId)
            } else {
                fixed(id)
            };
            match found {
                Ok(endpoint) => (endpoint, None, false),
                Err(reason) => (default()?, Some(reason), true),
            }
        }
    };
    Ok(SelectedEndpoint {
        endpoint,
        requested: selection.clone(),
        fallback,
        used_default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::DEVICE_STATE_UNPLUGGED;

    #[test]
    fn audio_devices_selection_round_trips_without_normalizing_ids() {
        for choice in [
            EndpointSelection::Default,
            EndpointSelection::Fixed {
                id: " {Endpoint-耳机-Aa} ".into(),
            },
        ] {
            let text = serde_json::to_string(&choice).unwrap();
            assert_eq!(
                serde_json::from_str::<EndpointSelection>(&text).unwrap(),
                choice
            );
        }
        assert_eq!(
            serde_json::to_value(EndpointSelection::default()).unwrap(),
            serde_json::json!({"mode": "default"})
        );
    }

    #[test]
    fn audio_devices_valid_fixed_selection_does_not_require_default() {
        let choice = EndpointSelection::Fixed {
            id: "endpoint-fixed".into(),
        };
        let result = choose_endpoint(
            &choice,
            |id| Ok(id.to_owned()),
            || panic!("有效固定端点不查默认回退"),
        )
        .unwrap();
        assert_eq!(result.endpoint, "endpoint-fixed");
        assert_eq!(result.requested, choice);
        assert_eq!(result.fallback, None);
        assert!(!result.used_default);
        assert_eq!(
            default_annotation(&result.endpoint, result.used_default, || Ok(None)),
            (false, None)
        );
        let metadata_failure = EndpointFallbackReason::Unavailable {
            stage: EndpointCheckStage::Identity,
            hresult: E_POINTER.0,
        };
        assert_eq!(
            default_annotation(&result.endpoint, result.used_default, || Err(
                metadata_failure
            )),
            (false, Some(metadata_failure))
        );
        assert_eq!(result.endpoint, "endpoint-fixed");
        assert_eq!(result.fallback, None);
    }

    #[test]
    fn audio_devices_fallback_keeps_saved_choice_and_specific_cause() {
        let choice = EndpointSelection::Fixed {
            id: "saved-endpoint".into(),
        };
        let reasons = [
            lookup_failure(windows::core::HRESULT::from_win32(ERROR_NOT_FOUND.0).0),
            check_direction(EndpointKind::Render, eCapture).unwrap_err(),
            check_active(DEVICE_STATE_UNPLUGGED).unwrap_err(),
            lookup_failure(E_POINTER.0),
        ];
        for reason in reasons {
            let result =
                choose_endpoint(&choice, |_| Err(reason), || Ok("current-default")).unwrap();
            assert_eq!(result.endpoint, "current-default");
            assert_eq!(result.requested, choice);
            assert_eq!(result.fallback, Some(reason));
            assert!(result.used_default);
        }
        assert_eq!(reasons[0], EndpointFallbackReason::NotFound);
        assert_eq!(
            reasons[1],
            EndpointFallbackReason::WrongKind {
                actual_flow: eCapture.0
            }
        );
        assert_eq!(
            reasons[2],
            EndpointFallbackReason::Inactive {
                state: DEVICE_STATE_UNPLUGGED.0
            }
        );
        assert_eq!(
            reasons[3],
            EndpointFallbackReason::Unavailable {
                stage: EndpointCheckStage::Lookup,
                hresult: E_POINTER.0
            }
        );
    }

    #[test]
    fn audio_devices_invalid_ids_never_reach_endpoint_lookup() {
        for id in ["", "first\0second"] {
            let choice = EndpointSelection::Fixed { id: id.into() };
            let result =
                choose_endpoint(&choice, |_| panic!("无效 ID 不能被截断后查询"), || Ok(7)).unwrap();
            assert_eq!(result.endpoint, 7);
            assert_eq!(result.requested, choice);
            assert_eq!(result.fallback, Some(EndpointFallbackReason::InvalidId));
        }
    }

    #[test]
    fn audio_devices_missing_default_is_an_error_for_both_selection_modes() {
        for choice in [
            EndpointSelection::Default,
            EndpointSelection::Fixed {
                id: "unplugged".into(),
            },
        ] {
            let error = choose_endpoint::<()>(
                &choice,
                |_| Err(EndpointFallbackReason::NotFound),
                || Err("没有默认音频输出设备".into()),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "没有默认音频输出设备");
        }
    }

    #[test]
    fn audio_devices_same_names_keep_distinct_complete_ids() {
        let mut entries = vec![
            EndpointInfo {
                kind: EndpointKind::Render,
                id: "{full-device-id-b}".into(),
                name: "同名扬声器".into(),
                is_default: false,
            },
            EndpointInfo {
                kind: EndpointKind::Render,
                id: "{full-device-id-a}".into(),
                name: "同名扬声器".into(),
                is_default: true,
            },
        ];
        sort_endpoints(&mut entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "{full-device-id-a}");
        assert_eq!(entries[1].id, "{full-device-id-b}");
        assert!(entries[0].is_default);
        assert!(!entries[1].is_default);
    }
}
