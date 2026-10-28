//! CLI直接复用录屏产品的GIF转换器，可查看真实阶段耗时；不创建窗口或读取桌面。
use crate::Result;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
struct Command {
    source: PathBuf,
    output: PathBuf,
    fps: u32,
}

fn parse(args: &[String]) -> Result<Option<Command>> {
    if args.first().map(String::as_str) != Some("--convert-gif") {
        return Ok(None);
    }
    let source = args
        .get(1)
        .filter(|s| !s.starts_with("--"))
        .ok_or("--convert-gif 缺少视频文件")?;
    let mut output = None;
    let mut fps = None;
    let mut index = 2;
    while index < args.len() {
        let value = args.get(index + 1).ok_or("GIF参数缺少值")?;
        match args[index].as_str() {
            "--output" => {
                if value.starts_with("--") || output.replace(PathBuf::from(value)).is_some() {
                    return Err("GIF输出路径无效或重复".into());
                }
            }
            "--gif-fps" => {
                let rate: u32 = value.parse()?;
                if !(1..=15).contains(&rate) || fps.replace(rate).is_some() {
                    return Err("--gif-fps 必须为1至15且不能重复".into());
                }
            }
            other => return Err(format!("未知GIF参数：{other}").into()),
        }
        index += 2;
    }
    Ok(Some(Command {
        source: source.into(),
        output: output.ok_or("--convert-gif 请同时指定 --output <文件.gif>")?,
        fps: fps.unwrap_or(15),
    }))
}

pub(super) fn execute(args: &[String]) -> Result<bool> {
    let Some(command) = parse(args)? else {
        return Ok(false);
    };
    let _runtime = crate::media_foundation::MfRuntime::new()?;
    let stats = crate::gif_export::mp4_to_gif_with_stats(
        &command.source,
        &command.output,
        command.fps,
        |_| true,
        64 * 1024 * 1024,
    )?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "backend": "mf-adaptive-global-palette",
            "source": command.source,
            "output": command.output,
            "fps": command.fps,
            "stats": stats,
        }))?
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).into()).collect()
    }
    #[test]
    fn gif_file_command_accepts_unicode_paths_and_explicit_fps() {
        let actual = parse(&arguments(&[
            "--convert-gif",
            "测试视频.mp4",
            "--gif-fps",
            "12",
            "--output",
            "结果.gif",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            actual,
            Command {
                source: "测试视频.mp4".into(),
                output: "结果.gif".into(),
                fps: 12
            }
        );
        let default = parse(&arguments(&[
            "--convert-gif",
            "source.mp4",
            "--output",
            "out.gif",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(default.fps, 15);
    }
    #[test]
    fn gif_file_command_rejects_missing_duplicate_and_out_of_range_arguments() {
        for values in [
            vec!["--convert-gif"],
            vec!["--convert-gif", "--output", "out.gif"],
            vec!["--convert-gif", "source.mp4"],
            vec![
                "--convert-gif",
                "source.mp4",
                "--output",
                "out.gif",
                "--gif-fps",
                "0",
            ],
            vec![
                "--convert-gif",
                "source.mp4",
                "--output",
                "out.gif",
                "--gif-fps",
                "16",
            ],
            vec![
                "--convert-gif",
                "source.mp4",
                "--output",
                "out.gif",
                "--output",
                "other.gif",
            ],
            vec![
                "--convert-gif",
                "source.mp4",
                "--output",
                "out.gif",
                "--gif-fps",
                "15",
                "--gif-fps",
                "12",
            ],
        ] {
            assert!(parse(&arguments(&values)).is_err(), "{values:?}");
        }
        assert!(parse(&arguments(&["--capture"])).unwrap().is_none());
    }
}
