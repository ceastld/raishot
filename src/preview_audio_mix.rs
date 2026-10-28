//! 录屏原始分轨按固定块重混；不随录制长度增长 PCM 占用。
use super::wav_blocks::WavBlockReader;
use crate::{
    audio_capture::{mix_tracks, AudioSource, MixLevels},
    media_foundation::{self, MfRuntime, AUDIO_CHANNELS, AUDIO_RATE},
    Result,
};
use std::path::{Path, PathBuf};

const MIX_BLOCK_FRAMES: usize = AUDIO_RATE as usize / 2;

struct WavMixer {
    tracks: Vec<(AudioSource, WavBlockReader)>,
    levels: MixLevels,
    frames: u64,
    consumed: u64,
}

impl WavMixer {
    fn open(tracks: &[(AudioSource, PathBuf)], levels: MixLevels) -> Result<Self> {
        if tracks.is_empty()
            || tracks.len() > 2
            || (tracks.len() == 2 && tracks[0].0 == tracks[1].0)
        {
            return Err(tr!("录屏分轨来源无效", "Invalid recording audio sources").into());
        }
        let tracks = tracks
            .iter()
            .map(|(source, path)| Ok((*source, WavBlockReader::open(path)?)))
            .collect::<Result<Vec<_>>>()?;
        let frames = tracks
            .iter()
            .map(|(_, reader)| reader.frames())
            .max()
            .unwrap_or(0);
        Ok(Self {
            tracks,
            levels: levels.normalized(),
            frames,
            consumed: 0,
        })
    }

    fn next_pcm(&mut self) -> Result<Option<Vec<i16>>> {
        if self.consumed == self.frames {
            return Ok(None);
        }
        let frames = (self.frames - self.consumed).min(MIX_BLOCK_FRAMES as u64) as usize;
        let blocks = self
            .tracks
            .iter_mut()
            .map(|(source, reader)| Ok((*source, reader.read(frames)?)))
            .collect::<Result<Vec<_>>>()?;
        let labelled: Vec<_> = blocks
            .iter()
            .map(|(source, block)| (*source, block.as_slice()))
            .collect();
        let pcm = mix_tracks(&labelled, self.levels);
        if pcm.len() != frames * AUDIO_CHANNELS as usize {
            return Err(tr!(
                "录屏分轨长度发生变化，未保存混音",
                "Recording audio length changed; mix was not saved"
            )
            .into());
        }
        self.consumed = self
            .consumed
            .checked_add(frames as u64)
            .ok_or(tr!("混音帧数溢出", "Mixed audio frame count overflow"))?;
        Ok(Some(pcm))
    }
}

/// 外层输出事务负责资格、暂存和发布；取消时仅返回错误，不修改原片或原始分轨。
pub(crate) fn remux_recording_tracks(
    source: &Path,
    target: &Path,
    tracks: &[(AudioSource, PathBuf)],
    levels: MixLevels,
    mut keep_going: impl FnMut() -> bool,
) -> Result<()> {
    if !keep_going() {
        return Err(tr!("混音已取消", "Audio mix canceled").into());
    }
    let mut mixer = WavMixer::open(tracks, levels)?;
    if mixer.frames == 0 {
        return Err(tr!(
            "录屏分轨没有音频数据",
            "Recording tracks contain no audio data"
        )
        .into());
    }
    let _runtime = MfRuntime::new()?;
    media_foundation::remux_audio_streaming(source, target, || mixer.next_pcm(), keep_going)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav_track::{self, WavWriter};

    #[test]
    fn preview_audio_mix_blocks_match_existing_mix_with_unequal_track_tails() {
        let temp = crate::config_json::TestDir::new("audio-mix-blocks");
        let tracks = [
            (AudioSource::System, temp.join("system.wav")),
            (AudioSource::Microphone, temp.join("microphone.wav")),
        ];
        for (index, (_, path)) in tracks.iter().enumerate() {
            let frames = MIX_BLOCK_FRAMES + if index == 0 { 23 } else { 1 };
            let mut writer = WavWriter::create(path).unwrap();
            let values: Vec<_> = (0..frames)
                .flat_map(|frame| {
                    let value = (frame % 31) as f32 / 100.;
                    if index == 0 {
                        [value, -value]
                    } else {
                        [-value, value]
                    }
                })
                .collect();
            writer.write(&values).unwrap();
            writer.finish().unwrap();
        }
        let full: Vec<_> = tracks
            .iter()
            .map(|(source, path)| (*source, wav_track::read(path).unwrap()))
            .collect();
        let labelled: Vec<_> = full
            .iter()
            .map(|(source, samples)| (*source, samples.as_slice()))
            .collect();
        for levels in [
            MixLevels::default(),
            MixLevels {
                system: 0,
                microphone: 125,
            },
            MixLevels {
                system: 400,
                microphone: 400,
            },
        ] {
            let expected = mix_tracks(&labelled, levels);
            let mut mixer = WavMixer::open(&tracks, levels).unwrap();
            let first = mixer.next_pcm().unwrap().unwrap();
            assert_eq!(first.len(), MIX_BLOCK_FRAMES * 2);
            let tail = mixer.next_pcm().unwrap().unwrap();
            assert_eq!(tail.len(), 46);
            assert!(mixer.next_pcm().unwrap().is_none());
            assert!(mixer.next_pcm().unwrap().is_none());
            assert_eq!([first, tail].concat(), expected);
        }
    }

    #[test]
    fn preview_audio_mix_cancel_precedes_opening_media_or_writing_target() {
        let temp = crate::config_json::TestDir::new("audio-mix-cancel");
        let source = temp.join("missing.mp4");
        let target = temp.join("must-not-exist.mp4");
        let tracks = [(AudioSource::System, temp.join("missing.wav"))];
        let result =
            remux_recording_tracks(&source, &target, &tracks, MixLevels::default(), || false);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains(tr!("取消", "canceled")));
        assert!(!source.exists() && !target.exists() && !tracks[0].1.exists());
    }
}
