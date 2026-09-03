//! Indexed H.264 MP4/MOV playback through native reconstruction with an optional fallback.

use std::{
    collections::HashMap,
    io::Write as _,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::Duration,
};

use mmrecode_core::{
    ColorDescription, ColorRange, Decoder, Error, FieldOrder, FrameTiming, Packet, PacketFlags,
    PixelFormat, Plane, Rational, Timestamp, VideoFrame,
};
use mmrecode_h264::{
    AvcDecoderConfigurationRecord, H264Decoder, H264StreamIndexer, PictureTiming, PictureType,
    length_prefixed_nal_units, nal_units_to_annex_b,
};
use mmrecode_isobmff::{IsoBmffFile, Track};

use crate::PlaybackTimeline;

/// Lightweight metadata for one H.264 picture in presentation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedH264Frame {
    /// ISO-BMFF sample number in decode order.
    pub sample_index: usize,
    /// Decode timestamp in track timescale units.
    pub dts: i64,
    /// Presentation timestamp in track timescale units.
    pub pts: i64,
    /// Duration in track timescale units.
    pub duration: u32,
    /// Broad slice type.
    pub picture_type: PictureType,
    /// Whether this is an IDR picture.
    pub is_idr: bool,
    /// Whether this picture is retained as a reference.
    pub is_reference: bool,
    /// Conservative dependencies expressed as decode-order sample indexes.
    pub dependencies: Vec<usize>,
}

/// Presentation-ordered H.264 MP4/MOV index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H264VideoIndex {
    frame_rate: Rational,
    time_base: Rational,
    width: usize,
    height: usize,
    rotation_degrees: i16,
    progressive: bool,
    frames: Vec<IndexedH264Frame>,
}

impl H264VideoIndex {
    /// Returns the exact average frame rate derived from sample durations.
    #[must_use]
    pub const fn frame_rate(&self) -> Rational {
        self.frame_rate
    }

    /// Returns the track timestamp unit.
    #[must_use]
    pub const fn time_base(&self) -> Rational {
        self.time_base
    }

    /// Returns presentation-ordered picture metadata.
    #[must_use]
    pub fn frames(&self) -> &[IndexedH264Frame] {
        &self.frames
    }

    /// Returns the number of displayable pictures.
    #[must_use]
    pub const fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Returns the display width after container rotation.
    #[must_use]
    pub const fn display_width(&self) -> usize {
        if matches!(self.rotation_degrees, 90 | 270) {
            self.height
        } else {
            self.width
        }
    }

    /// Returns the display height after container rotation.
    #[must_use]
    pub const fn display_height(&self) -> usize {
        if matches!(self.rotation_degrees, 90 | 270) {
            self.width
        } else {
            self.height
        }
    }

    /// Returns whether the active SPS restricts pictures to complete frames.
    #[must_use]
    pub const fn is_progressive(&self) -> bool {
        self.progressive
    }

    /// Builds a playback timeline from exact presentation timestamps and sample durations.
    ///
    /// # Errors
    ///
    /// Returns an error when timestamps cannot be normalized or represented as durations.
    pub fn playback_timeline(&self) -> Result<PlaybackTimeline, String> {
        let origin = self.frames[0].pts;
        let mut positions = Vec::with_capacity(self.frames.len());
        let mut duration = Duration::ZERO;
        for frame in &self.frames {
            let delta = frame
                .pts
                .checked_sub(origin)
                .ok_or_else(|| "H.264 presentation timestamp underflows".to_owned())?;
            let position = timestamp_duration(delta, self.time_base)?;
            let end = position.saturating_add(timestamp_duration(
                i64::from(frame.duration),
                self.time_base,
            )?);
            positions.push(position);
            duration = duration.max(end);
        }
        PlaybackTimeline::variable(self.frame_rate, positions, duration)
            .map_err(|error| error.to_string())
    }
}

fn timestamp_duration(value: i64, time_base: Rational) -> Result<Duration, String> {
    let value = u128::try_from(value)
        .map_err(|_| "negative H.264 presentation time is not normalized".to_owned())?;
    let numerator = value
        .checked_mul(
            u128::try_from(time_base.numerator())
                .map_err(|_| "invalid H.264 time-base numerator".to_owned())?,
        )
        .and_then(|value| value.checked_mul(1_000_000_000))
        .ok_or_else(|| "H.264 presentation duration overflows".to_owned())?;
    let denominator = u128::try_from(time_base.denominator())
        .map_err(|_| "invalid H.264 time-base denominator".to_owned())?;
    let nanos = u64::try_from(numerator / denominator)
        .map_err(|_| "H.264 presentation duration exceeds platform".to_owned())?;
    Ok(Duration::from_nanos(nanos))
}

/// One asynchronous result from an [`H264PlaybackSource`].
#[derive(Debug)]
pub enum H264PlaybackEvent {
    /// One requested picture was reconstructed.
    Frame {
        /// Request generation used to discard obsolete seek results.
        generation: u64,
        /// Presentation-order frame index.
        frame_index: usize,
        /// Reconstructed RGB frame.
        frame: Box<VideoFrame>,
    },
    /// Reconstruction failed for the active request.
    Error {
        /// Request generation that failed.
        generation: u64,
        /// Human-readable decoder error.
        message: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct DecodeRequest {
    generation: u64,
    frame_index: usize,
    look_ahead: usize,
}

/// Indexed H.264 MP4/MOV source decoded natively in bounded GOP windows when supported.
///
/// `MMRecode` performs container parsing, timing, NAL conversion, parameter parsing, indexing, and
/// seek-window selection itself. Its native decoder is attempted first. An optional external
/// process currently fills unsupported reconstruction-tool gaps and is never used for demuxing.
#[derive(Debug)]
pub struct H264PlaybackSource {
    index: H264VideoIndex,
    requests: Sender<DecodeRequest>,
    events: Receiver<H264PlaybackEvent>,
    generation: u64,
}

impl H264PlaybackSource {
    /// Parses and indexes an owned MP4/MOV file and starts its decode worker.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid container/AVC syntax or when the worker cannot be started.
    pub fn new(file_data: Vec<u8>) -> Result<Self, String> {
        let movie = IsoBmffFile::parse(file_data).map_err(|error| error.to_string())?;
        let track = movie
            .h264_track()
            .cloned()
            .ok_or_else(|| "ISO-BMFF file has no H.264 video track".to_owned())?;
        let configuration =
            AvcDecoderConfigurationRecord::parse(&track.descriptor.codec.configuration)
                .map_err(|error| error.to_string())?;
        let index = build_index(&movie, &track, &configuration)?;
        let worker_index = index.clone();
        let (request_tx, request_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("mmrecode-h264-playback".into())
            .spawn(move || {
                decode_worker(
                    movie,
                    track,
                    configuration,
                    worker_index,
                    request_rx,
                    event_tx,
                );
            })
            .map_err(|error| format!("cannot start H.264 decode worker: {error}"))?;
        Ok(Self {
            index,
            requests: request_tx,
            events: event_rx,
            generation: 0,
        })
    }

    /// Returns the lightweight stream index.
    #[must_use]
    pub const fn index(&self) -> &H264VideoIndex {
        &self.index
    }

    /// Requests one presentation frame and a bounded number of following frames.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range index or if the decode worker stopped.
    pub fn request(&mut self, frame_index: usize, look_ahead: usize) -> Result<u64, String> {
        if frame_index >= self.index.frame_count() {
            return Err(format!(
                "H.264 frame {frame_index} is outside 0..{}",
                self.index.frame_count()
            ));
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.generation = 1;
        }
        self.requests
            .send(DecodeRequest {
                generation: self.generation,
                frame_index,
                look_ahead,
            })
            .map_err(|_| "H.264 decode worker stopped".to_owned())?;
        Ok(self.generation)
    }

    /// Returns the next available worker result without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when the decode worker disconnected.
    pub fn try_event(&self) -> Result<Option<H264PlaybackEvent>, String> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("H.264 decode worker stopped".into()),
        }
    }
}

fn build_index(
    movie: &IsoBmffFile,
    track: &Track,
    configuration: &AvcDecoderConfigurationRecord,
) -> Result<H264VideoIndex, String> {
    let mut syntax = H264StreamIndexer::default();
    syntax
        .configure_avcc(configuration)
        .map_err(|error| error.to_string())?;
    for (sample_index, sample) in track.samples.iter().enumerate() {
        let nals = length_prefixed_nal_units(
            movie
                .sample_data(sample)
                .map_err(|error| error.to_string())?,
            configuration.length_size,
        )
        .map_err(|error| format!("H.264 sample {sample_index}: {error}"))?;
        syntax
            .push_access_unit(
                sample_index,
                PictureTiming {
                    dts: sample.dts,
                    pts: sample.pts,
                    duration: sample.duration,
                },
                &nals,
            )
            .map_err(|error| format!("H.264 sample {sample_index}: {error}"))?;
    }
    let syntax = syntax.finish();
    let first_sps = syntax
        .sequence_parameter_sets
        .values()
        .next()
        .ok_or_else(|| "H.264 stream has no sequence parameter set".to_owned())?;
    let mut frames = syntax
        .access_units
        .into_iter()
        .filter_map(|unit| unit.picture)
        .map(|picture| IndexedH264Frame {
            sample_index: picture.sample_index,
            dts: picture.timing.dts,
            pts: picture.timing.pts,
            duration: picture.timing.duration,
            picture_type: picture.slice.picture_type,
            is_idr: picture.is_idr,
            is_reference: picture.is_reference,
            dependencies: picture.dependencies,
        })
        .collect::<Vec<_>>();
    frames.sort_by_key(|frame| (frame.pts, frame.dts, frame.sample_index));
    if frames.is_empty() {
        return Err("H.264 track has no coded pictures".into());
    }
    let total_duration = frames.iter().try_fold(0_u64, |total, frame| {
        total
            .checked_add(u64::from(frame.duration))
            .ok_or_else(|| "H.264 duration overflows".to_owned())
    })?;
    let timescale = u64::try_from(track.descriptor.time_base.denominator())
        .map_err(|_| "invalid H.264 track time base".to_owned())?;
    let numerator = timescale
        .checked_mul(u64::try_from(frames.len()).map_err(|_| "frame count overflows".to_owned())?)
        .ok_or_else(|| "H.264 frame rate overflows".to_owned())?;
    let divisor = gcd(numerator, total_duration);
    let frame_rate = Rational::new(
        i64::try_from(numerator / divisor).map_err(|_| "frame rate overflows".to_owned())?,
        i64::try_from(total_duration / divisor).map_err(|_| "frame rate overflows".to_owned())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(H264VideoIndex {
        frame_rate,
        time_base: track.descriptor.time_base,
        width: usize::try_from(first_sps.width).map_err(|_| "H.264 width overflows".to_owned())?,
        height: usize::try_from(first_sps.height)
            .map_err(|_| "H.264 height overflows".to_owned())?,
        rotation_degrees: track.rotation_degrees,
        progressive: first_sps.frame_mbs_only,
        frames,
    })
}

#[allow(clippy::needless_pass_by_value)]
fn decode_worker(
    movie: IsoBmffFile,
    track: Track,
    configuration: AvcDecoderConfigurationRecord,
    index: H264VideoIndex,
    requests: Receiver<DecodeRequest>,
    events: Sender<H264PlaybackEvent>,
) {
    while let Ok(mut request) = requests.recv() {
        while let Ok(newer) = requests.try_recv() {
            request = newer;
        }
        let result = decode_request(&movie, &track, &configuration, &index, request);
        match result {
            Ok(frames) => {
                for (frame_index, frame) in frames {
                    if events
                        .send(H264PlaybackEvent::Frame {
                            generation: request.generation,
                            frame_index,
                            frame: Box::new(frame),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(message) => {
                if events
                    .send(H264PlaybackEvent::Error {
                        generation: request.generation,
                        message,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn decode_request(
    movie: &IsoBmffFile,
    track: &Track,
    configuration: &AvcDecoderConfigurationRecord,
    index: &H264VideoIndex,
    request: DecodeRequest,
) -> Result<Vec<(usize, VideoFrame)>, String> {
    match decode_request_native(movie, track, index, request) {
        Ok(frames) => Ok(frames),
        Err(Error::Unsupported(native_error)) => {
            decode_request_external(movie, track, configuration, index, request).map_err(
                |external_error| {
                    format!(
                        "native H.264 decoder unsupported: {native_error}; optional external fallback: {external_error}"
                    )
                },
            )
        }
        Err(native_error) => Err(format!("native H.264 decoder failed: {native_error}")),
    }
}

fn decode_request_native(
    movie: &IsoBmffFile,
    track: &Track,
    index: &H264VideoIndex,
    request: DecodeRequest,
) -> mmrecode_core::Result<Vec<(usize, VideoFrame)>> {
    let requested_end = request
        .frame_index
        .saturating_add(request.look_ahead + 1)
        .min(index.frames.len());
    let target_sample = index.frames[request.frame_index].sample_index;
    let end_sample = index.frames[request.frame_index..requested_end]
        .iter()
        .map(|frame| frame.sample_index)
        .max()
        .unwrap_or(target_sample);
    let start_sample = track.samples[..=target_sample]
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample.is_sync)
        .map(|(sample_index, _)| sample_index)
        .next_back()
        .unwrap_or(0);
    let wanted = (request.frame_index..requested_end)
        .map(|frame_index| (index.frames[frame_index].sample_index, frame_index))
        .collect::<HashMap<_, _>>();
    let mut decoder = H264Decoder::default();
    decoder.configure(&track.descriptor.codec)?;
    let mut frames = Vec::new();
    for (sample_index, sample) in track.samples[start_sample..=end_sample]
        .iter()
        .enumerate()
        .map(|(offset, sample)| (start_sample + offset, sample))
    {
        let mut flags = PacketFlags::empty();
        if sample.is_sync {
            flags.insert(PacketFlags::KEY);
        }
        decoder.send_packet(Packet {
            stream_id: track.descriptor.id,
            data: movie.sample_data(sample)?.to_vec(),
            pts: Some(Timestamp {
                value: sample.pts,
                time_base: track.descriptor.time_base,
            }),
            dts: Some(Timestamp {
                value: sample.dts,
                time_base: track.descriptor.time_base,
            }),
            duration: Some(Timestamp {
                value: i64::from(sample.duration),
                time_base: track.descriptor.time_base,
            }),
            flags,
            side_data: Vec::new(),
        })?;
        while let Some(frame) = decoder.receive_frame()? {
            if let Some(&frame_index) = wanted.get(&sample_index) {
                frames.push((
                    frame_index,
                    rotate_native_frame(frame, track.rotation_degrees)?,
                ));
            }
        }
    }
    if frames.len() != wanted.len() {
        return Err(Error::InvalidData(format!(
            "native H.264 decoder returned {} of {} requested frames",
            frames.len(),
            wanted.len()
        )));
    }
    frames.sort_by_key(|(frame_index, _)| *frame_index);
    Ok(frames)
}

#[allow(clippy::too_many_lines)]
fn decode_request_external(
    movie: &IsoBmffFile,
    track: &Track,
    configuration: &AvcDecoderConfigurationRecord,
    index: &H264VideoIndex,
    request: DecodeRequest,
) -> Result<Vec<(usize, VideoFrame)>, String> {
    let requested_end = request
        .frame_index
        .saturating_add(request.look_ahead + 1)
        .min(index.frames.len());
    let target_sample = index.frames[request.frame_index].sample_index;
    let end_sample = index.frames[request.frame_index..requested_end]
        .iter()
        .map(|frame| frame.sample_index)
        .max()
        .unwrap_or(target_sample);
    let start_sample = track.samples[..=target_sample]
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample.is_sync)
        .map(|(sample_index, _)| sample_index)
        .next_back()
        .unwrap_or(0);
    let mut annex_b = configuration.parameter_sets_annex_b();
    for sample in &track.samples[start_sample..=end_sample] {
        annex_b.extend_from_slice(
            &nal_units_to_annex_b(
                movie
                    .sample_data(sample)
                    .map_err(|error| error.to_string())?,
                configuration.length_size,
            )
            .map_err(|error| error.to_string())?,
        );
    }
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-an",
            "-sn",
            "-dn",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!("optional H.264 pixel-decoder fallback is unavailable ({error})")
        })?;
    child
        .stdin
        .take()
        .ok_or_else(|| "cannot open H.264 decoder input".to_owned())?
        .write_all(&annex_b)
        .map_err(|error| format!("cannot feed H.264 decoder: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("H.264 decoder failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("H.264 decoder failed: {}", stderr.trim()));
    }
    let frame_size = index
        .width
        .checked_mul(index.height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "decoded H.264 frame size overflows".to_owned())?;
    if frame_size == 0 || output.stdout.len() % frame_size != 0 {
        return Err(format!(
            "H.264 decoder returned {} bytes, not complete {}-byte RGB frames",
            output.stdout.len(),
            frame_size
        ));
    }
    let mut decoded_order = index
        .frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| (start_sample..=end_sample).contains(&frame.sample_index))
        .collect::<Vec<_>>();
    decoded_order.sort_by_key(|(_, frame)| (frame.pts, frame.dts, frame.sample_index));
    let decoded_count = output.stdout.len() / frame_size;
    if decoded_count != decoded_order.len() {
        return Err(format!(
            "H.264 decoder returned {decoded_count} frames for {} indexed pictures",
            decoded_order.len()
        ));
    }
    let wanted = (request.frame_index..requested_end)
        .map(|frame_index| (index.frames[frame_index].sample_index, frame_index))
        .collect::<HashMap<_, _>>();
    let mut frames = Vec::new();
    for (decoded_index, (_, metadata)) in decoded_order.into_iter().enumerate() {
        let Some(&frame_index) = wanted.get(&metadata.sample_index) else {
            continue;
        };
        let rgb =
            output.stdout[decoded_index * frame_size..(decoded_index + 1) * frame_size].to_vec();
        frames.push((frame_index, rgb_frame(rgb, metadata, index)?));
    }
    Ok(frames)
}

fn rotate_native_frame(mut frame: VideoFrame, degrees: i16) -> mmrecode_core::Result<VideoFrame> {
    match degrees.rem_euclid(360) {
        0 => Ok(frame),
        90 | 180 | 270 => {
            for plane in &mut frame.planes {
                let (data, width, height) = rotate_plane(plane, degrees)?;
                plane.data = data;
                plane.stride = width;
                plane.width = width;
                plane.height = height;
            }
            if matches!(degrees.rem_euclid(360), 90 | 270) {
                (frame.width, frame.height) = (frame.height, frame.width);
            }
            Ok(frame)
        }
        _ => Err(Error::Unsupported(format!(
            "H.264 display rotation {degrees}"
        ))),
    }
}

fn rotate_plane(plane: &Plane, degrees: i16) -> mmrecode_core::Result<(Vec<u8>, usize, usize)> {
    let (width, height) = if matches!(degrees.rem_euclid(360), 90 | 270) {
        (plane.height, plane.width)
    } else {
        (plane.width, plane.height)
    };
    let mut data = vec![0; width.saturating_mul(height)];
    for source_y in 0..plane.height {
        for source_x in 0..plane.width {
            let source = source_y
                .checked_mul(plane.stride)
                .and_then(|offset| offset.checked_add(source_x))
                .filter(|offset| *offset < plane.data.len())
                .ok_or_else(|| Error::InvalidData("native H.264 plane layout is invalid".into()))?;
            let (target_x, target_y) = match degrees.rem_euclid(360) {
                90 => (plane.height - 1 - source_y, source_x),
                180 => (plane.width - 1 - source_x, plane.height - 1 - source_y),
                270 => (source_y, plane.width - 1 - source_x),
                _ => (source_x, source_y),
            };
            data[target_y * width + target_x] = plane.data[source];
        }
    }
    Ok((data, width, height))
}

fn rgb_frame(
    mut data: Vec<u8>,
    metadata: &IndexedH264Frame,
    index: &H264VideoIndex,
) -> Result<VideoFrame, String> {
    let (width, height) = match index.rotation_degrees {
        90 => {
            data = rotate_rgb(&data, index.width, index.height, true);
            (index.height, index.width)
        }
        180 => {
            data = rotate_rgb_180(&data, index.width, index.height);
            (index.width, index.height)
        }
        270 => {
            data = rotate_rgb(&data, index.width, index.height, false);
            (index.height, index.width)
        }
        _ => (index.width, index.height),
    };
    Ok(VideoFrame {
        format: PixelFormat::Rgb24,
        width,
        height,
        planes: vec![Plane {
            data,
            stride: width
                .checked_mul(3)
                .ok_or_else(|| "RGB stride overflows".to_owned())?,
            width,
            height,
        }],
        timing: FrameTiming {
            pts: Some(Timestamp {
                value: metadata.pts,
                time_base: index.time_base,
            }),
            duration: Some(Timestamp {
                value: i64::from(metadata.duration),
                time_base: index.time_base,
            }),
        },
        color: ColorDescription {
            range: ColorRange::Full,
            primaries: Some("sRGB".into()),
            transfer: Some("sRGB".into()),
            matrix: Some("RGB".into()),
        },
        field_order: if index.progressive {
            FieldOrder::Progressive
        } else {
            FieldOrder::Unspecified
        },
    })
}

fn rotate_rgb(input: &[u8], width: usize, height: usize, clockwise: bool) -> Vec<u8> {
    let mut output = vec![0; input.len()];
    for y in 0..height {
        for x in 0..width {
            let source = (y * width + x) * 3;
            let (target_x, target_y) = if clockwise {
                (height - 1 - y, x)
            } else {
                (y, width - 1 - x)
            };
            let target = (target_y * height + target_x) * 3;
            output[target..target + 3].copy_from_slice(&input[source..source + 3]);
        }
    }
    output
}

fn rotate_rgb_180(input: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut output = vec![0; input.len()];
    for pixel in 0..width * height {
        let target = (width * height - 1 - pixel) * 3;
        output[target..target + 3].copy_from_slice(&input[pixel * 3..pixel * 3 + 3]);
    }
    output
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    use super::{H264PlaybackEvent, H264PlaybackSource, rotate_rgb, rotate_rgb_180};

    #[test]
    fn rotates_rgb_pixels_without_changing_channels() {
        let input = [1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0];
        assert_eq!(
            rotate_rgb(&input, 2, 2, true),
            [3, 0, 0, 1, 0, 0, 4, 0, 0, 2, 0, 0]
        );
        assert_eq!(
            rotate_rgb_180(&input, 2, 2),
            [4, 0, 0, 3, 0, 0, 2, 0, 0, 1, 0, 0]
        );
    }

    #[test]
    fn indexes_seeks_and_decodes_a_real_mp4_when_ffmpeg_is_available() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping H.264 playback bridge test: ffmpeg is unavailable");
            return;
        }
        let path =
            std::env::temp_dir().join(format!("mmrecode-h264-playback-{}.mp4", std::process::id()));
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10",
                "-frames:v",
                "12",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&path)
            .status()
            .expect("run ffmpeg test encoder");
        if !status.success() {
            eprintln!("skipping H.264 playback bridge test: libx264 is unavailable");
            return;
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut source = H264PlaybackSource::new(bytes).unwrap();
        assert_eq!(source.index().frame_count(), 12);
        assert_eq!(
            (
                source.index().display_width(),
                source.index().display_height()
            ),
            (64, 48)
        );
        let generation = source.request(5, 2).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut decoded = Vec::new();
        while Instant::now() < deadline && decoded.len() < 3 {
            match source.try_event().unwrap() {
                Some(H264PlaybackEvent::Frame {
                    generation: found,
                    frame_index,
                    frame,
                }) => {
                    assert_eq!(found, generation);
                    assert_eq!((frame.width, frame.height), (64, 48));
                    decoded.push(frame_index);
                }
                Some(H264PlaybackEvent::Error { message, .. }) => panic!("{message}"),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        decoded.sort_unstable();
        assert_eq!(decoded, [5, 6, 7]);
    }
}
