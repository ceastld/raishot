//! OCR CPU 线程自动选择：默认上限四线程，按逻辑 CPU／进程 affinity 预算降档。
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};

fn select(available: usize) -> usize {
    if available >= 4 {
        4
    } else if available >= 2 {
        2
    } else {
        1
    }
}

pub(super) fn current() -> usize {
    select(available())
}

fn logical_affinity_budget(logical_cpus: usize, process_mask: Option<usize>) -> usize {
    match process_mask {
        Some(mask) if mask != 0 => logical_cpus.max(1).min(mask.count_ones() as usize),
        _ => 1,
    }
}

/// 取逻辑 CPU 数和当前进程 affinity 数的较小值，查询失败保守使用一线程。
/// 该预算不衡量瞬时 CPU 空闲量或 Job quota，也不调整任何系统设置或亲和性。
fn available() -> usize {
    let logical_cpus = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    // 只读查询：当前进程伪句柄无需关闭，两个输出指针在调用期间始终有效。
    let process_mask = unsafe {
        GetProcessAffinityMask(GetCurrentProcess(), &mut process_mask, &mut system_mask)
            .ok()
            .map(|()| process_mask)
    };
    logical_affinity_budget(logical_cpus, process_mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatically_uses_four_unless_the_budget_requires_a_lower_tier() {
        for (available, expected) in [
            (0, 1),
            (1, 1),
            (2, 2),
            (3, 2),
            (4, 4),
            (5, 4),
            (32, 4),
            (usize::MAX, 4),
        ] {
            assert_eq!(select(available), expected);
        }
    }

    #[test]
    fn current_budget_produces_a_valid_automatic_tier() {
        assert!(available() >= 1);
        assert!(matches!(current(), 1 | 2 | 4));
    }

    #[test]
    fn affinity_budget_counts_allowed_bits_and_handles_failed_or_empty_queries() {
        for (logical, mask, expected) in [
            (32, None, 1),
            (32, Some(0), 1),
            (32, Some(1), 1),
            (32, Some(0b101), 2),
            (32, Some(0b111), 3),
            (2, Some(0b1111), 2),
            (32, Some(usize::MAX), 32.min(usize::BITS as usize)),
            (0, Some(0b1111), 1),
            (32, Some(1usize << (usize::BITS - 1)), 1),
        ] {
            assert_eq!(logical_affinity_budget(logical, mask), expected);
        }
        assert_eq!(select(logical_affinity_budget(32, Some(0b101))), 2);
        assert_eq!(select(logical_affinity_budget(32, Some(0b100))), 1);
    }
}
