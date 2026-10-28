//! 显式选择设备后的内部摄像头录屏入口；不改变默认配置或自动选择设备。
use super::{camera::session::CameraRecordingRequest, EncodingOptions, Recording};
use crate::{
    geometry::ScreenRect,
    recording_encoder_policy::codec::CodecRequest,
    recording_options::{advanced::ResolvedQualityRequest, RecordingOptions},
    Result,
};
use std::{path::PathBuf, sync::Arc};

impl Recording {
    /// 只有显式设备请求且媒体实验已启用时才允许摄像头；其它编码请求沿原有预检。
    pub(crate) fn start_with_camera_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        encoding: EncodingOptions,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
        camera_request: CameraRecordingRequest,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_capture_requests(
            region,
            options,
            output,
            true,
            encoding,
            quality_request,
            codec_request,
            Some(camera_request),
        )
    }

    /// 倒计时准备沿原信号门；摄像头在 go 后开始，准备失败不自动替换设备。
    pub(crate) fn prepare_with_camera_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        encoding: EncodingOptions,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
        camera_request: CameraRecordingRequest,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_capture_requests(
            region,
            options,
            output,
            false,
            encoding,
            quality_request,
            codec_request,
            Some(camera_request),
        )
    }
}
