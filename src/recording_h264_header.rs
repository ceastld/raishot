//! Bounded observation of initialization SPS declarations, not sample activation.
//!
//! Grammar: ITU-T H.264 (08/2024), 7.3.2.1.1, 7.3.2.11, 9.1 and E.1/E.2.
//! This does not validate PPS syntax, slices, access units, decoder behaviour or
//! colour accuracy, and cannot establish full profile/level bitstream conformance.

use serde_json::{json, Value};

pub(super) const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_NALS: usize = 64;
const MAX_SPS: usize = 32;

pub(super) fn inspect_annex_b(bytes: &[u8]) -> crate::Result<Value> {
    if bytes.is_empty() || bytes.len() > MAX_HEADER_BYTES {
        return Err("H.264 Annex B header is empty or exceeds 64 KiB".into());
    }
    let (first, prefix) = start_code(bytes, 0).ok_or("H.264 header has no Annex B start code")?;
    if bytes[..first].iter().any(|&byte| byte != 0) {
        return Err(
            "H.264 header has nonzero bytes before its first start code; avcC is unsupported"
                .into(),
        );
    }
    let mut start = first + prefix;
    let mut nal_count = 0;
    let mut pps_count = 0;
    let mut sps = Vec::new();
    loop {
        let next = start_code(bytes, start);
        let mut end = next.map_or(bytes.len(), |(offset, _)| offset);
        // Annex B leading/trailing zero bytes are framing, not SPS RBSP bytes.
        while end > start && bytes[end - 1] == 0 {
            end -= 1;
        }
        if end <= start {
            return Err("H.264 Annex B header contains an empty NAL unit".into());
        }
        nal_count += 1;
        if nal_count > MAX_NALS {
            return Err("H.264 header exceeds 64 NAL units".into());
        }
        let nal = &bytes[start..end];
        let header = nal[0];
        if header & 0x80 != 0 {
            return Err("H.264 forbidden_zero_bit is nonzero".into());
        }
        let kind = header & 31;
        if !matches!(kind, 7 | 8) {
            return Err(format!(
                "H.264 initialization header NAL type {kind} is unsupported; expected SPS/PPS"
            )
            .into());
        }
        if header & 0x60 == 0 {
            return Err("H.264 SPS/PPS has zero nal_ref_idc".into());
        }
        let rbsp = unescape(&nal[1..])?;
        if kind == 7 {
            if sps.len() == MAX_SPS {
                return Err("H.264 header exceeds 32 SPS declarations".into());
            }
            // Fail the entire observation if any SPS is unsupported or malformed.
            sps.push(parse_sps(&rbsp)?);
        } else {
            pps_count += 1;
        }
        match next {
            Some((offset, length)) => start = offset + length,
            None => break,
        }
    }
    if sps.is_empty() {
        return Err("H.264 initialization header contains no SPS".into());
    }
    Ok(json!({
        "format": "h264_annex_b",
        "sps": sps,
        "nal_count": nal_count,
        "pps_count": pps_count,
        "pps_syntax": "not_parsed",
        "parameter_set_selection": "not_observed",
        "scope": "Initialization header SPS declarations only; not sample activation, PPS/slice validation, full bitstream conformance, decoded pixels or colour accuracy. Colour codes are raw H.264 values, not Media Foundation enum values."
    }))
}

fn start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index.checked_add(3)? <= bytes.len() {
        if bytes[index] == 0 && bytes[index + 1] == 0 {
            if bytes[index + 2] == 1 {
                return Some((index, 3));
            }
            if bytes[index + 2] == 0 && bytes.get(index + 3) == Some(&1) {
                return Some((index, 4));
            }
        }
        index += 1;
    }
    None
}

fn unescape(ebsp: &[u8]) -> crate::Result<Vec<u8>> {
    if ebsp.is_empty() {
        return Err("H.264 NAL unit has no RBSP".into());
    }
    let mut rbsp = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    let mut index = 0;
    while index < ebsp.len() {
        let byte = ebsp[index];
        if zeros == 2 {
            if byte < 3 {
                return Err("H.264 EBSP contains an unescaped 00 00 00/01/02 sequence".into());
            }
            if byte == 3 {
                if !matches!(ebsp.get(index + 1), Some(0..=3)) {
                    return Err("H.264 emulation-prevention byte is dangling or followed by a value above 3".into());
                }
                // Count input EBSP bytes, not the already unescaped output.
                zeros = 0;
                index += 1;
                continue;
            }
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        index += 1;
    }
    Ok(rbsp)
}

struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read(&mut self, count: u32) -> crate::Result<u32> {
        if count > 32 || self.position + count as usize > self.bytes.len() * 8 {
            return Err("H.264 SPS bit field is truncated or too wide".into());
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1)
                | u32::from((self.bytes[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn flag(&mut self) -> crate::Result<bool> {
        Ok(self.read(1)? != 0)
    }

    fn ue(&mut self) -> crate::Result<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 32 {
                return Err("H.264 Exp-Golomb prefix exceeds u32".into());
            }
        }
        let suffix = u64::from(self.read(zeros)?);
        let value = ((1_u64 << zeros) - 1) + suffix;
        u32::try_from(value).map_err(|_| "H.264 Exp-Golomb value exceeds u32".into())
    }

    fn bounded_ue(&mut self, name: &str, maximum: u32) -> crate::Result<u32> {
        let value = self.ue()?;
        if value > maximum {
            return Err(format!("H.264 {name}={value} exceeds {maximum}").into());
        }
        Ok(value)
    }

    fn se(&mut self) -> crate::Result<i32> {
        let code = u64::from(self.ue()?);
        let magnitude = code.div_ceil(2) as i64;
        let value = if code & 1 == 0 { -magnitude } else { magnitude };
        i32::try_from(value).map_err(|_| "H.264 signed Exp-Golomb value exceeds i32".into())
    }

    fn finish(&mut self) -> crate::Result<()> {
        if !self.flag()? {
            return Err("H.264 SPS rbsp_stop_one_bit is missing".into());
        }
        while self.position % 8 != 0 {
            if self.flag()? {
                return Err("H.264 SPS rbsp_alignment_zero_bit is nonzero".into());
            }
        }
        if self.position != self.bytes.len() * 8 {
            return Err("H.264 SPS has extra bytes after rbsp_trailing_bits".into());
        }
        Ok(())
    }
}

fn parse_sps(rbsp: &[u8]) -> crate::Result<Value> {
    let mut bits = Bits::new(rbsp);
    let profile = bits.read(8)?;
    if !matches!(profile, 66 | 77 | 88 | 100 | 110 | 122 | 244) {
        return Err(format!("H.264 SPS profile_idc {profile} is unsupported").into());
    }
    let constraints = bits.read(6)?;
    if bits.read(2)? != 0 {
        return Err("H.264 SPS reserved_zero_2bits is nonzero".into());
    }
    let level = bits.read(8)?;
    let id = bits.bounded_ue("seq_parameter_set_id", 31)?;
    let mut chroma = 1;
    let mut separate = false;
    let mut luma_depth = 8;
    let mut chroma_depth = 8;
    if matches!(profile, 100 | 110 | 122 | 244) {
        chroma = bits.bounded_ue("chroma_format_idc", 3)?;
        if chroma == 3 {
            separate = bits.flag()?;
        }
        luma_depth += bits.bounded_ue("bit_depth_luma_minus8", 6)?;
        chroma_depth += bits.bounded_ue("bit_depth_chroma_minus8", 6)?;
        let bypass = bits.flag()?;
        // SPS-local profile restrictions from Annex A.2; slices/PPS remain unobserved.
        if (profile == 100 && (chroma > 1 || luma_depth != 8 || chroma_depth != 8))
            || (profile == 110 && (chroma > 1 || luma_depth > 10 || chroma_depth > 10))
            || (profile == 122 && (chroma > 2 || luma_depth > 10 || chroma_depth > 10))
            || (profile != 244 && bypass)
        {
            return Err("H.264 SPS chroma/depth/bypass conflicts with its profile".into());
        }
        if bits.flag()? {
            for index in 0..if chroma == 3 { 12 } else { 8 } {
                if bits.flag()? {
                    skip_scaling_list(&mut bits, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.bounded_ue("log2_max_frame_num_minus4", 12)?;
    match bits.bounded_ue("pic_order_cnt_type", 2)? {
        0 => {
            bits.bounded_ue("log2_max_pic_order_cnt_lsb_minus4", 12)?;
        }
        1 => {
            bits.flag()?;
            bits.se()?;
            bits.se()?;
            let cycle = bits.bounded_ue("num_ref_frames_in_pic_order_cnt_cycle", 255)?;
            for _ in 0..cycle {
                bits.se()?;
            }
        }
        _ => {}
    }
    let refs = bits.bounded_ue("max_num_ref_frames", 16)?;
    bits.flag()?;
    let width_mbs = u64::from(bits.ue()?) + 1;
    let height_map_units = u64::from(bits.ue()?) + 1;
    let frame_only = bits.flag()?;
    if !frame_only {
        bits.flag()?;
    }
    let direct = bits.flag()?;
    let field_factor = if frame_only { 1_u64 } else { 2 };
    let frame_height_mbs = height_map_units * field_factor;
    let max_dpb =
        validate_picture_limits(profile, level, constraints, width_mbs, frame_height_mbs)?;
    if refs > max_dpb {
        return Err("H.264 max_num_ref_frames exceeds its level/picture DPB bound".into());
    }
    let set4 = constraints & 2 != 0;
    let set5 = constraints & 1 != 0;
    if (!frame_only && (!direct || profile == 66 || set4))
        || (set4 && !matches!(profile, 77 | 88 | 100 | 110))
        || (set5 && !matches!(profile, 77 | 88 | 100))
    {
        return Err("H.264 SPS frame/constraint flags are inconsistent or reserved".into());
    }
    let coded_width = width_mbs * 16;
    let coded_height = frame_height_mbs * 16;
    let (mut cropped_width, mut cropped_height) = (coded_width, coded_height);
    if bits.flag()? {
        let left = u64::from(bits.ue()?);
        let right = u64::from(bits.ue()?);
        let top = u64::from(bits.ue()?);
        let bottom = u64::from(bits.ue()?);
        let array_type = if separate { 0 } else { chroma };
        let (crop_x, crop_y) = match array_type {
            0 => (1, field_factor),
            1 => (2, 2 * field_factor),
            2 => (2, field_factor),
            3 => (1, field_factor),
            _ => unreachable!(),
        };
        cropped_width = coded_width
            .checked_sub((left + right) * crop_x)
            .filter(|&value| value != 0)
            .ok_or("H.264 horizontal crop removes the entire picture")?;
        cropped_height = coded_height
            .checked_sub((top + bottom) * crop_y)
            .filter(|&value| value != 0)
            .ok_or("H.264 vertical crop removes the entire picture")?;
    }
    let video_signal = if bits.flag()? {
        parse_vui(&mut bits, refs, max_dpb)?
    } else {
        None
    };
    bits.finish()?;
    Ok(json!({
        "seq_parameter_set_id": id,
        "profile_idc": profile,
        "level_idc": level,
        "chroma_format_idc": chroma,
        "separate_colour_plane_flag": separate,
        "bit_depth_luma": luma_depth,
        "bit_depth_chroma": chroma_depth,
        "frame_mbs_only_flag": frame_only,
        "coded_size": [coded_width, coded_height],
        "cropped_size": [cropped_width, cropped_height],
        "video_signal_type": video_signal
    }))
}

fn skip_scaling_list(bits: &mut Bits<'_>, size: usize) -> crate::Result<()> {
    let mut last = 8;
    let mut next = 8;
    for _ in 0..size {
        if next != 0 {
            let delta = bits.se()?;
            if !(-128..=127).contains(&delta) {
                return Err("H.264 scaling-list delta_scale is outside -128..127".into());
            }
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

fn validate_picture_limits(
    profile: u32,
    level: u32,
    constraints: u32,
    width: u64,
    height: u64,
) -> crate::Result<u32> {
    // Only header-local size/DPB limits from Table A-1, not bitrate/slice conformance.
    let (max_fs, max_dpb_mbs): (u64, u64) = match level {
        9 if profile >= 100 => (99, 396),
        10 => (99, 396),
        11 if profile < 100 && constraints & 4 != 0 => (99, 396),
        11 => (396, 900),
        12 | 13 | 20 => (396, 2376),
        21 => (792, 4752),
        22 | 30 => (1620, 8100),
        31 => (3600, 18000),
        32 => (5120, 20480),
        40 | 41 => (8192, 32768),
        42 => (8704, 34816),
        50 => (22080, 110400),
        51 | 52 => (36864, 184320),
        60..=62 => (139264, 696320),
        _ => {
            return Err(format!(
                "H.264 level_idc {level} is unsupported or invalid for this profile"
            )
            .into())
        }
    };
    if width > max_fs
        || height > max_fs
        || width * height > max_fs
        || width * width > max_fs * 8
        || height * height > max_fs * 8
    {
        return Err("H.264 coded picture exceeds its declared level size limits".into());
    }
    Ok((max_dpb_mbs / (width * height)).min(16) as u32)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Hrd {
    cpb_count_minus1: u32,
    lengths: [u32; 4],
}

fn parse_hrd(bits: &mut Bits<'_>) -> crate::Result<Hrd> {
    let cpb_count_minus1 = bits.bounded_ue("cpb_cnt_minus1", 31)?;
    bits.read(4)?;
    bits.read(4)?;
    let mut previous = None;
    for _ in 0..=cpb_count_minus1 {
        let rate = bits.bounded_ue("bit_rate_value_minus1", u32::MAX - 1)?;
        let size = bits.bounded_ue("cpb_size_value_minus1", u32::MAX - 1)?;
        bits.flag()?;
        if let Some((previous_rate, previous_size)) = previous {
            if rate <= previous_rate || size > previous_size {
                return Err("H.264 HRD scheduling values are not ordered".into());
            }
        }
        previous = Some((rate, size));
    }
    Ok(Hrd {
        cpb_count_minus1,
        lengths: [bits.read(5)?, bits.read(5)?, bits.read(5)?, bits.read(5)?],
    })
}

fn parse_vui(bits: &mut Bits<'_>, refs: u32, max_dpb: u32) -> crate::Result<Option<Value>> {
    if bits.flag()? {
        match bits.read(8)? {
            255 => {
                let width = bits.read(16)?;
                let height = bits.read(16)?;
                if width != 0 && height != 0 && gcd(width, height) != 1 {
                    return Err("H.264 extended SAR dimensions are not relatively prime".into());
                }
            }
            0..=16 => {}
            _ => return Err("H.264 aspect_ratio_idc is reserved".into()),
        }
    }
    if bits.flag()? {
        bits.flag()?;
    }
    let signal = if bits.flag()? {
        let format = bits.read(3)?;
        if format > 5 {
            return Err("H.264 video_format is reserved".into());
        }
        let full_range = bits.flag()?;
        let colour = if bits.flag()? {
            Some(json!({
                "colour_primaries": bits.read(8)?,
                "transfer_characteristics": bits.read(8)?,
                "matrix_coefficients": bits.read(8)?
            }))
        } else {
            None
        };
        Some(json!({
            "video_format": format,
            "video_full_range_flag": full_range,
            "colour_description": colour
        }))
    } else {
        None
    };
    if bits.flag()? {
        bits.bounded_ue("chroma_sample_loc_type_top_field", 5)?;
        bits.bounded_ue("chroma_sample_loc_type_bottom_field", 5)?;
    }
    let mut fixed_rate = false;
    if bits.flag()? {
        let tick = bits.read(32)?;
        let scale = bits.read(32)?;
        fixed_rate = bits.flag()?;
        if tick == 0 || scale == 0 {
            return Err("H.264 timing_info has a zero tick/time scale".into());
        }
    }
    let nal_hrd = if bits.flag()? {
        Some(parse_hrd(bits)?)
    } else {
        None
    };
    let vcl_hrd = if bits.flag()? {
        Some(parse_hrd(bits)?)
    } else {
        None
    };
    if let (Some(nal), Some(vcl)) = (nal_hrd, vcl_hrd) {
        if nal.lengths != vcl.lengths {
            return Err("H.264 NAL/VCL HRD delay lengths disagree".into());
        }
    }
    if nal_hrd.is_some() || vcl_hrd.is_some() {
        let low_delay = bits.flag()?;
        if low_delay
            && (fixed_rate
                || nal_hrd.is_some_and(|hrd| hrd.cpb_count_minus1 != 0)
                || vcl_hrd.is_some_and(|hrd| hrd.cpb_count_minus1 != 0))
        {
            return Err("H.264 low-delay HRD conflicts with fixed rate or multiple CPBs".into());
        }
    }
    bits.flag()?; // pic_struct_present_flag
    if bits.flag()? {
        bits.flag()?;
        bits.bounded_ue("max_bytes_per_pic_denom", 16)?;
        bits.bounded_ue("max_bits_per_mb_denom", 16)?;
        bits.bounded_ue("log2_max_mv_length_horizontal", 15)?;
        bits.bounded_ue("log2_max_mv_length_vertical", 15)?;
        let reorder = bits.bounded_ue("max_num_reorder_frames", max_dpb)?;
        let buffering = bits.bounded_ue("max_dec_frame_buffering", max_dpb)?;
        if reorder > buffering || buffering < refs {
            return Err(
                "H.264 bitstream restriction contradicts reorder/reference buffering".into(),
            );
        }
    }
    Ok(signal)
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        position: usize,
    }

    impl Writer {
        fn bits(&mut self, value: u64, count: u32) {
            for shift in (0..count).rev() {
                if self.position % 8 == 0 {
                    self.bytes.push(0);
                }
                let byte = self.bytes.last_mut().unwrap();
                *byte |= (((value >> shift) & 1) as u8) << (7 - self.position % 8);
                self.position += 1;
            }
        }
        fn flag(&mut self, value: bool) {
            self.bits(u64::from(value), 1);
        }
        fn ue(&mut self, value: u32) {
            let code = u64::from(value) + 1;
            let length = 64 - code.leading_zeros();
            self.bits(0, length - 1);
            self.bits(code, length);
        }
        fn se(&mut self, value: i32) {
            let code = if value > 0 {
                i64::from(value) * 2 - 1
            } else {
                -i64::from(value) * 2
            };
            self.ue(u32::try_from(code).unwrap());
        }
        fn finish(mut self) -> Vec<u8> {
            self.flag(true);
            while self.position % 8 != 0 {
                self.flag(false);
            }
            self.bytes
        }
    }

    fn annex(rbsp: &[u8], prefix: usize) -> Vec<u8> {
        let mut output = vec![0; prefix - 1];
        output.extend([1, 0x67]);
        let mut zeros = 0;
        for &byte in rbsp {
            if zeros == 2 && byte <= 3 {
                output.push(3);
                zeros = 0;
            }
            output.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        output
    }

    fn sps_prefix(
        profile: u32,
        chroma: u32,
        separate: bool,
        field: bool,
        crop: [u32; 4],
        scaling: bool,
    ) -> Writer {
        let mut w = Writer::default();
        w.bits(u64::from(profile), 8);
        w.bits(0, 8); // constraints and reserved zero bits
        w.bits(31, 8);
        w.ue(0);
        if matches!(profile, 100 | 110 | 122 | 244) {
            w.ue(chroma);
            if chroma == 3 {
                w.flag(separate);
            }
            w.ue(if profile == 244 { 6 } else { 0 });
            w.ue(if profile == 244 { 6 } else { 0 });
            w.flag(false);
            w.flag(scaling);
            if scaling {
                for index in 0..if chroma == 3 { 12 } else { 8 } {
                    w.flag(index == 0 || index == 6);
                    if index == 0 {
                        w.se(-8);
                    } // nextScale=0: no further deltas
                    if index == 6 {
                        for _ in 0..64 {
                            w.se(0);
                        }
                    }
                }
            }
        }
        w.ue(0);
        w.ue(1); // exercise full POC type 1 grammar
        w.flag(false);
        w.se(-1);
        w.se(1);
        w.ue(2);
        w.se(0);
        w.se(1);
        w.ue(1);
        w.flag(false);
        w.ue(59); // coded width 960
        w.ue(if field { 16 } else { 33 }); // coded height 544
        w.flag(!field);
        if field {
            w.flag(true);
        }
        w.flag(true);
        w.flag(crop != [0; 4]);
        if crop != [0; 4] {
            for value in crop {
                w.ue(value);
            }
        }
        w
    }

    fn simple_sps(profile: u32, crop: [u32; 4]) -> Vec<u8> {
        let mut w = sps_prefix(profile, 1, false, false, crop, false);
        w.flag(false);
        w.finish()
    }

    fn hrd(w: &mut Writer, lengths: [u32; 4]) {
        w.ue(0);
        w.bits(0, 4);
        w.bits(0, 4);
        w.ue(99);
        w.ue(99);
        w.flag(true);
        for length in lengths {
            w.bits(u64::from(length), 5);
        }
    }

    fn full_vui(w: &mut Writer, mv: u32, tick: u32, mismatch_hrd: bool) {
        w.flag(true); // VUI present
        w.flag(true); // extended SAR; zero means unspecified, legally
        w.bits(255, 8);
        w.bits(0, 16);
        w.bits(0, 16);
        w.flag(true);
        w.flag(false);
        w.flag(true);
        w.bits(5, 3);
        w.flag(false); // explicit limited-range declaration, not inferred
        w.flag(true);
        w.bits(9, 8);
        w.bits(16, 8);
        w.bits(9, 8);
        w.flag(true);
        w.ue(5);
        w.ue(0);
        w.flag(true);
        w.bits(u64::from(tick), 32);
        w.bits(60, 32);
        w.flag(true);
        w.flag(true);
        hrd(w, [23, 23, 23, 24]);
        w.flag(true);
        hrd(w, [23, 23, 23, if mismatch_hrd { 25 } else { 24 }]);
        w.flag(false); // low delay
        w.flag(true); // pic struct
        w.flag(true);
        w.flag(true);
        w.ue(16);
        w.ue(16);
        w.ue(mv);
        w.ue(15);
        w.ue(1);
        w.ue(2);
    }

    #[test]
    fn recording_h264_header_baseline_main_extended_and_missing_vui() {
        for profile in [66, 77, 88] {
            let mut bytes = annex(&simple_sps(profile, [0, 0, 0, 2]), 3);
            bytes.extend([0, 0, 0, 1, 0x68, 0x80, 0, 0]);
            let report = inspect_annex_b(&bytes).unwrap();
            let sps = &report["sps"][0];
            assert_eq!(sps["coded_size"], json!([960, 544]));
            assert_eq!(sps["cropped_size"], json!([960, 540]));
            assert_eq!(sps["bit_depth_luma"], 8);
            assert!(sps["video_signal_type"].is_null());
            assert_eq!(report["parameter_set_selection"], "not_observed");
            assert_eq!(report["pps_syntax"], "not_parsed");
        }
    }

    #[test]
    fn recording_h264_header_high_colour_scaling_hrd_and_crop_units() {
        for (profile, chroma, separate, field, crop, expected) in [
            (100, 1, false, false, [0, 0, 0, 2], [960, 540]),
            (110, 1, false, false, [1, 1, 1, 1], [956, 540]),
            (122, 2, false, true, [1, 1, 1, 1], [956, 540]),
            (244, 3, true, true, [1, 1, 1, 1], [958, 540]),
        ] {
            let mut w = sps_prefix(profile, chroma, separate, field, crop, true);
            full_vui(&mut w, 15, 1, false);
            let report = inspect_annex_b(&annex(&w.finish(), 4)).unwrap();
            let sps = &report["sps"][0];
            assert_eq!(sps["cropped_size"], json!(expected));
            assert_eq!(sps["separate_colour_plane_flag"], separate);
            assert_eq!(sps["bit_depth_luma"], if profile == 244 { 14 } else { 8 });
            assert_eq!(
                sps["video_signal_type"],
                json!({
                    "video_format": 5,
                    "video_full_range_flag": false,
                    "colour_description": {"colour_primaries": 9, "transfer_characteristics": 16, "matrix_coefficients": 9}
                })
            );
        }
        let mut w = sps_prefix(100, 1, false, false, [0; 4], false);
        w.flag(true); // VUI with video signal, but no colour description
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.bits(5, 3);
        w.flag(true);
        w.flag(false);
        for _ in 0..6 {
            w.flag(false);
        }
        let report = inspect_annex_b(&annex(&w.finish(), 4)).unwrap();
        assert_eq!(
            report["sps"][0]["video_signal_type"]["video_full_range_flag"],
            true
        );
        assert!(report["sps"][0]["video_signal_type"]["colour_description"].is_null());
    }

    #[test]
    fn recording_h264_header_rejects_truncation_tail_and_vui_constraints() {
        let mut w = sps_prefix(100, 1, false, false, [0, 0, 0, 2], true);
        full_vui(&mut w, 15, 1, false);
        let rbsp = w.finish();
        for length in 0..rbsp.len() {
            assert!(
                inspect_annex_b(&annex(&rbsp[..length], 4)).is_err(),
                "prefix length {length}"
            );
        }
        let mut extra = rbsp.clone();
        extra.push(0x80);
        assert!(inspect_annex_b(&annex(&extra, 4)).is_err());
        let mut bad_alignment = simple_sps(66, [0; 4]);
        *bad_alignment.last_mut().unwrap() |= 1;
        assert!(inspect_annex_b(&annex(&bad_alignment, 4)).is_err());
        for (mv, tick, mismatch) in [(16, 1, false), (15, 0, false), (15, 1, true)] {
            let mut w = sps_prefix(100, 1, false, false, [0; 4], false);
            full_vui(&mut w, mv, tick, mismatch);
            assert!(inspect_annex_b(&annex(&w.finish(), 4)).is_err());
        }
        assert!(inspect_annex_b(&annex(&simple_sps(66, [480, 0, 0, 0]), 4)).is_err());
    }

    #[test]
    fn recording_h264_header_annex_emulation_and_resource_limits() {
        assert_eq!(
            unescape(&[0, 0, 3, 0, 0, 3, 1, 0x80]).unwrap(),
            [0, 0, 0, 0, 1, 0x80]
        );
        for bytes in [&[0, 0, 0][..], &[0, 0, 2], &[0, 0, 3], &[0, 0, 3, 4]] {
            assert!(unescape(bytes).is_err());
        }
        assert!(inspect_annex_b(&[1, 100, 0, 31]).is_err()); // avcC, not guessed
        assert!(inspect_annex_b(&vec![0; MAX_HEADER_BYTES + 1]).is_err());
        assert!(inspect_annex_b(&[0, 0, 1, 0x68, 0x80]).is_err()); // no SPS
        assert!(inspect_annex_b(&[0, 0, 1, 0, 0, 1]).is_err());
        let nal = annex(&simple_sps(100, [0; 4]), 4);
        assert_eq!(
            inspect_annex_b(&nal.repeat(MAX_SPS)).unwrap()["sps"]
                .as_array()
                .unwrap()
                .len(),
            MAX_SPS
        );
        assert!(inspect_annex_b(&nal.repeat(MAX_SPS + 1)).is_err());
        let mut maximum = nal.clone();
        for _ in 1..MAX_NALS {
            maximum.extend([0, 0, 1, 0x68, 0x80]);
        }
        assert_eq!(inspect_annex_b(&maximum).unwrap()["nal_count"], MAX_NALS);
        maximum.extend([0, 0, 1, 0x68, 0x80]);
        assert!(inspect_annex_b(&maximum).is_err());
        let mut unsupported = simple_sps(66, [0; 4]);
        unsupported[0] = 83;
        let mut mixed = nal;
        mixed.extend(annex(&unsupported, 4));
        assert!(inspect_annex_b(&mixed).is_err()); // no partial SPS success
    }

    #[test]
    fn recording_h264_header_exp_golomb_and_local_semantic_bounds() {
        let mut w = Writer::default();
        w.ue(u32::MAX);
        let data = w.finish();
        assert_eq!(Bits::new(&data).ue().unwrap(), u32::MAX);
        assert!(Bits::new(&data).se().is_err()); // +2^31 is not an i32 offset
        let mut w = Writer::default();
        w.bits(0, 33);
        w.flag(true);
        assert!(Bits::new(&w.finish()).ue().is_err());
        let mut w = Writer::default();
        w.se(i32::MAX);
        w.se(-i32::MAX);
        let data = w.finish();
        let mut bits = Bits::new(&data);
        assert_eq!(bits.se().unwrap(), i32::MAX);
        assert_eq!(bits.se().unwrap(), -i32::MAX);
        assert!(validate_picture_limits(100, 31, 0, u64::from(u32::MAX) + 1, 1).is_err());
        assert!(validate_picture_limits(100, 1, 0, 60, 34).is_err());
        let mut reserved = simple_sps(100, [0; 4]);
        reserved[1] |= 1;
        assert!(inspect_annex_b(&annex(&reserved, 4)).is_err());
    }
}
