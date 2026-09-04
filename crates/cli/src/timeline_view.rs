//! Exact frame-space navigation for the interactive editor timeline.

use std::ops::Range;

/// Direction of a discrete timeline zoom operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimelineZoom {
    In,
    Out,
}

/// The source-frame interval currently visible in the timeline.
///
/// The viewport deliberately knows nothing about terminal cells or rendering. It is shared by
/// keyboard navigation, mouse interaction, and the current text renderer; a future pixel renderer
/// can consume the same exact frame-space model.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TimelineViewport {
    total_frames: usize,
    start: usize,
    end: usize,
}

impl TimelineViewport {
    pub(crate) fn reset(&mut self, total_frames: usize) {
        self.total_frames = total_frames;
        self.start = 0;
        self.end = total_frames;
    }

    pub(crate) fn sync_total_frames(&mut self, total_frames: usize) {
        if self.total_frames != total_frames {
            self.reset(total_frames);
        }
    }

    pub(crate) fn fit(&mut self) {
        self.start = 0;
        self.end = self.total_frames;
    }

    pub(crate) fn visible_range(&self) -> Range<usize> {
        self.start..self.end
    }

    pub(crate) fn visible_frame_count(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub(crate) fn is_fitted(&self) -> bool {
        self.start == 0 && self.end == self.total_frames
    }

    pub(crate) fn frame_at_column(&self, column: usize, width: usize) -> usize {
        let visible_frames = self.visible_frame_count();
        if width <= 1 || visible_frames <= 1 {
            return self.start.min(self.total_frames.saturating_sub(1));
        }
        let numerator = (column.min(width - 1) as u128) * ((visible_frames - 1) as u128);
        let offset =
            usize::try_from(numerator / ((width - 1) as u128)).unwrap_or(visible_frames - 1);
        self.start + offset
    }

    pub(crate) fn column_for_frame(&self, frame: usize, width: usize) -> usize {
        let visible_frames = self.visible_frame_count();
        if width <= 1 || visible_frames <= 1 {
            return 0;
        }
        let frame = frame.clamp(self.start, self.end - 1);
        let numerator = ((frame - self.start) as u128) * ((width - 1) as u128);
        usize::try_from(numerator / ((visible_frames - 1) as u128)).unwrap_or(width - 1)
    }

    pub(crate) fn zoom_around_frame(&mut self, anchor: usize, direction: TimelineZoom) {
        let target_span = self.zoomed_span(direction);
        self.place_anchor(anchor, target_span / 2, target_span);
    }

    pub(crate) fn zoom_at_column(&mut self, column: usize, width: usize, direction: TimelineZoom) {
        let anchor = self.frame_at_column(column, width);
        let target_span = self.zoomed_span(direction);
        let left_of_anchor = if width <= 1 || target_span <= 1 {
            0
        } else {
            let numerator = (column.min(width - 1) as u128) * ((target_span - 1) as u128);
            usize::try_from(numerator / ((width - 1) as u128)).unwrap_or(target_span - 1)
        };
        self.place_anchor(anchor, left_of_anchor, target_span);
    }

    pub(crate) fn pan_half_page(&mut self, forward: bool) {
        let amount = self.visible_frame_count().div_ceil(2);
        self.pan_by(amount, forward);
    }

    pub(crate) fn reveal(&mut self, frame: usize) {
        if self.visible_frame_count() == 0 || self.visible_range().contains(&frame) {
            return;
        }
        let span = self.visible_frame_count();
        if frame < self.start {
            self.start = frame;
            self.end = (self.start + span).min(self.total_frames);
        } else {
            self.end = frame.saturating_add(1).min(self.total_frames);
            self.start = self.end.saturating_sub(span);
        }
    }

    fn zoomed_span(&self, direction: TimelineZoom) -> usize {
        let span = self.visible_frame_count();
        match direction {
            TimelineZoom::In => span.div_ceil(2).max(1),
            TimelineZoom::Out => span.saturating_mul(2).min(self.total_frames),
        }
    }

    fn place_anchor(&mut self, anchor: usize, left_of_anchor: usize, span: usize) {
        if self.total_frames == 0 {
            return;
        }
        let span = span.clamp(1, self.total_frames);
        let anchor = anchor.min(self.total_frames - 1);
        let start = anchor
            .saturating_sub(left_of_anchor)
            .min(self.total_frames - span);
        self.start = start;
        self.end = start + span;
    }

    fn pan_by(&mut self, amount: usize, forward: bool) {
        let span = self.visible_frame_count();
        if span == 0 || span == self.total_frames {
            return;
        }
        let maximum_start = self.total_frames - span;
        let start = if forward {
            self.start.saturating_add(amount).min(maximum_start)
        } else {
            self.start.saturating_sub(amount)
        };
        self.start = start;
        self.end = start + span;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_coordinates_cover_the_complete_source() {
        let mut viewport = TimelineViewport::default();
        viewport.reset(769);
        assert_eq!(viewport.frame_at_column(0, 101), 0);
        assert_eq!(viewport.frame_at_column(100, 101), 768);
        assert_eq!(viewport.column_for_frame(0, 101), 0);
        assert_eq!(viewport.column_for_frame(768, 101), 100);
    }

    #[test]
    fn zoom_keeps_the_anchor_under_the_mouse_column() {
        let mut viewport = TimelineViewport::default();
        viewport.reset(1_001);
        let anchor = viewport.frame_at_column(75, 101);
        viewport.zoom_at_column(75, 101, TimelineZoom::In);
        assert_eq!(viewport.frame_at_column(75, 101), anchor);
        assert_eq!(viewport.visible_frame_count(), 501);
    }

    #[test]
    fn zoom_and_pan_remain_inside_source_bounds() {
        let mut viewport = TimelineViewport::default();
        viewport.reset(100);
        viewport.zoom_around_frame(0, TimelineZoom::In);
        assert_eq!(viewport.visible_range(), 0..50);
        viewport.pan_half_page(false);
        assert_eq!(viewport.visible_range(), 0..50);
        viewport.pan_half_page(true);
        assert_eq!(viewport.visible_range(), 25..75);
        viewport.pan_half_page(true);
        viewport.pan_half_page(true);
        assert_eq!(viewport.visible_range(), 50..100);
    }

    #[test]
    fn reveal_scrolls_only_when_playhead_leaves_the_viewport() {
        let mut viewport = TimelineViewport::default();
        viewport.reset(100);
        viewport.zoom_around_frame(50, TimelineZoom::In);
        let before = viewport.visible_range();
        viewport.reveal(50);
        assert_eq!(viewport.visible_range(), before);
        viewport.reveal(99);
        assert_eq!(viewport.visible_range(), 50..100);
    }
}
