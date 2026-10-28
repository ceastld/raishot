//! 两帧队列的保守纵向合并证明；不参与接缝评分，也不拥有帧或会话状态。
//! 调用方必须在锁外证明、重入锁后复核 sequence / epoch / region / barrier。
use crate::capture::Image;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const MAX_PIXELS: u64 = 32_000_000;
const MAX_SIDE: u32 = 65_535;
const PROOF_TIME: Duration = Duration::from_millis(8);
const ANCHOR_PROBES: usize = 16;
const MAX_CANDIDATES: usize = ANCHOR_PROBES * 3;
const BYTE_CHUNK: usize = 64 * 1024;

/// 只证明三幅同尺寸纵向帧可无损丢弃 middle；不足、歧义或超时均退回 FIFO。
pub(crate) fn exact_coalesce(first: &Image, middle: &Image, last: &Image) -> bool {
    prove(first, middle, last, PROOF_TIME).unwrap_or(false)
}

#[derive(Clone, Copy)]
struct Shape {
    row_bytes: usize,
    height: usize,
}
impl Shape {
    fn checked(image: &Image) -> Option<Self> {
        if image.width == 0
            || image.height == 0
            || image.width > MAX_SIDE
            || image.height > MAX_SIDE
            || u64::from(image.width) * u64::from(image.height) > MAX_PIXELS
        {
            return None;
        }
        let row_bytes = (image.width as usize).checked_mul(4)?;
        let bytes = row_bytes.checked_mul(image.height as usize)?;
        (bytes == image.bgra.len()).then_some(Self {
            row_bytes,
            height: image.height as usize,
        })
    }
    fn row(self, image: &Image, y: usize) -> &[u8] {
        let start = y * self.row_bytes;
        &image.bgra[start..start + self.row_bytes]
    }
}

struct Budget {
    started: Instant,
    duration: Duration,
    candidates: usize,
}
impl Budget {
    fn check(&self) -> Option<()> {
        (self.started.elapsed() < self.duration).then_some(())
    }
    fn candidate(&mut self) -> Option<()> {
        self.check()?;
        self.candidates += 1;
        (self.candidates <= MAX_CANDIDATES).then_some(())
    }
    fn equal(&self, a: &[u8], b: &[u8]) -> Option<bool> {
        if a.len() != b.len() {
            return Some(false);
        }
        // 不让一次巨幅 memcmp 跨越整个预算；成功必须比较每个 BGRA 字节。
        for (a, b) in a.chunks(BYTE_CHUNK).zip(b.chunks(BYTE_CHUNK)) {
            self.check()?;
            if a != b {
                return Some(false);
            }
        }
        self.check()?;
        Some(true)
    }
}

#[derive(Clone, Copy)]
struct Occurrence {
    row: usize,
    repeated: bool,
}
struct RowIndex {
    hashes: Vec<u64>,
    rows: HashMap<u64, Occurrence>,
}
impl RowIndex {
    fn build(image: &Image, shape: Shape, budget: &Budget) -> Option<Self> {
        budget.check()?;
        let mut hashes = Vec::new();
        hashes.try_reserve_exact(shape.height).ok()?;
        let mut rows: HashMap<u64, Occurrence> = HashMap::new();
        rows.try_reserve(shape.height).ok()?;
        for y in 0..shape.height {
            if y % 16 == 0 {
                budget.check()?;
            }
            let hash = row_hash(shape.row(image, y));
            hashes.push(hash);
            rows.entry(hash)
                .and_modify(|row| row.repeated = true)
                .or_insert(Occurrence {
                    row: y,
                    repeated: false,
                });
        }
        budget.check()?;
        Some(Self { hashes, rows })
    }
}

/// 采样 hash 仅减少候选。整行完全相同一定具有相同 hash，因此单次桶是
/// 整行唯一的充分条件；碰撞/重复桶直接放弃，候选仍要逐字节确认整行。
fn row_hash(row: &[u8]) -> u64 {
    let pixels = row.len() / 4;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for sample in 0..16 {
        let offset = sample * (pixels - 1) / 15 * 4;
        let pixel = u32::from_le_bytes([
            row[offset],
            row[offset + 1],
            row[offset + 2],
            row[offset + 3],
        ]);
        hash = (hash ^ u64::from(pixel)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn exact_shift(
    a: (&Image, &RowIndex),
    b: (&Image, &RowIndex),
    shape: Shape,
    identical: bool,
    budget: &mut Budget,
) -> Option<i32> {
    let (a, index_a) = a;
    let (b, index_b) = b;
    if identical {
        return Some(0);
    }
    let limit = (shape.height * 2 / 5) as i32;
    let mut shift = None;
    // 固定数量的分散锚点，不逐行嵌套搜索；未碰到唯一行时保守背压。
    for probe in 1..=ANCHOR_PROBES {
        budget.check()?;
        let anchor = probe * shape.height / (ANCHOR_PROBES + 1);
        let hash = index_a.hashes[anchor];
        let own = index_a.rows.get(&hash)?;
        let Some(other) = index_b.rows.get(&hash) else {
            continue;
        };
        if own.repeated || other.repeated {
            continue;
        }
        budget.candidate()?;
        if !budget.equal(shape.row(a, anchor), shape.row(b, other.row))? {
            continue;
        }
        let candidate = anchor as i32 - other.row as i32;
        // 零位移必须整图相等；锚点冲突和越过 40% 不提供合并证据。
        if candidate == 0
            || candidate.abs() > limit
            || shift.is_some_and(|previous| previous != candidate)
        {
            return None;
        }
        shift = Some(candidate);
    }
    let shift = shift?;
    let start_a = shift.max(0) as usize * shape.row_bytes;
    let start_b = (-shift).max(0) as usize * shape.row_bytes;
    let bytes = (shape.height - shift.unsigned_abs() as usize) * shape.row_bytes;
    budget
        .equal(
            &a.bgra[start_a..start_a + bytes],
            &b.bgra[start_b..start_b + bytes],
        )?
        .then_some(shift)
}

fn prove(first: &Image, middle: &Image, last: &Image, duration: Duration) -> Option<bool> {
    let mut budget = Budget {
        started: Instant::now(),
        duration,
        candidates: 0,
    };
    budget.check()?;
    let shape = Shape::checked(first)?;
    if (first.width, first.height) != (middle.width, middle.height)
        || (first.width, first.height) != (last.width, last.height)
    {
        return Some(false);
    }
    Shape::checked(middle)?;
    Shape::checked(last)?;
    let same_middle = budget.equal(&first.bgra, &middle.bgra)?;
    let same_last = budget.equal(&first.bgra, &last.bgra)?;
    if same_last {
        // 两端重回同一帧而中间不同，包括折返独有尾部，不能丢中间。
        return Some(same_middle);
    }
    let same_tail = budget.equal(&middle.bgra, &last.bgra)?;
    let a = RowIndex::build(first, shape, &budget)?;
    let b = RowIndex::build(middle, shape, &budget)?;
    let c = RowIndex::build(last, shape, &budget)?;
    let between = exact_shift((first, &a), (middle, &b), shape, same_middle, &mut budget)?;
    let end = exact_shift((first, &a), (last, &c), shape, false, &mut budget)?;
    let tail = exact_shift((middle, &b), (last, &c), shape, same_tail, &mut budget)?;
    if between < end.min(0) || between > end.max(0) || between + tail != end {
        return Some(false);
    }
    // 独立检查被删除帧的每一行；它必须落在至少一个端点内，且全部字节一致。
    for y in 0..shape.height {
        budget.check()?;
        let global_y = between + y as i32;
        let first_y = global_y;
        let last_y = global_y - end;
        let in_first = (0..shape.height as i32).contains(&first_y);
        let in_last = (0..shape.height as i32).contains(&last_y);
        if !in_first && !in_last {
            return Some(false);
        }
        let row = shape.row(middle, y);
        if in_first && !budget.equal(row, shape.row(first, first_y as usize))? {
            return Some(false);
        }
        if in_last && !budget.equal(row, shape.row(last, last_y as usize))? {
            return Some(false);
        }
    }
    budget.check()?;
    Some(true)
}

#[cfg(test)]
#[path = "long_frame_coalesce_tests.rs"]
mod tests;
