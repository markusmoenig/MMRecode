use std::{io::Cursor, num::NonZero, time::Duration};

use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source, buffer::SamplesBuffer};

#[derive(Clone, Debug)]
pub(crate) struct AudioTrack {
    pub(crate) sample_rate: u32,
    pub(crate) channels: u16,
    pub(crate) samples: Vec<f32>,
}

impl AudioTrack {
    pub(crate) fn from_i16(
        sample_rate: u32,
        channels: u16,
        samples: impl IntoIterator<Item = i16>,
    ) -> Result<Self, String> {
        let samples = samples
            .into_iter()
            .map(|sample| f32::from(sample) / 32_768.0)
            .collect();
        Self::new(sample_rate, channels, samples)
    }

    pub(crate) fn new(sample_rate: u32, channels: u16, samples: Vec<f32>) -> Result<Self, String> {
        if sample_rate == 0 || channels == 0 {
            return Err("audio playback requires a non-zero sample rate and channel count".into());
        }
        if samples.is_empty() || !samples.len().is_multiple_of(usize::from(channels)) {
            return Err("audio playback samples do not match the channel layout".into());
        }
        Ok(Self {
            sample_rate,
            channels,
            samples,
        })
    }

    pub(crate) fn duration(&self) -> Duration {
        let sample_frames = self.samples.len() / usize::from(self.channels);
        let nanos = sample_frames as u128 * 1_000_000_000 / u128::from(self.sample_rate);
        u64::try_from(nanos).map_or(Duration::MAX, Duration::from_nanos)
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn align_to_video(&mut self, audio_minus_video: f64) {
        let sample_frames =
            (audio_minus_video.abs() * f64::from(self.sample_rate)).round() as usize;
        let sample_count = sample_frames.saturating_mul(usize::from(self.channels));
        if audio_minus_video > 0.0 {
            let mut aligned = vec![0.0; sample_count];
            aligned.append(&mut self.samples);
            self.samples = aligned;
        } else if audio_minus_video < 0.0 {
            self.samples.drain(..sample_count.min(self.samples.len()));
        }
    }
}

pub(crate) fn decode_mpeg_layer2(data: &[u8]) -> Result<AudioTrack, String> {
    let byte_length = u64::try_from(data.len())
        .map_err(|_| "MPEG audio byte length does not fit u64".to_owned())?;
    let decoder = rodio::Decoder::builder()
        .with_data(Cursor::new(data.to_vec()))
        .with_byte_len(byte_length)
        .with_seekable(true)
        .with_coarse_seek(true)
        .with_hint("mp2")
        .with_gapless(false)
        .build()
        .map_err(|error| format!("cannot decode MPEG Layer II audio: {error}"))?;
    let channels = decoder.channels().get();
    let sample_rate = decoder.sample_rate().get();
    AudioTrack::new(sample_rate, channels, decoder.collect())
}

pub(crate) struct AudioOutput {
    _device: MixerDeviceSink,
    player: Player,
    track: AudioTrack,
}

impl AudioOutput {
    pub(crate) fn open(track: AudioTrack, volume: f32) -> Result<Self, String> {
        let mut device = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| format!("cannot open an audio output device: {error}"))?;
        device.log_on_drop(false);
        let player = Player::connect_new(device.mixer());
        player.pause();
        player.set_volume(volume);
        let output = Self {
            _device: device,
            player,
            track,
        };
        output.queue_track()?;
        Ok(output)
    }

    pub(crate) fn play(&self) {
        self.player.play();
    }

    pub(crate) fn pause(&self) {
        self.player.pause();
    }

    pub(crate) fn position(&self) -> Duration {
        self.player.get_pos()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.player.empty()
    }

    pub(crate) fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
    }

    pub(crate) fn seek(&self, position: Duration) -> Result<(), String> {
        if self.player.empty() {
            self.queue_track()?;
        }
        self.player
            .try_seek(position.min(self.track.duration()))
            .map_err(|error| format!("cannot seek audio: {error}"))
    }

    pub(crate) fn restart(&self) -> Result<(), String> {
        self.queue_track()?;
        self.player.play();
        Ok(())
    }

    fn queue_track(&self) -> Result<(), String> {
        let channels = NonZero::new(self.track.channels)
            .ok_or_else(|| "audio channel count is zero".to_owned())?;
        let sample_rate = NonZero::new(self.track.sample_rate)
            .ok_or_else(|| "audio sample rate is zero".to_owned())?;
        self.player.clear();
        self.player.append(SamplesBuffer::new(
            channels,
            sample_rate,
            self.track.samples.clone(),
        ));
        self.player.pause();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP2: &[u8] =
        include_bytes!("../../../../testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2");

    #[test]
    fn decodes_permanent_mpeg_layer2_vector_to_pcm() {
        let audio = decode_mpeg_layer2(MP2).expect("decode MP2");
        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.samples.len(), 20 * 1_152 * 2);
        assert!(audio.samples.iter().any(|sample| sample.abs() > 0.01));
        assert_eq!(audio.duration(), Duration::from_millis(480));
    }

    #[test]
    fn aligns_pcm_to_a_video_origin() {
        let mut delayed = AudioTrack::new(1_000, 2, vec![1.0; 2_000]).unwrap();
        delayed.align_to_video(0.25);
        assert_eq!(delayed.samples.len(), 2_500);
        assert!(delayed.samples[..500].iter().all(|sample| *sample == 0.0));

        delayed.align_to_video(-0.25);
        assert_eq!(delayed.samples.len(), 2_000);
    }

    #[test]
    #[ignore = "requires a real audio output device"]
    fn renders_samples_through_the_default_device() {
        let audio = decode_mpeg_layer2(MP2).expect("decode MP2");
        let output = AudioOutput::open(audio, 0.1).expect("open default output");
        output.play();
        std::thread::sleep(Duration::from_millis(250));
        output.pause();
        assert!(output.position() >= Duration::from_millis(100));
        assert!(!output.is_finished());
    }
}
