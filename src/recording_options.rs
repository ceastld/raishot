//! 录屏传输区选项与纯模型：帧率、预设、开始延迟、音频、输入显示与暂停时间轴。
//! 选项写入独立的 recording.json，仅在用户显式修改时保存；未知字段忽略。
#[path = "recording_quality_request.rs"]
pub(crate) mod advanced;

use crate::{audio_capture::MixLevels, audio_devices::EndpointSelection};
use crate::{config_json::JsonFile, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const FPS_CHOICES: [u32; 5] = [5, 16, 24, 30, 60];
pub(crate) const DEFAULT_FPS: u32 = 30;
pub(crate) const DELAY_CHOICES: [u32; 6] = [0, 1, 2, 3, 5, 10];
/// 与截图静态像素预算一致的单帧上限；更大区域请缩小范围。
pub(crate) const MAX_SIDE: u32 = 7680;
pub(crate) const MAX_HEIGHT: u32 = 4320;
/// 与选区采集的单帧预算一致；边长分别合法也不能使总像素越界。
pub(crate) const MAX_PIXELS: u64 = 32_000_000;
const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Preset {
    HighQualityMp4,
    CompactMp4,
    Gif,
}
impl Preset {
    pub(crate) const ALL: [Preset; 3] = [Preset::HighQualityMp4, Preset::CompactMp4, Preset::Gif];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Preset::HighQualityMp4 => tr!("高清 MP4", "HQ MP4"),
            Preset::CompactMp4 => tr!("压缩 MP4", "Compact MP4"),
            Preset::Gif => "GIF",
        }
    }
    pub(crate) fn quality_label(self) -> &'static str {
        match self {
            Preset::HighQualityMp4 => tr!("高清", "HQ"),
            Preset::CompactMp4 => tr!("压缩", "Compact"),
            Preset::Gif => tr!("标准", "Standard"),
        }
    }
    fn wire(self) -> &'static str {
        match self {
            Preset::HighQualityMp4 => "highQualityMp4",
            Preset::CompactMp4 => "compactMp4",
            Preset::Gif => "gif",
        }
    }
    fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.wire() == value)
    }
    pub(crate) fn is_gif(self) -> bool {
        self == Preset::Gif
    }
    /// 屏幕内容大多静止；按每像素位数估算平均码率，GIF 的中间 MP4 使用高清档。
    fn bits_per_pixel(self) -> f64 {
        match self {
            Preset::HighQualityMp4 | Preset::Gif => 0.16,
            Preset::CompactMp4 => 0.07,
        }
    }
}

/// 按键提示在画面中的位置；最新一行贴边，旧行向画面内侧堆叠。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum KeyPosition {
    TopLeft,
    TopRight,
    #[default]
    BottomLeft,
    BottomCenter,
    BottomRight,
}
impl KeyPosition {
    pub(crate) const ALL: [KeyPosition; 5] = [
        KeyPosition::TopLeft,
        KeyPosition::TopRight,
        KeyPosition::BottomLeft,
        KeyPosition::BottomCenter,
        KeyPosition::BottomRight,
    ];
    pub(crate) fn label(self) -> &'static str {
        match self {
            KeyPosition::TopLeft => tr!("左上", "Top left"),
            KeyPosition::TopRight => tr!("右上", "Top right"),
            KeyPosition::BottomLeft => tr!("左下", "Bottom left"),
            KeyPosition::BottomCenter => tr!("底部居中", "Bottom center"),
            KeyPosition::BottomRight => tr!("右下", "Bottom right"),
        }
    }
    pub(crate) fn top(self) -> bool {
        matches!(self, KeyPosition::TopLeft | KeyPosition::TopRight)
    }
    /// 水平对齐：0 左、0.5 居中、1 右。
    pub(crate) fn align(self) -> f32 {
        match self {
            KeyPosition::TopLeft | KeyPosition::BottomLeft => 0.,
            KeyPosition::BottomCenter => 0.5,
            KeyPosition::TopRight | KeyPosition::BottomRight => 1.,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct RecordingOptions {
    pub fps: u32,
    pub preset: Preset,
    pub delay: u32,
    pub microphone: bool,
    pub system_audio: bool,
    #[serde(default)]
    pub system_output: EndpointSelection,
    #[serde(default)]
    pub microphone_input: EndpointSelection,
    /// 录前设置只应用于成片混音；分轨保留原始 PCM。
    #[serde(default)]
    pub initial_mix: MixLevels,
    pub show_keys: bool,
    pub show_mouse: bool,
    pub filter_typing: bool,
    pub show_cursor: bool,
    pub key_position: KeyPosition,
}
impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            fps: DEFAULT_FPS,
            preset: Preset::CompactMp4,
            delay: 0,
            microphone: false,
            system_audio: false,
            system_output: EndpointSelection::Default,
            microphone_input: EndpointSelection::Default,
            initial_mix: MixLevels::default(),
            show_keys: false,
            show_mouse: false,
            filter_typing: true,
            show_cursor: true,
            key_position: KeyPosition::default(),
        }
    }
}
impl RecordingOptions {
    pub(crate) fn load() -> Self {
        Self::load_at(&config_path())
    }
    fn load_at(path: &Path) -> Self {
        match json_file(path).load() {
            Ok(options) => options.unwrap_or_default().normalized(),
            Err(error) => {
                crate::config_recovery::report_read_failure(path, &error);
                Self::default()
            }
        }
    }
    pub(crate) fn save(&self) -> Result<()> {
        self.save_at(&config_path())
    }
    fn save_at(&self, path: &Path) -> Result<()> {
        Ok(json_file(path).replace(&self.normalized())?)
    }
    /// 手写或旧版本的越界值回到可选档位。
    fn normalized(&self) -> Self {
        Self {
            fps: normalize_fps(self.fps),
            initial_mix: self.initial_mix.normalized(),
            delay: if DELAY_CHOICES.contains(&self.delay) {
                self.delay
            } else {
                0
            },
            ..self.clone()
        }
    }
    /// 旧 recording.ini 解析器，只用于迁移；下个版本删除。
    fn parse_legacy_ini(text: &str) -> Self {
        let mut options = Self::default();
        for line in text.lines().take(64) {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let flag = value == "true";
            match key {
                "fps" => {
                    if let Ok(fps) = value.parse::<u32>() {
                        options.fps = normalize_fps(fps);
                    }
                }
                "preset" => {
                    if let Some(preset) = Preset::from_wire(value) {
                        options.preset = preset;
                    }
                }
                "delay" => {
                    if let Some(delay) = value
                        .parse::<u32>()
                        .ok()
                        .filter(|d| DELAY_CHOICES.contains(d))
                    {
                        options.delay = delay;
                    }
                }
                "microphone" => options.microphone = flag,
                "systemAudio" => options.system_audio = flag,
                "showKeys" => options.show_keys = flag,
                "showMouse" => options.show_mouse = flag,
                "filterTyping" => options.filter_typing = value != "false",
                "showCursor" => options.show_cursor = value != "false",
                _ => {}
            }
        }
        options
    }
    #[cfg(test)]
    fn encode_legacy_ini(&self) -> String {
        format!(
            "version=1\nfps={}\npreset={}\ndelay={}\nmicrophone={}\nsystemAudio={}\nshowKeys={}\nshowMouse={}\nfilterTyping={}\nshowCursor={}\n",
            self.fps,
            self.preset.wire(),
            self.delay,
            self.microphone,
            self.system_audio,
            self.show_keys,
            self.show_mouse,
            self.filter_typing,
            self.show_cursor
        )
    }
    /// GIF 不带音轨；麦克风和系统声只在 MP4 预设下生效。
    pub(crate) fn audio_active(&self) -> bool {
        !self.preset.is_gif() && (self.microphone || self.system_audio)
    }
    pub(crate) fn input_overlay_active(&self) -> bool {
        self.show_keys || self.show_mouse
    }
}

fn config_path() -> PathBuf {
    crate::preferences::data_dir().join("recording.json")
}

fn json_file(path: &Path) -> JsonFile<'_, RecordingOptions> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: RecordingOptions::parse_legacy_ini,
    }
}

/// 旧值或手写值吸附到最近的支持档位，等距时取较高档；0 回到默认 30。
pub(crate) fn normalize_fps(fps: u32) -> u32 {
    if fps == 0 {
        return DEFAULT_FPS;
    }
    FPS_CHOICES
        .into_iter()
        .min_by_key(|choice| (choice.abs_diff(fps), u32::MAX - *choice))
        .unwrap_or(DEFAULT_FPS)
}

/// H.264 4:2:0 要求偶数宽高；奇数边向下取偶，最小 2。
pub(crate) fn encoded_size(width: u32, height: u32) -> (u32, u32) {
    ((width & !1).max(2), (height & !1).max(2))
}

pub(crate) fn validate_region(width: u32, height: u32) -> Result<()> {
    if width < 2 || height < 2 {
        return Err(tr!(
            "录屏区域太小，请至少选择 2×2 像素",
            "The recording area is too small; select at least 2×2 pixels"
        )
        .into());
    }
    if width > MAX_SIDE || height > MAX_HEIGHT {
        return Err(trf!(
            "录屏暂支持最大 {MAX_SIDE}×{MAX_HEIGHT} 区域，请缩小范围",
            "Recording supports areas up to {MAX_SIDE}×{MAX_HEIGHT}; select a smaller area"
        )
        .into());
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(tr!(
            "录屏区域超过 3200 万像素，请缩小范围",
            "Recording area exceeds 32 megapixels; select a smaller area"
        )
        .into());
    }
    Ok(())
}

pub(crate) fn video_bitrate(width: u32, height: u32, fps: u32, preset: Preset) -> u32 {
    let (width, height) = encoded_size(width, height);
    let bits =
        f64::from(width) * f64::from(height) * f64::from(fps.max(1)) * preset.bits_per_pixel();
    bits.clamp(1_000_000., 80_000_000.) as u32
}

/// 实验目标不写入 recording.json；必须完成同源成片比较后才能替换正式预设。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QualityProfile {
    Current(Preset),
    TextDetail,
    Scrolling,
    Motion,
    SmallFile,
}
impl QualityProfile {
    pub(crate) const EXPERIMENTS: [Self; 4] = [
        Self::TextDetail,
        Self::Scrolling,
        Self::Motion,
        Self::SmallFile,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Current(Preset::HighQualityMp4) => "current_hq",
            Self::Current(Preset::CompactMp4) => "current_compact",
            Self::Current(Preset::Gif) => "current_gif_intermediate",
            Self::TextDetail => "text_detail",
            Self::Scrolling => "scrolling",
            Self::Motion => "motion",
            Self::SmallFile => "small_file",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Current(preset) => preset.label(),
            Self::TextDetail => tr!("文字细节（实验）", "Text detail (test)"),
            Self::Scrolling => tr!("滚动（实验）", "Scrolling (test)"),
            Self::Motion => tr!("运动（实验）", "Motion (test)"),
            Self::SmallFile => tr!("小体积（实验）", "Small file (test)"),
        }
    }

    pub(crate) fn explanation(self) -> &'static str {
        match self {
            Self::Current(_) => tr!(
                "保留当前码率与编码参数；实际控制模式以编码器回读为准",
                "Keep current settings; read back the actual encoder mode"
            ),
            Self::TextDetail => tr!(
                "候选：0.16 bit/像素，峰值 VBR，质量优先与两秒缓冲；需比较细字和编码耗时",
                "Candidate: 0.16 bits/pixel, peak VBR, quality priority and a 2s buffer; compare text and encode time"
            ),
            Self::Scrolling => tr!(
                "候选：0.12 bit/像素，峰值 VBR 与两秒缓冲；需比较滚动文字和场景切换",
                "Candidate: 0.12 bits/pixel, peak VBR and a 2s buffer; compare scrolling and scene changes"
            ),
            Self::Motion => tr!(
                "候选：0.20 bit/像素，峰值 VBR；需比较运动细节、吞吐和体积",
                "Candidate: 0.20 bits/pixel and peak VBR; compare motion, throughput and file size"
            ),
            Self::SmallFile => tr!(
                "候选：0.07 bit/像素，峰值 VBR 与两秒缓冲；需比较同体积文字清晰度",
                "Candidate: 0.07 bits/pixel, peak VBR and a 2s buffer; compare text at similar file sizes"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RateControl {
    #[default]
    EncoderDefault,
    PeakConstrainedVbr,
}

/// GOP 最大 picture 数量的请求；不是精确关键帧间隔，也不保证驱动采用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "frames", rename_all = "snake_case")]
pub(crate) enum GopRequest {
    /// 保留当前路线的产品默认；只有调用方启用默认调参时才请求四秒 GOP。
    #[default]
    ProductDefault,
    /// 撤回 GOP 请求：不 Set 此属性，也不能再补产品默认值或暗中 Set(0)。
    EncoderDefault,
    /// 显式最大 picture 数量，须为正且不超过本次帧率下的 60 秒实验预算。
    Frames(u32),
}

impl GopRequest {
    /// 仅解析下发请求；60 秒是项目实验上限，不是驱动能力或实际 GOP 长度。
    /// fps 的支持档位仍由 quality_plan 校验；未请求时不启用产品默认调参。
    pub(crate) fn requested_frames(
        self,
        fps: u32,
        product_default_enabled: bool,
    ) -> Result<Option<u32>> {
        match self {
            Self::ProductDefault if product_default_enabled => {
                let frames = fps.checked_mul(4).filter(|&frames| frames != 0).ok_or(tr!(
                    "产品默认 GOP 帧数超出有效范围",
                    "The default GOP frame count is outside the valid range"
                ))?;
                Ok(Some(frames))
            }
            Self::ProductDefault | Self::EncoderDefault => Ok(None),
            Self::Frames(frames) => {
                let maximum = fps.checked_mul(60).ok_or(tr!(
                    "GOP 实验请求预算超出数值范围",
                    "The GOP experiment budget exceeds the numeric range"
                ))?;
                if frames == 0 || frames > maximum {
                    return Err(tr!(
                        "GOP 最大帧数须为正且不超过当前帧率的 60 秒实验预算，未自动调整",
                        "The maximum GOP frame count must be positive and within the 60-second experiment budget; it was not adjusted"
                    )
                    .into());
                }
                Ok(Some(frames))
            }
        }
    }
}

/// 仅描述请求，不代表驱动已经接受或实际采用；生效情况来自 writer 回读。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct QualityParameters {
    pub rate_control: RateControl,
    pub max_bitrate_bps: Option<u32>,
    pub quality_vs_speed: Option<u32>,
    pub low_latency: Option<bool>,
    /// 逻辑单位为 bit；下发前精确转换为 byte，Some 值必须按 8 bit 对齐。
    pub buffer_bits: Option<u32>,
    pub gop_request: GopRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct QualityPlan {
    pub profile: QualityProfile,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub parameters: QualityParameters,
    pub experimental: bool,
}

/// 使用传入帧率，不替用户归一化配置；裁偶数边与正式录屏共用规则。
pub(crate) fn quality_plan(
    width: u32,
    height: u32,
    fps: u32,
    profile: QualityProfile,
) -> Result<QualityPlan> {
    validate_region(width, height)?;
    if !FPS_CHOICES.contains(&fps) {
        return Err(tr!(
            "请选择支持的录屏帧率",
            "Choose a supported recording frame rate"
        )
        .into());
    }
    let (width, height) = encoded_size(width, height);
    let (bitrate_bps, parameters, experimental) = if let QualityProfile::Current(preset) = profile {
        (
            video_bitrate(width, height, fps, preset),
            QualityParameters::default(),
            false,
        )
    } else {
        let bpp = match profile {
            QualityProfile::TextDetail => 0.16,
            QualityProfile::Scrolling => 0.12,
            QualityProfile::Motion => 0.20,
            QualityProfile::SmallFile => 0.07,
            QualityProfile::Current(_) => unreachable!(),
        };
        let bitrate = (f64::from(width) * f64::from(height) * f64::from(fps) * bpp)
            .clamp(1_000_000., 80_000_000.) as u32;
        let buffered = profile != QualityProfile::Motion;
        (
            bitrate,
            QualityParameters {
                rate_control: RateControl::PeakConstrainedVbr,
                max_bitrate_bps: Some(bitrate.saturating_mul(2)),
                quality_vs_speed: (profile == QualityProfile::TextDetail).then_some(100),
                low_latency: buffered.then_some(false),
                // 两秒缓冲向上对齐到整 byte，仍以 bit 报告；误差小于一 byte。
                // bitrate 已封顶 80 Mbps，因此这里的乘法不会溢出。
                buffer_bits: buffered.then_some((bitrate * 2).div_ceil(8) * 8),
                gop_request: GopRequest::ProductDefault,
            },
            true,
        )
    };
    Ok(QualityPlan {
        profile,
        width,
        height,
        fps,
        bitrate_bps,
        parameters,
        experimental,
    })
}

/// 计时显示：1 小时内 m:ss，超过后 h:mm:ss；负数（倒计时）按秒向上取整。
pub(crate) fn format_clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let value = bytes as f64;
    if value >= KB * KB * KB {
        format!("{:.2} GB", value / (KB * KB * KB))
    } else if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 暂停期间不推进成片时间；成片时长等于实际录制时长。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Timeline {
    started: Option<Instant>,
    paused_at: Option<Instant>,
    paused_total: Duration,
}
impl Timeline {
    pub(crate) fn start(&mut self, now: Instant) {
        *self = Self {
            started: Some(now),
            ..Self::default()
        };
    }
    pub(crate) fn paused(&self) -> bool {
        self.paused_at.is_some()
    }
    pub(crate) fn pause(&mut self, now: Instant) {
        if self.started.is_some() && self.paused_at.is_none() {
            self.paused_at = Some(now);
        }
    }
    pub(crate) fn resume(&mut self, now: Instant) {
        if let Some(paused) = self.paused_at.take() {
            self.paused_total += now.saturating_duration_since(paused);
        }
    }
    pub(crate) fn active(&self, now: Instant) -> Duration {
        let Some(started) = self.started else {
            return Duration::ZERO;
        };
        let end = self.paused_at.unwrap_or(now);
        end.saturating_duration_since(started)
            .saturating_sub(self.paused_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_options_round_trip_and_reject_unknown_values() {
        let options = RecordingOptions {
            fps: 60,
            preset: Preset::Gif,
            delay: 3,
            microphone: true,
            system_audio: true,
            show_keys: true,
            show_mouse: false,
            filter_typing: false,
            show_cursor: false,
            ..RecordingOptions::default()
        };
        assert_eq!(
            RecordingOptions::parse_legacy_ini(&options.encode_legacy_ini()),
            options
        );
        let fallback =
            RecordingOptions::parse_legacy_ini("fps=abc\npreset=avi\ndelay=4\nunknown=1\n");
        assert_eq!(fallback, RecordingOptions::default());
        assert!(!options.audio_active(), "GIF 不带音轨");
    }

    fn sample() -> RecordingOptions {
        RecordingOptions {
            fps: 60,
            preset: Preset::Gif,
            delay: 3,
            microphone: true,
            system_audio: true,
            show_keys: true,
            show_mouse: false,
            filter_typing: false,
            show_cursor: false,
            ..RecordingOptions::default()
        }
    }

    #[test]
    fn recording_options_json_round_trip_and_invalid_values() {
        let dir = crate::config_json::TestDir::new("recording");
        let path = dir.join("recording.json");
        assert_eq!(
            RecordingOptions::load_at(&path),
            RecordingOptions::default()
        );
        sample().save_at(&path).unwrap();
        assert_eq!(RecordingOptions::load_at(&path), sample());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schema_version\": 1") && text.contains("\"preset\": \"gif\""));
        let top_right = RecordingOptions {
            key_position: KeyPosition::TopRight,
            ..sample()
        };
        top_right.save_at(&path).unwrap();
        assert_eq!(RecordingOptions::load_at(&path), top_right);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"key_position\": \"topRight\""));
        std::fs::write(
            &path,
            r#"{"schema_version":1,"fps":25,"preset":"avi","delay":4,"microphone":true,"key_position":"middle"}"#,
        )
        .unwrap();
        let loaded = RecordingOptions::load_at(&path);
        assert_eq!(
            (loaded.fps, loaded.preset, loaded.delay, loaded.key_position),
            (24, Preset::CompactMp4, 0, KeyPosition::BottomLeft)
        );
        assert!(loaded.microphone && loaded.filter_typing);
    }

    #[test]
    fn recording_options_migrate_legacy_ini_to_json() {
        let dir = crate::config_json::TestDir::new("recording");
        let path = dir.join("recording.json");
        std::fs::write(dir.join("recording.ini"), sample().encode_legacy_ini()).unwrap();
        assert_eq!(RecordingOptions::load_at(&path), sample());
        assert!(path.exists() && dir.join("recording.ini.bak").exists());
        assert!(!dir.join("recording.ini").exists());
        assert_eq!(RecordingOptions::load_at(&path), sample());
    }

    #[test]
    fn recording_options_audio_device_fields_default_and_preserve_fixed_ids() {
        let old: RecordingOptions =
            serde_json::from_str(r#"{"microphone":true,"fps":30}"#).unwrap();
        assert!(old.microphone);
        assert_eq!(old.system_output, EndpointSelection::Default);
        assert_eq!(old.microphone_input, EndpointSelection::Default);
        let mut options = old;
        options.system_output = EndpointSelection::Fixed {
            id: "output-A".into(),
        };
        options.microphone_input = EndpointSelection::Fixed {
            id: "input-B".into(),
        };
        let text = serde_json::to_string(&options).unwrap();
        assert_eq!(
            serde_json::from_str::<RecordingOptions>(&text).unwrap(),
            options
        );
    }

    #[test]
    fn recording_options_initial_mix_defaults_mute_and_bounds_survive_round_trip() {
        let old: RecordingOptions = serde_json::from_str(r#"{"microphone":true}"#).unwrap();
        assert_eq!(old.initial_mix, MixLevels::default());
        let partial: RecordingOptions =
            serde_json::from_str(r#"{"initial_mix":{"system":0}}"#).unwrap();
        assert_eq!(
            partial.initial_mix,
            MixLevels {
                system: 0,
                microphone: 200
            }
        );
        let mut options = old;
        options.initial_mix = MixLevels {
            system: 0,
            microphone: u32::MAX,
        };
        let normalized = options.normalized();
        assert_eq!(
            normalized.initial_mix,
            MixLevels {
                system: 0,
                microphone: 400
            }
        );
        let text = serde_json::to_string(&normalized).unwrap();
        assert_eq!(
            serde_json::from_str::<RecordingOptions>(&text).unwrap(),
            normalized
        );
    }

    #[test]
    fn recording_fps_snaps_to_supported_choices() {
        assert_eq!(normalize_fps(0), 30);
        assert_eq!(normalize_fps(15), 16);
        assert_eq!(normalize_fps(10), 5);
        assert_eq!(normalize_fps(27), 30);
        assert_eq!(normalize_fps(240), 60);
        assert_eq!(RecordingOptions::parse_legacy_ini("fps=25").fps, 24);
    }

    #[test]
    fn recording_size_bitrate_and_labels() {
        assert_eq!(encoded_size(321, 181), (320, 180));
        assert_eq!(encoded_size(1, 1), (2, 2));
        assert!(validate_region(1, 10).is_err());
        assert!(validate_region(7681, 10).is_err());
        assert!(validate_region(MAX_SIDE, MAX_HEIGHT).is_err());
        assert!(validate_region(MAX_SIDE, 4166).is_ok());
        assert!(validate_region(1920, 1080).is_ok());
        let high = video_bitrate(1920, 1080, 30, Preset::HighQualityMp4);
        let compact = video_bitrate(1920, 1080, 30, Preset::CompactMp4);
        assert!(high > compact && compact >= 1_000_000);
        assert_eq!(video_bitrate(10, 10, 5, Preset::CompactMp4), 1_000_000);
        assert_eq!(format_clock(Duration::from_millis(59_999)), "0:59");
        assert_eq!(format_clock(Duration::from_secs(3_725)), "1:02:05");
        assert_eq!(format_bytes(2_200_000), "2.1 MB");
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn recording_quality_plan_keeps_current_presets_and_defaults() {
        for preset in Preset::ALL {
            let plan = quality_plan(1921, 1081, 30, QualityProfile::Current(preset)).unwrap();
            assert_eq!((plan.width, plan.height), (1920, 1080));
            assert_eq!(plan.bitrate_bps, video_bitrate(1921, 1081, 30, preset));
            assert_eq!(plan.parameters, QualityParameters::default());
            assert!(!plan.experimental);
        }
        assert_eq!(RecordingOptions::default().preset, Preset::CompactMp4);
    }

    #[test]
    fn recording_gop_request_keeps_product_default_withdrawal_and_frames_distinct() {
        assert_eq!(GopRequest::default(), GopRequest::ProductDefault);
        for fps in FPS_CHOICES {
            assert_eq!(
                GopRequest::ProductDefault
                    .requested_frames(fps, true)
                    .unwrap(),
                Some(fps * 4)
            );
            assert_eq!(
                GopRequest::ProductDefault
                    .requested_frames(fps, false)
                    .unwrap(),
                None
            );
            for enabled in [false, true] {
                assert_eq!(
                    GopRequest::EncoderDefault
                        .requested_frames(fps, enabled)
                        .unwrap(),
                    None
                );
                for frames in [1, fps * 60] {
                    assert_eq!(
                        GopRequest::Frames(frames)
                            .requested_frames(fps, enabled)
                            .unwrap(),
                        Some(frames)
                    );
                }
            }
        }
        assert_eq!(
            serde_json::to_value(GopRequest::EncoderDefault).unwrap()["kind"],
            "encoder_default"
        );
        assert_eq!(
            serde_json::to_value(GopRequest::Frames(120)).unwrap()["frames"],
            120
        );
    }

    #[test]
    fn recording_gop_request_rejects_zero_over_budget_and_arithmetic_overflow() {
        for fps in FPS_CHOICES {
            for frames in [0, fps * 60 + 1, u32::MAX] {
                assert!(GopRequest::Frames(frames)
                    .requested_frames(fps, true)
                    .is_err());
            }
        }
        assert!(GopRequest::Frames(1).requested_frames(0, true).is_err());
        assert!(GopRequest::Frames(1)
            .requested_frames(u32::MAX, false)
            .is_err());
        assert!(GopRequest::ProductDefault
            .requested_frames(0, true)
            .is_err());
        assert!(GopRequest::ProductDefault
            .requested_frames(u32::MAX, true)
            .is_err());
        assert_eq!(
            GopRequest::ProductDefault
                .requested_frames(u32::MAX, false)
                .unwrap(),
            None
        );
    }

    #[test]
    fn recording_quality_plan_bounds_and_experimental_parameters() {
        assert!(quality_plan(1920, 1080, 0, QualityProfile::Motion).is_err());
        assert!(quality_plan(1920, 1080, 31, QualityProfile::Motion).is_err());
        assert!(quality_plan(MAX_SIDE + 1, 1080, 30, QualityProfile::Motion).is_err());
        for profile in QualityProfile::EXPERIMENTS {
            let plan = quality_plan(MAX_SIDE, 4166, 60, profile).unwrap();
            assert!(plan.experimental && plan.bitrate_bps <= 80_000_000);
            assert_eq!(plan.parameters.max_bitrate_bps, Some(plan.bitrate_bps * 2));
            assert_eq!(
                plan.parameters.rate_control,
                RateControl::PeakConstrainedVbr
            );
        }
        let text = quality_plan(1920, 1080, 30, QualityProfile::TextDetail).unwrap();
        assert_eq!(text.parameters.quality_vs_speed, Some(100));
        assert_eq!(text.parameters.buffer_bits, Some(text.bitrate_bps * 2));
        assert_eq!(text.parameters.low_latency, Some(false));
    }

    #[test]
    fn recording_quality_plan_buffers_align_to_bytes_without_shortening_two_second_target() {
        let mut rounded_up = false;
        for (width, height, fps) in [(962, 542, 30), (1922, 1082, 24), (MAX_SIDE, 4166, 60)] {
            for profile in QualityProfile::EXPERIMENTS {
                let plan = quality_plan(width, height, fps, profile).unwrap();
                if profile == QualityProfile::Motion {
                    assert_eq!(plan.parameters.buffer_bits, None);
                    continue;
                }
                let bits = plan.parameters.buffer_bits.expect("候选应保留两秒缓冲请求");
                let target = plan.bitrate_bps * 2;
                assert_eq!(bits % 8, 0);
                assert!(bits >= target && bits - target < 8);
                rounded_up |= bits > target;
            }
        }
        assert!(rounded_up, "素材尺寸应覆盖实际需要向上对齐的码率");
    }

    #[test]
    fn recording_timeline_excludes_paused_time() {
        let base = Instant::now();
        let mut timeline = Timeline::default();
        assert_eq!(timeline.active(base), Duration::ZERO);
        timeline.start(base);
        timeline.pause(base + Duration::from_secs(2));
        assert_eq!(
            timeline.active(base + Duration::from_secs(5)),
            Duration::from_secs(2)
        );
        timeline.pause(base + Duration::from_secs(6));
        timeline.resume(base + Duration::from_secs(7));
        assert_eq!(
            timeline.active(base + Duration::from_secs(8)),
            Duration::from_secs(3)
        );
        assert!(!timeline.paused());
    }
}
