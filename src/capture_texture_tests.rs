//! 裁剪 GPU 纹理的纯坐标 oracle；不创建 D3D 设备或读取桌面。
use super::*;

const RAW: (u32, u32) = (11, 7);
const ROI: Rect = Rect {
    x: 1,
    y: 2,
    width: 3,
    height: 4,
};
const SUB: Rect = Rect {
    x: 2,
    y: 3,
    width: 1,
    height: 2,
};

fn cases() -> [(Rotation, Rect, Rect); 4] {
    [
        (
            Rotation::Identity,
            Rect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            Rect {
                x: 2,
                y: 3,
                width: 1,
                height: 2,
            },
        ),
        (
            Rotation::R90,
            Rect {
                x: 2,
                y: 3,
                width: 4,
                height: 3,
            },
            Rect {
                x: 3,
                y: 4,
                width: 2,
                height: 1,
            },
        ),
        (
            Rotation::R180,
            Rect {
                x: 7,
                y: 1,
                width: 3,
                height: 4,
            },
            Rect {
                x: 8,
                y: 2,
                width: 1,
                height: 2,
            },
        ),
        (
            Rotation::R270,
            Rect {
                x: 5,
                y: 1,
                width: 4,
                height: 3,
            },
            Rect {
                x: 6,
                y: 2,
                width: 2,
                height: 1,
            },
        ),
    ]
}

#[test]
fn cropped_capture_four_rotations_preserve_raw_extent_and_subregion_offsets() {
    // 矩形期望由四种旋转的屏幕角点手算，不调用被测 raw_rect 产生 oracle。
    for (rotation, expected_storage, expected_sub) in cases() {
        let storage = capture_texture_storage(RAW, rotation, Some(ROI)).unwrap();
        assert_eq!(storage, expected_storage);
        assert_eq!(storage.width * storage.height, 12);
        let (raw, local) = capture_texture_read_region(RAW, rotation, storage, SUB).unwrap();
        assert_eq!(raw, expected_sub);
        assert_eq!(
            local,
            Rect {
                x: 1,
                y: 1,
                width: expected_sub.width,
                height: expected_sub.height
            }
        );
        // 初帧/完整刷新写入原始 ROI 的小纹理 (0,0)，完整 raw 大小继续用于旋转。
        let (raw, local) = capture_texture_read_region(RAW, rotation, storage, ROI).unwrap();
        assert_eq!(raw, expected_storage);
        assert_eq!(
            local,
            Rect::full(expected_storage.width, expected_storage.height)
        );
        assert_eq!(
            capture_texture_storage(RAW, rotation, None).unwrap(),
            Rect::full(11, 7)
        );
    }
}

#[test]
fn cropped_capture_storage_boundaries_and_full_copy_contract() {
    let storage = Rect {
        x: 3,
        y: 2,
        width: 5,
        height: 4,
    };
    let corner = Rect {
        x: 7,
        y: 5,
        width: 1,
        height: 1,
    };
    assert_eq!(
        capture_texture_read_region(RAW, Rotation::Identity, storage, corner).unwrap(),
        (
            corner,
            Rect {
                x: 4,
                y: 3,
                width: 1,
                height: 1
            }
        )
    );
    for outside in [
        Rect { x: 2, ..storage },
        Rect { x: 4, ..storage },
        Rect { y: 1, ..storage },
        Rect { y: 3, ..storage },
        Rect {
            width: 0,
            ..storage
        },
        Rect {
            x: u32::MAX,
            width: 2,
            ..corner
        },
    ] {
        assert!(capture_texture_read_region(RAW, Rotation::Identity, storage, outside).is_err());
        assert!(
            capture_texture_refresh_region(RAW, Rotation::Identity, storage, Some(outside))
                .is_err()
        );
    }
    assert!(capture_texture_refresh_region(RAW, Rotation::Identity, storage, None).is_err());
    let full = Rect::full(11, 7);
    assert!(
        capture_texture_refresh_region(RAW, Rotation::Identity, full, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        capture_texture_refresh_region(RAW, Rotation::Identity, full, Some(storage)).unwrap(),
        Some((storage, storage))
    );
    assert!(
        capture_texture_refresh_region((0, 7), Rotation::Identity, Rect::full(0, 7), None).is_err()
    );
}

#[test]
fn cropped_capture_budget_limits_only_cropped_storage_and_keeps_full_metadata() {
    let full_size = (8000, 8000);
    let selected = Rect {
        x: 100,
        y: 200,
        width: 6400,
        height: 5000,
    };
    for rotation in [
        Rotation::Identity,
        Rotation::R90,
        Rotation::R180,
        Rotation::R270,
    ] {
        let storage = capture_texture_storage(full_size, rotation, Some(selected)).unwrap();
        assert_eq!(
            u64::from(storage.width) * u64::from(storage.height),
            32_000_000
        );
        assert_ne!(storage, Rect::full(8000, 8000));
        assert!(capture_texture_storage(
            full_size,
            rotation,
            Some(Rect {
                height: 5001,
                ..selected
            })
        )
        .is_err());
        assert!(
            capture_texture_storage(full_size, rotation, Some(Rect::full(8000, 8000))).is_err()
        );
        // 普通全屏冻结保持原有行为；不能用长截图 ROI 预算缩小 full raw 元数据。
        assert_eq!(
            capture_texture_storage(full_size, rotation, None).unwrap(),
            Rect::full(8000, 8000)
        );
    }
    for invalid in [(0, 8000), (8000, 0), (16385, 4), (4, 16385)] {
        assert!(capture_texture_storage(invalid, Rotation::Identity, None).is_err());
    }
    for invalid in [
        Rect { width: 0, ..ROI },
        Rect { height: 0, ..ROI },
        Rect { x: 10, ..ROI },
    ] {
        assert!(capture_texture_storage(RAW, Rotation::Identity, Some(invalid)).is_err());
    }
}

#[test]
fn cropped_capture_rotated_staging_with_padding_matches_independent_pixels() {
    let expected: [[u8; 8]; 4] = [
        [2, 3, 35, 255, 2, 4, 46, 255],
        [3, 4, 47, 255, 4, 4, 48, 255],
        [8, 3, 41, 255, 8, 2, 30, 255],
        [7, 2, 29, 255, 6, 2, 28, 255],
    ];
    for ((rotation, _, _), expected) in cases().into_iter().zip(expected) {
        let storage = capture_texture_storage(RAW, rotation, Some(ROI)).unwrap();
        let mut texture = Vec::new();
        for y in storage.y..storage.y + storage.height {
            for x in storage.x..storage.x + storage.width {
                texture.extend_from_slice(&[x as u8, y as u8, (x + y * 11) as u8, 17]);
            }
        }
        let (raw, local) = capture_texture_read_region(RAW, rotation, storage, SUB).unwrap();
        let row_bytes = raw.width as usize * 4;
        let row_pitch = row_bytes + 12;
        // 最后一行不提供尾部 padding，防止依赖未初始化/不存在的额外字节。
        let mut staging = vec![203; row_pitch * (raw.height as usize - 1) + row_bytes];
        for y in 0..raw.height as usize {
            let source = ((local.y as usize + y) * storage.width as usize + local.x as usize) * 4;
            staging[y * row_pitch..y * row_pitch + row_bytes]
                .copy_from_slice(&texture[source..source + row_bytes]);
        }
        let mut actual = [0; 8];
        if matches!(rotation, Rotation::Identity) {
            copy_identity_bgra_rows(&staging, row_pitch, &mut actual, SUB.width as usize * 4)
                .unwrap();
        } else {
            for y in 0..SUB.height {
                let (rx, ry) = rotation.raw_pixel(SUB.x, SUB.y + y, RAW.0, RAW.1);
                let source = (ry - raw.y) as usize * row_pitch + (rx - raw.x) as usize * 4;
                actual[y as usize * 4..y as usize * 4 + 4]
                    .copy_from_slice(&staging[source..source + 4]);
                actual[y as usize * 4 + 3] = 255;
            }
        }
        assert_eq!(actual, expected);
    }
}
