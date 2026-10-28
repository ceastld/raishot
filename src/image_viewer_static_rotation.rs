//! 静态显示旋转整批执行；只接受归一后的原始图块，成功后由宿主一次提交。
use crate::{
    geometry::{Rect, Rotation},
    image_decode::MAX_DECODE_BYTES,
};
use gpui::RenderImage;
use std::sync::{atomic::Ordering, Arc};

pub(super) fn rotate(
    request: &super::RotationRequest,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Vec<(Rect, Arc<RenderImage>)>, String> {
    let mut cancelled = || request.cancel.load(Ordering::Relaxed) || cancelled();
    check_cancel(&mut cancelled)?;
    let (width, height) = request.projection.source_dimensions();
    let expected_total = u64::from(width) * u64::from(height) * 4;
    let mut total = 0u64;
    for (index, (rect, image)) in request.tiles.iter().enumerate() {
        check_cancel(&mut cancelled)?;
        request
            .projection
            .source_rect_to_display(*rect)
            .ok_or_else(invalid_tiles)?;
        let pixels = image.as_bytes(0).ok_or_else(invalid_tiles)?;
        let dimensions = image.size(0);
        let expected = u64::from(rect.width) * u64::from(rect.height) * 4;
        if u32::from(dimensions.width) != rect.width
            || u32::from(dimensions.height) != rect.height
            || pixels.len() as u64 != expected
        {
            return Err(invalid_tiles());
        }
        // 界内、不重叠且面积相同，才能证明图块覆盖完整原图，避免重复块掩盖缺块。
        if request.tiles[..index].iter().any(|(previous, _)| {
            rect.x < previous.x + previous.width
                && previous.x < rect.x + rect.width
                && rect.y < previous.y + previous.height
                && previous.y < rect.y + rect.height
        }) {
            return Err(invalid_tiles());
        }
        total = total.checked_add(expected).ok_or_else(invalid_tiles)?;
        if total > expected_total {
            return Err(invalid_tiles());
        }
    }
    if total != expected_total {
        return Err(invalid_tiles());
    }
    let identity = matches!(request.projection.rotation(), Rotation::Identity);
    // Identity只增加Arc引用，不分配新像素；其它方向预留整批输出后才开始分配。
    let new_bytes = if identity { 0 } else { expected_total };
    if request
        .resident_bytes
        .checked_add(new_bytes)
        .is_none_or(|bytes| bytes > MAX_DECODE_BYTES)
    {
        return Err(tr!(
            "图片旋转超过总驻留预算",
            "Image rotation exceeds the resident budget"
        )
        .into());
    }
    check_cancel(&mut cancelled)?;
    let mut output = Vec::new();
    output.try_reserve_exact(request.tiles.len()).map_err(|_| {
        tr!(
            "图片旋转图块分配失败",
            "Image rotation tile allocation failed"
        )
        .to_string()
    })?;
    for (rect, image) in &request.tiles {
        check_cancel(&mut cancelled)?;
        if identity {
            output.push((*rect, image.clone()));
            continue;
        }
        let tile = request.projection.rotate_tile(
            *rect,
            image.as_bytes(0).ok_or_else(invalid_tiles)?,
            MAX_DECODE_BYTES,
            &mut cancelled,
        )?;
        // 来源已是显示图块；直接封装整数排列后的BGRA，不能再叠一次透明棋盘格。
        let image =
            crate::canvas_source::render_image(tile.rect.width, tile.rect.height, tile.bgra);
        output.push((tile.rect, image));
    }
    check_cancel(&mut cancelled)?;
    Ok(output)
}

fn check_cancel(cancelled: &mut impl FnMut() -> bool) -> Result<(), String> {
    if cancelled() {
        Err(tr!("图片旋转已取消", "Image rotation canceled").into())
    } else {
        Ok(())
    }
}

fn invalid_tiles() -> String {
    tr!(
        "图片旋转源图块无效",
        "Invalid source tiles for image rotation"
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_viewer_rotation::ImageRotation;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn image_viewer_static_rotation_whole_output_budget_and_identity_reuse() {
        let rect = Rect::full(2, 1);
        let bytes = vec![11, 22, 33, 44, 55, 66, 77, 88];
        let image = crate::canvas_source::render_image(2, 1, bytes.clone());
        let mut request = super::super::RotationRequest {
            serial: 1,
            epoch: 1,
            projection: ImageRotation::new((2, 1), Rotation::R90).unwrap(),
            tiles: vec![(rect, image.clone())],
            resident_bytes: MAX_DECODE_BYTES - 8,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let rotated = rotate(&request, || false).unwrap();
        assert_eq!(rotated[0].0, Rect::full(1, 2));
        assert_eq!(rotated[0].1.as_bytes(0).unwrap(), bytes);
        request.resident_bytes += 1;
        assert!(rotate(&request, || false).is_err());
        request.projection = ImageRotation::new((2, 1), Rotation::Identity).unwrap();
        request.resident_bytes = MAX_DECODE_BYTES;
        let unchanged = rotate(&request, || false).unwrap();
        assert_eq!(unchanged[0].0, rect);
        assert!(Arc::ptr_eq(&unchanged[0].1, &image));
    }
}
