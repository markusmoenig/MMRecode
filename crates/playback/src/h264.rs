//! Indexed H.264 MP4/MOV playback through native reconstruction with an optional fallback.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
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

#[cfg(not(target_arch = "wasm32"))]
use mmrecode_core::{ColorDescription, ColorRange, FieldOrder, FrameTiming, PixelFormat};
use mmrecode_core::{Decoder, Error, Packet, PacketFlags, Plane, Rational, Timestamp, VideoFrame};
#[cfg(not(target_arch = "wasm32"))]
use mmrecode_h264::nal_units_to_annex_b;
use mmrecode_h264::{
    AvcDecoderConfigurationRecord, H264Decoder, H264StreamIndexer, NalUnitType, PictureTiming,
    PictureType, RecoveryPoint, length_prefixed_nal_units,
};
use mmrecode_isobmff::{IsoBmffFile, Track};

use crate::{DecodeExecutor, PlaybackTimeline, default_decode_executor};

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
    /// Wrapping AVC frame number from the primary slice header.
    pub frame_num: u32,
    /// Modulus of `frame_num` from the active SPS.
    pub max_frame_num: u32,
    /// Whether this is an IDR picture.
    pub is_idr: bool,
    /// Whether this picture is retained as a reference.
    pub is_reference: bool,
    /// Recovery-point SEI carried by this access unit, if present.
    pub recovery_point: Option<RecoveryPoint>,
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

/// Indexed H.264 MP4/MOV source decoded through a persistent, seekable scheduled session.
///
/// `MMRecode` performs container parsing, timing, NAL conversion, parameter parsing, indexing, and
/// seek-window selection itself. Its native decoder is attempted first. An optional external
/// process currently fills unsupported reconstruction-tool gaps and is never used for demuxing.
#[derive(Debug)]
pub struct H264PlaybackSource {
    index: H264VideoIndex,
    executor: Arc<dyn DecodeExecutor>,
    worker: Arc<Mutex<H264WorkerState>>,
    event_sender: Sender<H264PlaybackEvent>,
    events: Receiver<H264PlaybackEvent>,
    active_generation: Arc<AtomicU64>,
    generation: u64,
}

impl H264PlaybackSource {
    /// Parses and indexes an owned MP4/MOV file and prepares scheduled decoding.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid container/AVC syntax or when the default executor cannot start.
    pub fn new(file_data: Vec<u8>) -> Result<Self, String> {
        Self::with_executor(file_data, default_decode_executor()?)
    }

    /// Parses and indexes an owned MP4/MOV file using a caller-supplied decode executor.
    ///
    /// This is useful for hosts that provide their own native pool and for browser WebAssembly,
    /// where [`InlineDecodeExecutor`](crate::InlineDecodeExecutor) advances bounded decoder work
    /// whenever [`Self::try_event`] is polled.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid container/AVC syntax.
    pub fn with_executor(
        file_data: Vec<u8>,
        executor: Arc<dyn DecodeExecutor>,
    ) -> Result<Self, String> {
        let movie = IsoBmffFile::parse(file_data).map_err(|error| error.to_string())?;
        let track = movie
            .h264_track()
            .cloned()
            .ok_or_else(|| "ISO-BMFF file has no H.264 video track".to_owned())?;
        let configuration =
            AvcDecoderConfigurationRecord::parse(&track.descriptor.codec.configuration)
                .map_err(|error| error.to_string())?;
        let index = build_index(&movie, &track, &configuration)?;
        let (event_tx, event_rx) = mpsc::channel();
        let worker = Arc::new(Mutex::new(H264WorkerState {
            movie,
            track,
            configuration,
            index: index.clone(),
            native_session: None,
            active_request: None,
        }));
        Ok(Self {
            index,
            executor,
            worker,
            event_sender: event_tx,
            events: event_rx,
            active_generation: Arc::new(AtomicU64::new(0)),
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
    /// Returns an error for an out-of-range index or if work cannot be scheduled.
    pub fn request(&mut self, frame_index: usize, look_ahead: usize) -> Result<u64, String> {
        if frame_index >= self.index.frame_count() {
            return Err(format!(
                "H.264 frame {frame_index} is outside 0..{}",
                self.index.frame_count()
            ));
        }
        let mut generation = self.generation.wrapping_add(1);
        if generation == 0 {
            generation = 1;
        }
        let previous = self.active_generation.swap(generation, Ordering::AcqRel);
        let request = DecodeRequest {
            generation,
            frame_index,
            look_ahead,
        };
        if let Err(error) = schedule_decode_step(
            &self.executor,
            &self.worker,
            &self.event_sender,
            &self.active_generation,
            request,
        ) {
            self.active_generation.store(previous, Ordering::Release);
            return Err(error);
        }
        self.generation = generation;
        Ok(generation)
    }

    /// Returns the next available worker result without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when the event channel disconnected.
    pub fn try_event(&self) -> Result<Option<H264PlaybackEvent>, String> {
        self.executor.poll(1);
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("H.264 decode event channel stopped".into()),
        }
    }
}

#[derive(Debug)]
struct H264WorkerState {
    movie: IsoBmffFile,
    track: Track,
    configuration: AvcDecoderConfigurationRecord,
    index: H264VideoIndex,
    native_session: Option<NativeDecodeSession>,
    active_request: Option<NativeRequestState>,
}

#[derive(Debug)]
struct NativeRequestState {
    request: DecodeRequest,
    requested_end: usize,
    end_sample: usize,
    remaining: HashMap<usize, usize>,
    frame_index_by_sample: HashMap<usize, usize>,
}

#[derive(Debug)]
enum DecodeStepOutcome {
    Continue,
    Complete,
    Spawn {
        jobs: Vec<ParallelDecodeJob>,
        continue_after: bool,
    },
}

#[derive(Debug)]
struct ParallelDecodeJob {
    decoder: H264Decoder,
    packet: Packet,
    frame_index: usize,
    rotation_degrees: i16,
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
            frame_num: picture.slice.frame_num,
            max_frame_num: picture.max_frame_num,
            is_idr: picture.is_idr,
            is_reference: picture.is_reference,
            recovery_point: picture.recovery_point,
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

fn schedule_decode_step(
    executor: &Arc<dyn DecodeExecutor>,
    worker: &Arc<Mutex<H264WorkerState>>,
    events: &Sender<H264PlaybackEvent>,
    active_generation: &Arc<AtomicU64>,
    request: DecodeRequest,
) -> Result<(), String> {
    let next_executor = Arc::clone(executor);
    let next_worker = Arc::clone(worker);
    let next_events = events.clone();
    let next_generation = Arc::clone(active_generation);
    executor
        .submit(Box::new(move || {
            run_decode_step(
                &next_executor,
                &next_worker,
                &next_events,
                &next_generation,
                request,
            );
        }))
        .map_err(|error| format!("cannot schedule H.264 decode work: {error}"))
}

fn run_decode_step(
    executor: &Arc<dyn DecodeExecutor>,
    worker: &Arc<Mutex<H264WorkerState>>,
    events: &Sender<H264PlaybackEvent>,
    active_generation: &Arc<AtomicU64>,
    request: DecodeRequest,
) {
    if active_generation.load(Ordering::Acquire) != request.generation {
        return;
    }

    let result = if let Ok(mut state) = worker.lock() {
        if active_generation.load(Ordering::Acquire) != request.generation {
            return;
        }
        decode_request_native_step(&mut state, request, events)
    } else {
        let _ = send_error(
            events,
            request.generation,
            "H.264 decode state is poisoned".into(),
        );
        return;
    };

    match result {
        Ok(DecodeStepOutcome::Continue) => {
            schedule_continuation(executor, worker, events, active_generation, request);
        }
        Ok(DecodeStepOutcome::Complete) => {}
        Ok(DecodeStepOutcome::Spawn {
            jobs,
            continue_after,
        }) => {
            for job in jobs {
                let job_events = events.clone();
                let job_generation = Arc::clone(active_generation);
                if let Err(error) = executor.submit(Box::new(move || {
                    run_parallel_decode_job(job, &job_events, &job_generation, request.generation);
                })) {
                    cancel_generation(active_generation, request.generation);
                    let _ = send_error(
                        events,
                        request.generation,
                        format!("cannot schedule parallel H.264 picture: {error}"),
                    );
                    return;
                }
            }
            if continue_after {
                schedule_continuation(executor, worker, events, active_generation, request);
            }
        }
        Err(Error::Unsupported(native_error)) => {
            run_external_fallback(worker, events, active_generation, request, &native_error);
        }
        Err(error) => {
            if let Ok(mut state) = worker.lock() {
                state.native_session = None;
                state.active_request = None;
            }
            if active_generation.load(Ordering::Acquire) == request.generation {
                let _ = send_error(
                    events,
                    request.generation,
                    format!("native H.264 decoder failed: {error}"),
                );
            }
        }
    }
}

fn schedule_continuation(
    executor: &Arc<dyn DecodeExecutor>,
    worker: &Arc<Mutex<H264WorkerState>>,
    events: &Sender<H264PlaybackEvent>,
    active_generation: &Arc<AtomicU64>,
    request: DecodeRequest,
) {
    if active_generation.load(Ordering::Acquire) == request.generation
        && let Err(error) =
            schedule_decode_step(executor, worker, events, active_generation, request)
    {
        cancel_generation(active_generation, request.generation);
        let _ = send_error(events, request.generation, error);
    }
}

fn cancel_generation(active_generation: &AtomicU64, generation: u64) {
    let _ = active_generation.compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire);
}

fn run_parallel_decode_job(
    mut job: ParallelDecodeJob,
    events: &Sender<H264PlaybackEvent>,
    active_generation: &AtomicU64,
    generation: u64,
) {
    if active_generation.load(Ordering::Acquire) != generation {
        return;
    }
    let result = (|| {
        job.decoder.send_packet(job.packet)?;
        let frame = job
            .decoder
            .receive_frame()?
            .ok_or_else(|| Error::InvalidData("parallel H.264 picture produced no frame".into()))?;
        if job.decoder.receive_frame()?.is_some() {
            return Err(Error::InvalidData(
                "parallel H.264 access unit produced multiple frames".into(),
            ));
        }
        rotate_native_frame(frame, job.rotation_degrees)
    })();
    if active_generation.load(Ordering::Acquire) != generation {
        return;
    }
    match result {
        Ok(frame) => {
            let _ = send_frame(events, generation, job.frame_index, frame);
        }
        Err(error) => {
            cancel_generation(active_generation, generation);
            let _ = send_error(
                events,
                generation,
                format!("parallel native H.264 decoder failed: {error}"),
            );
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_external_fallback(
    worker: &Mutex<H264WorkerState>,
    events: &Sender<H264PlaybackEvent>,
    active_generation: &AtomicU64,
    request: DecodeRequest,
    native_error: &str,
) {
    let result = match worker.lock() {
        Ok(mut state) => {
            state.native_session = None;
            state.active_request = None;
            if active_generation.load(Ordering::Acquire) != request.generation {
                return;
            }
            decode_request_external(
                &state.movie,
                &state.track,
                &state.configuration,
                &state.index,
                request,
            )
        }
        Err(_) => Err("H.264 decode state is poisoned".into()),
    };
    if active_generation.load(Ordering::Acquire) != request.generation {
        return;
    }
    match result {
        Ok(frames) => {
            for (frame_index, frame) in frames {
                if active_generation.load(Ordering::Acquire) != request.generation
                    || send_frame(events, request.generation, frame_index, frame).is_err()
                {
                    return;
                }
            }
        }
        Err(external_error) => {
            let _ = send_error(
                events,
                request.generation,
                format!(
                    "native H.264 decoder unsupported: {native_error}; optional external fallback: {external_error}"
                ),
            );
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn run_external_fallback(
    worker: &Mutex<H264WorkerState>,
    events: &Sender<H264PlaybackEvent>,
    active_generation: &AtomicU64,
    request: DecodeRequest,
    native_error: &str,
) {
    if let Ok(mut state) = worker.lock() {
        state.native_session = None;
        state.active_request = None;
    }
    if active_generation.load(Ordering::Acquire) == request.generation {
        let _ = send_error(
            events,
            request.generation,
            format!(
                "native H.264 decoder unsupported: {native_error}; external process fallback is unavailable in WebAssembly"
            ),
        );
    }
}

#[derive(Debug)]
struct NativeDecodeSession {
    decoder: H264Decoder,
    next_sample: usize,
    pending: HashMap<usize, VideoFrame>,
}

fn decode_request_native_step(
    state: &mut H264WorkerState,
    request: DecodeRequest,
    events: &Sender<H264PlaybackEvent>,
) -> mmrecode_core::Result<DecodeStepOutcome> {
    if state
        .active_request
        .as_ref()
        .is_none_or(|active| active.request.generation != request.generation)
    {
        state.active_request = Some(begin_native_request(
            &state.track,
            &state.index,
            request,
            &mut state.native_session,
            events,
        )?);
    }
    let mut active = state
        .active_request
        .take()
        .expect("native request was initialized");
    if active.remaining.is_empty() {
        finish_native_request(
            state
                .native_session
                .as_mut()
                .expect("native session was initialized"),
            &active,
        );
        return Ok(DecodeStepOutcome::Complete);
    }
    if let Some(jobs) = plan_parallel_b_batch(state, &mut active)? {
        let continue_after = !active.remaining.is_empty();
        if continue_after {
            state.active_request = Some(active);
        } else {
            finish_native_request(
                state
                    .native_session
                    .as_mut()
                    .expect("native session was initialized"),
                &active,
            );
        }
        return Ok(DecodeStepOutcome::Spawn {
            jobs,
            continue_after,
        });
    }
    let session = state
        .native_session
        .as_mut()
        .expect("native session was initialized");
    let sample_index = session.next_sample;
    if sample_index > active.end_sample {
        return Err(Error::InvalidData(format!(
            "native H.264 decoder did not return {} requested frame(s)",
            active.remaining.len()
        )));
    }
    let sample = &state.track.samples[sample_index];
    let mut flags = PacketFlags::empty();
    if sample.is_sync {
        flags.insert(PacketFlags::KEY);
    }
    session.decoder.send_packet(Packet {
        stream_id: state.track.descriptor.id,
        data: state.movie.sample_data(sample)?.to_vec(),
        pts: Some(Timestamp {
            value: sample.pts,
            time_base: state.track.descriptor.time_base,
        }),
        dts: Some(Timestamp {
            value: sample.dts,
            time_base: state.track.descriptor.time_base,
        }),
        duration: Some(Timestamp {
            value: i64::from(sample.duration),
            time_base: state.track.descriptor.time_base,
        }),
        flags,
        side_data: Vec::new(),
    })?;
    while let Some(frame) = session.decoder.receive_frame()? {
        let frame = rotate_native_frame(frame, state.track.rotation_degrees)?;
        if let Some(frame_index) = active.remaining.remove(&sample_index) {
            send_frame(events, request.generation, frame_index, frame)?;
        } else if active
            .frame_index_by_sample
            .get(&sample_index)
            .is_some_and(|frame_index| *frame_index >= active.requested_end)
        {
            session.pending.insert(sample_index, frame);
        }
    }
    session.next_sample = sample_index + 1;
    if active.remaining.is_empty() {
        finish_native_request(session, &active);
        Ok(DecodeStepOutcome::Complete)
    } else {
        state.active_request = Some(active);
        Ok(DecodeStepOutcome::Continue)
    }
}

fn plan_parallel_b_batch(
    state: &mut H264WorkerState,
    active: &mut NativeRequestState,
) -> mmrecode_core::Result<Option<Vec<ParallelDecodeJob>>> {
    if !state.index.progressive {
        return Ok(None);
    }
    let session = state
        .native_session
        .as_mut()
        .expect("native session was initialized");
    let first_sample = session.next_sample;
    let mut sample_index = first_sample;
    let mut jobs = Vec::new();
    while sample_index <= active.end_sample {
        let Some(&metadata_index) = active.frame_index_by_sample.get(&sample_index) else {
            break;
        };
        let metadata = &state.index.frames[metadata_index];
        if metadata.picture_type != PictureType::B || metadata.is_reference {
            break;
        }
        let sample = &state.track.samples[sample_index];
        let sample_data = state.movie.sample_data(sample)?;
        let units = length_prefixed_nal_units(sample_data, state.configuration.length_size)?;
        let mut coded_slice_count = 0;
        let mut safe = true;
        for unit in &units {
            match unit.header.unit_type {
                NalUnitType::CodedSlice => {
                    coded_slice_count += 1;
                    safe &= unit.header.reference_idc == 0;
                }
                NalUnitType::Sps | NalUnitType::Pps | NalUnitType::IdrSlice => safe = false,
                _ => {}
            }
        }
        if !safe || coded_slice_count == 0 {
            break;
        }
        if let Some(frame_index) = active.remaining.remove(&sample_index) {
            let mut flags = PacketFlags::empty();
            if sample.is_sync {
                flags.insert(PacketFlags::KEY);
            }
            jobs.push(ParallelDecodeJob {
                decoder: session.decoder.fork_for_non_reference_picture()?,
                packet: Packet {
                    stream_id: state.track.descriptor.id,
                    data: sample_data.to_vec(),
                    pts: Some(Timestamp {
                        value: sample.pts,
                        time_base: state.track.descriptor.time_base,
                    }),
                    dts: Some(Timestamp {
                        value: sample.dts,
                        time_base: state.track.descriptor.time_base,
                    }),
                    duration: Some(Timestamp {
                        value: i64::from(sample.duration),
                        time_base: state.track.descriptor.time_base,
                    }),
                    flags,
                    side_data: Vec::new(),
                },
                frame_index,
                rotation_degrees: state.track.rotation_degrees,
            });
        }
        sample_index += 1;
    }
    if sample_index == first_sample {
        Ok(None)
    } else {
        session.next_sample = sample_index;
        Ok(Some(jobs))
    }
}

fn begin_native_request(
    track: &Track,
    index: &H264VideoIndex,
    request: DecodeRequest,
    session: &mut Option<NativeDecodeSession>,
    events: &Sender<H264PlaybackEvent>,
) -> mmrecode_core::Result<NativeRequestState> {
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
    let sync_start_sample = track.samples[..=target_sample]
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample.is_sync)
        .map(|(sample_index, _)| sample_index)
        .next_back()
        .unwrap_or(0);
    let start_sample =
        native_recovery_start_sample(&index.frames, request.frame_index, target_sample)
            .map_or(sync_start_sample, |recovery| {
                recovery.max(sync_start_sample)
            });
    let wanted = (request.frame_index..requested_end)
        .map(|frame_index| (index.frames[frame_index].sample_index, frame_index))
        .collect::<HashMap<_, _>>();
    let can_continue = session.as_ref().is_some_and(|session| {
        session.next_sample >= start_sample
            && wanted.keys().all(|sample| {
                *sample >= session.next_sample || session.pending.contains_key(sample)
            })
    });
    if !can_continue {
        *session = Some(start_native_session(track, index, start_sample)?);
    }
    let session = session.as_mut().expect("native session was initialized");
    let mut remaining = wanted;
    let cached = remaining
        .keys()
        .copied()
        .filter(|sample| session.pending.contains_key(sample))
        .collect::<Vec<_>>();
    for sample_index in cached {
        let frame_index = remaining
            .remove(&sample_index)
            .expect("cached sample is requested");
        let frame = session
            .pending
            .remove(&sample_index)
            .expect("cached sample has a decoded frame");
        send_frame(events, request.generation, frame_index, frame)?;
    }
    let frame_index_by_sample = index
        .frames
        .iter()
        .enumerate()
        .map(|(frame_index, frame)| (frame.sample_index, frame_index))
        .collect::<HashMap<_, _>>();
    Ok(NativeRequestState {
        request,
        requested_end,
        end_sample,
        remaining,
        frame_index_by_sample,
    })
}

fn finish_native_request(session: &mut NativeDecodeSession, request: &NativeRequestState) {
    session.pending.retain(|sample_index, _| {
        request
            .frame_index_by_sample
            .get(sample_index)
            .is_some_and(|frame_index| *frame_index >= request.requested_end)
    });
}

fn start_native_session(
    track: &Track,
    index: &H264VideoIndex,
    start_sample: usize,
) -> mmrecode_core::Result<NativeDecodeSession> {
    let mut decoder = H264Decoder::default();
    decoder.configure(&track.descriptor.codec)?;
    if let Some(recovery) = index
        .frames
        .iter()
        .find(|frame| frame.sample_index == start_sample)
        .and_then(|frame| frame.recovery_point)
    {
        decoder.begin_recovery(recovery)?;
    }
    Ok(NativeDecodeSession {
        decoder,
        next_sample: start_sample,
        pending: HashMap::new(),
    })
}

fn send_frame(
    events: &Sender<H264PlaybackEvent>,
    generation: u64,
    frame_index: usize,
    frame: VideoFrame,
) -> mmrecode_core::Result<()> {
    events
        .send(H264PlaybackEvent::Frame {
            generation,
            frame_index,
            frame: Box::new(frame),
        })
        .map_err(|_| Error::InvalidState("H.264 playback receiver stopped".into()))
}

fn send_error(
    events: &Sender<H264PlaybackEvent>,
    generation: u64,
    message: String,
) -> Result<(), ()> {
    events
        .send(H264PlaybackEvent::Error {
            generation,
            message,
        })
        .map_err(|_| ())
}

fn native_recovery_start_sample(
    frames: &[IndexedH264Frame],
    target_frame: usize,
    target_sample: usize,
) -> Option<usize> {
    frames
        .iter()
        .take(target_frame.saturating_add(1))
        .filter_map(|frame| {
            let recovery = frame.recovery_point?;
            let recovery_frame_num =
                frame.frame_num.checked_add(recovery.recovery_frame_count)? % frame.max_frame_num;
            let recovery_complete = frames
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.is_reference
                        && candidate.frame_num == recovery_frame_num
                        && candidate.sample_index >= frame.sample_index
                })
                .min_by_key(|(_, candidate)| candidate.sample_index)
                .map(|(index, _)| index)?;
            (recovery_complete <= target_frame && frame.sample_index <= target_sample)
                .then_some(frame.sample_index)
        })
        .max()
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    use mmrecode_h264::{PictureType, RecoveryPoint};

    use crate::InlineDecodeExecutor;

    use super::{
        H264PlaybackEvent, H264PlaybackSource, IndexedH264Frame, native_recovery_start_sample,
        rotate_rgb, rotate_rgb_180,
    };

    #[test]
    fn recovery_window_starts_only_after_the_target_reference_matures() {
        let frame = |sample_index, picture_type, recovery_point| IndexedH264Frame {
            sample_index,
            dts: i64::try_from(sample_index).unwrap(),
            pts: i64::try_from(sample_index).unwrap(),
            duration: 1,
            picture_type,
            frame_num: u32::try_from(sample_index).unwrap(),
            max_frame_num: 16,
            is_idr: sample_index == 0,
            is_reference: true,
            recovery_point,
            dependencies: Vec::new(),
        };
        let recovery = RecoveryPoint {
            recovery_frame_count: 2,
            exact_match: true,
            broken_link: false,
            changing_slice_group_idc: 0,
        };
        let frames = vec![
            frame(0, PictureType::I, None),
            frame(1, PictureType::P, None),
            frame(2, PictureType::I, Some(recovery)),
            frame(3, PictureType::B, None),
            frame(4, PictureType::P, None),
            frame(
                5,
                PictureType::P,
                Some(RecoveryPoint {
                    recovery_frame_count: 0,
                    ..recovery
                }),
            ),
        ];
        assert_eq!(native_recovery_start_sample(&frames, 3, 3), None);
        assert_eq!(native_recovery_start_sample(&frames, 4, 4), Some(2));
        assert_eq!(native_recovery_start_sample(&frames, 5, 5), Some(5));
    }

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
    #[allow(clippy::too_many_lines)]
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
                "-profile:v",
                "main",
                "-pix_fmt",
                "yuv420p",
                "-bf",
                "3",
                "-g",
                "12",
                "-x264-params",
                "cabac=1:8x8dct=0:b-adapt=0:direct=spatial:weightb=0:weightp=0:ref=1:no-deblock=1:analyse=none:scenecut=0",
                "-y",
            ])
            .arg(&path)
            .status()
            .expect("run ffmpeg test encoder");
        if !status.success() {
            eprintln!("skipping H.264 playback bridge test: libx264 is unavailable");
            return;
        }
        let independent = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&path)
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "pipe:1"])
            .output()
            .expect("decode H.264 playback vector independently");
        assert!(independent.status.success());
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let executor = Arc::new(InlineDecodeExecutor::new(64).unwrap());
        let mut source = H264PlaybackSource::with_executor(bytes, executor).unwrap();
        assert_eq!(source.index().frame_count(), 12);
        assert_eq!(
            (
                source.index().display_width(),
                source.index().display_height()
            ),
            (64, 48)
        );
        let obsolete_generation = source.request(0, 0).unwrap();
        let generation = source.request(5, 2).unwrap();
        assert_ne!(obsolete_generation, generation);
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
                    decoded.push((frame_index, frame));
                }
                Some(H264PlaybackEvent::Error { message, .. }) => panic!("{message}"),
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        decoded.sort_unstable_by_key(|(frame_index, _)| *frame_index);
        assert_eq!(
            decoded
                .iter()
                .map(|(frame_index, _)| *frame_index)
                .collect::<Vec<_>>(),
            [5, 6, 7]
        );
        let frame_size = 64 * 48 * 3 / 2;
        for (frame_index, frame) in decoded {
            let native = frame
                .planes
                .iter()
                .flat_map(|plane| plane.data.iter().copied())
                .collect::<Vec<_>>();
            let expected =
                &independent.stdout[frame_index * frame_size..(frame_index + 1) * frame_size];
            if native != expected {
                let first = native
                    .iter()
                    .zip(expected)
                    .position(|(native, expected)| native != expected)
                    .unwrap();
                let matching_frame = independent
                    .stdout
                    .chunks_exact(frame_size)
                    .position(|candidate| candidate == native);
                panic!(
                    "parallel playback mismatch at frame {frame_index}, byte {first}: native {}, expected {}; exact independent match {matching_frame:?}",
                    native[first], expected[first]
                );
            }
        }
    }

    #[test]
    fn indexes_and_decodes_a_real_open_gop_recovery_point() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping H.264 recovery playback test: ffmpeg is unavailable");
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "mmrecode-h264-recovery-playback-{}.mp4",
            std::process::id()
        ));
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=96x64:rate=8",
                "-frames:v",
                "16",
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-bf",
                "2",
                "-g",
                "8",
                "-keyint_min",
                "8",
                "-x264-params",
                "cabac=1:slices=4:open-gop=1:scenecut=0",
                "-y",
            ])
            .arg(&path)
            .status()
            .expect("run open-GOP recovery encoder");
        if !status.success() {
            eprintln!("skipping H.264 recovery playback test: libx264 is unavailable");
            return;
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut source = H264PlaybackSource::new(bytes).unwrap();
        let recovery_index = source
            .index()
            .frames()
            .iter()
            .position(|frame| {
                !frame.is_idr
                    && frame.picture_type == PictureType::I
                    && frame.recovery_point.is_some()
            })
            .expect("x264 open GOP contains a non-IDR recovery point");
        let recovery = source.index().frames()[recovery_index]
            .recovery_point
            .unwrap();
        assert_eq!(recovery.recovery_frame_count, 0);

        let generation = source.request(recovery_index, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match source.try_event().unwrap() {
                Some(H264PlaybackEvent::Frame {
                    generation: found,
                    frame_index,
                    frame,
                }) => {
                    assert_eq!(found, generation);
                    assert_eq!(frame_index, recovery_index);
                    assert_eq!((frame.width, frame.height), (96, 64));
                    break;
                }
                Some(H264PlaybackEvent::Error { message, .. }) => panic!("{message}"),
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => panic!("timed out waiting for recovery-point frame"),
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn decodes_a_real_cyclic_intra_refresh_window_exactly() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping H.264 intra-refresh test: ffmpeg is unavailable");
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "mmrecode-h264-intra-refresh-{}.mp4",
            std::process::id()
        ));
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=96x64:rate=10",
                "-frames:v",
                "40",
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-bf",
                "0",
                "-g",
                "30",
                "-keyint_min",
                "30",
                "-x264-params",
                "cabac=1:intra-refresh=1:ref=1:scenecut=0",
                "-y",
            ])
            .arg(&path)
            .status()
            .expect("run cyclic intra-refresh encoder");
        if !status.success() {
            eprintln!("skipping H.264 intra-refresh test: libx264 is unavailable");
            return;
        }
        let independent = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&path)
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "pipe:1"])
            .output()
            .expect("decode complete intra-refresh stream independently");
        assert!(independent.status.success());
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut source = H264PlaybackSource::new(bytes).unwrap();
        let (recovery_index, recovery_frame) = source
            .index()
            .frames()
            .iter()
            .enumerate()
            .find(|(_, frame)| {
                frame.picture_type == PictureType::P && frame.recovery_point.is_some()
            })
            .expect("x264 cyclic refresh contains a P-picture recovery point");
        let recovery = recovery_frame.recovery_point.unwrap();
        assert!(recovery.recovery_frame_count > 0);
        let target_frame_num = (recovery_frame.frame_num + recovery.recovery_frame_count)
            % recovery_frame.max_frame_num;
        let target_index = source
            .index()
            .frames()
            .iter()
            .enumerate()
            .filter(|(_, frame)| {
                frame.is_reference
                    && frame.frame_num == target_frame_num
                    && frame.sample_index >= recovery_frame.sample_index
            })
            .min_by_key(|(_, frame)| frame.sample_index)
            .map(|(index, _)| index)
            .expect("recovery target reference picture exists");
        assert!(target_index > recovery_index);

        let generation = source.request(target_index, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let recovered = loop {
            match source.try_event().unwrap() {
                Some(H264PlaybackEvent::Frame {
                    generation: found,
                    frame_index,
                    frame,
                }) => {
                    assert_eq!(found, generation);
                    assert_eq!(frame_index, target_index);
                    break frame;
                }
                Some(H264PlaybackEvent::Error { message, .. }) => panic!("{message}"),
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => panic!("timed out waiting for intra-refresh recovery target"),
            }
        };
        let native = recovered
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        let frame_size = 96 * 64 * 3 / 2;
        let expected =
            &independent.stdout[target_index * frame_size..(target_index + 1) * frame_size];
        assert_eq!(native, expected);
    }
}
