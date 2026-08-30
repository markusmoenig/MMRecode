//! YUV4MPEG2 frame input and output.
//!
//! Y4M is treated as a deliberately simple development format rather than a general encoded
//! media container.

use std::io::{BufRead, Write};

use mmrecode_core::{
    ColorDescription, ColorRange, Error, FieldOrder, FrameTiming, PixelFormat, Plane, Rational,
    Result, VideoFrame,
};

/// A parsed Y4M stream header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Y4mHeader {
    /// Frame width.
    pub width: usize,
    /// Frame height.
    pub height: usize,
    /// Pixel storage format.
    pub format: PixelFormat,
    /// Progressive or interlaced field order.
    pub field_order: FieldOrder,
    /// Declared frame rate, when present.
    pub frame_rate: Option<Rational>,
    /// Sample value range, when declared by an extension.
    pub color_range: ColorRange,
}

/// Streaming Y4M reader.
#[derive(Debug)]
pub struct Y4mReader<R> {
    input: R,
    header: Option<Y4mHeader>,
}

impl<R: BufRead> Y4mReader<R> {
    /// Wraps a buffered input source.
    #[must_use]
    pub const fn new(input: R) -> Self {
        Self {
            input,
            header: None,
        }
    }

    /// Returns the wrapped source without consuming further data.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.input
    }

    /// Returns the parsed stream header after the first frame-read attempt.
    #[must_use]
    pub const fn header(&self) -> Option<&Y4mHeader> {
        self.header.as_ref()
    }

    /// Reads the next frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is malformed, truncated, or unsupported.
    pub fn read_frame(&mut self) -> Result<Option<VideoFrame>> {
        if self.header.is_none() {
            self.header = Some(read_stream_header(&mut self.input)?);
        }
        let header = self
            .header
            .clone()
            .ok_or_else(|| Error::InvalidState("Y4M header was not initialized".into()))?;
        let mut frame_header = String::new();
        if self.input.read_line(&mut frame_header)? == 0 {
            return Ok(None);
        }
        if frame_header.trim_end() != "FRAME" {
            return Err(Error::InvalidData(format!(
                "expected Y4M FRAME marker, found {:?}",
                frame_header.trim_end()
            )));
        }

        let dimensions = plane_dimensions(header.format, header.width, header.height)?;
        let mut planes = Vec::with_capacity(dimensions.len());
        for (width, height) in dimensions {
            let byte_count = width.checked_mul(height).ok_or_else(|| {
                Error::InvalidData("Y4M plane dimensions overflow address space".into())
            })?;
            let mut data = vec![0; byte_count];
            self.input.read_exact(&mut data)?;
            planes.push(Plane {
                data,
                stride: width,
                width,
                height,
            });
        }
        Ok(Some(VideoFrame {
            format: header.format,
            width: header.width,
            height: header.height,
            planes,
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: header.color_range,
                primaries: None,
                transfer: None,
                matrix: None,
            },
            field_order: header.field_order,
        }))
    }
}

fn read_stream_header(input: &mut impl BufRead) -> Result<Y4mHeader> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Err(Error::InvalidData("empty Y4M stream".into()));
    }
    let mut tokens = line.split_ascii_whitespace();
    if tokens.next() != Some("YUV4MPEG2") {
        return Err(Error::InvalidData("missing YUV4MPEG2 signature".into()));
    }
    let mut width = None;
    let mut height = None;
    let mut format = PixelFormat::Yuv420p8;
    let mut field_order = FieldOrder::Unspecified;
    let mut frame_rate = None;
    let mut color_range = ColorRange::Unspecified;
    for token in tokens {
        if let Some(value) = token.strip_prefix('W') {
            width = Some(parse_dimension(value, "width")?);
        } else if let Some(value) = token.strip_prefix('H') {
            height = Some(parse_dimension(value, "height")?);
        } else if let Some(value) = token.strip_prefix('I') {
            field_order = parse_interlace(value)?;
        } else if let Some(value) = token.strip_prefix('F') {
            frame_rate = Some(parse_ratio(value, "frame rate")?);
        } else if let Some(value) = token.strip_prefix('C') {
            format = parse_chroma(value)?;
        } else if let Some(value) = token.strip_prefix("XCOLORRANGE=") {
            color_range = parse_color_range(value)?;
        }
    }
    Ok(Y4mHeader {
        width: width.ok_or_else(|| Error::InvalidData("Y4M header has no width".into()))?,
        height: height.ok_or_else(|| Error::InvalidData("Y4M header has no height".into()))?,
        format,
        field_order,
        frame_rate,
        color_range,
    })
}

fn parse_ratio(value: &str, name: &str) -> Result<Rational> {
    let (numerator, denominator) = value
        .split_once(':')
        .ok_or_else(|| Error::InvalidData(format!("invalid Y4M {name} {value:?}")))?;
    let numerator = numerator
        .parse::<i64>()
        .map_err(|_| Error::InvalidData(format!("invalid Y4M {name} {value:?}")))?;
    let denominator = denominator
        .parse::<i64>()
        .map_err(|_| Error::InvalidData(format!("invalid Y4M {name} {value:?}")))?;
    if numerator <= 0 || denominator <= 0 {
        return Err(Error::InvalidData(format!(
            "Y4M {name} must be a positive ratio"
        )));
    }
    Rational::new(numerator, denominator)
}

fn parse_dimension(value: &str, name: &str) -> Result<usize> {
    let dimension = value
        .parse::<usize>()
        .map_err(|_| Error::InvalidData(format!("invalid Y4M {name} {value:?}")))?;
    if dimension == 0 {
        Err(Error::InvalidData(format!("Y4M {name} must be non-zero")))
    } else {
        Ok(dimension)
    }
}

fn parse_chroma(value: &str) -> Result<PixelFormat> {
    match value {
        "mono" => Ok(PixelFormat::Gray8),
        "420jpeg" | "420" => Ok(PixelFormat::Yuv420p8),
        "411" => Ok(PixelFormat::Yuv411p8),
        "422" => Ok(PixelFormat::Yuv422p8),
        "444" => Ok(PixelFormat::Yuv444p8),
        _ => Err(Error::Unsupported(format!(
            "Y4M chroma mode C{value} is not supported"
        ))),
    }
}

fn parse_interlace(value: &str) -> Result<FieldOrder> {
    match value {
        "p" => Ok(FieldOrder::Progressive),
        "t" => Ok(FieldOrder::TopFirst),
        "b" => Ok(FieldOrder::BottomFirst),
        "?" => Ok(FieldOrder::Unspecified),
        _ => Err(Error::Unsupported(format!(
            "Y4M interlace mode I{value} is not supported"
        ))),
    }
}

fn parse_color_range(value: &str) -> Result<ColorRange> {
    match value {
        "FULL" => Ok(ColorRange::Full),
        "LIMITED" => Ok(ColorRange::Limited),
        _ => Err(Error::Unsupported(format!(
            "Y4M color range {value:?} is not supported"
        ))),
    }
}

/// Streaming Y4M writer.
#[derive(Debug)]
pub struct Y4mWriter<W> {
    output: W,
    stream_format: Option<(usize, usize, PixelFormat)>,
}

impl<W: Write> Y4mWriter<W> {
    /// Wraps an output destination.
    #[must_use]
    pub const fn new(output: W) -> Self {
        Self {
            output,
            stream_format: None,
        }
    }

    /// Returns the wrapped destination.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.output
    }

    /// Writes one frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the frame format is unsupported or output fails.
    pub fn write_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let chroma = chroma_tag(frame.format)?;
        validate_planes(frame)?;
        let format = (frame.width, frame.height, frame.format);
        if let Some(stream_format) = self.stream_format {
            if stream_format != format {
                return Err(Error::InvalidData(
                    "Y4M stream dimensions or pixel format changed between frames".into(),
                ));
            }
        } else {
            let color_range = color_range_tag(frame.color.range)?;
            writeln!(
                self.output,
                "YUV4MPEG2 W{} H{} I{} {chroma}{color_range}",
                frame.width,
                frame.height,
                interlace_tag(frame.field_order)?
            )?;
            self.stream_format = Some(format);
        }
        self.output.write_all(b"FRAME\n")?;
        for plane in &frame.planes {
            write_plane(&mut self.output, plane)?;
        }
        Ok(())
    }
}

fn color_range_tag(range: mmrecode_core::ColorRange) -> Result<&'static str> {
    match range {
        mmrecode_core::ColorRange::Full => Ok(" XCOLORRANGE=FULL"),
        mmrecode_core::ColorRange::Limited => Ok(" XCOLORRANGE=LIMITED"),
        mmrecode_core::ColorRange::Unspecified => Ok(""),
        _ => Err(Error::Unsupported(
            "color range has no Y4M representation".into(),
        )),
    }
}

fn chroma_tag(format: PixelFormat) -> Result<&'static str> {
    match format {
        PixelFormat::Gray8 => Ok("Cmono"),
        PixelFormat::Yuv420p8 => Ok("C420jpeg"),
        PixelFormat::Yuv411p8 => Ok("C411"),
        PixelFormat::Yuv422p8 => Ok("C422"),
        PixelFormat::Yuv444p8 => Ok("C444"),
        PixelFormat::Rgb24 => Err(Error::Unsupported(
            "Y4M does not support the shared packed RGB format".into(),
        )),
        _ => Err(Error::Unsupported(
            "pixel format has no Y4M representation".into(),
        )),
    }
}

fn interlace_tag(field_order: mmrecode_core::FieldOrder) -> Result<char> {
    match field_order {
        mmrecode_core::FieldOrder::Progressive => Ok('p'),
        mmrecode_core::FieldOrder::TopFirst => Ok('t'),
        mmrecode_core::FieldOrder::BottomFirst => Ok('b'),
        mmrecode_core::FieldOrder::Unspecified => Err(Error::InvalidData(
            "Y4M output requires a known field order".into(),
        )),
        _ => Err(Error::Unsupported(
            "field order has no Y4M representation".into(),
        )),
    }
}

fn validate_planes(frame: &VideoFrame) -> Result<()> {
    let expected = plane_dimensions(frame.format, frame.width, frame.height)?;
    if frame.planes.len() != expected.len() {
        return Err(Error::InvalidData(format!(
            "Y4M {:?} frame requires {} plane(s), found {}",
            frame.format,
            expected.len(),
            frame.planes.len()
        )));
    }
    for (index, (plane, &(width, height))) in frame.planes.iter().zip(&expected).enumerate() {
        if (plane.width, plane.height) != (width, height)
            || plane.stride < width
            || plane.data.len() < plane.stride * height
        {
            return Err(Error::InvalidData(format!(
                "Y4M plane {index} has inconsistent dimensions, stride, or storage"
            )));
        }
    }
    Ok(())
}

fn plane_dimensions(
    format: PixelFormat,
    width: usize,
    height: usize,
) -> Result<Vec<(usize, usize)>> {
    let half_width = width.div_ceil(2);
    let quarter_width = width.div_ceil(4);
    let half_height = height.div_ceil(2);
    match format {
        PixelFormat::Gray8 => Ok(vec![(width, height)]),
        PixelFormat::Yuv420p8 => Ok(vec![
            (width, height),
            (half_width, half_height),
            (half_width, half_height),
        ]),
        PixelFormat::Yuv411p8 => Ok(vec![
            (width, height),
            (quarter_width, height),
            (quarter_width, height),
        ]),
        PixelFormat::Yuv422p8 => Ok(vec![
            (width, height),
            (half_width, height),
            (half_width, height),
        ]),
        PixelFormat::Yuv444p8 => Ok(vec![(width, height); 3]),
        PixelFormat::Rgb24 => Err(Error::Unsupported(
            "Y4M does not support the shared packed RGB format".into(),
        )),
        _ => Err(Error::Unsupported(
            "pixel format has no Y4M representation".into(),
        )),
    }
}

fn write_plane(output: &mut impl Write, plane: &Plane) -> Result<()> {
    for row in 0..plane.height {
        let start = row * plane.stride;
        output.write_all(&plane.data[start..start + plane.width])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{
        ColorDescription, ColorRange, FieldOrder, FrameTiming, PixelFormat, Plane, VideoFrame,
    };

    use super::{Y4mReader, Y4mWriter};

    #[test]
    fn writes_a_grayscale_frame() {
        let frame = VideoFrame {
            format: PixelFormat::Gray8,
            width: 2,
            height: 2,
            planes: vec![Plane {
                data: vec![1, 2, 3, 4],
                stride: 2,
                width: 2,
                height: 2,
            }],
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Full,
                ..ColorDescription::default()
            },
            field_order: FieldOrder::Progressive,
        };
        let mut writer = Y4mWriter::new(Vec::new());
        writer.write_frame(&frame).expect("write frame");
        let bytes = writer.into_inner();
        assert_eq!(
            bytes,
            b"YUV4MPEG2 W2 H2 Ip Cmono XCOLORRANGE=FULL\nFRAME\n\x01\x02\x03\x04"
        );
        let mut reader = Y4mReader::new(std::io::Cursor::new(bytes));
        assert_eq!(reader.read_frame().unwrap(), Some(frame));
        assert_eq!(reader.read_frame().unwrap(), None);
    }

    #[test]
    fn reads_multiple_frames() {
        let bytes = b"YUV4MPEG2 W2 H1 F30000:1001 Ip C444 XCOLORRANGE=LIMITED\n\
                      FRAME\n\x01\x02\x03\x04\x05\x06\
                      FRAME\n\x07\x08\x09\x0a\x0b\x0c";
        let mut reader = Y4mReader::new(std::io::Cursor::new(bytes));
        let first = reader.read_frame().unwrap().expect("first frame");
        let second = reader.read_frame().unwrap().expect("second frame");
        assert_eq!(first.planes[0].data, [1, 2]);
        assert_eq!(second.planes[2].data, [11, 12]);
        assert_eq!(second.color.range, ColorRange::Limited);
        assert_eq!(
            reader.header().unwrap().frame_rate,
            Some(mmrecode_core::Rational::new(30_000, 1_001).unwrap())
        );
        assert!(reader.read_frame().unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_frame_rates() {
        for rate in ["0:1", "25:0", "25", "abc:1"] {
            let stream = format!("YUV4MPEG2 W2 H1 F{rate} Ip Cmono\nFRAME\n\0\0");
            let mut reader = Y4mReader::new(std::io::Cursor::new(stream.into_bytes()));
            assert!(reader.read_frame().is_err(), "accepted {rate}");
        }
    }
}
