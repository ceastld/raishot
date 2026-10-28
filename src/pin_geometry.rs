//! 已处理贴图的相对几何变换；仅独占像素可原地复用，原图与共享快照由宿主保护。
use crate::{pin_model::Adjustments, Result};

/// 尺寸不变的像素排列。计划不持有图片，也不改变已处理的颜色和 alpha。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GeometryPermutation {
    width: u32,
    height: u32,
    byte_len: usize,
    mirror_x: bool,
    mirror_y: bool,
}

impl GeometryPermutation {
    /// 从旧处理参数求当前输出坐标上的相对变换；无法复用时保留原来的重建路径。
    pub(crate) fn between(
        previous: Adjustments,
        next: Adjustments,
        original_size: (u32, u32),
    ) -> Result<Option<Self>> {
        let (original_width, original_height) = original_size;
        if original_width == 0 || original_height == 0 {
            return Err("贴图几何原始尺寸错误".into());
        }
        let byte_len = (original_width as usize)
            .checked_mul(original_height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("贴图几何像素长度溢出")?;
        let delta = (next.turns % 4 + 4 - previous.turns % 4) % 4;
        if previous.grayscale != next.grayscale
            || previous.invert != next.invert
            || previous.brightness != next.brightness
            || !delta.is_multiple_of(2)
        {
            // 即使是方图，90/270 度仍不走这一只支持两轴交换的路径。
            return Ok(None);
        }
        let (width, height) = previous.dimensions(original_width, original_height);
        // 原变换先旋转再按输出坐标翻转：Fnext * Rdelta * Fprevious。
        // 180 度与两轴翻转可交换，因此相对变换只需对各轴做异或。
        let half_turn = delta == 2;
        Ok(Some(Self {
            width,
            height,
            byte_len,
            mirror_x: previous.flip_x ^ next.flip_x ^ half_turn,
            mirror_y: previous.flip_y ^ next.flip_y ^ half_turn,
        }))
    }

    /// 全部拒绝条件必须在像素第一次写入之前检查。
    pub(crate) fn validate(&self, width: u32, height: u32, bgra: &[u8]) -> Result<()> {
        let byte_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("贴图几何像素长度溢出")?;
        if width == 0
            || height == 0
            || width != self.width
            || height != self.height
            || byte_len != self.byte_len
            || bgra.len() != self.byte_len
        {
            return Err("贴图几何当前像素尺寸错误".into());
        }
        Ok(())
    }

    /// 验证后零分配交换完整 BGRA 像素，失败不改输入；图片元数据仍由调用方保留。
    pub(crate) fn apply(&self, width: u32, height: u32, bgra: &mut [u8]) -> Result<()> {
        self.validate(width, height, bgra)?;
        if self.mirror_x && self.mirror_y {
            let pixels = self.byte_len / 4;
            for left in 0..pixels / 2 {
                swap_pixels(bgra, left, pixels - 1 - left);
            }
        } else if self.mirror_x {
            let row_bytes = self.byte_len / self.height as usize;
            for row in bgra.chunks_exact_mut(row_bytes) {
                for left in 0..self.width as usize / 2 {
                    swap_pixels(row, left, self.width as usize - 1 - left);
                }
            }
        } else if self.mirror_y {
            let row_bytes = self.byte_len / self.height as usize;
            for top_row in 0..self.height as usize / 2 {
                let top = top_row * row_bytes;
                let bottom = (self.height as usize - 1 - top_row) * row_bytes;
                let (before_bottom, from_bottom) = bgra.split_at_mut(bottom);
                before_bottom[top..top + row_bytes].swap_with_slice(&mut from_bottom[..row_bytes]);
            }
        }
        Ok(())
    }
}

fn swap_pixels(bgra: &mut [u8], left: usize, right: usize) {
    for channel in 0..4 {
        bgra.swap(left * 4 + channel, right * 4 + channel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pin_model::ProcessingHistory;

    fn source_pixels(width: u32, height: u32) -> Vec<u8> {
        (0..width * height)
            .flat_map(|token| {
                let token = token as u8;
                [
                    20 + token,
                    80 + token * 2,
                    140 + token * 3,
                    [0, 1, 127, 200, 255][token as usize % 5],
                ]
            })
            .collect()
    }

    fn brightened(source: &[u8]) -> Vec<u8> {
        source
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0] + 25, pixel[1] + 25, pixel[2] + 25, pixel[3]])
            .collect()
    }

    /// 独立从目标坐标反查原像素；不使用 production.apply 或相对变换的异或公式。
    fn projected(
        source: &[u8],
        width: u32,
        height: u32,
        state: Adjustments,
    ) -> ((u32, u32), Vec<u8>) {
        let turns = state.turns % 4;
        let (output_width, output_height) = if turns == 1 || turns == 3 {
            (height, width)
        } else {
            (width, height)
        };
        let mut output = Vec::with_capacity(source.len());
        for y in 0..output_height {
            for x in 0..output_width {
                let rx = if state.flip_x {
                    output_width - 1 - x
                } else {
                    x
                };
                let ry = if state.flip_y {
                    output_height - 1 - y
                } else {
                    y
                };
                let (source_x, source_y) = match turns {
                    1 => (ry, height - 1 - rx),
                    2 => (width - 1 - rx, height - 1 - ry),
                    3 => (width - 1 - ry, rx),
                    _ => (rx, ry),
                };
                let offset = ((source_y * width + source_x) * 4) as usize;
                output.extend_from_slice(&source[offset..offset + 4]);
            }
        }
        ((output_width, output_height), output)
    }

    fn assert_token_matrix(actual: &[u8], processed_source: &[u8], tokens: &[usize]) {
        let expected: Vec<_> = tokens
            .iter()
            .flat_map(|token| processed_source[token * 4..token * 4 + 4].iter().copied())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn pin_geometry_buffer_odd_and_single_axis_permutations_keep_allocation() {
        for original_size in [(3, 5), (5, 3), (7, 1), (1, 7), (1, 1)] {
            let source = source_pixels(original_size.0, original_size.1);
            let original = source.clone();
            let processed_source = brightened(&source);
            for turns in 0..4 {
                for flip_x in [false, true] {
                    for flip_y in [false, true] {
                        let previous = Adjustments {
                            turns,
                            flip_x,
                            flip_y,
                            brightness: 10,
                            ..Default::default()
                        };
                        for delta in [0, 2] {
                            for next_x in [false, true] {
                                for next_y in [false, true] {
                                    let next = Adjustments {
                                        turns: (turns + delta) % 4,
                                        flip_x: next_x,
                                        flip_y: next_y,
                                        ..previous
                                    };
                                    let (size, mut actual) = projected(
                                        &processed_source,
                                        original_size.0,
                                        original_size.1,
                                        previous,
                                    );
                                    let (expected_size, expected) = projected(
                                        &processed_source,
                                        original_size.0,
                                        original_size.1,
                                        next,
                                    );
                                    let pointer = actual.as_ptr();
                                    let capacity = actual.capacity();
                                    let plan =
                                        GeometryPermutation::between(previous, next, original_size)
                                            .unwrap()
                                            .unwrap();
                                    plan.validate(size.0, size.1, &actual).unwrap();
                                    plan.apply(size.0, size.1, &mut actual).unwrap();
                                    assert_eq!(size, expected_size);
                                    assert_eq!(actual, expected);
                                    assert_eq!(actual.as_ptr(), pointer);
                                    assert_eq!(actual.capacity(), capacity);
                                    assert_eq!(source, original);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 90 度后的当前轴必须正确；固定矩阵锚点独立于上面的坐标 oracle。
        let source = brightened(&source_pixels(3, 5));
        let previous = Adjustments {
            turns: 1,
            flip_x: true,
            brightness: 10,
            ..Default::default()
        };
        let (_, mut actual) = projected(&source, 3, 5, previous);
        assert_token_matrix(
            &actual,
            &source,
            &[0, 3, 6, 9, 12, 1, 4, 7, 10, 13, 2, 5, 8, 11, 14],
        );
        let next = Adjustments {
            turns: 3,
            ..previous
        };
        GeometryPermutation::between(previous, next, (3, 5))
            .unwrap()
            .unwrap()
            .apply(5, 3, &mut actual)
            .unwrap();
        assert_token_matrix(
            &actual,
            &source,
            &[14, 11, 8, 5, 2, 13, 10, 7, 4, 1, 12, 9, 6, 3, 0],
        );
    }

    #[test]
    fn pin_geometry_buffer_rejects_changes_and_invalid_pixels_without_mutation() {
        let previous = Adjustments {
            brightness: 10,
            ..Default::default()
        };
        for next in [
            Adjustments {
                grayscale: true,
                ..previous
            },
            Adjustments {
                invert: true,
                ..previous
            },
            Adjustments {
                brightness: 11,
                ..previous
            },
            Adjustments {
                turns: 1,
                ..previous
            },
            Adjustments {
                turns: 3,
                ..previous
            },
        ] {
            for original_size in [(3, 5), (3, 3)] {
                assert!(GeometryPermutation::between(previous, next, original_size)
                    .unwrap()
                    .is_none());
            }
        }
        for original_size in [(0, 3), (3, 0), (u32::MAX, u32::MAX)] {
            assert!(GeometryPermutation::between(previous, previous, original_size).is_err());
        }

        let previous = Adjustments {
            turns: 1,
            ..previous
        };
        let next = Adjustments {
            flip_x: true,
            ..previous
        };
        let plan = GeometryPermutation::between(previous, next, (3, 5))
            .unwrap()
            .unwrap();
        for (width, height, length) in [
            (0, 3, 60),
            (5, 0, 60),
            (3, 5, 60),
            (5, 3, 59),
            (5, 3, 61),
            (u32::MAX, u32::MAX, 60),
        ] {
            let mut pixels = vec![73; length];
            let before = pixels.clone();
            let pointer = pixels.as_ptr();
            let capacity = pixels.capacity();
            assert!(plan.validate(width, height, &pixels).is_err());
            assert!(plan.apply(width, height, &mut pixels).is_err());
            assert_eq!(pixels, before);
            assert_eq!(pixels.as_ptr(), pointer);
            assert_eq!(pixels.capacity(), capacity);
        }
    }

    #[test]
    fn pin_geometry_buffer_processing_history_undo_redo_preserves_processed_channels() {
        let source = source_pixels(3, 5);
        let original = source.clone();
        let processed_source = brightened(&source);
        let mut actual = processed_source.clone();
        let pointer = actual.as_ptr();
        let capacity = actual.capacity();
        let identity = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
        let horizontal = [2, 1, 0, 5, 4, 3, 8, 7, 6, 11, 10, 9, 14, 13, 12];
        let vertical = [12, 13, 14, 9, 10, 11, 6, 7, 8, 3, 4, 5, 0, 1, 2];
        let mut history = ProcessingHistory::default();
        let mut current = Adjustments::default();
        let bright = Adjustments {
            brightness: 10,
            ..current
        };
        assert!(GeometryPermutation::between(current, bright, (3, 5))
            .unwrap()
            .is_none());
        assert!(history.change(&mut current, bright));

        for (next, expected) in [
            (
                Adjustments {
                    flip_x: true,
                    ..bright
                },
                horizontal,
            ),
            (
                Adjustments {
                    flip_x: true,
                    ..bright
                }
                .rotated(2),
                vertical,
            ),
            (
                Adjustments {
                    turns: 2,
                    flip_x: true,
                    flip_y: true,
                    ..bright
                },
                identity,
            ),
        ] {
            let previous = current;
            assert!(history.change(&mut current, next));
            GeometryPermutation::between(previous, current, (3, 5))
                .unwrap()
                .unwrap()
                .apply(3, 5, &mut actual)
                .unwrap();
            assert_token_matrix(&actual, &processed_source, &expected);
            assert_eq!(current.brightness, 10);
        }
        for expected in [vertical, horizontal, identity] {
            let previous = current;
            assert!(history.undo(&mut current));
            GeometryPermutation::between(previous, current, (3, 5))
                .unwrap()
                .unwrap()
                .apply(3, 5, &mut actual)
                .unwrap();
            assert_token_matrix(&actual, &processed_source, &expected);
        }
        for expected in [horizontal, vertical, identity] {
            let previous = current;
            assert!(history.redo(&mut current));
            GeometryPermutation::between(previous, current, (3, 5))
                .unwrap()
                .unwrap()
                .apply(3, 5, &mut actual)
                .unwrap();
            assert_token_matrix(&actual, &processed_source, &expected);
        }
        assert_eq!(actual.as_ptr(), pointer);
        assert_eq!(actual.capacity(), capacity);
        assert_eq!(source, original);
        assert_eq!(current.brightness, 10);
    }

    #[test]
    fn pin_geometry_buffer_render_frame_consumption_preserves_pixels_and_metadata() {
        let static_pixels = [2, 7, 11, 0, 17, 23, 29, 127, 31, 37, 41, 255];
        let offset_pixels = [
            3, 5, 7, 0, 11, 13, 17, 1, 19, 23, 29, 127, 31, 37, 41, 200, 43, 47, 53, 254, 59, 61,
            67, 255,
        ];
        for (width, height, left, top, delay, expected) in [
            (
                3,
                1,
                0,
                0,
                image::Delay::from_numer_denom_ms(0, 1),
                &static_pixels[..],
            ),
            (
                2,
                3,
                7,
                11,
                image::Delay::from_numer_denom_ms(13, 7),
                &offset_pixels[..],
            ),
        ] {
            let pixels = expected.to_vec();
            let pointer = pixels.as_ptr();
            let buffer = image::RgbaImage::from_raw(width, height, pixels).unwrap();
            let frame = if left == 0 && top == 0 {
                image::Frame::new(buffer)
            } else {
                image::Frame::from_parts(buffer, left, top, delay)
            };
            let original = gpui::RenderImage::new(vec![frame]);
            let original_id = original.id;
            let frame = match original.try_into_single_frame() {
                Ok(frame) => frame,
                Err(_) => panic!("合法单帧不应被拒绝"),
            };
            assert_eq!(frame.buffer().as_raw().as_ptr(), pointer);
            assert_eq!(frame.buffer().as_raw().as_slice(), expected);
            assert_eq!(frame.buffer().dimensions(), (width, height));
            assert_eq!(
                (frame.left(), frame.top(), frame.delay()),
                (left, top, delay)
            );
            let replacement = gpui::RenderImage::new(vec![frame]);
            assert_ne!(replacement.id, original_id);
            assert_eq!(replacement.frame_count(), 1);
            assert_eq!(replacement.as_bytes(0).unwrap().as_ptr(), pointer);
            let frame = match replacement.try_into_single_frame() {
                Ok(frame) => frame,
                Err(_) => panic!("重新包装不能丢失单帧格式"),
            };
            assert_eq!(frame.buffer().as_raw().as_ptr(), pointer);
            assert_eq!(frame.buffer().as_raw().as_slice(), expected);
            assert_eq!(frame.buffer().dimensions(), (width, height));
            assert_eq!(
                (frame.left(), frame.top(), frame.delay()),
                (left, top, delay)
            );
        }

        let first_delay = image::Delay::from_numer_denom_ms(17, 5);
        let second_delay = image::Delay::from_numer_denom_ms(29, 7);
        let first_buffer = image::RgbaImage::from_raw(3, 1, static_pixels.to_vec()).unwrap();
        let second_buffer = image::RgbaImage::from_raw(2, 3, offset_pixels.to_vec()).unwrap();
        let first_pointer = first_buffer.as_raw().as_ptr();
        let second_pointer = second_buffer.as_raw().as_ptr();
        let multiple = gpui::RenderImage::new(vec![
            image::Frame::from_parts(first_buffer, 19, 23, first_delay),
            image::Frame::from_parts(second_buffer, 31, 37, second_delay),
        ]);
        let multiple_id = multiple.id;
        let rejected = match multiple.try_into_single_frame() {
            Err(image) => image,
            Ok(_) => panic!("多帧必须完整返回原 RenderImage"),
        };
        assert_eq!(rejected.id, multiple_id);
        assert_eq!(rejected.frame_count(), 2);
        for (index, width, height, delay, pointer, expected) in [
            (0, 3, 1, first_delay, first_pointer, &static_pixels[..]),
            (1, 2, 3, second_delay, second_pointer, &offset_pixels[..]),
        ] {
            let bytes = rejected.as_bytes(index).unwrap();
            let size = rejected.size(index);
            assert_eq!(bytes.as_ptr(), pointer);
            assert_eq!(bytes, expected);
            assert_eq!(
                (u32::from(size.width), u32::from(size.height)),
                (width, height)
            );
            assert_eq!(rejected.delay(index), delay);
        }
        // 多帧 offset 没有公开访问器；拒绝路径直接 Err(self)，不取帧、不改字段。
        let empty = gpui::RenderImage::new(Vec::<image::Frame>::new());
        let empty_id = empty.id;
        let rejected = match empty.try_into_single_frame() {
            Err(image) => image,
            Ok(_) => panic!("空图必须返回原 RenderImage"),
        };
        // 空图不能调用会访问第零帧的 Debug/size；这里只检查公开的安全字段。
        assert_eq!(rejected.id, empty_id);
        assert_eq!(rejected.frame_count(), 0);
        assert!(rejected.as_bytes(0).is_none());
    }
}
