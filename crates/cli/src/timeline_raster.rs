//! Pixel composition for the editor timeline.

use std::{collections::BTreeMap, ops::Range};

use image::{Rgb, RgbImage, imageops};

use crate::timeline_view::TimelineViewport;

const BACKGROUND: Rgb<u8> = Rgb([10, 14, 22]);
const GRID: Rgb<u8> = Rgb([43, 52, 67]);
const THUMBNAIL_PLACEHOLDER: Rgb<u8> = Rgb([23, 30, 42]);
const CLIP: Rgb<u8> = Rgb([38, 101, 140]);
const CLIP_OUTSIDE: Rgb<u8> = Rgb([25, 35, 48]);
const COPY: Rgb<u8> = Rgb([35, 145, 93]);
const BRIDGE: Rgb<u8> = Rgb([218, 151, 50]);
const FULL_RENDER: Rgb<u8> = Rgb([195, 67, 72]);
const REVIEW: Rgb<u8> = Rgb([100, 108, 124]);
const I_PICTURE: Rgb<u8> = Rgb([239, 175, 67]);
const P_PICTURE: Rgb<u8> = Rgb([66, 143, 209]);
const B_PICTURE: Rgb<u8> = Rgb([151, 91, 190]);
const OTHER_PICTURE: Rgb<u8> = Rgb([112, 120, 134]);
const PLAYHEAD: Rgb<u8> = Rgb([45, 220, 220]);
const PLAYHEAD_OUTLINE: Rgb<u8> = Rgb([3, 8, 12]);
const HANDLE: Rgb<u8> = Rgb([236, 240, 246]);
const CURRENT_OBJECT: Rgb<u8> = Rgb([79, 216, 191]);
const VIDEO_OBJECT: Rgb<u8> = Rgb([31, 70, 101]);
const AUDIO_OBJECT: Rgb<u8> = Rgb([64, 82, 54]);
const TEXT_OBJECT: Rgb<u8> = Rgb([91, 64, 104]);
const EFFECT_OBJECT: Rgb<u8> = Rgb([93, 67, 47]);
const OTHER_OBJECT: Rgb<u8> = Rgb([45, 53, 67]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimelinePictureKind {
    I,
    P,
    B,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TimelinePicture {
    pub(crate) frame: usize,
    pub(crate) kind: TimelinePictureKind,
    pub(crate) random_access: bool,
    pub(crate) reference: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SmartRenderState {
    Copy,
    Bridge,
    FullRender,
    Review,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SmartRenderSpan {
    pub(crate) frames: Range<usize>,
    pub(crate) state: SmartRenderState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TimelineObjectLane {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) frames: Range<usize>,
    pub(crate) current: bool,
    pub(crate) preview: bool,
}

pub(crate) struct TimelineRasterInput<'a> {
    pub(crate) viewport: &'a TimelineViewport,
    pub(crate) playhead: usize,
    pub(crate) retained: Range<usize>,
    pub(crate) thumbnail_frames: &'a [usize],
    pub(crate) thumbnails: &'a BTreeMap<usize, RgbImage>,
    pub(crate) pictures: &'a [TimelinePicture],
    pub(crate) smart_render: &'a [SmartRenderSpan],
    pub(crate) objects: &'a [TimelineObjectLane],
    pub(crate) ruler_height: u32,
    pub(crate) object_row_height: u32,
}

pub(crate) fn render_timeline(
    input: &TimelineRasterInput<'_>,
    width: u32,
    height: u32,
) -> RgbImage {
    let width = width.max(1);
    let height = height.max(1);
    let mut image = RgbImage::from_pixel(width, height, BACKGROUND);
    let ruler_height = input.ruler_height.clamp(1, height);
    let lane_height = (height / 12).clamp(4, 10);
    let gap = 2;
    let codec_y = height.saturating_sub(lane_height);
    let render_y = codec_y.saturating_sub(lane_height + gap);
    let clip_y = render_y.saturating_sub(lane_height + gap);

    draw_ruler(&mut image, ruler_height);
    draw_object_lanes(
        &mut image,
        input,
        ruler_height.saturating_add(gap),
        clip_y.saturating_sub(gap),
    );
    draw_range_lane(
        &mut image,
        input.viewport,
        input.retained.clone(),
        clip_y,
        lane_height,
    );
    draw_smart_render_lane(
        &mut image,
        input.viewport,
        input.smart_render,
        render_y,
        lane_height,
    );
    draw_codec_lane(
        &mut image,
        input.viewport,
        input.pictures,
        codec_y,
        lane_height,
    );
    draw_playhead(&mut image, input.viewport, input.playhead);
    image
}

fn draw_ruler(image: &mut RgbImage, height: u32) {
    fill_rect(image, 0, height.saturating_sub(1), image.width(), 1, GRID);
    for quarter in 0_u32..=4 {
        let x =
            u32::try_from((u128::from(quarter) * u128::from(image.width().saturating_sub(1))) / 4)
                .unwrap_or(image.width().saturating_sub(1));
        fill_rect(image, x, 0, 1, height, GRID);
    }
}

fn draw_object_lanes(
    image: &mut RgbImage,
    input: &TimelineRasterInput<'_>,
    start_y: u32,
    end_y: u32,
) {
    let row_height = input.object_row_height.max(3);
    let gap = 2;
    for (index, object) in input.objects.iter().enumerate() {
        let y = start_y.saturating_add(
            u32::try_from(index)
                .unwrap_or(u32::MAX)
                .saturating_mul(row_height.saturating_add(gap)),
        );
        if y >= end_y {
            break;
        }
        let height = row_height.min(end_y.saturating_sub(y));
        let Some((x, width)) = range_pixels(input.viewport, object.frames.clone(), image.width())
        else {
            continue;
        };
        let color = object_color(&object.kind);
        fill_rect(image, x, y, width, height, color);
        if object.preview {
            draw_thumbnails(
                image,
                input.thumbnail_frames,
                input.thumbnails,
                x,
                y,
                width,
                height,
            );
        }
        let border = if object.current { CURRENT_OBJECT } else { GRID };
        fill_rect(image, x, y, width, 2.min(height), border);
        fill_rect(
            image,
            x,
            y.saturating_add(height.saturating_sub(1)),
            width,
            1,
            border,
        );
        fill_rect(image, x, y, 2.min(width), height, border);
    }
}

fn object_color(kind: &str) -> Rgb<u8> {
    if kind.starts_with("video") {
        VIDEO_OBJECT
    } else if kind.starts_with("audio") {
        AUDIO_OBJECT
    } else if kind == "text" || kind.starts_with("text/") {
        TEXT_OBJECT
    } else if kind.starts_with("fx") || kind == "mask" || kind.starts_with("mask/") {
        EFFECT_OBJECT
    } else {
        OTHER_OBJECT
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_thumbnails(
    image: &mut RgbImage,
    frames: &[usize],
    thumbnails: &BTreeMap<usize, RgbImage>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) {
    if frames.is_empty() {
        fill_rect(image, x, y, width, height, THUMBNAIL_PLACEHOLDER);
        return;
    }
    for (slot, frame) in frames.iter().enumerate() {
        let x0 = x.saturating_add(proportional(slot, frames.len(), width));
        let x1 = x.saturating_add(proportional(slot + 1, frames.len(), width));
        let slot_width = x1.saturating_sub(x0).max(1);
        let placeholder = if slot % 2 == 0 {
            THUMBNAIL_PLACEHOLDER
        } else {
            Rgb([19, 25, 35])
        };
        fill_rect(image, x0, y, slot_width, height, placeholder);
        if let Some(thumbnail) = thumbnails.get(frame) {
            let (target_width, target_height) =
                fitted_size(thumbnail.width(), thumbnail.height(), slot_width, height);
            let resized = imageops::resize(
                thumbnail,
                target_width,
                target_height,
                imageops::FilterType::Triangle,
            );
            let target_x = x0 + slot_width.saturating_sub(target_width) / 2;
            let target_y = y + height.saturating_sub(target_height) / 2;
            imageops::replace(image, &resized, i64::from(target_x), i64::from(target_y));
        }
        fill_rect(image, x0, y, 1, height, GRID);
    }
}

fn fitted_size(source_width: u32, source_height: u32, width: u32, height: u32) -> (u32, u32) {
    if source_width == 0 || source_height == 0 || width == 0 || height == 0 {
        return (1, 1);
    }
    if u128::from(source_width) * u128::from(height) > u128::from(source_height) * u128::from(width)
    {
        let target_height =
            u32::try_from(u128::from(source_height) * u128::from(width) / u128::from(source_width))
                .unwrap_or(height)
                .clamp(1, height);
        (width, target_height)
    } else {
        let target_width = u32::try_from(
            u128::from(source_width) * u128::from(height) / u128::from(source_height),
        )
        .unwrap_or(width)
        .clamp(1, width);
        (target_width, height)
    }
}

fn draw_range_lane(
    image: &mut RgbImage,
    viewport: &TimelineViewport,
    retained: Range<usize>,
    y: u32,
    height: u32,
) {
    fill_rect(image, 0, y, image.width(), height, CLIP_OUTSIDE);
    if let Some((x, width)) = range_pixels(viewport, retained.clone(), image.width()) {
        fill_rect(image, x, y, width, height, CLIP);
    }
    for frame in [retained.start, retained.end.saturating_sub(1)] {
        if viewport.visible_range().contains(&frame) {
            let x = frame_x(viewport, frame, image.width());
            fill_rect(image, x.saturating_sub(1), y, 3, height, HANDLE);
        }
    }
}

fn draw_smart_render_lane(
    image: &mut RgbImage,
    viewport: &TimelineViewport,
    spans: &[SmartRenderSpan],
    y: u32,
    height: u32,
) {
    fill_rect(image, 0, y, image.width(), height, REVIEW);
    for span in spans {
        let color = match span.state {
            SmartRenderState::Copy => COPY,
            SmartRenderState::Bridge => BRIDGE,
            SmartRenderState::FullRender => FULL_RENDER,
            SmartRenderState::Review => REVIEW,
        };
        if let Some((x, width)) = range_pixels(viewport, span.frames.clone(), image.width()) {
            fill_rect(image, x, y, width, height, color);
        }
    }
}

fn draw_codec_lane(
    image: &mut RgbImage,
    viewport: &TimelineViewport,
    pictures: &[TimelinePicture],
    y: u32,
    height: u32,
) {
    fill_rect(image, 0, y, image.width(), height, Rgb([18, 24, 33]));
    for picture in pictures {
        if !viewport.visible_range().contains(&picture.frame) {
            continue;
        }
        let x = frame_x(viewport, picture.frame, image.width());
        let color = match picture.kind {
            TimelinePictureKind::I => I_PICTURE,
            TimelinePictureKind::P => P_PICTURE,
            TimelinePictureKind::B => B_PICTURE,
            TimelinePictureKind::Other => OTHER_PICTURE,
        };
        let bar_width = if picture.reference { 2 } else { 1 };
        fill_rect(image, x, y, bar_width, height, color);
        if picture.random_access {
            fill_rect(image, x.saturating_sub(1), y, 3, 2.min(height), HANDLE);
        }
    }
}

fn draw_playhead(image: &mut RgbImage, viewport: &TimelineViewport, playhead: usize) {
    if !viewport.visible_range().contains(&playhead) {
        return;
    }
    let x = frame_x(viewport, playhead, image.width());
    let shaft_width = (image.width() / 500).clamp(3, 7).min(image.width());
    let outline_width = shaft_width.saturating_add(4).min(image.width());
    fill_rect(
        image,
        x.saturating_sub(outline_width / 2),
        0,
        outline_width,
        image.height(),
        PLAYHEAD_OUTLINE,
    );
    fill_rect(
        image,
        x.saturating_sub(shaft_width / 2),
        0,
        shaft_width,
        image.height(),
        PLAYHEAD,
    );
    let head_width = 19.min(image.width());
    let head_height = 10.min(image.height());
    for row in 0..head_height {
        let width = head_width.saturating_sub(row.saturating_mul(head_width) / head_height);
        fill_rect(
            image,
            x.saturating_sub(width / 2),
            row,
            width.max(1),
            1,
            PLAYHEAD,
        );
    }
}

fn range_pixels(
    viewport: &TimelineViewport,
    range: Range<usize>,
    width: u32,
) -> Option<(u32, u32)> {
    let visible = viewport.visible_range();
    let start = range.start.max(visible.start);
    let end = range.end.min(visible.end);
    if start >= end {
        return None;
    }
    let x0 = frame_x(viewport, start, width);
    let x1 = if end >= visible.end {
        width
    } else {
        frame_x(viewport, end, width)
    };
    Some((x0, x1.saturating_sub(x0).max(1)))
}

fn frame_x(viewport: &TimelineViewport, frame: usize, width: u32) -> u32 {
    u32::try_from(viewport.column_for_frame(frame, width as usize))
        .unwrap_or(width.saturating_sub(1))
        .min(width.saturating_sub(1))
}

fn proportional(index: usize, count: usize, extent: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    u32::try_from((index as u128 * u128::from(extent)) / count as u128).unwrap_or(extent)
}

fn fill_rect(image: &mut RgbImage, x: u32, y: u32, width: u32, height: u32, color: Rgb<u8>) {
    let end_x = x.saturating_add(width).min(image.width());
    let end_y = y.saturating_add(height).min(image.height());
    for target_y in y.min(image.height())..end_y {
        for target_x in x.min(image.width())..end_x {
            image.put_pixel(target_x, target_y, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_contains_playhead_and_smart_render_colors() {
        let mut viewport = TimelineViewport::default();
        viewport.reset(100);
        let spans = vec![
            SmartRenderSpan {
                frames: 10..50,
                state: SmartRenderState::Copy,
            },
            SmartRenderSpan {
                frames: 50..60,
                state: SmartRenderState::Bridge,
            },
            SmartRenderSpan {
                frames: 60..90,
                state: SmartRenderState::FullRender,
            },
            SmartRenderSpan {
                frames: 90..100,
                state: SmartRenderState::Review,
            },
        ];
        let image = render_timeline(
            &TimelineRasterInput {
                viewport: &viewport,
                playhead: 55,
                retained: 10..90,
                thumbnail_frames: &[],
                thumbnails: &BTreeMap::new(),
                pictures: &[],
                smart_render: &spans,
                objects: &[TimelineObjectLane {
                    name: "Clip0".into(),
                    kind: "video/mpeg2".into(),
                    frames: 10..90,
                    current: true,
                    preview: true,
                }],
                ruler_height: 12,
                object_row_height: 28,
            },
            200,
            100,
        );
        assert_eq!(image.dimensions(), (200, 100));
        assert!(image.pixels().any(|pixel| *pixel == COPY));
        assert!(image.pixels().any(|pixel| *pixel == BRIDGE));
        assert!(image.pixels().any(|pixel| *pixel == FULL_RENDER));
        assert!(image.pixels().any(|pixel| *pixel == REVIEW));
        assert!(image.pixels().any(|pixel| *pixel == PLAYHEAD));
    }

    #[test]
    fn thumbnail_fit_preserves_source_aspect_ratio() {
        assert_eq!(fitted_size(4, 3, 160, 90), (120, 90));
        assert_eq!(fitted_size(16, 9, 80, 80), (80, 45));
    }
}
