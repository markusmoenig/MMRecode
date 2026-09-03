//! Reusable media timeline and playback-clock primitives.

mod h264;
mod mpeg2;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use mmrecode_core::{Error, Rational, Result};

pub use h264::{H264PlaybackEvent, H264PlaybackSource, H264VideoIndex, IndexedH264Frame};
pub use mpeg2::{IndexedMpeg2Frame, Mpeg2PlaybackEvent, Mpeg2PlaybackSource, Mpeg2VideoIndex};

/// A fixed- or variable-frame-rate video timeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackTimeline {
    frame_rate: Rational,
    rate_numerator: u128,
    rate_denominator: u128,
    frame_count: usize,
    duration: Duration,
    frame_positions: Option<Arc<[Duration]>>,
}

impl PlaybackTimeline {
    /// Creates a non-empty timeline with a positive frame rate.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty timeline, a non-positive frame rate, or a duration that
    /// cannot be represented by [`Duration`].
    pub fn new(frame_rate: Rational, frame_count: usize) -> Result<Self> {
        if frame_count == 0 {
            return Err(Error::InvalidData(
                "a playback timeline must contain at least one frame".into(),
            ));
        }
        if frame_rate.numerator() <= 0 || frame_rate.denominator() <= 0 {
            return Err(Error::InvalidData(
                "a playback frame rate must be positive".into(),
            ));
        }
        let rate_denominator = u128::try_from(frame_rate.denominator())
            .map_err(|_| Error::InvalidData("frame-rate denominator is negative".into()))?;
        let rate_numerator = u128::try_from(frame_rate.numerator())
            .map_err(|_| Error::InvalidData("frame-rate numerator is negative".into()))?;
        let duration = scaled_duration(frame_count as u128 * rate_denominator, rate_numerator)?;
        Ok(Self {
            frame_rate,
            rate_numerator,
            rate_denominator,
            frame_count,
            duration,
            frame_positions: None,
        })
    }

    /// Creates a variable-frame-rate timeline from normalized presentation positions.
    ///
    /// `nominal_frame_rate` remains available for timecode and UI scaling. Frame selection and
    /// playback use the exact positions, which must begin at zero and increase strictly.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid nominal rate, empty or unordered positions, or positions
    /// that do not fit within `duration`.
    pub fn variable(
        nominal_frame_rate: Rational,
        frame_positions: Vec<Duration>,
        duration: Duration,
    ) -> Result<Self> {
        if frame_positions.is_empty() {
            return Err(Error::InvalidData(
                "a playback timeline must contain at least one frame".into(),
            ));
        }
        if nominal_frame_rate.numerator() <= 0 || nominal_frame_rate.denominator() <= 0 {
            return Err(Error::InvalidData(
                "a playback frame rate must be positive".into(),
            ));
        }
        if frame_positions[0] != Duration::ZERO
            || !frame_positions.windows(2).all(|pair| pair[0] < pair[1])
            || frame_positions
                .last()
                .is_none_or(|position| *position >= duration)
        {
            return Err(Error::InvalidData(
                "variable playback positions must start at zero, increase, and precede duration"
                    .into(),
            ));
        }
        Ok(Self {
            frame_rate: nominal_frame_rate,
            rate_numerator: u128::try_from(nominal_frame_rate.numerator())
                .map_err(|_| Error::InvalidData("frame-rate numerator is negative".into()))?,
            rate_denominator: u128::try_from(nominal_frame_rate.denominator())
                .map_err(|_| Error::InvalidData("frame-rate denominator is negative".into()))?,
            frame_count: frame_positions.len(),
            duration,
            frame_positions: Some(frame_positions.into()),
        })
    }

    /// Returns the exact nominal frame rate.
    #[must_use]
    pub const fn frame_rate(&self) -> Rational {
        self.frame_rate
    }

    /// Returns the number of video frames.
    #[must_use]
    pub const fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Returns the presentation duration of all frames.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// Returns the presentation position of a frame, saturating at the last frame.
    #[must_use]
    pub fn position_of_frame(&self, frame_index: usize) -> Duration {
        let index = frame_index.min(self.frame_count - 1);
        if let Some(positions) = &self.frame_positions {
            return positions[index];
        }
        scaled_duration_saturating(index as u128 * self.rate_denominator, self.rate_numerator)
    }

    /// Selects the frame visible at `position`, saturating at the last frame.
    #[must_use]
    pub fn frame_at(&self, position: Duration) -> usize {
        if let Some(positions) = &self.frame_positions {
            return positions
                .partition_point(|candidate| *candidate <= position)
                .saturating_sub(1)
                .min(self.frame_count - 1);
        }
        let nanos = position.as_nanos();
        let numerator = nanos.saturating_mul(self.rate_numerator);
        let denominator = 1_000_000_000_u128 * self.rate_denominator;
        usize::try_from(numerator / denominator)
            .unwrap_or(usize::MAX)
            .min(self.frame_count - 1)
    }
}

/// Transition produced by advancing a playback controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackEvent {
    /// Playback remains within the timeline.
    None,
    /// The end was reached and playback stopped.
    Ended,
    /// The end was reached and playback wrapped to the beginning.
    Looped,
}

/// Play/pause/seek state with optional synchronization to an external audio clock.
#[derive(Clone, Debug)]
pub struct PlaybackController {
    timeline: PlaybackTimeline,
    position: Duration,
    anchor: Option<Instant>,
    looping: bool,
}

impl PlaybackController {
    /// Creates a paused controller at the first frame.
    #[must_use]
    pub const fn new(timeline: PlaybackTimeline) -> Self {
        Self {
            timeline,
            position: Duration::ZERO,
            anchor: None,
            looping: false,
        }
    }

    /// Returns the underlying video timeline.
    #[must_use]
    pub const fn timeline(&self) -> &PlaybackTimeline {
        &self.timeline
    }

    /// Returns whether playback is currently advancing.
    #[must_use]
    pub const fn is_playing(&self) -> bool {
        self.anchor.is_some()
    }

    /// Returns whether reaching the end restarts playback.
    #[must_use]
    pub const fn is_looping(&self) -> bool {
        self.looping
    }

    /// Enables or disables end-to-start looping.
    pub const fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    /// Returns the current media position.
    #[must_use]
    pub const fn position(&self) -> Duration {
        self.position
    }

    /// Returns the video frame visible at the current position.
    #[must_use]
    pub fn frame_index(&self) -> usize {
        self.timeline.frame_at(self.position)
    }

    /// Starts or resumes playback at `now`.
    pub fn play(&mut self, now: Instant) {
        if self.position >= self.timeline.duration() {
            self.position = Duration::ZERO;
        }
        self.anchor = Some(now);
    }

    /// Pauses playback after accounting for elapsed wall-clock time.
    pub fn pause(&mut self, now: Instant) {
        if let Some(anchor) = self.anchor.take() {
            self.position = self
                .position
                .saturating_add(now.saturating_duration_since(anchor));
            self.position = self.position.min(self.timeline.duration());
        }
    }

    /// Seeks to a media position, clamped to the timeline.
    pub fn seek(&mut self, position: Duration, now: Instant) {
        self.position = position.min(self.timeline.duration());
        if self.is_playing() {
            self.anchor = Some(now);
        }
    }

    /// Advances using the monotonic wall clock.
    pub fn advance(&mut self, now: Instant) -> PlaybackEvent {
        let Some(anchor) = self.anchor else {
            return PlaybackEvent::None;
        };
        let position = self
            .position
            .saturating_add(now.saturating_duration_since(anchor));
        self.apply_position(position, now)
    }

    /// Synchronizes the timeline to an external clock such as rendered audio samples.
    pub fn synchronize(&mut self, position: Duration, now: Instant) -> PlaybackEvent {
        if !self.is_playing() {
            return PlaybackEvent::None;
        }
        self.apply_position(position, now)
    }

    fn apply_position(&mut self, position: Duration, now: Instant) -> PlaybackEvent {
        if position < self.timeline.duration() {
            self.position = position;
            self.anchor = Some(now);
            PlaybackEvent::None
        } else if self.looping {
            self.position = Duration::ZERO;
            self.anchor = Some(now);
            PlaybackEvent::Looped
        } else {
            self.position = self.timeline.duration();
            self.anchor = None;
            PlaybackEvent::Ended
        }
    }
}

fn scaled_duration(seconds_numerator: u128, seconds_denominator: u128) -> Result<Duration> {
    let nanos = seconds_numerator
        .checked_mul(1_000_000_000)
        .ok_or_else(|| Error::InvalidData("playback duration overflows nanoseconds".into()))?
        / seconds_denominator;
    let nanos = u64::try_from(nanos)
        .map_err(|_| Error::InvalidData("playback duration exceeds Duration".into()))?;
    Ok(Duration::from_nanos(nanos))
}

fn scaled_duration_saturating(seconds_numerator: u128, seconds_denominator: u128) -> Duration {
    seconds_numerator
        .saturating_mul(1_000_000_000)
        .checked_div(seconds_denominator)
        .and_then(|nanos| u64::try_from(nanos).ok())
        .map_or(Duration::MAX, Duration::from_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntsc_timeline() -> PlaybackTimeline {
        PlaybackTimeline::new(Rational::new(30_000, 1_001).unwrap(), 300).unwrap()
    }

    #[test]
    fn maps_fractional_rate_frames_without_floating_point() {
        let timeline = ntsc_timeline();
        assert_eq!(timeline.duration(), Duration::from_millis(10_010));
        assert_eq!(timeline.frame_at(Duration::ZERO), 0);
        assert_eq!(timeline.frame_at(timeline.position_of_frame(123)), 123);
        assert_eq!(timeline.frame_at(timeline.duration()), 299);
    }

    #[test]
    fn maps_variable_frame_positions_exactly() {
        let timeline = PlaybackTimeline::variable(
            Rational::new(30, 1).unwrap(),
            vec![
                Duration::ZERO,
                Duration::from_millis(20),
                Duration::from_millis(70),
            ],
            Duration::from_millis(100),
        )
        .unwrap();
        assert_eq!(timeline.frame_at(Duration::from_millis(19)), 0);
        assert_eq!(timeline.frame_at(Duration::from_millis(20)), 1);
        assert_eq!(timeline.frame_at(Duration::from_millis(69)), 1);
        assert_eq!(timeline.frame_at(Duration::from_millis(70)), 2);
        assert_eq!(timeline.position_of_frame(2), Duration::from_millis(70));
    }

    #[test]
    fn wall_clock_pauses_and_reaches_the_end() {
        let timeline = PlaybackTimeline::new(Rational::new(25, 1).unwrap(), 10).unwrap();
        let start = Instant::now();
        let mut playback = PlaybackController::new(timeline);
        playback.play(start);
        assert_eq!(
            playback.advance(start + Duration::from_millis(120)),
            PlaybackEvent::None
        );
        assert_eq!(playback.frame_index(), 3);
        playback.pause(start + Duration::from_millis(160));
        assert_eq!(playback.position(), Duration::from_millis(160));
        playback.play(start + Duration::from_millis(200));
        assert_eq!(
            playback.advance(start + Duration::from_millis(500)),
            PlaybackEvent::Ended
        );
        assert!(!playback.is_playing());
        assert_eq!(playback.frame_index(), 9);
    }

    #[test]
    fn external_clock_can_loop() {
        let timeline = PlaybackTimeline::new(Rational::new(25, 1).unwrap(), 2).unwrap();
        let start = Instant::now();
        let mut playback = PlaybackController::new(timeline);
        playback.set_looping(true);
        playback.play(start);
        assert_eq!(
            playback.synchronize(Duration::from_millis(80), start),
            PlaybackEvent::Looped
        );
        assert_eq!(playback.position(), Duration::ZERO);
        assert!(playback.is_playing());
    }
}
