use std::{num::NonZero, time::Duration};

use mmrecode_core::AudioFrame;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, buffer::SamplesBuffer};

pub(crate) struct AudioOutput {
    _device: MixerDeviceSink,
    player: Player,
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

impl AudioOutput {
    pub(crate) fn open(mut frame: AudioFrame, audio_minus_video: f64) -> Result<Self, String> {
        align_to_video(&mut frame, audio_minus_video);
        let samples = frame
            .samples
            .into_iter()
            .map(|sample| f32::from(sample) / 32_768.0)
            .collect();
        let mut device = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| format!("cannot open an audio output device: {error}"))?;
        device.log_on_drop(false);
        let player = Player::connect_new(device.mixer());
        player.pause();
        let output = Self {
            _device: device,
            player,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            samples,
        };
        output.queue()?;
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

    pub(crate) fn seek(&self, position: Duration) -> Result<(), String> {
        if self.player.empty() {
            self.queue()?;
        }
        self.player
            .try_seek(position.min(self.duration()))
            .map_err(|error| format!("cannot seek AAC playback: {error}"))
    }

    pub(crate) fn restart(&self) -> Result<(), String> {
        self.queue()?;
        self.player.play();
        Ok(())
    }

    fn duration(&self) -> Duration {
        let frames = self.samples.len() / usize::from(self.channels);
        let nanos = frames as u128 * 1_000_000_000 / u128::from(self.sample_rate);
        u64::try_from(nanos).map_or(Duration::MAX, Duration::from_nanos)
    }

    fn queue(&self) -> Result<(), String> {
        let channels = NonZero::new(self.channels)
            .ok_or_else(|| "AAC output channel count is zero".to_owned())?;
        let sample_rate = NonZero::new(self.sample_rate)
            .ok_or_else(|| "AAC output sample rate is zero".to_owned())?;
        self.player.clear();
        self.player.append(SamplesBuffer::new(
            channels,
            sample_rate,
            self.samples.clone(),
        ));
        self.player.pause();
        Ok(())
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn align_to_video(frame: &mut AudioFrame, audio_minus_video: f64) {
    let frames = (audio_minus_video.abs() * f64::from(frame.sample_rate)).round() as usize;
    let sample_count = frames.saturating_mul(usize::from(frame.channels));
    if audio_minus_video > 0.0 {
        let mut aligned = vec![0; sample_count];
        aligned.append(&mut frame.samples);
        frame.samples = aligned;
        frame.samples_per_channel = frame.samples.len() / usize::from(frame.channels);
    } else if audio_minus_video < 0.0 {
        frame.samples.drain(..sample_count.min(frame.samples.len()));
        frame.samples_per_channel = frame.samples.len() / usize::from(frame.channels);
    }
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{AudioSampleFormat, FrameTiming};

    use super::*;

    #[test]
    fn aligns_pcm_to_video_origin() {
        let mut frame = AudioFrame {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate: 1_000,
            channels: 2,
            samples_per_channel: 1_000,
            samples: vec![1; 2_000],
            timing: FrameTiming::default(),
        };
        align_to_video(&mut frame, 0.25);
        assert_eq!(frame.samples_per_channel, 1_250);
        assert!(frame.samples[..500].iter().all(|sample| *sample == 0));
        align_to_video(&mut frame, -0.25);
        assert_eq!(frame.samples_per_channel, 1_000);
    }
}
