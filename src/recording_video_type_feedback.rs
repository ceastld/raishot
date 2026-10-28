//! 编码器输出类型的尺寸、帧率和色彩标记；不把类型标记当作实际码流或色准。
use crate::recording::encoding_report::SessionEncodingReport;
use windows::Win32::Media::MediaFoundation::{
    MFNominalRange_0_255, MFNominalRange_16_235, MFNominalRange_48_208, MFNominalRange_64_127,
    MFVideoChromaSubsampling_Cosited, MFVideoChromaSubsampling_DV_PAL,
    MFVideoChromaSubsampling_MPEG1, MFVideoChromaSubsampling_MPEG2, MFVideoPrimaries_ACES,
    MFVideoPrimaries_BT2020, MFVideoPrimaries_BT709, MFVideoPrimaries_DCI_P3,
    MFVideoPrimaries_SMPTE170M, MFVideoPrimaries_XYZ, MFVideoTransFunc_2020,
    MFVideoTransFunc_2020_const, MFVideoTransFunc_2084, MFVideoTransFunc_240M,
    MFVideoTransFunc_709, MFVideoTransFunc_HLG, MFVideoTransFunc_sRGB,
    MFVideoTransferMatrix_BT2020_10, MFVideoTransferMatrix_BT2020_12, MFVideoTransferMatrix_BT601,
    MFVideoTransferMatrix_BT709, MFVideoTransferMatrix_SMPTE240M,
};

pub(super) fn rows(report: &SessionEncodingReport) -> Vec<String> {
    let observed = report.initial_video_output_observation();
    let size = observed
        .size
        .map_or_else(unknown, |(width, height)| format!("{width}×{height}"));
    let fps = observed
        .fps
        .map_or_else(unknown, |(numerator, denominator)| {
            if denominator == 1 {
                format!("{numerator} FPS")
            } else {
                format!("{numerator}/{denominator} FPS")
            }
        });
    let color = observed.colorimetry;
    let range = enum_name(
        color.nominal_range,
        &[
            (MFNominalRange_0_255.0 as u32, "0–255"),
            (MFNominalRange_16_235.0 as u32, "16–235"),
            (MFNominalRange_48_208.0 as u32, "48–208"),
            (MFNominalRange_64_127.0 as u32, "64–127"),
        ],
    );
    let matrix = enum_name(
        color.transfer_matrix,
        &[
            (MFVideoTransferMatrix_BT709.0 as u32, "BT.709"),
            (MFVideoTransferMatrix_BT601.0 as u32, "BT.601"),
            (MFVideoTransferMatrix_SMPTE240M.0 as u32, "SMPTE 240M"),
            (MFVideoTransferMatrix_BT2020_10.0 as u32, "BT.2020-10"),
            (MFVideoTransferMatrix_BT2020_12.0 as u32, "BT.2020-12"),
        ],
    );
    let transfer = enum_name(
        color.transfer_function,
        &[
            (MFVideoTransFunc_709.0 as u32, "BT.709"),
            (MFVideoTransFunc_240M.0 as u32, "SMPTE 240M"),
            (MFVideoTransFunc_sRGB.0 as u32, "sRGB"),
            (MFVideoTransFunc_2020.0 as u32, "BT.2020"),
            (MFVideoTransFunc_2020_const.0 as u32, "BT.2020 constant"),
            (MFVideoTransFunc_2084.0 as u32, "SMPTE ST 2084"),
            (MFVideoTransFunc_HLG.0 as u32, "HLG"),
        ],
    );
    let primaries = enum_name(
        color.primaries,
        &[
            (MFVideoPrimaries_BT709.0 as u32, "BT.709"),
            (MFVideoPrimaries_SMPTE170M.0 as u32, "SMPTE 170M"),
            (MFVideoPrimaries_BT2020.0 as u32, "BT.2020"),
            (MFVideoPrimaries_DCI_P3.0 as u32, "DCI-P3"),
            (MFVideoPrimaries_XYZ.0 as u32, "XYZ"),
            (MFVideoPrimaries_ACES.0 as u32, "ACES"),
        ],
    );
    let chroma = enum_name(
        color.chroma_siting,
        &[
            (MFVideoChromaSubsampling_MPEG1.0 as u32, "MPEG-1"),
            (MFVideoChromaSubsampling_MPEG2.0 as u32, "MPEG-2"),
            (MFVideoChromaSubsampling_DV_PAL.0 as u32, "DV-PAL"),
            (
                MFVideoChromaSubsampling_Cosited.0 as u32,
                tr!("同址", "Cosited"),
            ),
        ],
    );
    vec![
        trf!(
            "初始化输出类型：{size} · {fps}",
            "Initial output type: {size} · {fps}"
        ),
        trf!(
            "初始化输出标记 · 名义范围：{range}；矩阵：{matrix}",
            "Initial output tags · nominal range: {range}; matrix: {matrix}"
        ),
        trf!(
            "初始化输出标记 · 传递函数：{transfer}；原色：{primaries}",
            "Initial output tags · transfer: {transfer}; primaries: {primaries}"
        ),
        trf!(
            "初始化输出标记 · 色度位置：{chroma}",
            "Initial output tags · chroma siting: {chroma}"
        ),
    ]
}

fn unknown() -> String {
    tr!("未知", "Unknown").into()
}

fn enum_name(value: Option<u32>, names: &[(u32, &str)]) -> String {
    match value {
        None | Some(0) => unknown(),
        Some(value) => names.iter().find(|(code, _)| *code == value).map_or_else(
            || trf!("未知（编码 {value}）", "Unknown (code {value})"),
            |(_, name)| (*name).to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recording_encoder_policy::EncoderPolicy,
        recording_options::{Preset, QualityProfile},
    };
    use serde_json::json;
    use windows::Win32::Media::MediaFoundation::{MFMediaType_Video, MFVideoFormat_H264};

    fn report() -> SessionEncodingReport {
        SessionEncodingReport::from_finalized_writer(
            EncoderPolicy::Auto,
            QualityProfile::Current(Preset::CompactMp4),
            None,
            json!({
                "observed_video_codec_matches_request": true,
                "encoding_trial": {"inspection": {"video": {
                    "expected_codec": "h264",
                    "outputs": [{"stream_id": 0,
                        "major_type": format!("{:?}", MFMediaType_Video),
                        "subtype": format!("{:?}", MFVideoFormat_H264),
                        "size": [384, 256], "fps": [30000, 1001],
                        "colorimetry": {"nominal_range": 2, "transfer_matrix": 1,
                            "transfer_function": 5, "primaries": 2, "chroma_siting": 5,
                            "read_errors": {}}}]
                }}}
            }),
        )
    }

    #[test]
    fn recording_video_type_feedback_keeps_observed_geometry_and_rational_rate() {
        let report = report();
        let feedback = rows(&report);
        assert_eq!(feedback.len(), 4);
        assert!(feedback[0].contains("384×256"));
        assert!(feedback[0].contains("30000/1001 FPS"));
        assert!(feedback[1].contains("16–235") && feedback[1].contains("BT.709"));
        assert!(feedback[2].contains("BT.709"));
        assert!(feedback[3].contains("MPEG-2"));
    }

    #[test]
    fn recording_video_type_feedback_does_not_replace_unknowns_with_requests() {
        let mut report = report();
        let output =
            &mut report.finalized_writer["encoding_trial"]["inspection"]["video"]["outputs"][0];
        output["size"] = serde_json::Value::Null;
        output["fps"] = json!([60, 0]);
        output["colorimetry"]["read_errors"]["nominal_range"] = "unavailable".into();
        output["colorimetry"]["transfer_matrix"] = 0.into();
        output["colorimetry"]["primaries"] = 99.into();
        report.finalized_writer["output_request"] = json!({
            "width": 384, "height": 256, "fps": 60, "nominal_range": 1,
            "transfer_matrix": 1, "primaries": 2,
        });
        let feedback = rows(&report);
        assert!(!feedback[0].contains("384×256") && !feedback[0].contains("60 FPS"));
        assert!(!feedback[1].contains("16–235") && !feedback[1].contains("BT.709"));
        assert!(feedback[2].contains("99"));
        assert_eq!(enum_name(Some(0), &[(1, "KNOWN")]), unknown());
    }
}
