//! 录屏分轨：48 kHz 立体声 16 位 PCM WAV。只读写本程序生成的格式，
//! 供预览实时混音与保存时按音量重新混音。
use crate::{
    media_foundation::{AUDIO_CHANNELS, AUDIO_RATE},
    Result,
};
use std::{
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

const HEADER: usize = 44;
const BLOCK_ALIGN: u64 = AUDIO_CHANNELS as u64 * 2;
// RIFF 长度还包含从 WAVE 到 data 长度字段的 36 字节，数据必须为完整立体声帧。
const MAX_DATA_BYTES: u64 = (u32::MAX as u64 - 36) / BLOCK_ALIGN * BLOCK_ALIGN;

fn header(data_bytes: u64) -> Result<[u8; HEADER]> {
    if data_bytes > MAX_DATA_BYTES || data_bytes % BLOCK_ALIGN != 0 {
        return Err(tr!(
            "音轨超过 WAV 容量或不是完整音频帧",
            "Audio exceeds WAV capacity or has an incomplete frame"
        )
        .into());
    }
    let data_bytes = data_bytes as u32;
    let block = AUDIO_CHANNELS * 2;
    let mut h = [0_u8; HEADER];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16_u32.to_le_bytes());
    h[20..22].copy_from_slice(&1_u16.to_le_bytes());
    h[22..24].copy_from_slice(&(AUDIO_CHANNELS as u16).to_le_bytes());
    h[24..28].copy_from_slice(&AUDIO_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(AUDIO_RATE * block).to_le_bytes());
    h[32..34].copy_from_slice(&(block as u16).to_le_bytes());
    h[34..36].copy_from_slice(&16_u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    Ok(h)
}

fn checked_data_bytes(current: u64, sample_count: usize) -> Result<u64> {
    if sample_count % AUDIO_CHANNELS as usize != 0 {
        return Err(tr!(
            "音轨采样数不是完整立体声帧",
            "Audio samples do not contain complete stereo frames"
        )
        .into());
    }
    u64::try_from(sample_count)
        .ok()
        .and_then(|count| count.checked_mul(2))
        .and_then(|bytes| current.checked_add(bytes))
        .filter(|bytes| *bytes <= MAX_DATA_BYTES)
        .ok_or_else(|| {
            tr!(
                "录制音轨已达到 WAV 容量上限",
                "Recording audio reached the WAV size limit"
            )
            .into()
        })
}

pub(crate) struct WavWriter {
    file: BufWriter<File>,
    bytes: u64,
}
impl WavWriter {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&header(0)?)?;
        Ok(Self { file, bytes: 0 })
    }
    pub(crate) fn write(&mut self, samples: &[f32]) -> Result<()> {
        let next_bytes = checked_data_bytes(self.bytes, samples.len())?;
        let mut buffer = Vec::with_capacity(samples.len() * 2);
        for value in samples {
            buffer.extend_from_slice(&((value.clamp(-1., 1.) * 32767.) as i16).to_le_bytes());
        }
        self.file.write_all(&buffer)?;
        self.bytes = next_bytes;
        Ok(())
    }
    pub(crate) fn finish(mut self) -> Result<()> {
        let header = header(self.bytes)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }
}

/// 读回交错立体声样本（-1..1）。格式不符时报错，不尝试解析外部 WAV。
pub(crate) fn read(path: &Path) -> Result<Vec<f32>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() < HEADER || bytes[0..4] != *b"RIFF" || bytes[36..40] != *b"data" {
        return Err("录屏音轨格式无效".into());
    }
    if bytes[12..36] != header(0)?[12..36] {
        return Err("录屏音轨不是 48 kHz 立体声 16 位".into());
    }
    let data = &bytes[HEADER..];
    Ok(data
        .chunks_exact(2)
        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_track_size_limit_includes_riff_header_and_stereo_alignment() {
        assert_eq!(
            checked_data_bytes(MAX_DATA_BYTES - BLOCK_ALIGN, 2).unwrap(),
            MAX_DATA_BYTES
        );
        assert!(checked_data_bytes(MAX_DATA_BYTES, 2).is_err());
        assert!(checked_data_bytes(0, 1).is_err());
        assert!(checked_data_bytes(u64::MAX, 2).is_err());
        let h = header(MAX_DATA_BYTES).unwrap();
        assert_eq!(
            u32::from_le_bytes(h[4..8].try_into().unwrap()) as u64,
            MAX_DATA_BYTES + 36
        );
        assert!(header(MAX_DATA_BYTES + BLOCK_ALIGN).is_err());
    }

    #[test]
    fn wav_track_rejects_invalid_append_before_writing_samples() {
        let path = std::env::temp_dir().join(format!(
            "rustshot-wav-append-limit-{}.wav",
            std::process::id()
        ));
        let mut writer = WavWriter::create(&path).unwrap();
        let buffered = writer.file.buffer().len();
        assert!(writer.write(&[0.5]).is_err());
        assert_eq!(writer.bytes, 0);
        assert_eq!(writer.file.buffer().len(), buffered);
        // 只替换计数模拟边界，不生成大文件；真实 write 必须在追加前拒绝。
        writer.bytes = MAX_DATA_BYTES;
        assert!(writer.write(&[0.5, -0.5]).is_err());
        assert_eq!(writer.bytes, MAX_DATA_BYTES);
        assert_eq!(writer.file.buffer().len(), buffered);
        writer.bytes = 0;
        writer.write(&[0.5, -0.5]).unwrap();
        writer.finish().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), (HEADER + 4) as u64);
        assert_eq!(read(&path).unwrap().len(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_track_round_trips_samples() {
        let path = std::env::temp_dir().join(format!("rustshot-wav-{}.wav", std::process::id()));
        let mut writer = WavWriter::create(&path).unwrap();
        writer.write(&[0., 0.5, -0.5, 1.]).unwrap();
        writer.write(&[0.25, -1.]).unwrap();
        writer.finish().unwrap();
        let samples = read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(samples.len(), 6);
        assert!((samples[1] - 0.5).abs() < 0.001 && (samples[5] + 1.).abs() < 0.001);
    }
}
