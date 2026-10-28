//! 输出成功之后才更新历史和最近目录；快速保存用原子提交避免同名覆盖。
use crate::{capture::Image, export, save_options::SaveConfig, save_store::SaveStore, Result};
use std::path::{Path, PathBuf};

pub(crate) struct SaveReport {
    pub path: PathBuf,
    pub warnings: Vec<String>,
    pub history_recorded: bool,
}

pub(crate) fn quick_save(
    image: &Image,
    protected: Option<&Path>,
    record_history: bool,
) -> Result<SaveReport> {
    let config = SaveStore::load().config;
    quick_save_with(image, protected, record_history, &config)
}

pub(crate) fn quick_save_with(
    image: &Image,
    protected: Option<&Path>,
    record_history: bool,
    config: &SaveConfig,
) -> Result<SaveReport> {
    quick_save_with_cancel(image, protected, record_history, config, None)
}

pub(crate) fn quick_save_cancellable(
    image: &Image,
    protected: Option<&Path>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<SaveReport> {
    let config = SaveStore::load().config;
    quick_save_with_cancel(image, protected, false, &config, Some(cancel))
}

fn quick_save_with_cancel(
    image: &Image,
    protected: Option<&Path>,
    record_history: bool,
    config: &SaveConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<SaveReport> {
    let check_cancel = || -> Result<()> {
        if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire)) {
            return Err(tr!("快速保存已取消", "Quick save canceled").into());
        }
        Ok(())
    };
    check_cancel()?;
    config.validate()?;
    let generated = crate::save_options::file_name(
        &config.suggested_stem(image.width, image.height)?,
        config.encoding.format,
    )?;
    let stem = Path::new(&generated)
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or(tr!("快速保存文件名无效", "Invalid quick save file name"))?;
    let encoded = export::encode_image(image, config.encoding)?;
    std::fs::create_dir_all(&config.directory)?;
    for number in 0..10_000 {
        let name = if number == 0 {
            stem.to_string()
        } else {
            format!("{stem} ({number})")
        };
        let full_name = format!("{name}.{}", config.encoding.format.extension());
        let path = config.directory.join(crate::save_options::file_name(
            &full_name,
            config.encoding.format,
        )?);
        if protected.is_some_and(|original| export::is_same_path(original, &path)) {
            continue;
        }
        check_cancel()?;
        let saved = match cancel {
            Some(cancel) => export::save_encoded_cancellable(&encoded, &path, false, cancel),
            None => export::save_encoded(&encoded, &path, false).map(|()| true),
        };
        match saved {
            Ok(false) => return Err(tr!("快速保存已取消", "Quick save canceled").into()),
            Ok(true) => {
                let mut warnings = Vec::new();
                if let Err(e) = SaveStore::record_success(&config.directory, None) {
                    warnings.push(trf!(
                        "最近目录保存失败：{e}",
                        "Failed to save recent folders: {e}"
                    ));
                }
                let history_recorded = append_history(image, record_history, &mut warnings);
                return Ok(SaveReport {
                    path,
                    warnings,
                    history_recorded,
                });
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
    Err(tr!(
        "同名文件过多，请调整名称模板或保存目录",
        "Too many files with the same name; adjust the name template or save folder"
    )
    .into())
}

pub(crate) fn append_history(image: &Image, enabled: bool, warnings: &mut Vec<String>) -> bool {
    if enabled {
        if let Err(e) = crate::history::record(image) {
            warnings.push(trf!(
                "图片已保存，但历史保存失败：{e}",
                "Image saved, but saving history failed: {e}"
            ));
            return false;
        }
        return true;
    }
    false
}

pub(crate) fn message(path: &Path, warnings: &[String]) -> String {
    let mut message = trf!("已保存：{}", "Saved: {}", path.display());
    if !warnings.is_empty() {
        message.push_str(&format!("；{}", warnings.join("；")));
    }
    message
}
