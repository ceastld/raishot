//! 从不可变原图重填独占贴图缓冲；这里只保存尺寸/参数/LUT，不持有图片或 Vec。
use crate::{pin_model::Adjustments, Result};

/// 像素数量不变的绝对处理计划；调用方必须先证明 destination 并非共享原图。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OriginalRefillPlan {
    original_size: (usize, usize),
    current_size: (u32, u32),
    output_size: (u32, u32),
    byte_len: usize,
    turns: u8,
    flip_x: bool,
    flip_y: bool,
    grayscale: bool,
    color: [u8; 256],
}

fn byte_len(size: (u32, u32)) -> Result<usize> {
    if size.0 == 0 || size.1 == 0 {
        return Err("贴图重填尺寸必须非零".into());
    }
    (size.0 as usize)
        .checked_mul(size.1 as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "贴图重填像素长度溢出".into())
}

impl OriginalRefillPlan {
    pub(crate) fn new(
        original_size: (u32, u32),
        previous: Adjustments,
        next: Adjustments,
    ) -> Result<Self> {
        let byte_len = byte_len(original_size)?;
        if !(-100..=100).contains(&previous.brightness) || !(-100..=100).contains(&next.brightness)
        {
            return Err("贴图重填亮度参数越界".into());
        }
        let current_size = previous.dimensions(original_size.0, original_size.1);
        let output_size = next.dimensions(original_size.0, original_size.1);
        // 四分之一旋转只交换宽高；私有计划不允许另一种像素长度进入首写之后。
        if self::byte_len(current_size)? != byte_len || self::byte_len(output_size)? != byte_len {
            return Err("贴图重填前后像素数量不同".into());
        }
        let offset = i32::from(next.brightness) * 255 / 100;
        let mut color = [0; 256];
        for (value, channel) in color.iter_mut().enumerate() {
            let value = if next.invert {
                255 - value as i32
            } else {
                value as i32
            };
            *channel = (value + offset).clamp(0, 255) as u8;
        }
        Ok(Self {
            original_size: (original_size.0 as usize, original_size.1 as usize),
            current_size,
            output_size,
            byte_len,
            turns: next.turns % 4,
            flip_x: next.flip_x,
            flip_y: next.flip_y,
            grayscale: next.grayscale,
            color,
        })
    }

    /// Header-fit 合同：成功 validate 后，output W×H×4 checked 且精确等于
    /// destination.len()；image0.25.10 RgbaImage::from_raw 的 fits 条件因此必为真。
    /// 宿主须先检查实际原/旧 Frame 尺寸、保存 left/top/delay，再移动同一 Vec。
    /// 不能把 from_raw(None) 当可恢复错误，它已消费 Vec；成功后必须新建 ImageId。
    pub(crate) fn output_size(&self) -> (u32, u32) {
        self.output_size
    }

    pub(crate) fn validate(
        &self,
        original_bgra: &[u8],
        current_size: (u32, u32),
        current_bgra: &[u8],
    ) -> Result<()> {
        if current_size != self.current_size
            || byte_len(current_size)? != self.byte_len
            || byte_len(self.output_size)? != self.byte_len
            || original_bgra.len() != self.byte_len
            || current_bgra.len() != self.byte_len
        {
            return Err("贴图重填原始或当前像素尺寸错误".into());
        }
        Ok(())
    }

    /// 所有可恢复错误先于首写；此后仅有已验证边界内的采样和完整 BGRA 写入。
    /// 输入必须来自独立不可变原图，不尝试逆转已灰度/裁剪的当前像素。
    pub(crate) fn apply(
        &self,
        original_bgra: &[u8],
        current_size: (u32, u32),
        current_bgra: &mut [u8],
    ) -> Result<()> {
        self.validate(original_bgra, current_size, current_bgra)?;
        let (width, height) = self.original_size;
        let (output_width, output_height) =
            (self.output_size.0 as usize, self.output_size.1 as usize);
        for y in 0..output_height {
            let ry = if self.flip_y {
                output_height - 1 - y
            } else {
                y
            };
            for x in 0..output_width {
                let rx = if self.flip_x { output_width - 1 - x } else { x };
                let (source_x, source_y) = match self.turns {
                    1 => (ry, height - 1 - rx),
                    2 => (width - 1 - rx, height - 1 - ry),
                    3 => (width - 1 - ry, rx),
                    _ => (rx, ry),
                };
                let source = (source_y * width + source_x) * 4;
                let target = (y * output_width + x) * 4;
                let pixel = &original_bgra[source..source + 4];
                if self.grayscale {
                    let gray = ((u32::from(pixel[0]) * 29
                        + u32::from(pixel[1]) * 150
                        + u32::from(pixel[2]) * 77
                        + 128)
                        >> 8) as usize;
                    current_bgra[target..target + 3].fill(self.color[gray]);
                } else {
                    for channel in 0..3 {
                        current_bgra[target + channel] = self.color[pixel[channel] as usize];
                    }
                }
                current_bgra[target + 3] = pixel[3];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pin_model::ProcessingHistory;

    const ORIGINAL: [u8; 24] = [
        0, 10, 240, 0, 255, 127, 1, 1, 17, 200, 90, 127, 254, 250, 230, 200, 128, 64, 32, 254, 13,
        97, 181, 255,
    ];
    // 独立预计算灰度与四个旋转矩阵，不调用生产坐标/色彩方法生成期望。
    const GRAY: [i32; 6] = [78, 104, 146, 244, 62, 113];
    const ROTATED: [[usize; 6]; 4] = [
        [0, 1, 2, 3, 4, 5],
        [3, 0, 4, 1, 5, 2],
        [5, 4, 3, 2, 1, 0],
        [2, 5, 1, 4, 0, 3],
    ];

    fn literal_pixels(state: Adjustments) -> ((u32, u32), Vec<u8>) {
        let turns = state.turns % 4;
        let size = if turns == 1 || turns == 3 {
            (2, 3)
        } else {
            (3, 2)
        };
        let brightness = match state.brightness {
            -100 => -255,
            -1 => -2,
            0 => 0,
            1 => 2,
            100 => 255,
            _ => panic!("literal oracle 只接受显式列出的亮度"),
        };
        let mut output = Vec::with_capacity(ORIGINAL.len());
        for y in 0..size.1 {
            for x in 0..size.0 {
                let row = if state.flip_y { size.1 - y - 1 } else { y };
                let column = if state.flip_x { size.0 - x - 1 } else { x };
                let token = ROTATED[turns as usize][(row * size.0 + column) as usize];
                for channel in 0..3 {
                    let value = if state.grayscale {
                        GRAY[token]
                    } else {
                        i32::from(ORIGINAL[token * 4 + channel])
                    };
                    let value = if state.invert { 255 - value } else { value };
                    output.push((value + brightness).clamp(0, 255) as u8);
                }
                output.push(ORIGINAL[token * 4 + 3]);
            }
        }
        (size, output)
    }

    #[test]
    fn pin_original_refill_absolute_quarter_flips_color_and_alpha_match_literal_pixels() {
        let original_before = ORIGINAL;
        for previous_turns in 0..4 {
            let previous = Adjustments {
                turns: previous_turns,
                grayscale: true,
                brightness: 100,
                ..Default::default()
            };
            let current_size = if previous_turns == 1 || previous_turns == 3 {
                (2, 3)
            } else {
                (3, 2)
            };
            for turns in 0..4 {
                for flip_x in [false, true] {
                    for flip_y in [false, true] {
                        for grayscale in [false, true] {
                            for invert in [false, true] {
                                for brightness in [-100, -1, 0, 1, 100] {
                                    let next = Adjustments {
                                        turns,
                                        flip_x,
                                        flip_y,
                                        grayscale,
                                        invert,
                                        brightness,
                                    };
                                    let plan =
                                        OriginalRefillPlan::new((3, 2), previous, next).unwrap();
                                    let (expected_size, expected) = literal_pixels(next);
                                    let mut current = vec![0xD5; ORIGINAL.len()];
                                    plan.apply(&ORIGINAL, current_size, &mut current).unwrap();
                                    assert_eq!(plan.output_size(), expected_size);
                                    assert_eq!(current, expected, "{previous:?} -> {next:?}");
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(ORIGINAL, original_before);
    }

    #[test]
    fn pin_original_refill_invalid_inputs_leave_destination_unchanged() {
        for original_size in [(0, 2), (3, 0), (u32::MAX, u32::MAX)] {
            assert!(OriginalRefillPlan::new(
                original_size,
                Adjustments::default(),
                Adjustments::default()
            )
            .is_err());
        }
        for brightness in [i16::MIN, -101, 101, i16::MAX] {
            let invalid = Adjustments {
                brightness,
                ..Default::default()
            };
            assert!(OriginalRefillPlan::new((3, 2), invalid, Adjustments::default()).is_err());
            assert!(OriginalRefillPlan::new((3, 2), Adjustments::default(), invalid).is_err());
        }
        let plan = OriginalRefillPlan::new(
            (3, 2),
            Adjustments::default(),
            Adjustments {
                turns: 1,
                ..Default::default()
            },
        )
        .unwrap();
        for size in [(0, 2), (3, 0), (2, 3), (4, 2), (u32::MAX, u32::MAX)] {
            let mut current = vec![0xA3; ORIGINAL.len()];
            let before = current.clone();
            assert!(plan.apply(&ORIGINAL, size, &mut current).is_err());
            assert_eq!(current, before);
        }
        for source_length in [0, 1, 23, 25] {
            let source = vec![0xE1; source_length];
            let mut current = vec![0xA3; ORIGINAL.len()];
            let before = current.clone();
            assert!(plan.apply(&source, (3, 2), &mut current).is_err());
            assert_eq!(current, before);
        }
        for destination_length in [0, 1, 23, 25] {
            let mut current = vec![0xA3; destination_length];
            let before = current.clone();
            assert!(plan.apply(&ORIGINAL, (3, 2), &mut current).is_err());
            assert_eq!(current, before);
        }
    }

    fn refill_without_allocation(
        previous: Adjustments,
        next: Adjustments,
        current_size: &mut (u32, u32),
        current: &mut Vec<u8>,
    ) {
        let pointer = current.as_ptr();
        let length = current.len();
        let capacity = current.capacity();
        let plan = OriginalRefillPlan::new((3, 2), previous, next).unwrap();
        plan.apply(&ORIGINAL, *current_size, current).unwrap();
        *current_size = plan.output_size();
        assert_eq!(current.as_ptr(), pointer);
        assert_eq!(current.len(), length);
        assert_eq!(current.capacity(), capacity);
        let (expected_size, expected) = literal_pixels(next);
        assert_eq!(*current_size, expected_size);
        assert_eq!(*current, expected);
    }

    #[test]
    fn pin_original_refill_reuses_allocation_and_undo_reads_original_after_clipping() {
        let mut current = Vec::with_capacity(ORIGINAL.len() + 64);
        current.extend_from_slice(&ORIGINAL);
        let mut current_size = (3, 2);
        let mut state = Adjustments::default();
        let mut history = ProcessingHistory::default();
        let clipped = Adjustments {
            grayscale: true,
            brightness: 100,
            ..Default::default()
        };
        let previous = state;
        assert!(history.change(&mut state, clipped));
        refill_without_allocation(previous, state, &mut current_size, &mut current);
        assert!(current.chunks_exact(4).all(|pixel| pixel[..3] == [255; 3]));
        let previous = state;
        assert!(history.undo(&mut state));
        refill_without_allocation(previous, state, &mut current_size, &mut current);
        assert_eq!(current, ORIGINAL, "不可逆裁剪后的 Undo 必须读取原图");
        let previous = state;
        assert!(history.redo(&mut state));
        refill_without_allocation(previous, state, &mut current_size, &mut current);
        let previous = state;
        assert!(history.change(
            &mut state,
            Adjustments {
                turns: 1,
                flip_y: true,
                invert: true,
                brightness: -100,
                ..Default::default()
            }
        ));
        refill_without_allocation(previous, state, &mut current_size, &mut current);
        assert_eq!(current_size, (2, 3));
        assert!(current.chunks_exact(4).all(|pixel| pixel[..3] == [0; 3]));
        let previous = state;
        assert!(history.undo(&mut state));
        refill_without_allocation(previous, state, &mut current_size, &mut current);
        assert_eq!(current_size, (3, 2));
        let previous = state;
        assert!(history.redo(&mut state));
        refill_without_allocation(previous, state, &mut current_size, &mut current);
        assert_eq!(current_size, (2, 3));
    }
}
