//! 原生基准：程序生成一张带固定页眉页脚的长网页，按不同滚动速度逐帧喂给生产 Stitcher。
//! 每组输出结果指纹（逐帧类型、位移、追加量与最终长图像素的哈希），改算法前后指纹应一致。
//! 运行：cargo run --release --example bench --target-dir ../../target/promo-wasm
use rustshot_promo_wasm::{capture::Image, stitch::ScrollAxis, Session};
use std::{sync::Arc, time::Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.next() % (hi - lo)
    }
}

fn page(width: u32, height: u32) -> Vec<u8> {
    let mut px = vec![255u8; width as usize * height as usize * 4];
    let mut rng = Rng(7);
    let put = |px: &mut Vec<u8>, x: u32, y: u32, c: [u8; 3]| {
        let i = (y as usize * width as usize + x as usize) * 4;
        px[i..i + 3].copy_from_slice(&c);
    };
    let mut y = 40;
    while y + 40 < height {
        let kind = rng.range(0, 10);
        if kind == 0 {
            // 彩色卡片
            let h = rng.range(80, 220).min(height - y - 20);
            let c = [
                rng.range(40, 220) as u8,
                rng.range(40, 220) as u8,
                rng.range(40, 220) as u8,
            ];
            for yy in y..y + h {
                for xx in 40..width - 40 {
                    let t = ((xx + yy) % 64) as u8;
                    put(
                        &mut px,
                        xx,
                        yy,
                        [c[0].saturating_add(t), c[1], c[2].saturating_sub(t / 2)],
                    );
                }
            }
            y += h + 24;
            continue;
        }
        let size = if kind == 1 { 14 } else { 9 };
        let mut x = 40;
        let end = width - 40 - rng.range(0, width / 3);
        while x + 20 < end {
            let word = rng.range(2, 9);
            for _ in 0..word {
                let w = size * 2 / 3;
                for gx in 0..w {
                    let top = rng.range(0, size / 2);
                    for gy in top..size {
                        if rng.range(0, 3) > 0 {
                            put(&mut px, x + gx, y + gy, [30, 34, 40]);
                        }
                    }
                }
                x += w + 2;
            }
            x += size;
        }
        y += size + rng.range(8, 16);
    }
    px
}

fn frame(page: &[u8], width: u32, top: u32, height: u32) -> Image {
    let mut bgra = page
        [(top as usize * width as usize * 4)..((top + height) as usize * width as usize * 4)]
        .to_vec();
    // 固定页眉 64px、页脚 40px
    for y in (0..64).chain(height - 40..height) {
        for x in 0..width {
            let i = (y as usize * width as usize + x as usize) * 4;
            let c: [u8; 3] = if y < 64 { [36, 120, 210] } else { [40, 44, 52] };
            bgra[i..i + 3].copy_from_slice(&c);
            if (x / 90) % 2 == 0 && y % 64 > 24 && y % 64 < 36 {
                bgra[i..i + 3].copy_from_slice(&[240, 240, 240]);
            }
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash = (*hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
}

fn main() {
    for &(width, height) in &[(560u32, 500u32), (1000, 1300), (1920, 1080)] {
        let total = 30_000;
        let doc = page(width, total);
        // 匀速三档 + 变速往返（含往回滚与停顿），帧在计时外生成
        let mut rng = Rng(height as u64);
        let mixed: Vec<i32> = (0..120)
            .map(|i| match i % 12 {
                5 => -((rng.range(20, height / 3)) as i32),
                9 => 0,
                _ => rng.range(10, height * 11 / 20) as i32,
            })
            .collect();
        let plans: Vec<(String, Vec<i32>)> = [40, 120, 200]
            .iter()
            .map(|&v| (format!("匀速{v:>3}"), vec![v; 120]))
            .chain([("变速往返".to_string(), mixed)])
            .collect();
        for (name, steps) in plans {
            let mut top = 2000i64;
            let mut frames = Vec::new();
            for &step in &steps {
                top += step as i64;
                if top < 0 || top as u32 + height >= total {
                    break;
                }
                frames.push(Arc::new(frame(&doc, width, top as u32, height)));
            }
            let mut session =
                Session::new(frame(&doc, width, 2000, height), ScrollAxis::Vertical, true).unwrap();
            let mut times = Vec::new();
            let mut hash = 0xcbf29ce484222325u64;
            let mut added = 0;
            for f in frames {
                let t = Instant::now();
                let kind = session.process(f);
                times.push(t.elapsed().as_secs_f64() * 1000.);
                let (delta, px) = session.last_update();
                fnv(&mut hash, &kind.to_le_bytes());
                fnv(&mut hash, &delta.to_le_bytes());
                fnv(&mut hash, &px.to_le_bytes());
                added += u32::from(kind == 2);
            }
            fnv(&mut hash, &session.stitcher.image().bgra);
            times.sort_by(|a, b| a.total_cmp(b));
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            println!(
                "{width}x{height} {name} frames={} added={added} mean={mean:.2}ms p50={:.2}ms length={} fp={hash:016x}",
                times.len(),
                times[times.len() / 2],
                session.stitcher.height
            );
        }
    }
}
