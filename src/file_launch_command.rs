//! 外部文件查看、编辑与贴图入口，不依赖启动宿主或读取桌面。
use crate::Result;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, PartialEq, Eq)]
enum Launch {
    PinFile {
        path: PathBuf,
        at: Option<(i32, i32)>,
    },
    EditFile(PathBuf),
    ViewVideo(Vec<PathBuf>),
    ViewImage(Option<PathBuf>),
    PinClipboard {
        at: Option<(i32, i32)>,
    },
}

fn parse(args: &[String]) -> Result<Option<Launch>> {
    let modes = [
        "--pin-file",
        "--edit-file",
        "--pin-clipboard",
        "--view-video",
        "--view-image",
    ];
    if !args
        .iter()
        .any(|arg| modes.contains(&arg.as_str()) || arg == "--at")
    {
        return Ok(None);
    }
    let mode = args.first().map(String::as_str).unwrap_or_default();
    if !modes.contains(&mode) {
        return Err(tr!(
            "文件查看、编辑或贴图入口必须放在首个参数，不能与其它入口混用",
            "File viewing, editing or pinning must be the first argument and can't be combined"
        )
        .into());
    }
    if mode == "--view-video" {
        let paths = &args[1..];
        if paths.len() > crate::video_file_queue::MAX_VIDEO_FILES {
            return Err(tr!(
                "一次最多打开128个视频文件",
                "Open up to 128 video files at a time"
            )
            .into());
        }
        if paths
            .iter()
            .any(|path| path.is_empty() || path.starts_with("--") || path.contains('\0'))
        {
            return Err(tr!(
                "视频入口只接受有效文件路径，不能与其它入口或参数混用",
                "Use valid video paths without other commands or options"
            )
            .into());
        }
        return Ok(Some(Launch::ViewVideo(
            paths.iter().map(PathBuf::from).collect(),
        )));
    }
    let optional_path = mode == "--view-image";
    let (path, mut index) = if mode == "--pin-clipboard" || (optional_path && args.len() == 1) {
        (None, 1)
    } else {
        let path = args
            .get(1)
            .filter(|path| !path.is_empty() && !path.starts_with("--") && !path.contains('\0'))
            .ok_or(tr!("文件入口缺少有效路径", "A valid file path is required"))?;
        (Some(PathBuf::from(path)), 2)
    };
    let mut at = None;
    while index < args.len() {
        if args[index] != "--at"
            || !matches!(mode, "--pin-file" | "--pin-clipboard")
            || at.is_some()
        {
            return Err(format!("文件入口不支持或重复指定参数：{}", args[index]).into());
        }
        let value = args.get(index + 1).ok_or("--at 缺少 x,y 位置")?;
        let (x, y) = value
            .split_once(',')
            .ok_or("--at 格式应为 x,y，使用屏幕物理像素")?;
        at = Some((x.parse()?, y.parse()?));
        index += 2;
    }
    Ok(Some(match mode {
        "--pin-file" => Launch::PinFile {
            path: path.unwrap(),
            at,
        },
        "--edit-file" => Launch::EditFile(path.unwrap()),
        "--view-image" => Launch::ViewImage(path),
        _ => Launch::PinClipboard { at },
    }))
}

impl Launch {
    fn worker_command(&self, worker: PathBuf) -> Command {
        let mut command = Command::new(worker);
        let at = match self {
            Self::PinFile { path, at } => {
                command.arg("--pin-file").arg(path);
                *at
            }
            Self::EditFile(path) => {
                command.arg("--edit-file").arg(path);
                None
            }
            Self::ViewVideo(paths) => {
                command.arg("--view-video").args(paths);
                None
            }
            Self::ViewImage(path) => {
                command.arg("--view-image");
                if let Some(path) = path {
                    command.arg(path);
                }
                None
            }
            Self::PinClipboard { at } => {
                command.arg("--pin-clipboard");
                *at
            }
        };
        if let Some((x, y)) = at {
            command.arg("--at").arg(format!("{x},{y}"));
        }
        command
    }

    fn validate_file(&self) -> Result<()> {
        let path = match self {
            Self::PinFile { path, .. } | Self::EditFile(path) | Self::ViewImage(Some(path)) => path,
            // 视频队列由窗口后台逐项预检，坏项不能阻止其它明确输入打开。
            Self::PinClipboard { .. } | Self::ViewVideo(_) | Self::ViewImage(None) => return Ok(()),
        };
        validate_file_path(path)
    }
}

fn validate_file_path(path: &Path) -> Result<()> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(tr!("输入路径必须是文件", "The input path must be a file").into());
    }
    Ok(())
}

/// 在独立工作进程查看文件；调用方不等待播放器关闭。
pub(crate) fn open_video(path: &Path) -> Result<()> {
    validate_file_path(path)?;
    open_file_viewer(Launch::ViewVideo(vec![path.to_owned()]))
}

/// 历史图片沿普通查看入口打开，不占用常驻进程的 GPU 会话。
pub(crate) fn open_image(path: &Path) -> Result<()> {
    validate_file_path(path)?;
    open_file_viewer(Launch::ViewImage(Some(path.to_owned())))
}

pub(crate) fn choose_video() -> Result<()> {
    open_file_viewer(Launch::ViewVideo(Vec::new()))
}

pub(crate) fn choose_image() -> Result<()> {
    open_file_viewer(Launch::ViewImage(None))
}

fn open_file_viewer(launch: Launch) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let worker = std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE);
    let child = launch
        .worker_command(worker)
        .creation_flags(0x08000000)
        .spawn()?;
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id());
    }
    Ok(())
}

/// 尚未通过人工验收的高级录屏与恢复操作，只在显式启用的本机实验环境显示。
pub(crate) fn experimental_media_enabled() -> bool {
    std::env::var("RUSTSHOT_EXPERIMENTAL_MEDIA").is_ok_and(|value| value == "1")
}

/// CLI 全部参数检查成功后才启动独立工作进程，不使用 shell 拼接。
pub(crate) fn execute_cli(args: &[String]) -> Result<bool> {
    let Some(launch) = parse(args)? else {
        return Ok(false);
    };
    launch.validate_file()?;
    let worker = std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE);
    let status = launch.worker_command(worker).status()?;
    if !status.success() {
        return Err(tr!(
            "文件查看、编辑或贴图工作进程失败",
            "The file viewing, editing or pinning process failed"
        )
        .into());
    }
    Ok(true)
}

/// GPUI 二次核验；只有托盘原有的裸 --pin-file 保留图片选择对话框。
pub(crate) fn execute_worker(args: &[String]) -> Result<bool> {
    if args == ["--pin-file"] {
        return Ok(false);
    }
    let Some(launch) = parse(args)? else {
        return Ok(false);
    };
    launch.validate_file()?;
    match launch {
        Launch::PinFile { path, at } => crate::pin_app::run_file(path, at)?,
        Launch::EditFile(path) => crate::image_editor::run(Some(path), false)?,
        Launch::ViewVideo(paths) => crate::video_view::run_paths(paths)?,
        Launch::ViewImage(path) => crate::image_viewer::run(path)?,
        Launch::PinClipboard { at } => crate::pin_app::run_clipboard(at)?,
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn file_launch_keeps_native_path_argument_and_negative_physical_position() {
        let path = r"C:\测试 图片\A & $() ; report.png";
        let launch = parse(&args(&["--pin-file", path, "--at", "-1920,120"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            launch,
            Launch::PinFile {
                path: path.into(),
                at: Some((-1920, 120))
            }
        );
        let command = launch.worker_command(crate::brand::GPUI_EXE.into());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--pin-file", path, "--at", "-1920,120"]
                .map(std::ffi::OsStr::new)
                .to_vec()
        );
        assert_eq!(
            parse(&args(&["--edit-file", path])).unwrap(),
            Some(Launch::EditFile(path.into()))
        );
        assert_eq!(
            parse(&args(&["--pin-clipboard", "--at", "0,-1080"])).unwrap(),
            Some(Launch::PinClipboard {
                at: Some((0, -1080))
            })
        );
        assert!(parse(&args(&["--ocr-file", "input.png"]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn invalid_file_launch_never_falls_through_to_screenshot_or_clipboard() {
        for values in [
            vec!["--at", "-1920,120"],
            vec!["--pin-file"],
            vec!["--edit-file"],
            vec!["--pin-file", ""],
            vec!["--pin-file", "--capture"],
            vec!["--capture", "--pin-file", "image.png"],
            vec!["--pin-file", "image.png", "--pin-clipboard"],
            vec!["--pin-clipboard", "--output", "x.png"],
            vec!["--edit-file", "image.png", "--at", "1,2"],
            vec!["--pin-file", "image.png", "--at"],
            vec!["--pin-file", "image.png", "--at", "1,2,3"],
            vec!["--pin-file", "image.png", "--at", "1.5,2"],
            vec!["--pin-file", "image.png", "--at", "2147483648,0"],
            vec!["--pin-clipboard", "--at", "1,2", "--at", "3,4"],
        ] {
            assert!(parse(&args(&values)).is_err(), "{values:?}");
        }
        assert!(!execute_worker(&args(&["--pin-file"])).unwrap());
        // 不创建 App、Window，也不读取真实剪贴板或桌面。
    }

    #[test]
    fn viewer_launch_preserves_optional_and_native_file_arguments() {
        let path = r"C:\测试 图片\A & $() ; report.mp4";
        for (mode, bare, with_path) in [
            (
                "--view-video",
                Launch::ViewVideo(Vec::new()),
                Launch::ViewVideo(vec![path.into()]),
            ),
            (
                "--view-image",
                Launch::ViewImage(None),
                Launch::ViewImage(Some(path.into())),
            ),
        ] {
            assert_eq!(parse(&args(&[mode])).unwrap(), Some(bare));
            let launch = parse(&args(&[mode, path])).unwrap().unwrap();
            assert_eq!(launch, with_path);
            let command = launch.worker_command(crate::brand::GPUI_EXE.into());
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [mode, path].map(std::ffi::OsStr::new)
            );
        }
    }

    #[test]
    fn viewer_launch_rejects_mixed_modes_and_pin_coordinates() {
        for mode in ["--view-video", "--view-image"] {
            for values in [
                vec![mode, ""],
                vec![mode, "a\0b"],
                vec![mode, "--capture"],
                vec!["--capture", mode, "input.mp4"],
                vec!["--settings", mode, "input.mp4"],
                vec![mode, "input.mp4", "--view-image"],
                vec![mode, "input.mp4", "--view-video"],
                vec![mode, "input.mp4", "--pin-file", "input.png"],
                vec![mode, "input.mp4", "--edit-file", "input.png"],
                vec![mode, "input.mp4", "--output", "output.mp4"],
                vec![mode, "input.mp4", "--at", "1,2"],
                vec![mode, "--at", "1,2"],
            ] {
                assert!(parse(&args(&values)).is_err(), "{values:?}");
            }
        }
        assert!(parse(&args(&["--view-image", "input.png", "extra.png"])).is_err());
    }

    #[test]
    fn viewer_launch_video_queue_keeps_order_duplicates_and_native_paths_with_a_limit() {
        let first = r"C:\测试 视频\A & $() ; clip.mp4";
        let second = r"D:\另一个目录\second.webm";
        let launch = parse(&args(&["--view-video", first, second, first]))
            .unwrap()
            .unwrap();
        assert_eq!(
            launch,
            Launch::ViewVideo(vec![first.into(), second.into(), first.into()])
        );
        assert_eq!(
            launch
                .worker_command(crate::brand::GPUI_EXE.into())
                .get_args()
                .collect::<Vec<_>>(),
            ["--view-video", first, second, first].map(std::ffi::OsStr::new)
        );
        let mut many = vec!["--view-video".to_owned()];
        many.extend(std::iter::repeat_n(
            first.to_owned(),
            crate::video_file_queue::MAX_VIDEO_FILES,
        ));
        assert!(parse(&many).is_ok());
        many.push(second.to_owned());
        assert!(parse(&many).is_err());
    }
}
