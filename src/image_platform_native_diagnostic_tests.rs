//! 原始与单图重建输入使用相同系统操作，定位容器兼容与原生像素交付的失败边界。
use super::*;

#[derive(Clone, Copy)]
enum PixelRequest {
    BgraStraight,
    BgraIgnore,
    RgbaStraight,
    NativeParameters,
}

#[derive(Debug)]
struct WhitePixel {
    bytes: [u8; 4],
    alpha_mode: BitmapAlphaMode,
}

fn white_pixels(
    bytes: &[u8],
    request: PixelRequest,
    cancel: &AtomicBool,
) -> Result<WhitePixel, PlatformError> {
    // 本诊断只读固定1×1样本；在任何系统像素请求前限制输入与返回尺寸。
    if bytes.len() > 4096 {
        return Err(budget());
    }
    let memory = call(cancel, || {
        unsafe { SHCreateMemStream(Some(bytes)) }.ok_or(PlatformError::Native(E_OUTOFMEMORY.0))
    })?;
    let stream: IRandomAccessStream = native_at("diagnostic stream", cancel, || unsafe {
        CreateRandomAccessStreamOverStream(&memory, BSOS_DEFAULT)
    })?;
    let operation = native_at("diagnostic decoder create", cancel, || {
        BitmapDecoder::CreateAsync(&stream)
    })?;
    let decoder = native_at("diagnostic decoder completion", cancel, || operation.get())?;
    drop(operation);
    if native_at("diagnostic frame count", cancel, || decoder.FrameCount())? != 1 {
        return Err(invalid(
            "AVIF诊断样本必须只有一帧",
            "The AVIF diagnostic sample must have one frame",
        ));
    }
    let operation = native_at("diagnostic frame open", cancel, || decoder.GetFrameAsync(0))?;
    let frame = native_at("diagnostic frame completion", cancel, || operation.get())?;
    drop(operation);
    let dimensions = (
        native_at("diagnostic width", cancel, || frame.PixelWidth())?,
        native_at("diagnostic height", cancel, || frame.PixelHeight())?,
    );
    if dimensions != (1, 1) {
        return Err(invalid(
            "AVIF诊断样本必须为1×1像素",
            "The AVIF diagnostic sample must be 1x1 pixels",
        ));
    }
    let native_format = native_at("diagnostic native format", cancel, || {
        frame.BitmapPixelFormat()
    })?;
    let native_alpha = native_at("diagnostic native alpha", cancel, || {
        frame.BitmapAlphaMode()
    })?;
    eprintln!("AVIF diagnostic native format={native_format:?} alpha={native_alpha:?}");
    let (format, alpha) = match request {
        PixelRequest::BgraStraight => (BitmapPixelFormat::Bgra8, BitmapAlphaMode::Straight),
        PixelRequest::BgraIgnore => (BitmapPixelFormat::Bgra8, BitmapAlphaMode::Ignore),
        PixelRequest::RgbaStraight => (BitmapPixelFormat::Rgba8, BitmapAlphaMode::Straight),
        PixelRequest::NativeParameters => {
            // 白色各通道对称；仅允许这两种明确的四字节布局，不猜其它格式。
            if ![BitmapPixelFormat::Bgra8, BitmapPixelFormat::Rgba8].contains(&native_format) {
                return Err(invalid(
                    "AVIF诊断原生格式不是四通道八位布局",
                    "The AVIF diagnostic native format is not a four-channel 8-bit layout",
                ));
            }
            if ![
                BitmapAlphaMode::Straight,
                BitmapAlphaMode::Premultiplied,
                BitmapAlphaMode::Ignore,
            ]
            .contains(&native_alpha)
            {
                return Err(invalid(
                    "AVIF诊断原生alpha模式无效",
                    "The AVIF diagnostic native alpha mode is invalid",
                ));
            }
            (native_format, native_alpha)
        }
    };
    let transform = native_at("diagnostic transform", cancel, BitmapTransform::new)?;
    let operation = native_at("diagnostic pixel request", cancel, || {
        frame.GetPixelDataTransformedAsync(
            format,
            alpha,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
    })?;
    let provider = native_at("diagnostic pixel completion", cancel, || operation.get())?;
    drop(operation);
    let pixels = native_at("diagnostic pixel detach", cancel, || {
        provider.DetachPixelData()
    })?;
    if pixels.len() != 4 {
        return Err(invalid(
            "AVIF诊断样本必须返回四字节像素",
            "The AVIF diagnostic sample must return a four-byte pixel",
        ));
    }
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&pixels);
    Ok(WhitePixel {
        bytes,
        alpha_mode: alpha,
    })
}

#[test]
#[ignore = "原始与冻结AVIF的真实系统像素诊断；需显式运行，无窗口"]
fn image_platform_native_avif_original_and_frozen_white_diagnostic() {
    let bytes = include_bytes!("../tests/data/image_platform/white_1x1.avif");
    let cancel = AtomicBool::new(false);
    let plan = preflight::inspect(bytes, 4, &cancel).unwrap();
    let frozen = plan.freeze_item(0, &cancel).unwrap();
    assert!(bytes.len() <= 4096 && frozen.bytes.len() <= 4096);
    let _apartment = Apartment::initialize(&cancel).unwrap();
    let mut results = Vec::new();
    for (view, input) in [("original", &bytes[..]), ("frozen", &frozen.bytes[..])] {
        for (parameters, request) in [
            ("bgra-straight", PixelRequest::BgraStraight),
            ("bgra-ignore", PixelRequest::BgraIgnore),
            ("rgba-straight", PixelRequest::RgbaStraight),
            ("native-parameters", PixelRequest::NativeParameters),
        ] {
            eprintln!(
                "AVIF diagnostic view={view} request={parameters} bytes={}",
                input.len()
            );
            let result = white_pixels(input, request, &cancel);
            eprintln!("AVIF diagnostic view={view} request={parameters} result={result:?}");
            results.push((view, parameters, result));
        }
    }
    // 不把任一原生失败转换成条件可用或成功；每项必须交出与请求模式相符的白色像素。
    for (view, parameters, result) in results {
        let pixel = result.unwrap_or_else(|error| panic!("AVIF {view}/{parameters}: {error}"));
        // Ignore按不透明解释第四字节，API允许其存储为0；原Straight强门仍要求255。
        if pixel.alpha_mode != BitmapAlphaMode::Ignore {
            assert_eq!(pixel.bytes[3], 255, "AVIF {view}/{parameters} alpha");
        }
        assert!(
            pixel.bytes[..3].iter().all(|value| *value >= 250),
            "AVIF {view}/{parameters} color: {pixel:?}"
        );
    }
}
