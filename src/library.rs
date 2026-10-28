//! 统一历史时间线：把截图历史、贴图工作区记录与录屏成片合成一条按创建时间倒序的条目列表。
//! 索引只保存可重建元数据；贴图权威状态与缩略图、删除、打开动作仍按来源分派。
#[path = "library_catalog.rs"]
mod catalog;
#[path = "library_pin_thumbnail.rs"]
mod pin_thumbnail;
#[path = "library_recording_thumbnail.rs"]
mod recording_thumbnail;

use crate::{
    history,
    pin_workspace::{self, PinStatus, Workspace, WorkspaceSummary},
    recording_library_model as recordings, Result,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// 缩略图外框；卡片最高约 260 DIP，150% 缩放下仍够清晰。
pub(crate) const THUMB_WIDTH: u32 = 480;
pub(crate) const THUMB_HEIGHT: u32 = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinState {
    /// 窗口正在屏幕上（租约仍在）。
    Live,
    /// 清单记为活动，但窗口未打开（尚未恢复或异常退出）。
    Idle,
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) enum Source {
    Shot(history::Entry),
    Pin {
        id: String,
        image: Option<String>,
        text: bool,
        preview_text: Option<String>,
        state: PinState,
        group: String,
    },
    Recording(recordings::Entry),
}

/// 列表身份跨刷新稳定，缩略图内容还要随文件版本或贴图资源变化。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThumbnailRevision {
    Shot(PathBuf, u64, SystemTime),
    Pin(Option<String>, Option<(u64, Option<SystemTime>)>),
    Recording(PathBuf, u64, Option<SystemTime>),
}

pub(crate) fn thumbnail_revision(source: &Source) -> ThumbnailRevision {
    match source {
        Source::Shot(entry) => {
            ThumbnailRevision::Shot(entry.path.clone(), entry.bytes, entry.modified)
        }
        Source::Pin { image, .. } => ThumbnailRevision::Pin(
            image.clone(),
            image.as_deref().and_then(pin_thumbnail::revision),
        ),
        Source::Recording(entry) => {
            ThumbnailRevision::Recording(entry.path.clone(), entry.bytes, entry.modified)
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    /// 跨刷新稳定的身份：来源前缀 + 路径或记录编号。
    pub key: String,
    pub created: SystemTime,
    pub title: String,
    pub size: Option<(u32, u32)>,
    pub bytes: Option<u64>,
    pub source: Source,
}
impl Item {
    pub(crate) fn display_title(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        match &self.source {
            Source::Shot(_) => crate::tr!("截图", "Capture").to_owned(),
            Source::Pin { text: true, .. } => crate::tr!("文字贴图", "Text pin").to_owned(),
            Source::Pin { .. } => pin_workspace::display_pin_title(""),
            Source::Recording(entry) => Path::new(&entry.name)
                .file_stem()
                .map_or_else(|| entry.name.clone(), |s| s.to_string_lossy().into_owned()),
        }
    }
    pub(crate) fn kind_name(&self) -> &'static str {
        match &self.source {
            Source::Shot(_) => crate::tr!("截图", "Capture"),
            Source::Pin { text: true, .. } => crate::tr!("文字贴图", "Text pin"),
            Source::Pin { .. } => crate::tr!("贴图", "Pin"),
            Source::Recording(_) => crate::tr!("录屏", "Recording"),
        }
    }
    pub(crate) fn duration(&self) -> Option<Duration> {
        match &self.source {
            Source::Recording(entry) => entry.info.duration,
            _ => None,
        }
    }
    pub(crate) fn path(&self) -> Option<&Path> {
        match &self.source {
            Source::Shot(entry) => Some(&entry.path),
            Source::Recording(entry) => Some(&entry.path),
            Source::Pin { .. } => None,
        }
    }
    pub(crate) fn pin_state(&self) -> Option<PinState> {
        match &self.source {
            Source::Pin { state, .. } => Some(*state),
            _ => None,
        }
    }
    /// 正在屏幕上的贴图须先关闭；其余条目可删除。
    pub(crate) fn can_delete(&self) -> bool {
        self.pin_state() != Some(PinState::Live)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Filter {
    #[default]
    All,
    Shots,
    Pins,
    Recordings,
}
impl Filter {
    pub(crate) const ALL: [Self; 4] = [Self::All, Self::Shots, Self::Pins, Self::Recordings];
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::All => crate::tr!("全部", "All"),
            Self::Shots => crate::tr!("截图", "Captures"),
            Self::Pins => crate::tr!("贴图", "Pins"),
            Self::Recordings => crate::tr!("录屏", "Recordings"),
        }
    }
    pub(crate) fn accepts(self, item: &Item) -> bool {
        match self {
            Self::All => true,
            Self::Shots => matches!(item.source, Source::Shot(_)),
            Self::Pins => matches!(item.source, Source::Pin { .. }),
            Self::Recordings => matches!(item.source, Source::Recording(_)),
        }
    }
}

/// 空格分词、全部命中才算匹配；`×` 与 `x`、`/` 与 `-` 视为相同。
pub(crate) fn matches(item: &Item, query: &str, today: Option<&LocalTime>) -> bool {
    let normalize = |value: &str| value.to_lowercase().replace('×', "x").replace('/', "-");
    let query = normalize(query);
    if query.trim().is_empty() {
        return true;
    }
    let mut hay = vec![item.display_title(), item.kind_name().to_owned()];
    if let Some((w, h)) = item.size {
        hay.push(format!("{w}x{h}"));
    }
    if let Some(path) = item.path() {
        hay.push(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
        );
    }
    if let Source::Pin { group, .. } = &item.source {
        hay.push(group.clone());
    }
    if let Some(local) = local_time(item.created) {
        hay.push(format!(
            "{:04}-{:02}-{:02}",
            local.year, local.month, local.day
        ));
        if let Some(today) = today {
            hay.push(day_label(&local, today));
        }
    }
    let hay = normalize(&hay.join(" "));
    query.split_whitespace().all(|word| hay.contains(word))
}

#[derive(Default)]
pub(crate) struct Scan {
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
}

/// 三处来源各自失败只记警告，其余照常显示。
pub(crate) fn scan(cancelled: &dyn Fn() -> bool) -> Scan {
    if cancelled() {
        return Scan::default();
    }
    let mut warnings = Vec::new();
    let mut catalog = match catalog::Catalog::open() {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            warnings.push(crate::trf!(
                "历史索引打开失败：{error}",
                "History index failed to open: {error}"
            ));
            None
        }
    };
    let mut complete = [false; 3];
    let shots = match history::scan(cancelled) {
        Ok(shots) => {
            complete[0] = true;
            shots
        }
        Err(error) => {
            warnings.push(crate::trf!(
                "截图历史读取失败：{error}",
                "Capture history failed to load: {error}"
            ));
            Vec::new()
        }
    };
    if !cancelled() {
        if let Err(error) = pin_workspace::prune_missing_records() {
            warnings.push(crate::trf!(
                "失效贴图记录清理失败：{error}",
                "Unavailable pin cleanup failed: {error}"
            ));
        }
    }
    let pins = match pin_workspace::load_summary() {
        Ok(workspace) => {
            complete[1] = true;
            pin_summary_items(&workspace, pin_workspace::lease_live)
        }
        Err(error) => {
            warnings.push(crate::trf!(
                "贴图工作区读取失败：{error}",
                "Pin workspace failed to load: {error}"
            ));
            Vec::new()
        }
    };
    let recording_scan = match catalog.as_ref() {
        Some(catalog) => recordings::scan_with_probe(cancelled, &mut |path, cancelled| {
            catalog.probe_recording(path, cancelled)
        }),
        None => recordings::scan(cancelled),
    };
    let recordings = match recording_scan {
        Ok(scan) => {
            complete[2] = !scan.truncated && scan.warnings.is_empty();
            warnings.extend(scan.warnings);
            if scan.truncated {
                warnings.push(crate::trf!(
                    "录屏目录文件较多，只显示最近 {} 个成片",
                    "The recording folder is large; only the latest {} videos are shown",
                    recordings::MAX_ENTRIES
                ));
            }
            scan.entries
        }
        Err(error) => {
            warnings.push(crate::trf!(
                "录屏目录读取失败：{error}",
                "Recording folder failed to load: {error}"
            ));
            Vec::new()
        }
    };
    let mut items = merge(shots, pins, recordings);
    if let Some(catalog) = catalog.as_mut() {
        match catalog.reconcile(&items, complete, cancelled) {
            Ok(indexed) => items = indexed,
            Err(error) if !cancelled() => warnings.push(crate::trf!(
                "历史索引更新失败：{error}",
                "History index failed to update: {error}"
            )),
            Err(_) => {}
        }
    }
    Scan { items, warnings }
}

pub(crate) fn merge(
    shots: Vec<history::Entry>,
    pins: Vec<Item>,
    recordings: Vec<recordings::Entry>,
) -> Vec<Item> {
    let mut items = pins;
    items.extend(shots.into_iter().map(|entry| Item {
        key: format!("shot:{}", entry.path.display()),
        created: entry.modified,
        title: String::new(),
        size: entry.dimensions,
        bytes: Some(entry.bytes),
        source: Source::Shot(entry),
    }));
    items.extend(recordings.into_iter().map(|entry| Item {
        key: format!("rec:{}", entry.path.display()),
        created: entry.modified.unwrap_or(UNIX_EPOCH),
        title: String::new(),
        size: entry.info.width.zip(entry.info.height),
        bytes: Some(entry.bytes),
        source: Source::Recording(entry),
    }));
    items.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.key.cmp(&b.key)));
    items
}

pub(crate) fn pin_items(workspace: &Workspace, live: impl Fn(&str) -> bool) -> Vec<Item> {
    workspace
        .pins
        .iter()
        .map(|pin| {
            let state = if pin.status == PinStatus::Closed {
                PinState::Closed
            } else if live(&pin.id) {
                PinState::Live
            } else {
                PinState::Idle
            };
            let group = workspace
                .groups
                .iter()
                .find(|group| group.id == pin.group_id)
                .map(|group| group.display_name())
                .unwrap_or_default();
            Item {
                key: format!("pin:{}", pin.id),
                created: pin_created(&pin.id)
                    .unwrap_or(UNIX_EPOCH + Duration::from_millis(pin.updated_ms)),
                // 新贴图默认标题“贴图”视为未设置，避免整页都叫同一个名字。
                title: match pin.title.as_str() {
                    "贴图" => String::new(),
                    title => title.to_owned(),
                },
                size: Some((pin.placement.width, pin.placement.height)),
                bytes: None,
                source: Source::Pin {
                    id: pin.id.clone(),
                    image: pin.image_file.clone(),
                    text: pin.text_document.is_some(),
                    preview_text: pin
                        .text_document
                        .as_ref()
                        .and_then(pin_thumbnail::text_preview),
                    state,
                    group,
                },
            }
        })
        .collect()
}

/// 管理列表只读轻量摘要，恢复文档仍在真正打开贴图时由工作区取得。
pub(crate) fn pin_summary_items(
    workspace: &WorkspaceSummary,
    live: impl Fn(&str) -> bool,
) -> Vec<Item> {
    workspace
        .pins
        .iter()
        .map(|pin| {
            let state = if pin.status == PinStatus::Closed {
                PinState::Closed
            } else if live(&pin.id) {
                PinState::Live
            } else {
                PinState::Idle
            };
            Item {
                key: format!("pin:{}", pin.id),
                created: pin_created(&pin.id)
                    .unwrap_or(UNIX_EPOCH + Duration::from_millis(pin.updated_ms)),
                title: match pin.title.as_str() {
                    "贴图" => String::new(),
                    title => title.to_owned(),
                },
                size: Some((pin.placement.width, pin.placement.height)),
                bytes: None,
                source: Source::Pin {
                    id: pin.id.clone(),
                    image: pin.image_file.clone(),
                    text: pin.text,
                    preview_text: pin.preview_text.clone(),
                    state,
                    group: workspace
                        .groups
                        .iter()
                        .find(|group| group.id == pin.group_id)
                        .map(|group| group.display_name())
                        .unwrap_or_default(),
                },
            }
        })
        .collect()
}

/// 贴图编号形如 `pin-<32 位十六进制纳秒>-<进程>-<序号>`；创建时间不随拖动、缩放变化。
pub(crate) fn pin_created(id: &str) -> Option<SystemTime> {
    let nanos = id.split('-').nth(1).filter(|part| part.len() == 32)?;
    let nanos = u128::from_str_radix(nanos, 16).ok()?;
    let nanos = u64::try_from(nanos).ok()?;
    Some(UNIX_EPOCH + Duration::from_nanos(nanos))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    /// 0 = 周日。
    pub weekday: u32,
    /// 自 1970-01-01 起的本地日序号，只用于比较相差几天。
    pub days: i64,
}

/// 按系统当前时区换算为本地时间。
pub(crate) fn local_time(time: SystemTime) -> Option<LocalTime> {
    use windows::Win32::{Foundation::FILETIME, Storage::FileSystem::FileTimeToLocalFileTime};
    let ticks = u64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_nanos() / 100)
        .ok()?
        .checked_add(EPOCH_TICKS)?;
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME::default();
    unsafe { FileTimeToLocalFileTime(&utc, &mut local) }.ok()?;
    let local = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
    Some(civil(local.checked_sub(EPOCH_TICKS)? / 10_000_000))
}
/// 1601-01-01 到 1970-01-01 的 100ns 计数。
const EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// Unix 秒（已是本地时钟）→ 公历日期与星期。
pub(crate) fn civil(seconds: u64) -> LocalTime {
    let days = (seconds / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    let clock = seconds % 86_400;
    LocalTime {
        year,
        month,
        day,
        hour: (clock / 3_600) as u32,
        minute: (clock / 60 % 60) as u32,
        weekday: (days + 4).rem_euclid(7) as u32,
        days,
    }
}

/// 日期分段标题：今天、昨天、一周内写星期，更早写月日（跨年加年份）。
pub(crate) fn day_label(day: &LocalTime, today: &LocalTime) -> String {
    const WEEK_ZH: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
    const WEEK_EN: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTH_EN: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let english = crate::i18n::is_english();
    match today.days - day.days {
        0 => crate::tr!("今天", "Today").to_owned(),
        1 => crate::tr!("昨天", "Yesterday").to_owned(),
        2..=6 => {
            let names = if english { WEEK_EN } else { WEEK_ZH };
            names[day.weekday as usize].to_owned()
        }
        _ => {
            let month = day.month as usize - 1;
            match (english, day.year == today.year) {
                (false, true) => format!("{}月{}日", day.month, day.day),
                (false, false) => format!("{}年{}月{}日", day.year, day.month, day.day),
                (true, true) => format!("{} {}", MONTH_EN[month], day.day),
                (true, false) => format!("{} {}, {}", MONTH_EN[month], day.day, day.year),
            }
        }
    }
}
pub(crate) fn date_text(time: &LocalTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        time.year, time.month, time.day, time.hour, time.minute
    )
}
pub(crate) fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
pub(crate) fn bytes_text(bytes: u64) -> String {
    match bytes {
        0..=1_023 => format!("{bytes} B"),
        1_024..=1_048_575 => format!("{:.0} KB", bytes as f64 / 1_024.),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", bytes as f64 / 1_048_576.),
        _ => format!("{:.2} GB", bytes as f64 / 1_073_741_824.),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Row {
    pub start: usize,
    pub end: usize,
    pub height: f32,
}
/// 等高行照片墙：每行按宽高比放满 `width`，末行不拉伸。宽高比先夹到 0.6–2.4，更宽或更长的图裁切显示。
pub(crate) fn justify(aspects: &[f32], width: f32, target: f32, gap: f32) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut sum = 0.;
    for (index, aspect) in aspects.iter().enumerate() {
        sum += aspect_clamp(*aspect);
        let count = (index + 1 - start) as f32;
        let gaps = gap * (count - 1.);
        if sum * target + gaps >= width {
            rows.push(Row {
                start,
                end: index + 1,
                height: ((width - gaps) / sum).max(1.),
            });
            start = index + 1;
            sum = 0.;
        }
    }
    if start < aspects.len() {
        rows.push(Row {
            start,
            end: aspects.len(),
            height: target,
        });
    }
    rows
}
pub(crate) fn aspect_clamp(aspect: f32) -> f32 {
    if aspect.is_finite() {
        aspect.clamp(0.6, 2.4)
    } else {
        1.
    }
}

pub(crate) struct Thumb {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}
/// 文字内容直接交给界面排版，不为历史列表生成文字贴图底图。
pub(crate) fn pin_preview_text(source: &Source) -> Option<String> {
    match source {
        Source::Pin { preview_text, .. } => preview_text.clone(),
        _ => None,
    }
}

/// 截图按行解码，录屏只读首帧；图片贴图使用已有兼容图，文字由界面预览。
pub(crate) fn thumbnail(source: &Source, cancelled: &dyn Fn() -> bool) -> Result<Option<Thumb>> {
    match source {
        Source::Shot(entry) => {
            // 与其它历史解码共用全进程预算；忙时短暂等待，取消即放弃。
            let _permit = loop {
                if let Some(permit) = history::decode_gate().try_enter() {
                    break permit;
                }
                if cancelled() {
                    return Err(
                        crate::tr!("缩略图解码已取消", "Thumbnail decoding cancelled").into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut thumb = history::thumbnail(entry, cancelled)?;
            history::rgba_to_bgra(&mut thumb.rgba);
            Ok(Some(Thumb {
                width: thumb.width,
                height: thumb.height,
                bgra: thumb.rgba,
            }))
        }
        Source::Pin { text: true, .. } => Ok(None),
        Source::Pin {
            image: Some(name), ..
        } => pin_thumbnail::load(name, cancelled),
        Source::Recording(entry) => recording_thumbnail::load(entry, cancelled),
        _ => Ok(None),
    }
}
/// 区域平均缩小到外框内，不放大。
pub(crate) fn downscale(width: u32, height: u32, bgra: &[u8], max_w: u32, max_h: u32) -> Thumb {
    let scale = (f64::from(max_w) / f64::from(width.max(1)))
        .min(f64::from(max_h) / f64::from(height.max(1)))
        .min(1.);
    let tw = ((f64::from(width) * scale).round() as u32).max(1);
    let th = ((f64::from(height) * scale).round() as u32).max(1);
    let mut out = vec![0_u8; tw as usize * th as usize * 4];
    for ty in 0..th {
        let y0 = (u64::from(ty) * u64::from(height) / u64::from(th)) as u32;
        let y1 =
            ((u64::from(ty + 1) * u64::from(height)).div_ceil(u64::from(th)) as u32).max(y0 + 1);
        for tx in 0..tw {
            let x0 = (u64::from(tx) * u64::from(width) / u64::from(tw)) as u32;
            let x1 =
                ((u64::from(tx + 1) * u64::from(width)).div_ceil(u64::from(tw)) as u32).max(x0 + 1);
            let mut sum = [0_u64; 4];
            for y in y0..y1.min(height) {
                let row = y as usize * width as usize * 4;
                for x in x0..x1.min(width) {
                    let pixel = &bgra[row + x as usize * 4..row + x as usize * 4 + 4];
                    for channel in 0..4 {
                        sum[channel] += u64::from(pixel[channel]);
                    }
                }
            }
            let count = u64::from(y1.min(height) - y0) * u64::from(x1.min(width) - x0);
            let target = (ty as usize * tw as usize + tx as usize) * 4;
            for channel in 0..4 {
                out[target + channel] = (sum[channel] / count.max(1)) as u8;
            }
        }
    }
    Thumb {
        width: tw,
        height: th,
        bgra: out,
    }
}

/// 截图缓存直接删除；关闭的贴图删记录与资源；录屏成片移到回收站。
pub(crate) fn delete(item: &Item) -> Result<()> {
    match &item.source {
        Source::Shot(entry) => history::delete(entry),
        Source::Pin {
            state: PinState::Live,
            ..
        } => Err(crate::tr!(
            "贴图仍在屏幕上，请先关闭",
            "The pin is still on screen; close it first"
        )
        .into()),
        Source::Pin { id, .. } => pin_workspace::forget(id),
        Source::Recording(entry) => recordings::recycle(&entry.path),
    }
}

/// 在资源管理器中选中文件。
pub(crate) fn reveal(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(crate::tr!("文件已不存在", "The file no longer exists").into());
    }
    // Windows 文件名不能含引号；标准参数转义会错误地包住 /select 开关。
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pin_model::{PinState as WindowState, PinWindowSizing, Placement},
        pin_workspace::{Group, PinEntry},
    };
    use std::path::PathBuf;

    fn shot(name: &str, seconds: u64) -> history::Entry {
        history::Entry {
            path: PathBuf::from(name),
            bytes: 10,
            modified: UNIX_EPOCH + Duration::from_secs(seconds),
            dimensions: Some((1920, 1080)),
        }
    }
    fn pin(id: &str, title: &str, status: PinStatus, text: bool) -> PinEntry {
        PinEntry {
            id: id.into(),
            group_id: "0".into(),
            title: title.into(),
            status,
            hidden: false,
            click_through: false,
            placement: Placement {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            window_sizing: PinWindowSizing::General,
            state: WindowState::default(),
            image_file: (!text).then(|| "asset.png".into()),
            text_document: None,
            presentation_style: None,
            original_file: None,
            raster_document: None,
            processing_history: None,
            updated_ms: 1_000,
        }
    }

    #[test]
    fn library_merges_sources_newest_first_with_pin_states() {
        let id = format!("pin-{:032x}-00000001-{:016x}", 5_000_000_000_u128, 1);
        let workspace = Workspace {
            groups: vec![Group {
                id: "0".into(),
                name: "资料".into(),
            }],
            pins: vec![
                pin(&id, "贴图", PinStatus::Active, false),
                pin("legacy", "旧标题", PinStatus::Closed, false),
            ],
            ..Default::default()
        };
        let pins = pin_items(&workspace, |id| id != "legacy");
        let items = merge(
            vec![shot("shot-a.png", 3), shot("shot-b.png", 9)],
            pins,
            vec![],
        );
        let keys: Vec<_> = items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "shot:shot-b.png",
                &format!("pin:{id}") as &str,
                "shot:shot-a.png",
                "pin:legacy"
            ]
        );
        // 默认标题“贴图”视为未设置；ID 中的纳秒是创建时间。
        assert_eq!(items[1].title, "");
        assert_eq!(items[1].pin_state(), Some(PinState::Live));
        assert_eq!(items[1].created, UNIX_EPOCH + Duration::from_secs(5));
        assert_eq!(items[3].pin_state(), Some(PinState::Closed));
        assert_eq!(items[3].created, UNIX_EPOCH + Duration::from_millis(1_000));
        assert!(!items[1].can_delete());
        assert!(items[3].can_delete());
    }

    #[test]
    fn library_filter_and_search_match_kind_title_size_and_date() {
        let mut items = merge(vec![shot("shot-20261019.png", 86_400 * 3)], vec![], vec![]);
        let shot = items.remove(0);
        assert!(Filter::Shots.accepts(&shot));
        assert!(!Filter::Pins.accepts(&shot));
        assert!(matches(&shot, "", None));
        assert!(matches(&shot, "1920×1080", None));
        assert!(matches(&shot, "20261019", None));
        assert!(!matches(&shot, "1920x1080 missing", None));
    }

    #[test]
    fn library_civil_dates_weekdays_and_day_labels() {
        // 2026-10-19 14:32 是周一。
        let monday = civil(1_792_420_320);
        assert_eq!((monday.year, monday.month, monday.day), (2026, 10, 19));
        assert_eq!((monday.hour, monday.minute, monday.weekday), (14, 32, 1));
        let leap = civil(951_782_400); // 2000-02-29
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
        let day = |offset: i64| civil((1_792_420_320 + offset * 86_400) as u64);
        assert_eq!(day_label(&monday, &monday), crate::tr!("今天", "Today"));
        assert_eq!(
            day_label(&day(-1), &monday),
            crate::tr!("昨天", "Yesterday")
        );
        assert_eq!(day_label(&day(-3), &monday), crate::tr!("周五", "Friday"));
        assert_eq!(
            day_label(&day(-10), &monday),
            crate::tr!("10月9日", "Oct 9")
        );
        assert_eq!(
            day_label(&day(-300), &monday),
            crate::tr!("2025年12月23日", "Dec 23, 2025")
        );
        assert_eq!(duration_text(Duration::from_secs(42)), "0:42");
        assert_eq!(duration_text(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn library_justify_fills_rows_and_keeps_last_row_natural() {
        let rows = justify(&[1., 1., 1., 1., 1.], 310., 100., 10.);
        // 前三张 3×100+20 ≥ 310 成行并按宽度缩放，剩余两张保持目标高度。
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].start, rows[0].end), (0, 3));
        assert!((rows[0].height - 96.666_67).abs() < 0.01);
        assert_eq!((rows[1].start, rows[1].end, rows[1].height), (3, 5, 100.));
        // 超宽图被夹到 2.4，单张也不会溢出。
        let wide = justify(&[20.], 200., 100., 10.);
        assert_eq!(wide.len(), 1);
        assert!((wide[0].height - 200. / 2.4).abs() < 0.01);
        assert!(justify(&[], 200., 100., 10.).is_empty());
    }

    #[test]
    fn library_downscale_averages_into_bounds_without_upscaling() {
        let pixels: Vec<u8> = [[0, 0, 0, 255], [200, 100, 50, 255]].repeat(2).concat();
        let thumb = downscale(2, 2, &pixels, 1, 1);
        assert_eq!((thumb.width, thumb.height), (1, 1));
        assert_eq!(thumb.bgra, vec![100, 50, 25, 255]);
        let same = downscale(2, 2, &pixels, 480, 320);
        assert_eq!((same.width, same.height), (2, 2));
        assert_eq!(same.bgra, pixels);
    }
}
