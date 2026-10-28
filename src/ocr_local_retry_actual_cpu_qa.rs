//! 显式 ignored 的真实 CPU 局部流程；只读固定自有 PNG/缓存，无 GUI 或剪贴板。
use super::*;
use crate::{capture_view::ocr_task::Receipt, recognition::task::CancelOwner};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const IDS: [&str; 4] = [
    "english-light-48",
    "english-light-48-180",
    "chinese-digits-56",
    "chinese-digits-56-180",
];
const EXPECTED_SHA: &str = "068469C04BDAFFE08C1845602C7104226C58C459BEF62924B6C68B7B9AB3405E";

fn sha(bytes: &[u8]) -> String {
    format!("{:X}", Sha256::digest(bytes))
}
fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}
fn owned_image(image: &Image) -> Image {
    Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: image.readback_ms,
    }
}
fn env_path(name: &str) -> crate::Result<PathBuf> {
    let path =
        PathBuf::from(std::env::var_os(name).ok_or_else(|| format!("缺少显式QA参数 {name}"))?);
    if !path.is_absolute() {
        return Err(format!("QA路径必须绝对：{name}").into());
    }
    Ok(path)
}
fn emit(file: &mut File, value: Value) -> crate::Result<()> {
    serde_json::to_writer(&mut *file, &value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

struct LockedFile {
    _handle: File,
    identity: Value,
}
fn lock_verified(
    path: &Path,
    expected_bytes: u64,
    expected_sha: &str,
) -> crate::Result<LockedFile> {
    // FILE_SHARE_READ：允许产品只读校验／DLL／ORT读模型，拒绝写入、替换和删除。
    // 先开句柄再读／核 SHA；持有至所有真正推理及末核完成，消除冷下载竞态。
    let file = OpenOptions::new().read(true).share_mode(1).open(path)?;
    if file.metadata()?.len() != expected_bytes {
        return Err(format!("固定文件bytes不符：{}", path.display()).into());
    }
    let bytes = std::fs::read(path)?;
    let digest = sha(&bytes);
    if bytes.len() as u64 != expected_bytes || digest != expected_sha.to_uppercase() {
        return Err(format!("固定文件bytes/SHA不符：{}", path.display()).into());
    }
    Ok(LockedFile {
        _handle: file,
        identity: json!({"path":std::fs::canonicalize(path)?,
        "logical_path":path,"bytes":bytes.len(),"sha256":digest,
        "expected_bytes":expected_bytes,"expected_sha256":expected_sha.to_uppercase(),"fixed_match":true}),
    })
}
fn cache_guards() -> crate::Result<Vec<LockedFile>> {
    let manifest: Value = serde_json::from_str(include_str!("../dependencies/ppocr-cpu.json"))?;
    let data = env_path("RUSTSHOT_DATA_DIR")?;
    if std::fs::canonicalize(crate::preferences::data_dir())? != std::fs::canonicalize(&data)? {
        return Err("实际产品data_dir不等显式QA数据目录".into());
    }
    let cache = data
        .join("ocr")
        .join(manifest["cache_revision"].as_str().ok_or("缓存版本缺失")?);
    let runtime = cache.join(
        manifest["runtime"]["directory"]
            .as_str()
            .ok_or("runtime目录缺失")?,
    );
    let models = cache.join(
        manifest["models"]["directory"]
            .as_str()
            .ok_or("模型目录缺失")?,
    );
    let mut guards = Vec::new();
    for asset in manifest["runtime"]["files"]
        .as_array()
        .ok_or("runtime files缺失")?
    {
        guards.push(lock_verified(
            &runtime.join(asset["name"].as_str().ok_or("asset name缺失")?),
            asset["bytes"].as_u64().ok_or("asset bytes缺失")?,
            asset["sha256"].as_str().ok_or("asset sha缺失")?,
        )?);
    }
    for key in [
        "detection",
        "classification",
        "recognition",
        "dictionary",
        "license",
    ] {
        let asset = &manifest["models"][key];
        guards.push(lock_verified(
            &models.join(asset["name"].as_str().ok_or("asset name缺失")?),
            asset["bytes"].as_u64().ok_or("asset bytes缺失")?,
            asset["sha256"].as_str().ok_or("asset sha缺失")?,
        )?);
    }
    if guards.len() != 9 {
        return Err("固定缓存guard必须是六件主资产＋三件附属资产".into());
    }
    Ok(guards)
}
fn snapshots(files: &[LockedFile]) -> crate::Result<Vec<Value>> {
    files.iter().map(|file| {
        let path = PathBuf::from(file.identity["logical_path"].as_str().ok_or("guard path缺失")?);
        let bytes = std::fs::read(&path)?;
        let current = json!({"path":std::fs::canonicalize(&path)?,"logical_path":path,
            "bytes":bytes.len(),"sha256":sha(&bytes),"expected_bytes":file.identity["expected_bytes"],
            "expected_sha256":file.identity["expected_sha256"],"fixed_match":true});
        if current != file.identity { return Err("只读guard期间固定文件身份变化".into()); }
        Ok(current)
    }).collect()
}
#[derive(Clone, PartialEq, Eq)]
struct Source {
    fixture: String,
    png_sha: String,
    pixels_sha: String,
    origin: (i32, i32),
}

fn one_fixture(record: &mut File, directory: &Path, fixture: &Value) -> crate::Result<LockedFile> {
    let id = fixture["id"].as_str().ok_or("fixture id缺失")?;
    let path = directory.join(fixture["file"].as_str().ok_or("fixture file缺失")?);
    let png = lock_verified(
        &path,
        fixture["bytes"].as_u64().ok_or("PNG bytes缺失")?,
        fixture["sha256"].as_str().ok_or("PNG sha缺失")?,
    )?;
    let image = Arc::new(crate::export::read_image(&path)?); // 同产品 CLI 文件解码入口。
    if image.width as u64 != fixture["width"].as_u64().ok_or("width缺失")?
        || image.height as u64 != fixture["height"].as_u64().ok_or("height缺失")?
    {
        return Err("实际PNG解码尺寸不符".into());
    }
    let before = sha(&image.bgra);
    let owner = CancelOwner::new();
    let full = match Receipt::from_result(crate::recognition::recognize_with_provider_cancellable(
        owned_image(&image),
        OcrProvider::Ppocr,
        &owner.token(),
    )) {
        Receipt::Completed(full) => full,
        Receipt::Failed(error) => return Err(format!("实际完整OCR失败 {id}：{error}").into()),
        Receipt::Cancelled => return Err("本QA未取消，但完整OCR返回Cancelled".into()),
    };
    let expected_line = fixture["expected_lines"]
        .as_array()
        .and_then(|lines| lines.first())
        .ok_or("expected line缺失")?;
    let expected = expected_line["compact_text"]
        .as_str()
        .ok_or("expected compact缺失")?;
    let direction = expected_line["horizontal_direction"]
        .as_str()
        .ok_or("expected direction缺失")?;
    if full.lines.len() != 1
        || compact(&full.text) != expected
        || full.lines[0].horizontal_direction.wire_name() != direction
    {
        return Err(format!("真实单行原文／方向未满足固定期待：{id}").into());
    }
    let original = full.text.clone();
    let result = Arc::new(full);
    let source_identity = Arc::new(());
    let body_identity = Arc::new(());
    let binding = Binding {
        source: Source {
            fixture: id.into(),
            png_sha: png.identity["sha256"].as_str().unwrap().into(),
            pixels_sha: before.clone(),
            origin: (-1700, -900),
        },
        source_identity,
        result: Arc::clone(&result),
        result_generation: 1,
        installed_body_identity: Arc::clone(&body_identity),
        body_identity,
        installed_body_revision: 0,
        body_revision: 0,
        frame: SourceFrame {
            width: image.width,
            height: image.height,
            origin: (-1700, -900),
        },
        provider: OcrProvider::Ppocr,
    };
    let selected = 0..original.len();
    let target = LocalTarget::resolve(&result, selected.clone(), binding.frame, 4)
        .map_err(|e| format!("真实DTO没有可信局部行 {id}：{e:?}"))?;
    let mut retry = LocalRetry::default();
    let request = retry
        .begin(&binding, &original, selected, 4, true)
        .map_err(|e| format!("实际begin拒绝 {id}：{e:?}"))?;
    if target.roi() != request.target().roi() || !retry.pending(&binding, &original, true) {
        return Err("真实LocalTarget/TaskSlot pending不符".into());
    }
    let roi = request.target().roi();
    let cropped = request
        .target()
        .extract_pixels(&image)
        .map_err(|e| format!("真实输入ROI错误 {e:?}"))?;
    // 独立的原 BGRA 行／列切片作为字节 oracle；不重实现几何或 OCR。
    let expected_pixels: Vec<u8> = (roi.y..roi.y + roi.height)
        .flat_map(|y| {
            let start = (y as usize * image.width as usize + roi.x as usize) * 4;
            image.bgra[start..start + roi.width as usize * 4]
                .iter()
                .copied()
        })
        .collect();
    if cropped.bgra != expected_pixels || request.target().original_line() != original {
        return Err("ROI字节／原行不符".into());
    }
    let global = request.target().global_roi();
    if global
        != (
            -1700 + i64::from(roi.x),
            -900 + i64::from(roi.y),
            roi.width,
            roi.height,
        )
    {
        return Err("负屏全局ROI偏移不符".into());
    }
    let geometry = request.target().geometry();
    let global_geometry = request.target().global_geometry();
    if global_geometry
        != geometry
            .quad
            .map(|p| [f64::from(p.x) - 1700., f64::from(p.y) - 900.])
    {
        return Err("负屏全局四角不符".into());
    }
    let roi_sha = sha(&cropped.bgra);
    emit(
        record,
        json!({"event":"roi","fixture":id,"rotation_degrees":fixture["rotation_degrees"],"direction":direction,
        "source":png.identity,"source_pixels_sha256":before,"original_text":original,
        "roi":[roi.x,roi.y,roi.width,roi.height],"global_roi":global,"global_geometry":global_geometry,
        "roi_pixels_sha256":roi_sha,"oracle_pixels_sha256":sha(&expected_pixels),"byte_exact":true,
        "actual_full_provider":"ppocr","actual_full_call":true,"trusted_line":true,"pending":true}),
    )?;
    let local = Receipt::from_result(crate::recognition::recognize_with_provider_cancellable(
        cropped,
        request.provider(),
        request.token(),
    ));
    let actual_local_text = match &local {
        Receipt::Completed(local)
            if compact(&local.text) == expected
                && local.lines.len() == 1
                && local.lines[0].horizontal_direction.wire_name() == direction =>
        {
            local.text.clone()
        }
        Receipt::Completed(local) => {
            return Err(format!("实际ROI候选原文／方向不符 {id}：{}", local.text).into())
        }
        Receipt::Failed(error) => return Err(format!("实际ROI推理失败 {id}：{error}").into()),
        Receipt::Cancelled => return Err("本QA未取消，但ROI推理返回Cancelled".into()),
    };
    let completion = retry.complete(&request, &binding, &original, true, local);
    if completion != Completion::Ready || retry.pending(&binding, &original, true) {
        return Err(format!("候选完整回执不为Ready：{completion:?}").into());
    }
    let candidate = retry
        .candidate(&binding, &original, true)
        .ok_or("真实候选未安装")?;
    if candidate.text() != actual_local_text
        || candidate.target().roi() != roi
        || result.text != original
        || sha(&image.bgra) != before
        || !Arc::ptr_eq(&binding.result, &result)
    {
        return Err("实际候选／原结果或固定输入保持断言失败".into());
    }
    emit(
        record,
        json!({"event":"candidate","fixture":id,"completion":"Ready","provider":"ppocr",
        "actual_roi_call":true,"candidate_text":candidate.text(),"compact_text":compact(candidate.text()),
        "expected_compact_text":expected,"original_text_unchanged":true,"original_pixels_unchanged":true,
        "source_pixels_sha256_after":sha(&image.bgra),"result_arc_unchanged":true,"pending_after":false,
        "clipboard_used":false,"gui_used":false}),
    )?;
    Ok(png)
}

#[test]
#[ignore = "显式后台CPU QA；固定缓存/PNG全部存在并只读guard后才推理"]
fn real_fixed_png_full_to_local_retry_cpu() -> crate::Result<()> {
    let output = env_path("RUSTSHOT_LOCAL_RETRY_QA_RECORD")?;
    let mut record = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut work = || -> crate::Result<()> {
        let expected_path = env_path("RUSTSHOT_LOCAL_RETRY_QA_EXPECTED")?;
        let expected_bytes = std::fs::read(&expected_path)?;
        if sha(&expected_bytes) != EXPECTED_SHA {
            return Err("固定expected.json身份不符".into());
        }
        let expected: Value = serde_json::from_slice(&expected_bytes)?;
        let fixtures = expected["fixtures"].as_array().ok_or("fixtures缺失")?;
        let directory = expected_path.parent().ok_or("expected父目录缺失")?;
        let guards = cache_guards()?; // 任何缺失：还没调用 provider，不下载／不准备缓存。
        emit(
            &mut record,
            json!({"event":"cache-before","assets":snapshots(&guards)?,"six_main_plus_three_support":true,"write_delete_denied":true}),
        )?;
        let mut png_guards = Vec::new();
        for id in IDS {
            let found: Vec<_> = fixtures.iter().filter(|f| f["id"] == id).collect();
            if found.len() != 1 {
                return Err("固定fixture必须唯一".into());
            }
            png_guards.push(one_fixture(&mut record, directory, found[0])?);
        }
        emit(
            &mut record,
            json!({"event":"cache-after","assets":snapshots(&guards)?,"unchanged":true}),
        )?;
        emit(
            &mut record,
            json!({"event":"png-after","assets":snapshots(&png_guards)?,"unchanged":true}),
        )?;
        emit(
            &mut record,
            json!({"event":"summary","fixture_count":4,"actual_full_calls":4,"actual_roi_calls":4,
            "english_chinese_0_180":true,"negative_global_origin":[-1700,-900],"original_kept":true,
            "roi_byte_exact":true,"clipboard":false,"gui":false,"network_download":false}),
        )?;
        Ok(())
    };
    let result = work();
    if let Err(error) = &result {
        emit(
            &mut record,
            json!({"event":"failure","message":error.to_string()}),
        )?;
    }
    result
}
