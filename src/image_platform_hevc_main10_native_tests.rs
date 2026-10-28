//! 显式验证SDR Main10的原始P010编码画布，不授予HDR映射或产品入口资格。
use super::*;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::NTSTATUS,
        Security::Cryptography::{
            BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
            BCryptGetProperty, BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE,
            BCRYPT_HASH_HANDLE, BCRYPT_HASH_LENGTH, BCRYPT_MD5_ALGORITHM, BCRYPT_OBJECT_LENGTH,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
        },
    },
};

const MAX_HASH_OBJECT_BYTES: usize = 64 * 1024;
const HASH_ROW_BYTES: usize = 512;
const CODED_DIMENSIONS: (u32, u32) = (256, 128);
const DISPLAY_OUTPUT_BYTES: u64 = 248 * 122 * 4;
const CODED_P010_BYTES: usize = 256 * 128 * 3;

// 独立decoded-picture-hash SEI的完整编码画布MD5，不是有损编码前的源YUV哈希。
const EXPECTED_MD5: [[u8; 16]; 3] = [
    [
        0xac, 0x4b, 0xe5, 0x9f, 0x1a, 0x01, 0xfb, 0xaf, 0xb7, 0x8b, 0x19, 0x76, 0x8d, 0x00, 0xc5,
        0x59,
    ],
    [
        0xba, 0x36, 0xc2, 0x43, 0x79, 0x7e, 0x5a, 0x43, 0x0c, 0x86, 0x4d, 0x8d, 0x88, 0x6a, 0xbc,
        0xf7,
    ],
    [
        0xe5, 0xac, 0x9a, 0x5c, 0xdf, 0x5d, 0xbe, 0xb2, 0xb8, 0x10, 0xfe, 0x3d, 0x65, 0x5b, 0x69,
        0x58,
    ],
];

fn hash_cancel(cancel: &AtomicBool) -> Result<(), String> {
    super::super::cancelled(cancel).map_err(|error| error.to_string())
}

fn hash_status(status: NTSTATUS, operation: &str, cancel: &AtomicBool) -> Result<(), String> {
    hash_cancel(cancel)?;
    if status.0 < 0 {
        // BCrypt返回NTSTATUS；保留其原始值，不套用HRESULT或缺组件分类。
        Err(format!("{operation}: NTSTATUS 0x{:08X}", status.0 as u32))
    } else {
        Ok(())
    }
}

struct Md5Algorithm(BCRYPT_ALG_HANDLE);

impl Drop for Md5Algorithm {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
}

impl Md5Algorithm {
    fn open(cancel: &AtomicBool) -> Result<Self, String> {
        hash_cancel(cancel)?;
        let mut handle = BCRYPT_ALG_HANDLE::default();
        let status = unsafe {
            BCryptOpenAlgorithmProvider(
                &mut handle,
                BCRYPT_MD5_ALGORITHM,
                PCWSTR::null(),
                BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
            )
        };
        let algorithm = Self(handle);
        hash_status(status, "BCryptOpenAlgorithmProvider(MD5)", cancel)?;
        if algorithm.0.is_invalid() {
            return Err("BCrypt returned an empty MD5 provider handle".into());
        }
        Ok(algorithm)
    }

    fn property(&self, name: PCWSTR, cancel: &AtomicBool) -> Result<u32, String> {
        hash_cancel(cancel)?;
        let mut bytes = [0; 4];
        let mut returned = 0;
        let status =
            unsafe { BCryptGetProperty(self.0.into(), name, Some(&mut bytes), &mut returned, 0) };
        hash_status(status, "BCryptGetProperty(MD5)", cancel)?;
        if returned != bytes.len() as u32 {
            return Err("BCrypt returned an invalid MD5 property size".into());
        }
        Ok(u32::from_ne_bytes(bytes))
    }

    fn create<'a>(
        &'a self,
        object: &'a mut [u8],
        cancel: &AtomicBool,
    ) -> Result<Md5Hash<'a>, String> {
        hash_cancel(cancel)?;
        let mut handle = BCRYPT_HASH_HANDLE::default();
        let status = unsafe { BCryptCreateHash(self.0, &mut handle, Some(&mut *object), None, 0) };
        let hash = Md5Hash {
            handle,
            _algorithm: self,
            _object: object,
        };
        hash_status(status, "BCryptCreateHash(MD5)", cancel)?;
        if hash.handle.is_invalid() {
            return Err("BCrypt returned an empty MD5 hash handle".into());
        }
        Ok(hash)
    }
}

struct Md5Hash<'a> {
    handle: BCRYPT_HASH_HANDLE,
    _algorithm: &'a Md5Algorithm,
    // 保持调用者对象缓冲存活且不可扩容，直到Drop先销毁原生hash句柄。
    _object: &'a mut [u8],
}

impl Drop for Md5Hash<'_> {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = BCryptDestroyHash(self.handle);
            }
        }
    }
}

impl Md5Hash<'_> {
    fn update(&mut self, bytes: &[u8], cancel: &AtomicBool) -> Result<(), String> {
        hash_cancel(cancel)?;
        let status = unsafe { BCryptHashData(self.handle, bytes, 0) };
        hash_status(status, "BCryptHashData(MD5)", cancel)
    }

    fn finish(self, cancel: &AtomicBool) -> Result<[u8; 16], String> {
        hash_cancel(cancel)?;
        let mut digest = [0; 16];
        let status = unsafe { BCryptFinishHash(self.handle, &mut digest, 0) };
        hash_status(status, "BCryptFinishHash(MD5)", cancel)?;
        Ok(digest)
    }
}

fn hash_budget(resident: u64, raw_capacity: usize, object_capacity: usize) -> Result<(), String> {
    // 只核本叶拥有的可见缓冲；CNG与MFT的私有状态不在此容量承诺内。
    resident
        .checked_add(raw_capacity as u64)
        .and_then(|bytes| bytes.checked_add(object_capacity as u64))
        .and_then(|bytes| bytes.checked_add(HASH_ROW_BYTES as u64))
        .filter(|bytes| *bytes <= MAX_BYTES)
        .ok_or_else(|| "Main10 MD5 visible buffer budget exceeded".to_string())?;
    Ok(())
}

fn plane_md5(raw: &RawP010, resident: u64, cancel: &AtomicBool) -> Result<[[u8; 16]; 3], String> {
    hash_cancel(cancel)?;
    // 裁后输出缺少编码画布的边缘样本，不能用于此oracle，也不补零或忽略裁窗。
    if (raw.width, raw.height) != CODED_DIMENSIONS
        || raw.stride != HASH_ROW_BYTES
        || raw.bytes.len() != CODED_P010_BYTES
    {
        return Err("Coded-picture MD5 requires the complete 256x128 tight P010 canvas".into());
    }
    hash_budget(resident, raw.bytes.capacity(), 0)?;
    let algorithm = Md5Algorithm::open(cancel)?;
    if algorithm.property(BCRYPT_HASH_LENGTH, cancel)? != 16 {
        return Err("BCrypt MD5 digest must contain 16 bytes".into());
    }
    let object_length = algorithm.property(BCRYPT_OBJECT_LENGTH, cancel)? as usize;
    if object_length == 0 || object_length > MAX_HASH_OBJECT_BYTES {
        return Err("BCrypt MD5 object exceeds the 64KiB diagnostic bound".into());
    }
    hash_budget(resident, raw.bytes.capacity(), object_length)?;
    let mut object = Vec::new();
    let allocation = object.try_reserve_exact(object_length);
    hash_cancel(cancel)?;
    allocation.map_err(|_| "Unable to reserve the bounded MD5 object".to_string())?;
    if object.capacity() > MAX_HASH_OBJECT_BYTES {
        return Err("BCrypt MD5 object capacity exceeds 64KiB".into());
    }
    hash_budget(resident, raw.bytes.capacity(), object.capacity())?;
    object.resize(object_length, 0);

    let mut scratch = [0; HASH_ROW_BYTES];
    let mut digests = [[0; 16]; 3];
    for (plane, digest) in digests.iter_mut().enumerate() {
        hash_cancel(cancel)?;
        let mut hash = algorithm.create(&mut object, cancel)?;
        let (rows, columns) = if plane == 0 { (128, 256) } else { (64, 128) };
        for y in 0..rows {
            hash_cancel(cancel)?;
            for x in 0..columns {
                let offset = if plane == 0 {
                    y * HASH_ROW_BYTES + x * 2
                } else {
                    128 * HASH_ROW_BYTES + y * HASH_ROW_BYTES + x * 4 + (plane - 1) * 2
                };
                let word = raw
                    .bytes
                    .get(offset..offset + 2)
                    .ok_or_else(|| "P010 plane exceeds the diagnostic canvas".to_string())?;
                let word = u16::from_le_bytes([word[0], word[1]]);
                if word & 0x3f != 0 {
                    return Err("P010 has nonzero low padding bits".into());
                }
                // HEVC哈希使用10位样本本身的低/高字节，不使用P010左对齐的WORD。
                scratch[x * 2..x * 2 + 2].copy_from_slice(&(word >> 6).to_le_bytes());
            }
            hash.update(&scratch[..columns * 2], cancel)?;
        }
        *digest = hash.finish(cancel)?;
    }
    hash_cancel(cancel)?;
    Ok(digests)
}

#[test]
fn image_platform_hevc_main10_native_sample_gate_preserves_production_hdr_scope() {
    let bytes = include_bytes!("../tests/data/image_platform/hevc-confwin-420-10bit-all.heic");
    let cancel = AtomicBool::new(false);
    let plan = super::super::preflight::inspect(bytes, DISPLAY_OUTPUT_BYTES, &cancel)
        .expect("Main10 fixture preflight");
    let input = plan.hevc_input(0).expect("Main10 fixture input");
    let resident = (bytes.len() as u64)
        .checked_add(plan.retained_bytes().expect("Main10 directory capacity"))
        .expect("Main10 source and directory budget");
    // 生产入口在创建MF运行时前仍由原HDR颜色门拒绝这个SDR来源。
    match decode(input, resident, DISPLAY_OUTPUT_BYTES, &cancel) {
        Err(PlatformError::Unsupported { .. }) => {}
        Err(error) => panic!("SDR sample must fail the production HDR gate: {error:?}"),
        Ok(_) => panic!("The production HDR gate unexpectedly accepted the SDR sample"),
    }
    let expected = sdr_main10_sample_color(input).expect("Exact SDR sample color gate");
    assert_eq!(expected.primaries, Some(MFVideoPrimaries_BT709.0 as u32));
    assert_eq!(expected.transfer, Some(MFVideoTransFunc_sRGB.0 as u32));
    assert_eq!(expected.matrix, Some(MFVideoTransferMatrix_BT601.0 as u32));
    assert_eq!(expected.nominal_range, Some(MFNominalRange_0_255.0 as u32));

    for cicp in [
        Cicp {
            primaries: 9,
            ..input.cicp
        },
        Cicp {
            transfer: 16,
            ..input.cicp
        },
        Cicp {
            matrix: 9,
            ..input.cicp
        },
        Cicp {
            full_range: false,
            ..input.cicp
        },
    ] {
        assert!(matches!(
            sdr_main10_sample_color(Input { cicp, ..input }),
            Err(PlatformError::Unsupported { .. })
        ));
    }
    for field in 0..3 {
        let mut changed = input;
        match field {
            0 => changed.coded.coded_dimensions = (254, 128),
            1 => changed.coded.display_dimensions = (246, 122),
            _ => changed.coded.display_origin = (4, 4),
        }
        assert!(matches!(
            sdr_main10_sample_color(changed),
            Err(PlatformError::Unsupported { .. })
        ));
    }
}

#[test]
#[ignore = "真实Main10软件MFT与完整编码画布P010哈希验证；需显式运行，无窗口"]
fn image_platform_hevc_main10_native_coded_p010_matches_sei_md5() {
    let bytes = include_bytes!("../tests/data/image_platform/hevc-confwin-420-10bit-all.heic");
    let cancel = AtomicBool::new(false);
    let plan = super::super::preflight::inspect(bytes, DISPLAY_OUTPUT_BYTES, &cancel)
        .expect("Main10 fixture preflight");
    let input = plan.hevc_input(0).expect("Main10 fixture input");
    assert_eq!(input.coded.coded_dimensions, CODED_DIMENSIONS);
    assert_eq!(input.coded.display_dimensions, (248, 122));
    assert_eq!(input.coded.display_origin, (6, 4));
    assert_eq!(input.coded.bits_per_channel, 10);
    assert_eq!(
        input.cicp,
        Cicp {
            primaries: 1,
            transfer: 13,
            matrix: 6,
            full_range: true,
        }
    );
    let resident = (bytes.len() as u64)
        .checked_add(plan.retained_bytes().expect("Main10 directory capacity"))
        .expect("Main10 source and directory budget");
    let result = decode_sdr_main10_sample(input, resident, DISPLAY_OUTPUT_BYTES, &cancel);
    match &result {
        Ok(raw) => eprintln!(
            "Main10 P010 width={} height={} stride={} len={} aperture={:?} color={:?}",
            raw.width,
            raw.height,
            raw.stride,
            raw.bytes.len(),
            raw.aperture,
            raw.color,
        ),
        Err(error) => eprintln!("Main10 P010 native failure: {error:?}"),
    }
    // 缺组件、Unsupported与其它原生错误都保留失败；裁后图不能当作coded像素通过。
    let raw = result.expect("Main10 MFT must return the complete coded P010 canvas");
    let actual = plane_md5(&raw, resident, &cancel).expect("Complete coded P010 MD5");
    eprintln!("Main10 coded-picture MD5 Y/Cb/Cr: {actual:02x?}");
    for (plane, (actual, expected)) in ["Y", "Cb", "Cr"]
        .into_iter()
        .zip(actual.iter().zip(EXPECTED_MD5.iter()))
    {
        assert_eq!(actual, expected, "Main10 coded {plane} MD5");
    }
}
