//! 仅汇总最终 writer 实际输出端口的 JSON 观测，不读取请求、输入或相邻属性补证。
//! 父 getter 还须先确认完整匹配的视频 codec inspection；本文件不代替该资格门。
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use windows::Win32::Media::MediaFoundation::MFMediaType_Video;

const MAX_OUTPUTS: usize = 16;
const COLOR_KEYS: [&str; 5] = [
    "nominal_range",
    "transfer_matrix",
    "transfer_function",
    "primaries",
    "chroma_siting",
];
const MAX_ERROR_CHARS: usize = 256;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoOutputObservation {
    pub size: Option<(u32, u32)>,
    pub fps: Option<(u32, u32)>,
    pub colorimetry: ObservedColorimetry,
}

/// 原始 UINT32 观测；包括 0 在内都不在这里解释为范围、标准、默认值或色准。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ObservedColorimetry {
    pub nominal_range: Option<u32>,
    pub transfer_matrix: Option<u32>,
    pub transfer_function: Option<u32>,
    pub primaries: Option<u32>,
    pub chroma_siting: Option<u32>,
}

pub(crate) fn from_outputs(outputs: Option<&Value>) -> VideoOutputObservation {
    let Some(outputs) = validated_outputs(outputs) else {
        return VideoOutputObservation::default();
    };
    VideoOutputObservation {
        size: consensus(outputs, |output| positive_pair(output.get("size")?)),
        fps: consistent_fps(outputs),
        colorimetry: ObservedColorimetry {
            nominal_range: consensus(outputs, |output| color_value(output, "nominal_range")),
            transfer_matrix: consensus(outputs, |output| color_value(output, "transfer_matrix")),
            transfer_function: consensus(outputs, |output| {
                color_value(output, "transfer_function")
            }),
            primaries: consensus(outputs, |output| color_value(output, "primaries")),
            chroma_siting: consensus(outputs, |output| color_value(output, "chroma_siting")),
        },
    }
}

fn validated_outputs(outputs: Option<&Value>) -> Option<&[Value]> {
    let outputs = outputs?.as_array()?;
    if !(1..=MAX_OUTPUTS).contains(&outputs.len()) {
        return None;
    }
    // 与 MediaTypeInfo 的正式 GUID Debug 字符串一致，不接受 Video 等友好名称。
    let video_major = format!("{:?}", MFMediaType_Video);
    let mut streams = BTreeSet::new();
    for output in outputs {
        let output = output.as_object()?;
        let stream = u32_value(output.get("stream_id")?)?;
        if !streams.insert(stream) || output.get("major_type")?.as_str()? != video_major {
            return None;
        }
    }
    Some(outputs)
}

fn u32_value(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok()
}

fn positive_pair(value: &Value) -> Option<(u32, u32)> {
    let values = value.as_array()?;
    if values.len() != 2 {
        return None;
    }
    let pair = (u32_value(&values[0])?, u32_value(&values[1])?);
    (pair.0 != 0 && pair.1 != 0).then_some(pair)
}

fn consensus<T: Copy + Eq>(outputs: &[Value], read: impl Fn(&Value) -> Option<T>) -> Option<T> {
    let first = read(outputs.first()?)?;
    outputs
        .iter()
        .skip(1)
        .all(|output| read(output) == Some(first))
        .then_some(first)
}

fn consistent_fps(outputs: &[Value]) -> Option<(u32, u32)> {
    let first = positive_pair(outputs.first()?.get("fps")?)?;
    outputs
        .iter()
        .skip(1)
        .all(|output| {
            positive_pair(output.get("fps").unwrap_or(&Value::Null)).is_some_and(|pair| {
                // 两个正 u32 的乘积严格小于 u64::MAX，无需 float 或有损约分。
                u64::from(first.0) * u64::from(pair.1) == u64::from(pair.0) * u64::from(first.1)
            })
        })
        .then_some(first)
}

fn color_value(output: &Value, key: &str) -> Option<u32> {
    let color = output.get("colorimetry")?.as_object()?;
    let errors = validated_errors(color)?;
    if errors.get(key).is_some_and(|error| !error.is_null()) {
        return None;
    }
    u32_value(color.get(key)?)
}

fn validated_errors(color: &Map<String, Value>) -> Option<&Map<String, Value>> {
    let errors = color.get("read_errors")?.as_object()?;
    if errors.len() > COLOR_KEYS.len() {
        return None;
    }
    for (key, value) in errors {
        if !COLOR_KEYS.contains(&key.as_str()) {
            return None;
        }
        if !value.is_null()
            && value.as_str().is_none_or(|error| {
                error.chars().take(MAX_ERROR_CHARS + 1).count() > MAX_ERROR_CHARS
            })
        {
            return None;
        }
    }
    Some(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn port(stream: u32) -> Value {
        json!({"stream_id":stream,"major_type":format!("{:?}",MFMediaType_Video),
            "size":[1920,1080],"fps":[30000,1001],
            "colorimetry":{"nominal_range":0,"transfer_matrix":1,"transfer_function":2,
                "primaries":3,"chroma_siting":4,"read_errors":{}}})
    }

    fn observed(outputs: &Value) -> VideoOutputObservation {
        from_outputs(Some(outputs))
    }

    #[test]
    fn recording_video_observation_requires_bounded_unique_video_ports() {
        let outputs = json!([port(0), port(7)]);
        let observation = observed(&outputs);
        assert_eq!(observation.size, Some((1920, 1080)));
        assert_eq!(observation.fps, Some((30000, 1001)));
        assert_eq!(observation.colorimetry.nominal_range, Some(0));
        let maximum = Value::Array((0..16).map(port).collect());
        assert_eq!(observed(&maximum), observation);
        assert_eq!(from_outputs(None), VideoOutputObservation::default());
        for invalid in [
            Value::Null,
            json!({"outputs":outputs}),
            json!([]),
            json!([port(0), port(0)]),
            Value::Array((0..17).map(port).collect()),
            json!([port(0), []]),
        ] {
            assert_eq!(observed(&invalid), VideoOutputObservation::default());
        }
        for (key, value) in [
            ("stream_id", json!(-1)),
            ("stream_id", json!(u64::from(u32::MAX) + 1)),
            ("stream_id", json!(1.0)),
            ("major_type", json!("Video")),
            ("major_type", Value::Null),
        ] {
            let mut invalid = outputs.clone();
            invalid[1][key] = value;
            assert_eq!(
                observed(&invalid),
                VideoOutputObservation::default(),
                "{key}"
            );
        }
    }

    #[test]
    fn recording_video_observation_fields_are_independent_and_never_borrow_requests() {
        let outputs = json!([port(0), port(1)]);
        for invalid_pair in [
            json!([0, 1080]),
            json!([1920]),
            json!([1920, 1080, 1]),
            json!([1920, -1]),
            json!([1920, 1080.0]),
            json!([1920, u64::from(u32::MAX) + 1]),
            Value::Null,
        ] {
            let mut invalid = outputs.clone();
            invalid[1]["size"] = invalid_pair;
            let observation = observed(&invalid);
            assert_eq!(observation.size, None);
            assert_eq!(observation.fps, Some((30000, 1001)));
            assert_eq!(observation.colorimetry.primaries, Some(3));
        }
        let mut missing = outputs.clone();
        missing[1].as_object_mut().unwrap().remove("size");
        missing[1]["output_request"] = json!({"size":[1920,1080]});
        missing[1]["input"] = json!({"size":[1920,1080]});
        assert_eq!(observed(&missing).size, None);
        missing[0]["size"] = json!([1280, 720]);
        missing[1]["size"] = json!([1920, 1080]);
        assert_eq!(observed(&missing).size, None);
        let mut no_actual_fps = outputs;
        no_actual_fps[1].as_object_mut().unwrap().remove("fps");
        no_actual_fps[1]["frame_rate"] = json!([30000, 1001]);
        no_actual_fps[1]["quality"] = json!({"fps":30});
        assert_eq!(observed(&no_actual_fps).fps, None);
        assert_eq!(observed(&no_actual_fps).size, Some((1920, 1080)));
    }

    #[test]
    fn recording_video_observation_fps_uses_exact_u64_products_and_keeps_first_pair() {
        let mut outputs = json!([port(0), port(1)]);
        outputs[0]["fps"] = json!([60000, 2002]);
        assert_eq!(observed(&outputs).fps, Some((60000, 2002)));
        let max = u32::MAX;
        outputs[0]["fps"] = json!([max, max]);
        outputs[1]["fps"] = json!([max - 1, max - 1]);
        assert_eq!(observed(&outputs).fps, Some((max, max)));
        outputs[0]["fps"] = json!([max, max - 1]);
        outputs[1]["fps"] = json!([max - 1, max - 2]);
        assert_eq!(
            observed(&outputs).fps,
            None,
            "交叉乘积只差 1，float 会丢失该差别"
        );
        for invalid in [
            json!([max, 0]),
            json!([0, max]),
            json!([u64::from(max) + 1, 1]),
            json!([30, 1.0]),
            json!([30]),
            json!(30),
        ] {
            outputs[1]["fps"] = invalid;
            assert_eq!(observed(&outputs).fps, None);
            assert_eq!(observed(&outputs).size, Some((1920, 1080)));
        }
    }

    #[test]
    fn recording_video_observation_color_keeps_zero_and_requires_every_port() {
        let outputs = json!([port(0), port(1)]);
        let baseline = observed(&outputs);
        assert_eq!(
            baseline.colorimetry,
            ObservedColorimetry {
                nominal_range: Some(0),
                transfer_matrix: Some(1),
                transfer_function: Some(2),
                primaries: Some(3),
                chroma_siting: Some(4)
            }
        );
        for invalid in [Value::Null, json!([]), json!("RGB32")] {
            let mut missing = outputs.clone();
            missing[1]["colorimetry"] = invalid;
            assert_eq!(
                observed(&missing).colorimetry,
                ObservedColorimetry::default()
            );
            assert_eq!(observed(&missing).size, baseline.size);
        }
        let mut old_schema = outputs.clone();
        old_schema[1].as_object_mut().unwrap().remove("colorimetry");
        old_schema[1]["output_request"] = outputs[0]["colorimetry"].clone();
        assert_eq!(
            observed(&old_schema).colorimetry,
            ObservedColorimetry::default()
        );
        for value in [
            json!(9),
            json!(-1),
            json!(u64::from(u32::MAX) + 1),
            json!("0"),
            Value::Null,
        ] {
            let mut conflict = outputs.clone();
            conflict[1]["colorimetry"]["nominal_range"] = value;
            let observed = observed(&conflict).colorimetry;
            assert_eq!(observed.nominal_range, None);
            assert_eq!(observed.transfer_matrix, Some(1));
            assert_eq!(observed.primaries, Some(3));
        }
        let mut missing_attribute = outputs;
        missing_attribute[1]["colorimetry"]
            .as_object_mut()
            .unwrap()
            .remove("primaries");
        assert_eq!(observed(&missing_attribute).colorimetry.primaries, None);
        assert_eq!(
            observed(&missing_attribute).colorimetry.chroma_siting,
            Some(4)
        );
    }

    #[test]
    fn recording_video_observation_color_read_errors_are_bounded_and_conservative() {
        let mut outputs = json!([port(0), port(1)]);
        outputs[1]["colorimetry"]["read_errors"] = json!({"nominal_range":"", "primaries":null});
        let observation = observed(&outputs);
        assert_eq!(
            observation.colorimetry.nominal_range, None,
            "空错误串仍是非 null 错误"
        );
        assert_eq!(observation.colorimetry.primaries, Some(3));
        outputs[1]["colorimetry"]["read_errors"] = json!({"transfer_function":"错".repeat(256)});
        assert_eq!(observed(&outputs).colorimetry.transfer_function, None);
        assert_eq!(observed(&outputs).colorimetry.nominal_range, Some(0));
        for invalid_errors in [
            Value::Null,
            json!([]),
            json!({"other":"bad"}),
            json!({"nominal_range":true}),
            json!({"nominal_range":"错".repeat(257)}),
        ] {
            outputs[1]["colorimetry"]["read_errors"] = invalid_errors;
            let observation = observed(&outputs);
            assert_eq!(observation.colorimetry, ObservedColorimetry::default());
            assert_eq!(observation.fps, Some((30000, 1001)));
        }
        outputs[1]["colorimetry"]
            .as_object_mut()
            .unwrap()
            .remove("read_errors");
        assert_eq!(
            observed(&outputs).colorimetry,
            ObservedColorimetry::default()
        );
    }
}
