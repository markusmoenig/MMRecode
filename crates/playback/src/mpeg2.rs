//! Indexed, bounded-memory MPEG-2 playback decoding.

use std::{
    collections::HashMap,
    ops::Range,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
};

use mmrecode_core::{RandomAccessKind, Rational};
use mmrecode_mpeg2::{
    DecodedMpeg2Picture, Mpeg2PictureDecoder, PictureCodingExtension, PictureStructure,
    PictureType, SequenceParameters, analyze_dependencies, parse_stream,
};

/// Lightweight metadata for one MPEG-2 picture in presentation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedMpeg2Frame {
    /// Byte range in the demultiplexed elementary stream.
    pub source_range: Range<usize>,
    /// Index in elementary-stream decode order.
    pub decode_index: usize,
    /// Monotonic decode-order timestamp from dependency analysis.
    pub decode_order: i64,
    /// Monotonic presentation-order timestamp from dependency analysis.
    pub presentation_order: i64,
    /// Temporal reference from the picture header.
    pub temporal_reference: u16,
    /// I, P, or B picture type.
    pub picture_type: PictureType,
    /// Frame or field picture organization.
    pub picture_structure: PictureStructure,
    /// Active sequence parameters.
    pub sequence: SequenceParameters,
    /// Reference-picture identifiers required for reconstruction.
    pub references: Vec<u64>,
    /// Whether this picture is a clean or recovery random-access point.
    pub random_access: RandomAccessKind,
    /// Number of slices in the coded picture.
    pub slice_count: usize,
    /// Picture coding extension retained for inspection.
    pub coding_extension: PictureCodingExtension,
}

/// Presentation-ordered MPEG-2 index built without reconstructing video pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mpeg2VideoIndex {
    frame_rate: Rational,
    frames: Vec<IndexedMpeg2Frame>,
}

impl Mpeg2VideoIndex {
    /// Returns the nominal MPEG-2 frame rate.
    #[must_use]
    pub const fn frame_rate(&self) -> Rational {
        self.frame_rate
    }

    /// Returns presentation-ordered picture metadata.
    #[must_use]
    pub fn frames(&self) -> &[IndexedMpeg2Frame] {
        &self.frames
    }

    /// Returns the number of displayable pictures.
    #[must_use]
    pub const fn frame_count(&self) -> usize {
        self.frames.len()
    }
}

/// One asynchronous result from an [`Mpeg2PlaybackSource`].
#[derive(Debug)]
pub enum Mpeg2PlaybackEvent {
    /// One requested picture was reconstructed.
    Frame {
        /// Request generation used to discard obsolete seek results.
        generation: u64,
        /// Presentation-order frame index.
        frame_index: usize,
        /// Reconstructed picture and macroblock inspection data.
        picture: Box<DecodedMpeg2Picture>,
    },
    /// Reconstruction failed for the active request.
    Error {
        /// Request generation that failed.
        generation: u64,
        /// Human-readable codec error.
        message: String,
    },
}

#[derive(Clone, Copy, Debug)]
struct DecodeRequest {
    generation: u64,
    frame_index: usize,
    look_ahead: usize,
}

/// Background, indexed MPEG-2 source for editor and player preview.
///
/// Construction parses headers and dependencies but does not reconstruct pixels. Requested frames
/// are decoded on a worker thread from the closest preceding clean random-access point. Results are
/// delivered incrementally, allowing a client to maintain a small application-specific cache.
#[derive(Debug)]
pub struct Mpeg2PlaybackSource {
    index: Mpeg2VideoIndex,
    requests: Sender<DecodeRequest>,
    events: Receiver<Mpeg2PlaybackEvent>,
    generation: u64,
}

impl Mpeg2PlaybackSource {
    /// Indexes an owned elementary stream and starts its decode worker.
    ///
    /// # Errors
    ///
    /// Returns an error when MPEG-2 headers or picture dependencies are invalid, or the stream has
    /// no pictures.
    pub fn new(elementary_stream: Vec<u8>) -> Result<Self, String> {
        let index = build_index(&elementary_stream)?;
        let worker_index = index.clone();
        let (request_tx, request_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("mmrecode-mpeg2-playback".into())
            .spawn(move || decode_worker(elementary_stream, worker_index, request_rx, event_tx))
            .map_err(|error| format!("cannot start MPEG-2 decode worker: {error}"))?;
        Ok(Self {
            index,
            requests: request_tx,
            events: event_rx,
            generation: 0,
        })
    }

    /// Returns the lightweight stream index.
    #[must_use]
    pub const fn index(&self) -> &Mpeg2VideoIndex {
        &self.index
    }

    /// Requests one frame and a bounded number of following presentation frames.
    ///
    /// A newer request supersedes older generations. The worker checks for superseding seeks
    /// between pictures, so long-running decode work is abandoned promptly.
    ///
    /// # Errors
    ///
    /// Returns an error if `frame_index` is outside the indexed stream or the worker stopped.
    pub fn request(&mut self, frame_index: usize, look_ahead: usize) -> Result<u64, String> {
        if frame_index >= self.index.frame_count() {
            return Err(format!(
                "MPEG-2 frame {frame_index} is outside 0..{}",
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
            .map_err(|_| "MPEG-2 decode worker stopped".to_owned())?;
        Ok(self.generation)
    }

    /// Returns the next available worker result without blocking.
    ///
    /// # Errors
    ///
    /// Returns an error when the worker has stopped and all queued events were consumed.
    pub fn try_event(&self) -> Result<Option<Mpeg2PlaybackEvent>, String> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("MPEG-2 decode worker stopped".into()),
        }
    }
}

fn build_index(data: &[u8]) -> Result<Mpeg2VideoIndex, String> {
    let stream = parse_stream(data).map_err(|error| error.to_string())?;
    let dependencies = analyze_dependencies(&stream).map_err(|error| error.to_string())?;
    let mut frames = stream
        .pictures()
        .iter()
        .zip(&dependencies)
        .enumerate()
        .map(|(decode_index, (picture, access))| IndexedMpeg2Frame {
            source_range: picture.source_range.clone(),
            decode_index,
            decode_order: access.decode_order,
            presentation_order: access.presentation_order,
            temporal_reference: picture.header.temporal_reference,
            picture_type: picture.header.picture_coding_type,
            picture_structure: picture.coding_extension.picture_structure,
            sequence: picture.sequence.clone(),
            references: access
                .references
                .iter()
                .map(|reference| reference.0)
                .collect(),
            random_access: access.random_access,
            slice_count: picture.slices.len(),
            coding_extension: picture.coding_extension,
        })
        .collect::<Vec<_>>();
    frames.sort_by_key(|frame| frame.presentation_order);
    let frame_rate = frames
        .first()
        .map(|frame| frame.sequence.frame_rate)
        .ok_or_else(|| "MPEG-2 stream contains no displayable pictures".to_owned())?;
    Ok(Mpeg2VideoIndex { frame_rate, frames })
}

#[allow(clippy::needless_pass_by_value)]
fn decode_worker(
    data: Vec<u8>,
    index: Mpeg2VideoIndex,
    requests: Receiver<DecodeRequest>,
    events: Sender<Mpeg2PlaybackEvent>,
) {
    let stream = match parse_stream(&data) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = events.send(Mpeg2PlaybackEvent::Error {
                generation: 0,
                message: error.to_string(),
            });
            return;
        }
    };
    let dependencies = match analyze_dependencies(&stream) {
        Ok(dependencies) => dependencies,
        Err(error) => {
            let _ = events.send(Mpeg2PlaybackEvent::Error {
                generation: 0,
                message: error.to_string(),
            });
            return;
        }
    };
    let display_by_presentation = index
        .frames()
        .iter()
        .enumerate()
        .map(|(display_index, frame)| (frame.presentation_order, display_index))
        .collect::<HashMap<_, _>>();

    let mut pending = requests.recv().ok();
    while let Some(mut request) = pending.take() {
        if let Some(latest) = requests.try_iter().last() {
            request = latest;
        }
        match decode_request(
            &data,
            &stream,
            &dependencies,
            &display_by_presentation,
            &requests,
            &events,
            request,
            index.frame_count(),
        ) {
            Ok(interrupted) => pending = interrupted.or_else(|| requests.recv().ok()),
            Err(message) => {
                if events
                    .send(Mpeg2PlaybackEvent::Error {
                        generation: request.generation,
                        message,
                    })
                    .is_err()
                {
                    return;
                }
                pending = requests.recv().ok();
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_request(
    data: &[u8],
    stream: &mmrecode_mpeg2::Mpeg2Stream<'_>,
    dependencies: &[mmrecode_core::AccessUnitInfo],
    display_by_presentation: &HashMap<i64, usize>,
    requests: &Receiver<DecodeRequest>,
    events: &Sender<Mpeg2PlaybackEvent>,
    request: DecodeRequest,
    frame_count: usize,
) -> Result<Option<DecodeRequest>, String> {
    let target_presentation =
        stream_target_presentation(display_by_presentation, request.frame_index)?;
    let start = dependencies
        .iter()
        .enumerate()
        .filter(|(_, access)| {
            access.random_access == RandomAccessKind::Clean
                && access.presentation_order <= target_presentation
        })
        .map(|(decode_index, _)| decode_index)
        .next_back()
        .unwrap_or(0);
    let wanted_end = request
        .frame_index
        .saturating_add(request.look_ahead)
        .saturating_add(1)
        .min(frame_count);
    let mut remaining = wanted_end.saturating_sub(request.frame_index);
    let mut decoder = Mpeg2PictureDecoder::default();

    for (picture, access) in stream.pictures()[start..]
        .iter()
        .zip(&dependencies[start..])
    {
        if let Some(latest) = requests.try_iter().last() {
            return Ok(Some(latest));
        }
        let picture_result = decoder
            .decode_picture(
                data,
                picture,
                access.decode_order,
                access.presentation_order,
            )
            .map_err(|error| error.to_string())?;
        let Some(&frame_index) = display_by_presentation.get(&access.presentation_order) else {
            continue;
        };
        if (request.frame_index..wanted_end).contains(&frame_index) {
            events
                .send(Mpeg2PlaybackEvent::Frame {
                    generation: request.generation,
                    frame_index,
                    picture: Box::new(picture_result),
                })
                .map_err(|_| "MPEG-2 playback client stopped".to_owned())?;
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(None)
}

fn stream_target_presentation(
    display_by_presentation: &HashMap<i64, usize>,
    target_index: usize,
) -> Result<i64, String> {
    display_by_presentation
        .iter()
        .find_map(|(&presentation, &display)| (display == target_index).then_some(presentation))
        .ok_or_else(|| format!("MPEG-2 index has no presentation frame {target_index}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const MPEG2: &[u8] =
        include_bytes!("../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");

    #[test]
    fn indexing_does_not_decode_pixels() {
        let source = Mpeg2PlaybackSource::new(MPEG2.to_vec()).expect("index stream");
        assert_eq!(source.index().frame_count(), 12);
        assert_eq!(source.index().frames()[0].presentation_order, 0);
    }

    #[test]
    fn worker_returns_only_the_bounded_requested_window() {
        let mut source = Mpeg2PlaybackSource::new(MPEG2.to_vec()).expect("index stream");
        let generation = source.request(5, 2).expect("request frames");
        let mut returned = Vec::new();
        while returned.len() < 3 {
            let event = source
                .events
                .recv_timeout(Duration::from_secs(2))
                .expect("decode result");
            match event {
                Mpeg2PlaybackEvent::Frame {
                    generation: actual,
                    frame_index,
                    ..
                } => {
                    assert_eq!(actual, generation);
                    returned.push(frame_index);
                }
                Mpeg2PlaybackEvent::Error { message, .. } => panic!("{message}"),
            }
        }
        returned.sort_unstable();
        assert_eq!(returned, [5, 6, 7]);
        assert!(source.events.try_recv().is_err());
    }

    #[test]
    fn newer_seek_generation_supersedes_queued_work() {
        let mut source = Mpeg2PlaybackSource::new(MPEG2.to_vec()).expect("index stream");
        let _obsolete = source.request(0, 4).expect("first request");
        let current = source.request(9, 0).expect("seek request");
        loop {
            let event = source
                .events
                .recv_timeout(Duration::from_secs(2))
                .expect("decode result");
            match event {
                Mpeg2PlaybackEvent::Frame {
                    generation,
                    frame_index,
                    ..
                } if generation == current => {
                    assert_eq!(frame_index, 9);
                    break;
                }
                Mpeg2PlaybackEvent::Frame { .. } => {}
                Mpeg2PlaybackEvent::Error { message, .. } => panic!("{message}"),
            }
        }
    }
}
