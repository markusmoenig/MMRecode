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

use mmrecode_aac::{AacLcDecoder, AudioSpecificConfig};
use mmrecode_core::{
    AudioDecoder, AudioFrame, AudioSampleFormat, Error, FrameTiming, Packet, PacketFlags, Rational,
    Timestamp,
};
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
        /// Implementation that actually reconstructed this track.
        backend: AacDecodeBackend,
    },
    /// PCM reconstruction failed.
    Error {
        /// Request generation that failed.
        generation: u64,
        /// Human-readable decoder error.
        message: String,
    },
}

/// AAC reconstruction policy; external fallback is never part of the native codec itself.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AacDecodePolicy {
    /// Use only our Rust decoder, including in baseline WebAssembly.
    NativeOnly,
    /// Restart unsupported tracks through the optional native-host `FFmpeg` bridge.
    #[default]
    NativeWithExternalFallback,
}

/// Decoder used for an AAC playback result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AacDecodeBackend {
    /// `MMRecode`'s own Rust AAC-LC decoder.
    Native,
    /// Optional `FFmpeg` process, unavailable in WebAssembly.
    External,
}

#[derive(Debug)]
struct AacWorker {
    movie: IsoBmffFile,
    track_index: usize,
    configuration: AudioSpecificConfig,
    trim_start_samples: usize,
    decoded_samples_per_channel: usize,
    policy: AacDecodePolicy,
}

/// Indexed AAC source scheduled through `MMRecode`'s shared decode executor.
///
/// `MMRecode` owns ISO-BMFF demuxing, `esds`/ASC interpretation, timing, access-unit indexing, and
/// ADTS adaptation. Reconstruction tries the native Rust decoder first. Unsupported tracks may
/// restart through an optional `FFmpeg` process, unless [`AacDecodePolicy::NativeOnly`] is selected.
/// Malformed input and invalid decoder state never trigger fallback.
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
        Self::with_executor_and_policy(file_data, executor, AacDecodePolicy::default())
    }

    /// Parses a source with explicit scheduling and native-only/fallback policy.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or unsupported track configuration/timing.
    pub fn with_executor_and_policy(
        file_data: Vec<u8>,
        executor: Arc<dyn DecodeExecutor>,
        policy: AacDecodePolicy,
    ) -> Result<Self, String> {
        let movie = IsoBmffFile::parse(file_data).map_err(|error| error.to_string())?;
        let (track_index, _) = movie
            .tracks()
            .iter()
            .enumerate()
            .find(|(_, track)| track.descriptor.codec.codec_id.as_str() == "audio/aac")
            .ok_or_else(|| "ISO-BMFF file has no AAC audio track".to_owned())?;
        let (index, worker) = build_worker(movie, track_index, policy)?;
        let (event_sender, events) = mpsc::channel();
        Ok(Self {
            index,
            executor,
            worker: Arc::new(worker),
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
                    Ok((audio, backend)) => AacPlaybackEvent::Decoded {
                        generation,
                        audio: Box::new(audio),
                        backend,
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

fn build_worker(
    movie: IsoBmffFile,
    track_index: usize,
    policy: AacDecodePolicy,
) -> Result<(AacAudioIndex, AacWorker), String> {
    let track = movie
        .tracks()
        .get(track_index)
        .ok_or_else(|| "AAC track index is outside the parsed movie".to_owned())?;
    let configuration = AudioSpecificConfig::parse(&track.descriptor.codec.configuration)
        .map_err(|error| error.to_string())?;
    validate_track_metadata(track, &configuration)?;
    let index = build_index(track, configuration.clone())?;
    let worker = AacWorker {
        movie,
        track_index,
        configuration,
        trim_start_samples: index.trim_start_samples,
        decoded_samples_per_channel: index.decoded_samples_per_channel,
        policy,
    };
    Ok((index, worker))
}

pub(crate) fn decode_aac_track_native(
    movie: IsoBmffFile,
    track_index: usize,
) -> Result<AudioFrame, String> {
    let (_, worker) = build_worker(movie, track_index, AacDecodePolicy::NativeOnly)?;
    decode_native(&worker).map_err(|error| error.to_string())
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

fn decode_worker(worker: &AacWorker) -> Result<(AudioFrame, AacDecodeBackend), String> {
    match decode_native(worker) {
        Ok(audio) => Ok((audio, AacDecodeBackend::Native)),
        Err(error @ Error::Unsupported(_))
            if worker.policy == AacDecodePolicy::NativeWithExternalFallback =>
        {
            decode_external(worker)
                .map(|audio| (audio, AacDecodeBackend::External))
                .map_err(|external| format!("{error}; external AAC fallback: {external}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

fn decode_native(worker: &AacWorker) -> mmrecode_core::Result<AudioFrame> {
    let track = &worker.movie.tracks()[worker.track_index];
    let mut decoder = AacLcDecoder::default();
    decoder.configure(&track.descriptor.codec)?;
    let mut samples = Vec::new();
    for sample in &track.samples {
        let timestamp = |value| {
            Some(Timestamp {
                value,
                time_base: track.descriptor.time_base,
            })
        };
        decoder.send_packet(Packet {
            stream_id: track.descriptor.id,
            data: worker.movie.sample_data(sample)?.to_vec(),
            pts: timestamp(sample.pts),
            dts: timestamp(sample.dts),
            duration: timestamp(i64::from(sample.duration)),
            flags: PacketFlags::empty(),
            side_data: Vec::new(),
        })?;
        while let Some(frame) = decoder.receive_frame()? {
            samples.extend(frame.samples);
        }
    }
    decoder.flush()?;
    while let Some(frame) = decoder.receive_frame()? {
        samples.extend(frame.samples);
    }
    pcm_frame(worker, samples).map_err(Error::InvalidData)
}

#[cfg(not(target_arch = "wasm32"))]
fn decode_external(worker: &AacWorker) -> Result<AudioFrame, String> {
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
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| "cannot open AAC decoder input".to_owned())?;
    // Drain stdout/stderr while feeding compressed input: sequential write_all followed by
    // wait_with_output deadlocks once both input and PCM exceed their OS pipe capacities.
    let feeder = std::thread::Builder::new()
        .name("aac-input".into())
        .spawn(move || input.write_all(&adts));
    let feeder = match feeder {
        Ok(feeder) => feeder,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("cannot start AAC input feeder: {error}"));
        }
    };
    let output = child.wait_with_output();
    let fed = feeder
        .join()
        .map_err(|_| "AAC input feeder panicked".to_owned())?;
    let output = output.map_err(|error| format!("AAC decoder failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "AAC decoder failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    fed.map_err(|error| format!("cannot feed AAC decoder: {error}"))?;
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
    pcm_frame(worker, samples)
}

fn pcm_frame(worker: &AacWorker, samples: Vec<i16>) -> Result<AudioFrame, String> {
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
fn decode_external(_worker: &AacWorker) -> Result<AudioFrame, String> {
    Err("external processes are unavailable in WebAssembly; only the native AAC-LC subset is available".into())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{process::Command, thread, time::Instant};

    use super::*;

    #[test]
    #[ignore = "requires the local projects/ipad2.MP4 acceptance file"]
    fn decodes_local_ipad_audio_entirely_in_rust() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../projects/ipad2.MP4");
        let mut source = AacPlaybackSource::with_executor_and_policy(
            std::fs::read(path).unwrap(),
            Arc::new(crate::InlineDecodeExecutor::new(8).unwrap()),
            AacDecodePolicy::NativeOnly,
        )
        .unwrap();
        let expected = source.index().decoded_samples_per_channel();
        source.request_decode().unwrap();
        match source.try_event().unwrap().unwrap() {
            AacPlaybackEvent::Decoded { audio, backend, .. } => {
                assert_eq!(backend, AacDecodeBackend::Native);
                assert_eq!(audio.sample_rate, 44_100);
                assert_eq!(audio.channels, 2);
                assert_eq!(audio.samples_per_channel, expected);
                assert!(audio.samples.iter().all(|sample| *sample == 0));
            }
            AacPlaybackEvent::Error { message, .. } => panic!("{message}"),
        }
    }

    #[test]
    fn generated_silence_uses_native_decoder_with_cooperative_scheduling() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping AAC interoperability test: ffmpeg is unavailable");
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("mmrecode-aac-silence-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for rate in [44_100, 48_000] {
            for layout in ["mono", "stereo"] {
                let path = directory.join(format!("{rate}-{layout}.m4a"));
                let status = Command::new("ffmpeg")
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        &format!("anullsrc=r={rate}:cl={layout}"),
                        "-t",
                        "0.15",
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
                let mut source = AacPlaybackSource::with_executor_and_policy(
                    std::fs::read(&path).unwrap(),
                    Arc::new(crate::InlineDecodeExecutor::new(8).unwrap()),
                    AacDecodePolicy::NativeOnly,
                )
                .unwrap();
                let expected = source.index().decoded_samples_per_channel();
                let generation = source.request_decode().unwrap();
                let AacPlaybackEvent::Decoded {
                    audio,
                    backend,
                    generation: actual,
                } = source.try_event().unwrap().unwrap()
                else {
                    panic!("native silence decoding failed");
                };
                assert_eq!(actual, generation);
                assert_eq!(backend, AacDecodeBackend::Native);
                assert_eq!(audio.sample_rate, rate);
                assert_eq!(audio.channels, if layout == "mono" { 1 } else { 2 });
                assert_eq!(audio.samples_per_channel, expected);
                // Independent reference PCM. FFmpeg leaves terminal codec padding in its raw
                // output; compare the edited presentation interval, not that extra tail.
                let reference = Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&path)
                    .args(["-f", "s16le", "pipe:1"])
                    .output()
                    .unwrap();
                assert!(reference.status.success());
                let reference: Vec<i16> = reference
                    .stdout
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
                    .collect();
                assert!(reference.len() >= audio.samples.len());
                assert_eq!(audio.samples, reference[..audio.samples.len()]);
                std::fs::remove_file(path).unwrap();
            }
        }
        std::fs::remove_dir(directory).unwrap();
    }

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
                // Large enough that a sequential pipe feeder/drainer deadlocks on macOS.
                "sine=frequency=440:sample_rate=48000:duration=10",
                "-c:a",
                "aac",
                "-profile:a",
                "aac_low",
                "-ac",
                "6",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut source = AacPlaybackSource::new(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(source.index().configuration().sample_rate, 48_000);
        assert_eq!(source.index().configuration().channels, 6);
        assert!(!source.index().samples().is_empty());
        assert!(
            source
                .index()
                .samples()
                .iter()
                .map(|sample| sample.byte_length)
                .sum::<usize>()
                > 65_536
        );
        assert!(matches!(
            decode_native(&source.worker),
            Err(Error::Unsupported(_))
        ));
        // The very same unsupported multichannel stream must not invoke FFmpeg under
        // native-only policy.
        let native_source = AacPlaybackSource::with_executor_and_policy(
            std::fs::read(&path).unwrap(),
            Arc::new(crate::InlineDecodeExecutor::new(8).unwrap()),
            AacDecodePolicy::NativeOnly,
        )
        .unwrap();
        assert!(
            decode_worker(&native_source.worker)
                .unwrap_err()
                .starts_with("unsupported:")
        );
        let expected_samples = source.index().decoded_samples_per_channel();
        let generation = source.request_decode().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = source.try_event().unwrap() {
                match event {
                    AacPlaybackEvent::Decoded {
                        generation: event_generation,
                        audio,
                        backend,
                    } => {
                        assert_eq!(backend, AacDecodeBackend::External);
                        assert_eq!(event_generation, generation);
                        assert_eq!(audio.sample_rate, 48_000);
                        assert_eq!(audio.channels, 6);
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

    #[test]
    fn nonzero_mp4_uses_native_pcm_and_presentation_trimming() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "mmrecode-aac-native-playback-{}.m4a",
            std::process::id()
        ));
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.15",
                "-ac",
                "2",
                "-c:a",
                "aac",
                "-aac_pns",
                "0",
                "-aac_tns",
                "0",
                "-aac_is",
                "0",
                "-y",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let mut source = AacPlaybackSource::with_executor_and_policy(
            std::fs::read(&path).unwrap(),
            Arc::new(crate::InlineDecodeExecutor::new(8).unwrap()),
            AacDecodePolicy::NativeOnly,
        )
        .unwrap();
        source.request_decode().unwrap();
        let AacPlaybackEvent::Decoded { audio, backend, .. } = source.try_event().unwrap().unwrap()
        else {
            panic!("native nonzero playback failed");
        };
        assert_eq!(backend, AacDecodeBackend::Native);
        assert_eq!(
            audio.samples_per_channel,
            source.index().decoded_samples_per_channel()
        );
        assert!(audio.samples.iter().any(|v| v.unsigned_abs() > 100));
        let reference = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "s16le", "pipe:1"])
            .output()
            .unwrap();
        assert!(reference.status.success());
        let reference: Vec<i16> = reference
            .stdout
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| i16::from_le_bytes(*v))
            .collect();
        assert!(reference.len() >= audio.samples.len());
        assert!(
            audio
                .samples
                .iter()
                .zip(&reference)
                .all(|(a, b)| (i32::from(*a) - i32::from(*b)).abs() <= 2)
        );
        std::fs::remove_file(path).unwrap();
    }
}
