//! 同源画质试验的固定 8 位 BGRA 素材：自绘 ASCII、细线、滚动和简单运动。
//! 不依赖字体、截图或主题；合成字形不覆盖真实复杂文字、完整桌面、抗锯齿或 HDR。
use crate::Result;

const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;
const FPS: u32 = 30;
const SEGMENT_FRAMES: u32 = FPS;
const FRAME_COUNT: u32 = SEGMENT_FRAMES * 3;
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 4;
const SCROLL_STEP: u32 = 3;
type Color = [u8; 4];

pub(crate) const fn width() -> u32 {
    WIDTH
}
pub(crate) const fn height() -> u32 {
    HEIGHT
}
pub(crate) const fn fps() -> u32 {
    FPS
}
pub(crate) const fn frame_count() -> u32 {
    FRAME_COUNT
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Segment {
    StaticText,
    ScrollingText,
    Motion,
}
impl Segment {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::StaticText => "static_text",
            Self::ScrollingText => "scrolling_text",
            Self::Motion => "motion",
        }
    }
}

pub(crate) fn segment(index: u32) -> Option<Segment> {
    match index {
        index if index < SEGMENT_FRAMES => Some(Segment::StaticText),
        index if index < SEGMENT_FRAMES * 2 => Some(Segment::ScrollingText),
        index if index < FRAME_COUNT => Some(Segment::Motion),
        _ => None,
    }
}

/// 单次只分配一张 960×540 的 top-down BGRA 帧；越界在分配前拒绝。
pub(crate) fn sample(index: u32) -> Result<Vec<u8>> {
    let Some(part) = segment(index) else {
        return Err(trf!(
            "合成画质样本帧 {index} 超出范围 0..{FRAME_COUNT}",
            "Quality sample frame {index} is outside 0..{FRAME_COUNT}"
        )
        .into());
    };
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(FRAME_BYTES).map_err(|_| {
        tr!(
            "无法分配合成画质样本帧",
            "Cannot allocate a quality sample frame"
        )
    })?;
    pixels.resize(FRAME_BYTES, 0);
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&rgb(22, 25, 30));
    }
    let scroll = if part == Segment::ScrollingText {
        (index - SEGMENT_FRAMES) * SCROLL_STEP
    } else {
        0
    };
    draw_page(&mut pixels, scroll as i32);
    if part == Segment::Motion {
        draw_motion(&mut pixels, index - SEGMENT_FRAMES * 2);
    }
    Ok(pixels)
}

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    [blue, green, red, 255]
}

fn pixel(pixels: &mut [u8], x: i32, y: i32, color: Color) {
    if x >= 0 && x < WIDTH as i32 && y >= 0 && y < HEIGHT as i32 {
        let start = (y as usize * WIDTH as usize + x as usize) * 4;
        pixels[start..start + 4].copy_from_slice(&color);
    }
}

fn rectangle(pixels: &mut [u8], x: i32, y: i32, width: i32, height: i32, color: Color) {
    if width <= 0 || height <= 0 {
        return;
    }
    let left = x.clamp(0, WIDTH as i32);
    let right = x.saturating_add(width).clamp(0, WIDTH as i32);
    let top = y.clamp(0, HEIGHT as i32);
    let bottom = y.saturating_add(height).clamp(0, HEIGHT as i32);
    for row in top..bottom {
        let start = (row as usize * WIDTH as usize + left as usize) * 4;
        let end = (row as usize * WIDTH as usize + right as usize) * 4;
        for pixel in pixels[start..end].chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }
}

fn text(pixels: &mut [u8], value: &str, mut x: i32, y: i32, scale: i32, color: Color) {
    for byte in value.bytes() {
        for (row, bits) in glyph(byte).into_iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    rectangle(
                        pixels,
                        x + column * scale,
                        y + row as i32 * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
        x += 6 * scale;
    }
}

fn draw_page(pixels: &mut [u8], scroll: i32) {
    let white = rgb(235, 239, 245);
    let dim = rgb(135, 147, 163);
    let cyan = rgb(78, 204, 222);
    rectangle(pixels, 0, -scroll, WIDTH as i32, 38, rgb(36, 45, 59));
    text(
        pixels,
        "QUALITY SAMPLE 0123456789",
        12,
        9 - scroll,
        2,
        white,
    );
    text(
        pixels,
        "TEXT / SCROLL / MOTION",
        WIDTH as i32 - 300,
        15 - scroll,
        1,
        dim,
    );
    text(
        pixels,
        "ASCII TEXT / ONE PIXEL LINES",
        12,
        43 - scroll,
        1,
        cyan,
    );
    let middle = WIDTH as i32 / 2;
    let page_height = HEIGHT as i32 + ((SEGMENT_FRAMES - 1) * SCROLL_STEP) as i32;
    rectangle(pixels, middle - 16, 54 - scroll, 1, page_height, dim);
    for row in 0..=(page_height - 62) / 16 {
        let y = 62 + row * 16 - scroll;
        let label = format!("{row:02} ASCII SAMPLE TEXT 0123456789");
        text(
            pixels,
            &label,
            12,
            y,
            1,
            if row % 2 == 0 { white } else { dim },
        );
        text(
            pixels,
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            WIDTH as i32 / 4,
            y,
            1,
            dim,
        );
        rectangle(pixels, 12, y + 10, middle - 38, 1, rgb(49, 57, 69));
    }
    text(pixels, "5X7 ASCII", middle, 70 - scroll, 2, white);
    text(
        pixels,
        "FINE LINES",
        WIDTH as i32 - 160,
        76 - scroll,
        1,
        cyan,
    );
    let fine_width = WIDTH as i32 / 4 - 32;
    for line in 0..(HEIGHT as i32 - 210) / 4 {
        rectangle(
            pixels,
            middle,
            108 + line * 4 - scroll,
            fine_width,
            1,
            if line % 2 == 0 { white } else { dim },
        );
    }
    let vertical_x = WIDTH as i32 * 3 / 4;
    for line in 0..(WIDTH as i32 - 24 - vertical_x) / 4 {
        rectangle(
            pixels,
            vertical_x + line * 4,
            108 - scroll,
            1,
            HEIGHT as i32 - 204,
            if line % 2 == 0 { white } else { cyan },
        );
    }
    for step in 0..fine_width {
        pixel(
            pixels,
            middle + step,
            HEIGHT as i32 - 80 + step / 3 - scroll,
            cyan,
        );
    }
    text(
        pixels,
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        middle,
        HEIGHT as i32 - 22 - scroll,
        1,
        white,
    );
}

fn draw_motion(pixels: &mut [u8], frame: u32) {
    let x = 44 + frame as i32 * ((WIDTH as i32 - 88) / (SEGMENT_FRAMES - 1) as i32);
    let y = 88 + (frame as i32 * 11) % (HEIGHT as i32 - 160);
    rectangle(pixels, x, y, 44, 44, rgb(242, 244, 248));
    rectangle(pixels, x + 3, y + 3, 38, 38, rgb(225, 85, 45));
    let cursor_x =
        WIDTH as i32 - 90 - frame as i32 * ((WIDTH as i32 - 180) / (SEGMENT_FRAMES - 1) as i32);
    let cursor_y = 52 + frame as i32 * ((HEIGHT as i32 - 100) / (SEGMENT_FRAMES - 1) as i32);
    for row in 0..17 {
        for column in 0..=row / 2 {
            let border = column == 0 || column == row / 2 || row == 16;
            pixel(
                pixels,
                cursor_x + column,
                cursor_y + row,
                if border {
                    rgb(5, 7, 10)
                } else {
                    rgb(250, 250, 250)
                },
            );
        }
    }
    rectangle(pixels, cursor_x + 5, cursor_y + 13, 4, 9, rgb(5, 7, 10));
    rectangle(
        pixels,
        cursor_x + 6,
        cursor_y + 13,
        2,
        8,
        rgb(250, 250, 250),
    );
    text(
        pixels,
        "MOTION",
        WIDTH as i32 - 140,
        HEIGHT as i32 - 22,
        2,
        rgb(245, 182, 72),
    );
}

/// 每行低五位为像素，自带字形只服务本样本中的数字、大写字母和少量标点。
fn glyph(byte: u8) -> [u8; 7] {
    match byte {
        b'0' => [14, 17, 19, 21, 25, 17, 14],
        b'1' => [4, 12, 4, 4, 4, 4, 14],
        b'2' => [14, 17, 1, 2, 4, 8, 31],
        b'3' => [30, 1, 1, 14, 1, 1, 30],
        b'4' => [2, 6, 10, 18, 31, 2, 2],
        b'5' => [31, 16, 16, 30, 1, 1, 30],
        b'6' => [14, 16, 16, 30, 17, 17, 14],
        b'7' => [31, 1, 2, 4, 8, 8, 8],
        b'8' => [14, 17, 17, 14, 17, 17, 14],
        b'9' => [14, 17, 17, 15, 1, 1, 14],
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'G' => [14, 17, 16, 23, 17, 17, 15],
        b'H' => [17, 17, 17, 31, 17, 17, 17],
        b'I' => [14, 4, 4, 4, 4, 4, 14],
        b'J' => [7, 2, 2, 2, 18, 18, 12],
        b'K' => [17, 18, 20, 24, 20, 18, 17],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'M' => [17, 27, 21, 21, 17, 17, 17],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'P' => [30, 17, 17, 30, 16, 16, 16],
        b'Q' => [14, 17, 17, 17, 21, 18, 13],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        b'V' => [17, 17, 17, 17, 17, 10, 4],
        b'W' => [17, 17, 17, 21, 21, 21, 10],
        b'X' => [17, 17, 10, 4, 10, 17, 17],
        b'Y' => [17, 17, 10, 4, 4, 4, 4],
        b'Z' => [31, 1, 2, 4, 8, 16, 31],
        b'/' => [1, 2, 2, 4, 8, 8, 16],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        b' ' => [0; 7],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_quality_samples_are_reproducible_bounded_and_opaque() {
        let first = sample(0).unwrap();
        assert_eq!(first, sample(0).unwrap());
        assert_eq!(first, sample(SEGMENT_FRAMES - 1).unwrap());
        assert_eq!(first.len(), FRAME_BYTES);
        assert!(first.chunks_exact(4).all(|pixel| pixel[3] == 255));
        for index in [FRAME_COUNT, u32::MAX] {
            assert!(sample(index).is_err());
            assert_eq!(segment(index), None);
        }
    }

    #[test]
    fn recording_quality_samples_scroll_the_same_page_without_horizontal_motion() {
        let first = sample(0).unwrap();
        assert_eq!(first, sample(SEGMENT_FRAMES).unwrap());
        let scrolling = sample(SEGMENT_FRAMES + 1).unwrap();
        let stride = WIDTH as usize * 4;
        let shift = SCROLL_STEP as usize * stride;
        assert_eq!(&scrolling[..FRAME_BYTES - shift], &first[shift..]);
        assert_ne!(scrolling, first);
        assert_ne!(scrolling, sample(SEGMENT_FRAMES * 2 - 1).unwrap());
    }

    #[test]
    fn recording_quality_samples_keep_motion_and_segment_boundaries_distinct() {
        assert_eq!(segment(SEGMENT_FRAMES - 1), Some(Segment::StaticText));
        assert_eq!(segment(SEGMENT_FRAMES), Some(Segment::ScrollingText));
        assert_eq!(
            segment(SEGMENT_FRAMES * 2 - 1),
            Some(Segment::ScrollingText)
        );
        assert_eq!(segment(SEGMENT_FRAMES * 2), Some(Segment::Motion));
        assert_eq!(segment(FRAME_COUNT - 1), Some(Segment::Motion));
        let motion = sample(SEGMENT_FRAMES * 2).unwrap();
        let last = sample(FRAME_COUNT - 1).unwrap();
        assert_ne!(motion, sample(0).unwrap());
        assert_ne!(motion, last);
        assert!(last.chunks_exact(4).all(|pixel| pixel[3] == 255));
        // 运动块内部的红、蓝通道次序必须符合编码器输入的 BGRA。
        let start = (93 * WIDTH as usize + 49) * 4;
        assert_eq!(&motion[start..start + 4], &[45, 85, 225, 255]);
    }
}
