//! CPU-authoritative visual placement scaling for rendered frames.

use image::{GrayImage, imageops::FilterType};
use mmrecode_core::{ColorRange, Error, PixelFormat, Plane, Result, VideoFrame};
use mmrecode_edit::VisualScaleMode;

/// Maps one progressive planar 4:2:0 frame into an even-sized project canvas.
///
/// `Fit` and `Fill` preserve coded-pixel aspect ratio, `Stretch` uses the complete canvas, and
/// `Native` performs centered crop/padding without resizing. Lanczos3 is the normative first CPU
/// scaling filter. Padding is black in the frame's declared component range.
///
/// # Errors
///
/// Returns an error for non-Yuv420p8 input, malformed planes, odd/zero dimensions, oversized image
/// buffers, or an image-resize failure.
pub fn scale_yuv420_to_canvas(
    frame: &VideoFrame,
    canvas_width: usize,
    canvas_height: usize,
    mode: VisualScaleMode,
) -> Result<VideoFrame> {
    validate_frame(frame)?;
    if canvas_width == 0
        || canvas_height == 0
        || !canvas_width.is_multiple_of(2)
        || !canvas_height.is_multiple_of(2)
    {
        return Err(Error::Unsupported(
            "Yuv420p8 project canvas dimensions must be positive and even".into(),
        ));
    }
    let (scaled_width, scaled_height) =
        scaled_dimensions(frame.width, frame.height, canvas_width, canvas_height, mode);
    let luma_black = match frame.color.range {
        ColorRange::Limited => 16,
        _ => 0,
    };
    let mut planes = Vec::with_capacity(3);
    for (index, plane) in frame.planes.iter().enumerate() {
        let divisor = if index == 0 { 1 } else { 2 };
        let source = tight_image(plane)?;
        let scaled =
            if (scaled_width / divisor, scaled_height / divisor) == (plane.width, plane.height) {
                source
            } else {
                image::imageops::resize(
                    &source,
                    u32::try_from(scaled_width / divisor).map_err(integer_error)?,
                    u32::try_from(scaled_height / divisor).map_err(integer_error)?,
                    FilterType::Lanczos3,
                )
            };
        let width = canvas_width / divisor;
        let height = canvas_height / divisor;
        let background = if index == 0 { luma_black } else { 128 };
        let data = center_crop_or_pad(&scaled, width, height, background)?;
        planes.push(Plane {
            data,
            stride: width,
            width,
            height,
        });
    }
    Ok(VideoFrame {
        format: PixelFormat::Yuv420p8,
        width: canvas_width,
        height: canvas_height,
        planes,
        timing: frame.timing,
        color: frame.color.clone(),
        field_order: frame.field_order,
    })
}

fn validate_frame(frame: &VideoFrame) -> Result<()> {
    if frame.format != PixelFormat::Yuv420p8
        || frame.width == 0
        || frame.height == 0
        || !frame.width.is_multiple_of(2)
        || !frame.height.is_multiple_of(2)
        || frame.planes.len() != 3
    {
        return Err(Error::Unsupported(
            "visual scaling requires even-sized Yuv420p8 input".into(),
        ));
    }
    for (index, plane) in frame.planes.iter().enumerate() {
        let divisor = if index == 0 { 1 } else { 2 };
        let expected = (frame.width / divisor, frame.height / divisor);
        if (plane.width, plane.height) != expected
            || plane.stride < plane.width
            || plane.data.len() < plane.stride.saturating_mul(plane.height)
        {
            return Err(Error::InvalidData(format!(
                "visual scaling received malformed Yuv420p8 plane {index}"
            )));
        }
    }
    Ok(())
}

fn scaled_dimensions(
    source_width: usize,
    source_height: usize,
    canvas_width: usize,
    canvas_height: usize,
    mode: VisualScaleMode,
) -> (usize, usize) {
    if mode == VisualScaleMode::Stretch {
        return (canvas_width, canvas_height);
    }
    if mode == VisualScaleMode::Native {
        return (source_width, source_height);
    }
    let width_limited = (canvas_width as u128).saturating_mul(source_height as u128)
        <= (canvas_height as u128).saturating_mul(source_width as u128);
    let use_width = if mode == VisualScaleMode::Fill {
        !width_limited
    } else {
        width_limited
    };
    if use_width {
        (
            canvas_width,
            scaled_even(source_height, canvas_width, source_width),
        )
    } else {
        (
            scaled_even(source_width, canvas_height, source_height),
            canvas_height,
        )
    }
}

fn scaled_even(source: usize, target: usize, denominator: usize) -> usize {
    let numerator = (source as u128).saturating_mul(target as u128);
    let two_denominator = (denominator as u128).saturating_mul(2);
    let rounded_units = numerator
        .saturating_add(denominator as u128)
        .checked_div(two_denominator)
        .unwrap_or(u128::MAX);
    usize::try_from(rounded_units.saturating_mul(2))
        .unwrap_or(usize::MAX)
        .max(2)
}

fn tight_image(plane: &Plane) -> Result<GrayImage> {
    let mut data = Vec::with_capacity(
        plane
            .width
            .checked_mul(plane.height)
            .ok_or_else(|| Error::InvalidData("visual plane size overflows".into()))?,
    );
    for row in 0..plane.height {
        let start = row
            .checked_mul(plane.stride)
            .ok_or_else(|| Error::InvalidData("visual plane row offset overflows".into()))?;
        data.extend_from_slice(&plane.data[start..start + plane.width]);
    }
    GrayImage::from_raw(
        u32::try_from(plane.width).map_err(integer_error)?,
        u32::try_from(plane.height).map_err(integer_error)?,
        data,
    )
    .ok_or_else(|| Error::InvalidData("visual plane storage does not match its dimensions".into()))
}

fn center_crop_or_pad(
    source: &GrayImage,
    width: usize,
    height: usize,
    background: u8,
) -> Result<Vec<u8>> {
    let source_width = usize::try_from(source.width()).map_err(integer_error)?;
    let source_height = usize::try_from(source.height()).map_err(integer_error)?;
    let copy_width = source_width.min(width);
    let copy_height = source_height.min(height);
    let source_x = (source_width - copy_width) / 2;
    let source_y = (source_height - copy_height) / 2;
    let destination_x = (width - copy_width) / 2;
    let destination_y = (height - copy_height) / 2;
    let mut destination = vec![
        background;
        width.checked_mul(height).ok_or_else(|| Error::InvalidData(
            "project canvas size overflows".into()
        ))?
    ];
    let source = source.as_raw();
    for row in 0..copy_height {
        let source_start = (source_y + row) * source_width + source_x;
        let destination_start = (destination_y + row) * width + destination_x;
        destination[destination_start..destination_start + copy_width]
            .copy_from_slice(&source[source_start..source_start + copy_width]);
    }
    Ok(destination)
}

fn integer_error(error: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("visual dimension conversion failed: {error}"))
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{ColorDescription, FieldOrder, FrameTiming};

    use super::*;

    fn frame(width: usize, height: usize) -> VideoFrame {
        VideoFrame {
            format: PixelFormat::Yuv420p8,
            width,
            height,
            planes: vec![
                Plane {
                    data: vec![200; width * height],
                    stride: width,
                    width,
                    height,
                },
                Plane {
                    data: vec![100; width * height / 4],
                    stride: width / 2,
                    width: width / 2,
                    height: height / 2,
                },
                Plane {
                    data: vec![150; width * height / 4],
                    stride: width / 2,
                    width: width / 2,
                    height: height / 2,
                },
            ],
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Limited,
                ..ColorDescription::default()
            },
            field_order: FieldOrder::Progressive,
        }
    }

    #[test]
    fn fit_preserves_aspect_and_adds_black_bars() {
        let scaled = scale_yuv420_to_canvas(&frame(4, 4), 8, 4, VisualScaleMode::Fit).unwrap();
        assert_eq!((scaled.width, scaled.height), (8, 4));
        assert_eq!(&scaled.planes[0].data[0..2], &[16, 16]);
        assert_eq!(&scaled.planes[0].data[2..6], &[200; 4]);
        assert_eq!(&scaled.planes[0].data[6..8], &[16, 16]);
        assert_eq!(scaled.planes[1].data[0], 128);
    }

    #[test]
    fn stretch_and_native_have_explicit_canvas_behavior() {
        let stretched =
            scale_yuv420_to_canvas(&frame(4, 4), 8, 4, VisualScaleMode::Stretch).unwrap();
        assert!(stretched.planes[0].data.iter().all(|sample| *sample == 200));
        let native = scale_yuv420_to_canvas(&frame(4, 4), 8, 4, VisualScaleMode::Native).unwrap();
        assert_eq!(&native.planes[0].data[0..2], &[16, 16]);
        assert_eq!(&native.planes[0].data[2..6], &[200; 4]);
    }
}
