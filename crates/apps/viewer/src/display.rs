use eframe::egui::{Color32, ColorImage};
use mmrecode_core::{ColorRange, PixelFormat, Plane, VideoFrame};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplayMode {
    Composite,
    Luma,
    ChromaBlue,
    ChromaRed,
}

impl DisplayMode {
    pub(crate) const ALL: [Self; 4] = [
        Self::Composite,
        Self::Luma,
        Self::ChromaBlue,
        Self::ChromaRed,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Composite => "Image",
            Self::Luma => "Y",
            Self::ChromaBlue => "Cb",
            Self::ChromaRed => "Cr",
        }
    }
}

pub(crate) fn color_image(frame: &VideoFrame, mode: DisplayMode) -> Result<ColorImage, String> {
    match mode {
        DisplayMode::Composite => composite_image(frame),
        DisplayMode::Luma => plane_image(frame, 0, "Y"),
        DisplayMode::ChromaBlue => plane_image(frame, 1, "Cb"),
        DisplayMode::ChromaRed => plane_image(frame, 2, "Cr"),
    }
}

pub(crate) fn dimensions(frame: &VideoFrame, mode: DisplayMode) -> Option<(usize, usize)> {
    match mode {
        DisplayMode::Composite => Some((frame.width, frame.height)),
        DisplayMode::Luma => frame
            .planes
            .first()
            .map(|plane| (plane.width, plane.height)),
        DisplayMode::ChromaBlue => frame.planes.get(1).map(|plane| (plane.width, plane.height)),
        DisplayMode::ChromaRed => frame.planes.get(2).map(|plane| (plane.width, plane.height)),
    }
}

pub(crate) fn pixel_description(
    frame: &VideoFrame,
    mode: DisplayMode,
    x: usize,
    y: usize,
) -> String {
    match mode {
        DisplayMode::Composite => {
            if frame.format == PixelFormat::Rgb24 {
                let plane = &frame.planes[0];
                let offset = y * plane.stride + x * 3;
                return format!(
                    "DIF block {x}, sequence {y}  RGB {}, {}, {}",
                    plane.data[offset],
                    plane.data[offset + 1],
                    plane.data[offset + 2]
                );
            }
            let y_value = sample_scaled(&frame.planes[0], x, y, frame.width, frame.height);
            if frame.planes.len() == 1 {
                return format!("x {x}, y {y}  Y {y_value}");
            }
            let cb = sample_scaled(&frame.planes[1], x, y, frame.width, frame.height);
            let cr = sample_scaled(&frame.planes[2], x, y, frame.width, frame.height);
            format!("x {x}, y {y}  Y {y_value}  Cb {cb}  Cr {cr}")
        }
        DisplayMode::Luma => plane_description(&frame.planes[0], "Y", x, y),
        DisplayMode::ChromaBlue => plane_description(&frame.planes[1], "Cb", x, y),
        DisplayMode::ChromaRed => plane_description(&frame.planes[2], "Cr", x, y),
    }
}

fn composite_image(frame: &VideoFrame) -> Result<ColorImage, String> {
    validate_dimensions(frame)?;
    let mut pixels = Vec::with_capacity(frame.width * frame.height);
    match frame.format {
        PixelFormat::Gray8 => {
            validate_plane(&frame.planes[0], "Y")?;
            for y in 0..frame.height {
                for x in 0..frame.width {
                    let value = sample(&frame.planes[0], x, y);
                    pixels.push(Color32::from_gray(value));
                }
            }
        }
        PixelFormat::Yuv420p8
        | PixelFormat::Yuv411p8
        | PixelFormat::Yuv422p8
        | PixelFormat::Yuv444p8 => {
            let names = ["Y", "Cb", "Cr"];
            if frame.planes.len() != names.len() {
                return Err(format!(
                    "{:?} requires three planes, found {}",
                    frame.format,
                    frame.planes.len()
                ));
            }
            for (plane, name) in frame.planes.iter().zip(names) {
                validate_plane(plane, name)?;
            }
            for y in 0..frame.height {
                for x in 0..frame.width {
                    let luma = sample_scaled(&frame.planes[0], x, y, frame.width, frame.height);
                    let cb = sample_scaled(&frame.planes[1], x, y, frame.width, frame.height);
                    let cr = sample_scaled(&frame.planes[2], x, y, frame.width, frame.height);
                    pixels.push(ycbcr_to_rgb(luma, cb, cr, frame.color.range));
                }
            }
        }
        PixelFormat::Rgb24 => {
            let plane = &frame.planes[0];
            if plane.stride < frame.width * 3 || plane.height < frame.height {
                return Err("packed RGB plane has an invalid layout".into());
            }
            for y in 0..frame.height {
                for x in 0..frame.width {
                    let offset = y * plane.stride + x * 3;
                    let values = plane
                        .data
                        .get(offset..offset + 3)
                        .ok_or_else(|| "packed RGB plane is truncated".to_owned())?;
                    pixels.push(Color32::from_rgb(values[0], values[1], values[2]));
                }
            }
        }
        _ => return Err("pixel format is not supported by the viewer".into()),
    }
    Ok(ColorImage::new([frame.width, frame.height], pixels))
}

fn plane_image(frame: &VideoFrame, index: usize, name: &str) -> Result<ColorImage, String> {
    let plane = frame
        .planes
        .get(index)
        .ok_or_else(|| format!("frame has no {name} plane"))?;
    validate_plane(plane, name)?;
    let mut pixels = Vec::with_capacity(plane.width * plane.height);
    for y in 0..plane.height {
        for x in 0..plane.width {
            pixels.push(Color32::from_gray(sample(plane, x, y)));
        }
    }
    Ok(ColorImage::new([plane.width, plane.height], pixels))
}

fn validate_dimensions(frame: &VideoFrame) -> Result<(), String> {
    if frame.width == 0 || frame.height == 0 {
        return Err("frame dimensions must be nonzero".into());
    }
    frame
        .width
        .checked_mul(frame.height)
        .ok_or_else(|| "frame dimensions overflow address space".to_owned())?;
    if frame.planes.is_empty() {
        return Err("frame has no planes".into());
    }
    Ok(())
}

fn validate_plane(plane: &Plane, name: &str) -> Result<(), String> {
    if plane.width == 0 || plane.height == 0 || plane.stride < plane.width {
        return Err(format!("{name} plane has an invalid layout"));
    }
    let required = (plane.height - 1)
        .checked_mul(plane.stride)
        .and_then(|offset| offset.checked_add(plane.width))
        .ok_or_else(|| format!("{name} plane layout overflows address space"))?;
    if plane.data.len() < required {
        return Err(format!(
            "{name} plane needs {required} bytes, found {}",
            plane.data.len()
        ));
    }
    Ok(())
}

fn sample(plane: &Plane, x: usize, y: usize) -> u8 {
    plane.data[y * plane.stride + x]
}

fn sample_scaled(
    plane: &Plane,
    x: usize,
    y: usize,
    source_width: usize,
    source_height: usize,
) -> u8 {
    let plane_x = (x * plane.width / source_width).min(plane.width - 1);
    let plane_y = (y * plane.height / source_height).min(plane.height - 1);
    sample(plane, plane_x, plane_y)
}

fn plane_description(plane: &Plane, name: &str, x: usize, y: usize) -> String {
    format!("{name} x {x}, y {y}: {}", sample(plane, x, y))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8, range: ColorRange) -> Color32 {
    let (luma, blue_difference, red_difference) = if range == ColorRange::Limited {
        (
            (f32::from(y) - 16.0) * (255.0 / 219.0),
            (f32::from(cb) - 128.0) * (255.0 / 224.0),
            (f32::from(cr) - 128.0) * (255.0 / 224.0),
        )
    } else {
        (f32::from(y), f32::from(cb) - 128.0, f32::from(cr) - 128.0)
    };
    let red = (luma + 1.402 * red_difference).round().clamp(0.0, 255.0) as u8;
    let green = (luma - 0.344_136 * blue_difference - 0.714_136 * red_difference)
        .round()
        .clamp(0.0, 255.0) as u8;
    let blue = (luma + 1.772 * blue_difference).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(red, green, blue)
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{
        ColorDescription, FieldOrder, FrameTiming, PixelFormat, Plane, VideoFrame,
    };

    use super::*;

    fn frame(format: PixelFormat, planes: Vec<Plane>) -> VideoFrame {
        VideoFrame {
            format,
            width: 2,
            height: 1,
            planes,
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Full,
                primaries: None,
                transfer: None,
                matrix: None,
            },
            field_order: FieldOrder::Progressive,
        }
    }

    #[test]
    fn grayscale_conversion_preserves_samples() {
        let frame = frame(
            PixelFormat::Gray8,
            vec![Plane {
                data: vec![17, 231],
                stride: 2,
                width: 2,
                height: 1,
            }],
        );
        let image = color_image(&frame, DisplayMode::Composite).expect("valid grayscale");
        assert_eq!(
            image.pixels,
            [Color32::from_gray(17), Color32::from_gray(231)]
        );
    }

    #[test]
    fn neutral_chroma_preserves_luma() {
        let frame = frame(
            PixelFormat::Yuv444p8,
            vec![
                Plane {
                    data: vec![40, 200],
                    stride: 2,
                    width: 2,
                    height: 1,
                },
                Plane {
                    data: vec![128, 128],
                    stride: 2,
                    width: 2,
                    height: 1,
                },
                Plane {
                    data: vec![128, 128],
                    stride: 2,
                    width: 2,
                    height: 1,
                },
            ],
        );
        let image = color_image(&frame, DisplayMode::Composite).expect("valid YCbCr");
        assert_eq!(
            image.pixels,
            [Color32::from_gray(40), Color32::from_gray(200)]
        );
    }
}
