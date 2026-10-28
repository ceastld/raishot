//! 有界读取原生 H.264 初始化序列头；不观测样本实际激活的参数集，也不保存原始 blob。
use super::h264_header::MAX_HEADER_BYTES;
use crate::recording_encoder_caps::VideoCodec;
use serde_json::{json, Value};
use windows::{
    core::HRESULT,
    Win32::Media::MediaFoundation::{
        IMFMediaType, MF_ATTRIBUTE_BLOB, MF_ATTRIBUTE_TYPE, MF_E_ATTRIBUTENOTFOUND,
        MF_MT_MPEG_SEQUENCE_HEADER,
    },
};

const MAX_DIAGNOSTIC_CHARS: usize = 256;
const SCOPE: &str = "initial MF_MT_MPEG_SEQUENCE_HEADER Annex B parameter-set configuration only; not evidence of PPS references, parameter sets activated by any sample, full-stream conformance, or final color fidelity; missing VUI values remain unknown";
type NativeResult<T> = std::result::Result<T, (HRESULT, String)>;

/// 调用方已核验实际原生 major/subtype；所有 COM 操作同步留在调用线程。
/// 属性或解析失败只生成独立观测，不使调用方的负载统计失败。
pub(super) fn observe(native: &IMFMediaType, codec: VideoCodec) -> Value {
    observe_with(
        codec,
        || unsafe { native.GetItemType(&MF_MT_MPEG_SEQUENCE_HEADER) }.map_err(native_error),
        || unsafe { native.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.map_err(native_error),
        |bytes| {
            let mut returned = u32::MAX;
            unsafe { native.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, bytes, Some(&mut returned)) }
                .map(|()| returned)
                .map_err(native_error)
        },
        |bytes| super::h264_header::inspect_annex_b(bytes).map_err(|error| error.to_string()),
    )
}

fn native_error(error: windows::core::Error) -> (HRESULT, String) {
    (error.code(), error.to_string())
}

fn report(state: &str, stage: Option<&str>, diagnostic: Option<&str>) -> Value {
    json!({
        "state": state,
        "stage": stage,
        "diagnostic": diagnostic.map(|text| text.chars().take(MAX_DIAGNOSTIC_CHARS).collect::<String>()),
        "limit_bytes": MAX_HEADER_BYTES,
        "scope": SCOPE,
    })
}

fn read_failure(stage: &str, (code, message): (HRESULT, String)) -> Value {
    let state = if code == MF_E_ATTRIBUTENOTFOUND {
        "missing"
    } else {
        "read_failed"
    };
    let mut result = report(state, Some(stage), Some(&message));
    result["hresult"] = format!("{:#010x}", code.0 as u32).into();
    result
}

fn observe_with(
    codec: VideoCodec,
    get_type: impl FnOnce() -> NativeResult<MF_ATTRIBUTE_TYPE>,
    get_size: impl FnOnce() -> NativeResult<u32>,
    get_blob: impl FnOnce(&mut [u8]) -> NativeResult<u32>,
    parse: impl FnOnce(&[u8]) -> std::result::Result<Value, String>,
) -> Value {
    if codec != VideoCodec::H264 {
        return report("not_applicable", None, None);
    }
    let kind = match get_type() {
        Ok(kind) => kind,
        Err(error) => return read_failure("GetItemType", error),
    };
    if kind != MF_ATTRIBUTE_BLOB {
        let mut result = report(
            "wrong_type",
            Some("GetItemType"),
            Some("The sequence header attribute is not a BLOB"),
        );
        result["attribute_type"] = kind.0.into();
        return result;
    }
    let declared = match get_size() {
        Ok(size) => size,
        Err(error) => return read_failure("GetBlobSize", error),
    };
    if declared == 0 {
        let mut result = report(
            "invalid_length",
            Some("GetBlobSize"),
            Some("The sequence header is empty"),
        );
        result["declared_bytes"] = declared.into();
        return result;
    }
    let length = match usize::try_from(declared) {
        Ok(length) if length <= MAX_HEADER_BYTES => length,
        _ => {
            let mut result = report(
                "limit",
                Some("GetBlobSize"),
                Some("The sequence header exceeds the bounded observation limit"),
            );
            result["declared_bytes"] = declared.into();
            return result;
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = bytes.try_reserve_exact(length) {
        return report(
            "allocation_failed",
            Some("allocation"),
            Some(&error.to_string()),
        );
    }
    bytes.resize(length, 0);
    let returned = match get_blob(&mut bytes) {
        Ok(returned) => returned,
        Err(error) => {
            let mut result = read_failure("GetBlob", error);
            result["declared_bytes"] = declared.into();
            return result;
        }
    };
    if returned != declared {
        let mut result = report(
            "length_mismatch",
            Some("GetBlob"),
            Some("The returned sequence header length differs from GetBlobSize"),
        );
        result["declared_bytes"] = declared.into();
        result["returned_bytes"] = returned.into();
        return result;
    }
    match parse(&bytes) {
        Ok(parsed) => {
            let mut result = report("observed", Some("inspect_annex_b"), None);
            result["bytes"] = declared.into();
            result["initial_annex_b"] = parsed;
            result
        }
        Err(error) => {
            let mut result = report("parse_failed", Some("inspect_annex_b"), Some(&error));
            result["bytes"] = declared.into();
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use windows::Win32::{Foundation::E_NOTIMPL, Media::MediaFoundation::MF_ATTRIBUTE_UINT32};

    fn model(
        kind: NativeResult<MF_ATTRIBUTE_TYPE>,
        declared: NativeResult<u32>,
        returned: NativeResult<u32>,
        parsed: std::result::Result<Value, String>,
    ) -> (Value, Vec<&'static str>) {
        let calls = RefCell::new(Vec::new());
        let result = observe_with(
            VideoCodec::H264,
            || {
                calls.borrow_mut().push("type");
                kind
            },
            || {
                calls.borrow_mut().push("size");
                declared
            },
            |bytes| {
                calls.borrow_mut().push("blob");
                assert!(!bytes.is_empty() && bytes.len() <= MAX_HEADER_BYTES);
                returned
            },
            |_| {
                calls.borrow_mut().push("parse");
                parsed
            },
        );
        (result, calls.into_inner())
    }

    #[test]
    fn recording_h264_header_observation_non_h264_does_not_read() {
        for codec in [VideoCodec::Hevc, VideoCodec::Av1] {
            let result = observe_with(
                codec,
                || panic!("Non-H.264 must not read the attribute type"),
                || panic!("Non-H.264 must not read the attribute size"),
                |_| panic!("Non-H.264 must not read the attribute bytes"),
                |_| panic!("Non-H.264 must not parse H.264 configuration"),
            );
            assert_eq!(result["state"], "not_applicable");
            assert!(result.get("initial_annex_b").is_none());
        }
    }

    #[test]
    fn recording_h264_header_observation_enforces_quota_before_blob_read() {
        for (length, expected) in [
            (0, "invalid_length"),
            (MAX_HEADER_BYTES as u32 + 1, "limit"),
            (u32::MAX, "limit"),
        ] {
            let (result, calls) =
                model(Ok(MF_ATTRIBUTE_BLOB), Ok(length), Ok(length), Ok(json!({})));
            assert_eq!(result["state"], expected);
            assert_eq!(calls, ["type", "size"]);
        }
        let (result, calls) = model(
            Ok(MF_ATTRIBUTE_BLOB),
            Ok(MAX_HEADER_BYTES as u32),
            Ok(MAX_HEADER_BYTES as u32),
            Ok(json!({"validated": true})),
        );
        assert_eq!(result["state"], "observed");
        assert_eq!(result["bytes"], MAX_HEADER_BYTES);
        assert_eq!(calls, ["type", "size", "blob", "parse"]);
        assert!(result.get("blob").is_none());
    }

    #[test]
    fn recording_h264_header_observation_distinguishes_missing_wrong_type_and_read_errors() {
        let (result, calls) = model(
            Err((MF_E_ATTRIBUTENOTFOUND, "missing".into())),
            Ok(4),
            Ok(4),
            Ok(json!({})),
        );
        assert_eq!(result["state"], "missing");
        assert_eq!(calls, ["type"]);
        let (result, calls) = model(Ok(MF_ATTRIBUTE_UINT32), Ok(4), Ok(4), Ok(json!({})));
        assert_eq!(result["state"], "wrong_type");
        assert_eq!(calls, ["type"]);
        let (result, calls) = model(
            Ok(MF_ATTRIBUTE_BLOB),
            Err((E_NOTIMPL, "错".repeat(300))),
            Ok(4),
            Ok(json!({})),
        );
        assert_eq!(result["state"], "read_failed");
        assert_eq!(result["stage"], "GetBlobSize");
        assert_eq!(result["diagnostic"].as_str().unwrap().chars().count(), 256);
        assert_eq!(calls, ["type", "size"]);
        for (code, expected) in [
            (MF_E_ATTRIBUTENOTFOUND, "missing"),
            (E_NOTIMPL, "read_failed"),
        ] {
            let (result, calls) = model(
                Ok(MF_ATTRIBUTE_BLOB),
                Ok(4),
                Err((code, "read failed".into())),
                Ok(json!({})),
            );
            assert_eq!(result["state"], expected);
            assert_eq!(result["stage"], "GetBlob");
            assert_eq!(calls, ["type", "size", "blob"]);
        }
    }

    #[test]
    fn recording_h264_header_observation_never_parses_mismatched_or_failed_reads() {
        for returned in [0, 3, 5, u32::MAX] {
            let (result, calls) = model(Ok(MF_ATTRIBUTE_BLOB), Ok(4), Ok(returned), Ok(json!({})));
            assert_eq!(result["state"], "length_mismatch");
            assert_eq!(calls, ["type", "size", "blob"]);
            assert!(result.get("initial_annex_b").is_none());
        }
        let (result, calls) = model(
            Ok(MF_ATTRIBUTE_BLOB),
            Ok(4),
            Ok(4),
            Err("invalid SPS".repeat(100)),
        );
        assert_eq!(result["state"], "parse_failed");
        assert_eq!(result["diagnostic"].as_str().unwrap().chars().count(), 256);
        assert_eq!(calls, ["type", "size", "blob", "parse"]);
        assert!(result.get("initial_annex_b").is_none());
    }
}
