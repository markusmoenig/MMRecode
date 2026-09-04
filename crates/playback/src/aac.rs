//! Indexed AAC MP4/MOV playback with scheduled PCM reconstruction.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::Duration,
};

#[cfg(not(target_arch = "wasm32"))]
use std::{
    io::Write as _,
    process::{Command, Stdio},
};

use mmrecode_aac::AudioSpecificConfig;
use mmrecode_core::{AudioFrame, Rational};
#[cfg(not(target_arch = "wasm32"))]
use mmrecode_core::{AudioSampleFormat, FrameTiming};
use mmrecode_isobmff::{IsoBmffFile, Track};

use crate::{DecodeExecutor, default_decode_executor};

/// Timing and byte-localization metadata for one AAC access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedAacSample {
    /// Decode timestamp in the audio track time base.
    pub dts: i64,
    /// Presentation timestamp in the audio track time base.
    pub pts: i64,
    /// Duration in the audio track time base.
    pub duration: u32,
    /// Encoded payload size in bytes.
    pub byte_length: usize,
}

/// Presentation metadata for one indexed AAC-LC track.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AacAudioIndex {
    configuration: AudioSpecificConfig,
    time_base: Rational,
    samples: Vec<IndexedAacSample>,
    start_time: Duration,
    duration: Duration,
    trim_start_samples: usize,
    decoded_samples_per_channel: usize,
}

impl AacAudioIndex {
    /// Returns the parsed MPEG-4 audio configuration.
    #[must_use]
    pub const fn configuration(&self) -> &AudioSpecificConfig {
        &self.configuration
    }

    /// Returns the track timestamp unit.
    #[must_use]
    pub const fn time_base(&self) -> Rational {
        self.time_base
    }

    /// Returns indexed raw AAC access units in decode order.
    #[must_use]
    pub fn samples(&self) -> &[IndexedAacSample] {
        &self.samples
    }

    /// Returns the first audio presentation time relative to the movie clock origin.
    #[must_use]
    pub const fn start_time(&self) -> Duration {
        self.start_time
    }

    /// Returns the indexed presentation duration.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// Returns decoded priming samples to discard from the start of each channel.
    #[must_use]
    pub const fn trim_start_samples(&self) -> usize {
        self.trim_start_samples
    }

    /// Returns PCM samples per channel in the edited presentation interval.
    #[must_use]
    pub const fn decoded_samples_per_channel(&self) -> usize {
        self.decoded_samples_per_channel
    }
}

/// One asynchronous AAC playback result.
#[derive(Debug)]
pub enum AacPlaybackEvent {
    /// The complete indexed track was reconstructed to interleaved signed 16-bit PCM.
    Decoded {
        /// Request generation used to discard obsolete work.
        generation: u64,
        /// Decoded PCM samples.
        audio: Box<AudioFrame>,
    },
    /// PCM reconstruction failed.
    Error {
        /// Request generation that failed.
        generation: u64,
        /// Human-readable decoder error.
        message: String,
    },
}

#[derive(Debug)]
struct AacWorker {
    #[cfg(not(target_arch = "wasm32"))]
    movie: IsoBmffFile,
    #[cfg(not(target_arch = "wasm32"))]
    track_index: usize,
    #[cfg(not(target_arch = "wasm32"))]
    configuration: AudioSpecificConfig,
    #[cfg(not(target_arch = "wasm32"))]
    trim_start_samples: usize,
    #[cfg(not(target_arch = "wasm32"))]
    decoded_samples_per_channel: usize,
}

/// Indexed AAC source scheduled through `MMRecode`'s shared decode executor.
///
/// `MMRecode` owns ISO-BMFF demuxing, `esds`/ASC interpretation, timing, access-unit indexing, and
/// ADTS adaptation. This first usable native slice delegates spectral reconstruction to an
/// optional `FFmpeg` process while the native Rust AAC-LC decoder is built behind
/// [`AudioDecoder`](mmrecode_core::AudioDecoder).
#[derive(Debug)]
pub struct AacPlaybackSource {
    index: AacAudioIndex,
    executor: Arc<dyn DecodeExecutor>,
    worker: Arc<AacWorker>,
    events: Receiver<AacPlaybackEvent>,
    event_sender: Sender<AacPlaybackEvent>,
    active_generation: Arc<AtomicU64>,
    generation: u64,
}

impl AacPlaybackSource {
    /// Parses and indexes an owned MP4/MOV file.
    ///
    /// # Errors
    ///
    /// Returns an error when no AAC track exists or its configuration/timing is unsupported.
    pub fn new(file_data: Vec<u8>) -> Result<Self, String> {
        Self::with_executor(file_data, default_decode_executor()?)
    }

    /// Parses and indexes an owned MP4/MOV file using a caller-provided executor.
    ///
    /// # Errors
    ///
    /// Returns an error when no AAC track exists or its configuration/timing is unsupported.
    pub fn with_executor(
        file_data: Vec<u8>,
        executor: Arc<dyn DecodeExecutor>,
    ) -> Result<Self, String> {
        let movie = IsoBmffFile::parse(file_data).map_err(|error| error.to_string())?;
        let (track_index, track) = movie
            .tracks()
            .iter()
            .enumerate()
            .find(|(_, track)| track.descriptor.codec.codec_id.as_str() == "audio/aac")
            .ok_or_else(|| "ISO-BMFF file has no AAC audio track".to_owned())?;
        let configuration = AudioSpecificConfig::parse(&track.descriptor.codec.configuration)
            .map_err(|error| error.to_string())?;
        validate_track_metadata(track, &configuration)?;
        let index = build_index(track, configuration.clone())?;
        let trim_start_samples = index.trim_start_samples;
        let decoded_samples_per_channel = index.decoded_samples_per_channel;
        let (event_sender, events) = mpsc::channel();
        #[cfg(target_arch = "wasm32")]
        let _ = (
            &movie,
            track_index,
            &configuration,
            trim_start_samples,
            decoded_samples_per_channel,
        );
        Ok(Self {
            index,
            executor,
            worker: Arc::new(AacWorker {
                #[cfg(not(target_arch = "wasm32"))]
                movie,
                #[cfg(not(target_arch = "wasm32"))]
                track_index,
                #[cfg(not(target_arch = "wasm32"))]
                configuration,
                #[cfg(not(target_arch = "wasm32"))]
                trim_start_samples,
                #[cfg(not(target_arch = "wasm32"))]
                decoded_samples_per_channel,
            }),
            events,
            event_sender,
            active_generation: Arc::new(AtomicU64::new(0)),
            generation: 0,
        })
    }

    /// Returns parsed timing and format metadata.
    #[must_use]
    pub const fn index(&self) -> &AacAudioIndex {
        &self.index
    }

    /// Schedules decoding of the indexed audio track and returns its generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared executor cannot accept the job.
    pub fn request_decode(&mut self) -> Result<u64, String> {
        self.generation = self.generation.wrapping_add(1).max(1);
        let generation = self.generation;
        self.active_generation.store(generation, Ordering::Release);
        let worker = Arc::clone(&self.worker);
        let events = self.event_sender.clone();
        let active_generation = Arc::clone(&self.active_generation);
        self.executor
            .submit(Box::new(move || {
                if active_generation.load(Ordering::Acquire) != generation {
                    return;
                }
                let event = match decode_worker(&worker) {
                    Ok(audio) => AacPlaybackEvent::Decoded {
                        generation,
                        audio: Box::new(audio),
                    },
                    Err(message) => AacPlaybackEvent::Error {
                        generation,
                        message,
                    },
                };
                if active_generation.load(Ordering::Acquire) == generation {
                    let _ = events.send(event);
                }
            }))
            .map_err(|error| error.to_string())?;
        Ok(generation)
    }

    /// Polls one completed decode event without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error if the result channel disconnects unexpectedly.
    pub fn try_event(&self) -> Result<Option<AacPlaybackEvent>, String> {
        if self.executor.is_cooperative() {
            self.executor.poll(1);
        }
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err("AAC playback decoder result channel disconnected".into())
            }
        }
    }
}

fn validate_track_metadata(
    track: &Track,
    configuration: &AudioSpecificConfig,
) -> Result<(), String> {
    if track
        .sample_rate
        .is_some_and(|rate| rate != configuration.sample_rate)
    {
        return Err(format!(
            "AAC sample entry declares {} Hz but AudioSpecificConfig declares {} Hz",
            track.sample_rate.unwrap_or_default(),
            configuration.sample_rate
        ));
    }
    if track
        .channel_count
        .is_some_and(|channels| channels != u16::from(configuration.channels))
    {
        return Err(format!(
            "AAC sample entry declares {} channels but AudioSpecificConfig declares {}",
            track.channel_count.unwrap_or_default(),
            configuration.channels
        ));
    }
    Ok(())
}

fn build_index(track: &Track, configuration: AudioSpecificConfig) -> Result<AacAudioIndex, String> {
    let time_base = track.descriptor.time_base;
    let first_pts = track
        .samples
        .iter()
        .map(|sample| sample.pts)
        .min()
        .ok_or_else(|| "AAC track has no samples".to_owned())?;
    let last_end = track
        .samples
        .iter()
        .map(|sample| sample.pts.saturating_add(i64::from(sample.duration)))
        .max()
        .ok_or_else(|| "AAC track has no samples".to_owned())?;
    let start_ticks = first_pts.max(0);
    let presentation_ticks = track.presentation_duration.map_or_else(
        || Ok(last_end.max(0)),
        |duration| {
            i64::try_from(duration).map_err(|_| "AAC presentation duration exceeds i64".to_owned())
        },
    )?;
    if presentation_ticks < start_ticks {
        return Err("AAC presentation duration precedes its start time".into());
    }
    let start_time = timestamp_duration(start_ticks, time_base)?;
    let duration = timestamp_duration(presentation_ticks, time_base)?;
    let trim_start_samples = timestamp_samples(
        first_pts.saturating_neg().max(0),
        time_base,
        configuration.sample_rate,
    )?;
    let decoded_samples_per_channel = timestamp_samples(
        presentation_ticks - start_ticks,
        time_base,
        configuration.sample_rate,
    )?;
    let samples = track
        .samples
        .iter()
        .map(|sample| IndexedAacSample {
            dts: sample.dts,
            pts: sample.pts,
            duration: sample.duration,
            byte_length: sample.source_range.len(),
        })
        .collect();
    Ok(AacAudioIndex {
        configuration,
        time_base,
        samples,
        start_time,
        duration,
        trim_start_samples,
        decoded_samples_per_channel,
    })
}

fn timestamp_samples(value: i64, time_base: Rational, sample_rate: u32) -> Result<usize, String> {
    let value = u128::try_from(value).map_err(|_| "negative AAC sample duration".to_owned())?;
    let numerator = u128::try_from(time_base.numerator())
        .map_err(|_| "invalid AAC time-base numerator".to_owned())?;
    let denominator = u128::try_from(time_base.denominator())
        .map_err(|_| "invalid AAC time-base denominator".to_owned())?;
    let samples = value
        .checked_mul(numerator)
        .and_then(|value| value.checked_mul(u128::from(sample_rate)))
        .ok_or_else(|| "AAC sample count overflows".to_owned())?
        / denominator;
    usize::try_from(samples).map_err(|_| "AAC sample count exceeds platform".to_owned())
}

fn timestamp_duration(value: i64, time_base: Rational) -> Result<Duration, String> {
    let value = u128::try_from(value).map_err(|_| "negative AAC timestamp".to_owned())?;
    let numerator = u128::try_from(time_base.numerator())
        .map_err(|_| "invalid AAC time-base numerator".to_owned())?;
    let denominator = u128::try_from(time_base.denominator())
        .map_err(|_| "invalid AAC time-base denominator".to_owned())?;
    let nanos = value
        .checked_mul(numerator)
        .and_then(|value| value.checked_mul(1_000_000_000))
        .ok_or_else(|| "AAC timestamp overflows".to_owned())?
        / denominator;
    u64::try_from(nanos)
        .map(Duration::from_nanos)
        .map_err(|_| "AAC timestamp exceeds platform duration".to_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn decode_worker(worker: &AacWorker) -> Result<AudioFrame, String> {
    let track = &worker.movie.tracks()[worker.track_index];
    let mut adts = Vec::new();
    for sample in &track.samples {
        let payload = worker
            .movie
            .sample_data(sample)
            .map_err(|error| error.to_string())?;
        adts.extend_from_slice(
            &worker
                .configuration
                .adts_header(payload.len())
                .map_err(|error| error.to_string())?,
        );
        adts.extend_from_slice(payload);
    }
    let channels = worker.configuration.channels.to_string();
    let sample_rate = worker.configuration.sample_rate.to_string();
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "aac",
            "-i",
            "pipe:0",
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            &channels,
            "-ar",
            &sample_rate,
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("optional AAC decoder is unavailable ({error})"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "cannot open AAC decoder input".to_owned())?
        .write_all(&adts)
        .map_err(|error| format!("cannot feed AAC decoder: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("AAC decoder failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "AAC decoder failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if !output
        .stdout
        .len()
        .is_multiple_of(2 * usize::from(worker.configuration.channels))
    {
        return Err("AAC decoder returned an incomplete PCM sample frame".into());
    }
    let (sample_bytes, remainder) = output.stdout.as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    let samples = sample_bytes
        .iter()
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    if samples.is_empty() {
        return Err("AAC decoder returned no PCM samples".into());
    }
    let samples = trim_decoded_samples(worker, samples)?;
    let channels = usize::from(worker.configuration.channels);
    let samples_per_channel = samples.len() / channels;
    let frame = AudioFrame {
        format: AudioSampleFormat::I16Interleaved,
        sample_rate: worker.configuration.sample_rate,
        channels: u16::from(worker.configuration.channels),
        samples_per_channel,
        samples,
        timing: FrameTiming::default(),
    };
    frame.validate().map_err(|error| error.to_string())?;
    Ok(frame)
}

#[cfg(not(target_arch = "wasm32"))]
fn trim_decoded_samples(worker: &AacWorker, mut samples: Vec<i16>) -> Result<Vec<i16>, String> {
    let channels = usize::from(worker.configuration.channels);
    let trim_samples = worker
        .trim_start_samples
        .checked_mul(channels)
        .ok_or_else(|| "AAC priming trim overflows".to_owned())?;
    if trim_samples > samples.len() {
        return Err("AAC priming trim exceeds decoded PCM".into());
    }
    samples.drain(..trim_samples);
    let output_samples = worker
        .decoded_samples_per_channel
        .checked_mul(channels)
        .ok_or_else(|| "AAC presentation sample count overflows".to_owned())?;
    if output_samples > samples.len() {
        return Err(format!(
            "AAC decoder returned {} samples/channel, fewer than the edited presentation requires {}",
            samples.len() / channels,
            worker.decoded_samples_per_channel
        ));
    }
    samples.truncate(output_samples);
    Ok(samples)
}

#[cfg(target_arch = "wasm32")]
fn decode_worker(_worker: &AacWorker) -> Result<AudioFrame, String> {
    Err("native AAC-LC reconstruction is not implemented yet; external processes are unavailable in WebAssembly".into())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{process::Command, thread, time::Instant};

    use super::*;

    #[test]
    fn indexes_and_decodes_generated_aac_lc_mp4() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping AAC interoperability test: ffmpeg is unavailable");
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "mmrecode-aac-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("tone.m4a");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.1",
                "-c:a",
                "aac",
                "-profile:a",
                "aac_low",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut source = AacPlaybackSource::new(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(source.index().configuration().sample_rate, 48_000);
        assert_eq!(source.index().configuration().channels, 1);
        assert!(!source.index().samples().is_empty());
        let expected_samples = source.index().decoded_samples_per_channel();
        let generation = source.request_decode().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = source.try_event().unwrap() {
                match event {
                    AacPlaybackEvent::Decoded {
                        generation: event_generation,
                        audio,
                    } => {
                        assert_eq!(event_generation, generation);
                        assert_eq!(audio.sample_rate, 48_000);
                        assert_eq!(audio.channels, 1);
                        assert_eq!(audio.samples_per_channel, expected_samples);
                        assert!(
                            audio
                                .samples
                                .iter()
                                .any(|sample| sample.unsigned_abs() > 100)
                        );
                        break;
                    }
                    AacPlaybackEvent::Error { message, .. } => panic!("{message}"),
                }
            }
            assert!(Instant::now() < deadline, "timed out decoding AAC");
            thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(directory);
    }
}
