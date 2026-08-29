use mmrecode_core::{
    ColorDescription, ColorRange, Error, FieldOrder, FrameTiming, PixelFormat, Plane, Result,
    VideoFrame,
};

use crate::{
    FrameHeader, HuffmanTableClass, JpegImage, QuantizationPrecision, QuantizationTable,
    ScanHeader, SegmentData, entropy::EntropyReader, entropy::HuffmanDecoder,
    entropy::receive_extend, parse_jpeg, tables::ZIGZAG_TO_NATURAL, transform::inverse_dct,
};

const BLOCK_SIDE: usize = 8;

/// One reconstructed JPEG component plane at its encoded sampling resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedComponent {
    /// Component identifier from SOF0.
    pub id: u8,
    /// Horizontal sampling factor.
    pub horizontal_sampling: u8,
    /// Vertical sampling factor.
    pub vertical_sampling: u8,
    /// Visible component width in samples.
    pub width: usize,
    /// Visible component height in samples.
    pub height: usize,
    /// Row-major eight-bit reconstructed samples.
    pub samples: Vec<u8>,
}

/// Color interpretation inferred from JPEG application markers and component count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JpegColorModel {
    /// One luminance component.
    Grayscale,
    /// Three Y, Cb, and Cr components.
    YCbCr,
    /// Three red, green, and blue components.
    Rgb,
    /// Four cyan, magenta, yellow, and black components.
    Cmyk,
    /// Four Y, Cb, Cr, and black components.
    Ycck,
    /// The stream does not provide enough information for a safe interpretation.
    Unknown,
}

/// A baseline JPEG reconstructed into its native component planes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedJpeg {
    /// Visible image width in pixels.
    pub width: usize,
    /// Visible image height in pixels.
    pub height: usize,
    /// Inferred interpretation of the component samples.
    pub color_model: JpegColorModel,
    /// Reconstructed components in SOF0 order.
    pub components: Vec<DecodedComponent>,
}

impl DecodedJpeg {
    /// Converts native JPEG planes into a shared frame when their sampling has
    /// a direct grayscale, 4:2:0, 4:2:2, or 4:4:4 representation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] for unusual component counts or sampling
    /// arrangements that require an explicit color conversion step.
    pub fn into_video_frame(self) -> Result<VideoFrame> {
        let format = self.standard_pixel_format()?;
        let planes = self
            .components
            .into_iter()
            .map(|component| Plane {
                data: component.samples,
                stride: component.width,
                width: component.width,
                height: component.height,
            })
            .collect();
        Ok(VideoFrame {
            format,
            width: self.width,
            height: self.height,
            planes,
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Full,
                primaries: None,
                transfer: None,
                matrix: None,
            },
            field_order: FieldOrder::Progressive,
        })
    }

    fn standard_pixel_format(&self) -> Result<PixelFormat> {
        if self.color_model == JpegColorModel::Grayscale && self.components.len() == 1 {
            let component = &self.components[0];
            if (component.width, component.height) == (self.width, self.height) {
                return Ok(PixelFormat::Gray8);
            }
        }
        if self.color_model != JpegColorModel::YCbCr
            || self.components.len() != 3
            || (self.components[0].width, self.components[0].height) != (self.width, self.height)
            || (self.components[1].width, self.components[1].height)
                != (self.components[2].width, self.components[2].height)
        {
            return Err(Error::Unsupported(
                "JPEG components do not map directly to a shared planar pixel format".into(),
            ));
        }
        let chroma_dimensions = (self.components[1].width, self.components[1].height);
        if chroma_dimensions == (self.width, self.height) {
            Ok(PixelFormat::Yuv444p8)
        } else if chroma_dimensions == (self.width.div_ceil(2), self.height) {
            Ok(PixelFormat::Yuv422p8)
        } else if chroma_dimensions == (self.width.div_ceil(2), self.height.div_ceil(2)) {
            Ok(PixelFormat::Yuv420p8)
        } else {
            Err(Error::Unsupported(
                "JPEG chroma sampling is not 4:2:0, 4:2:2, or 4:4:4".into(),
            ))
        }
    }
}

/// Decodes a constrained baseline sequential JPEG into planar component samples.
///
/// The current decoder accepts an eight-bit SOF0 image with one interleaved scan
/// containing every frame component. Components remain at their encoded sampling
/// resolution; no chroma upsampling or YCbCr-to-RGB conversion is performed.
///
/// # Errors
///
/// Returns [`Error::InvalidData`] for malformed syntax or entropy data and
/// [`Error::Unsupported`] when the JPEG uses a feature outside the constrained
/// baseline subset.
pub fn decode_jpeg(input: &[u8]) -> Result<DecodedJpeg> {
    let image = parse_jpeg(input)?;
    let frame = image
        .frame_header()
        .ok_or_else(|| Error::InvalidData("JPEG has no baseline SOF0 frame header".into()))?;
    let (scan, scan_offset) = single_scan(&image)?;
    validate_baseline_subset(frame, scan)?;
    let color_model = infer_color_model(&image, frame);

    let entropy = &image.entropy_scans[0];
    let entropy_end = entropy
        .data_offset
        .checked_add(entropy.data_length)
        .ok_or_else(|| Error::InvalidData("JPEG entropy range overflows address space".into()))?;
    let entropy_bytes = input.get(entropy.data_offset..entropy_end).ok_or_else(|| {
        Error::InvalidData("JPEG entropy range lies outside the source buffer".into())
    })?;

    let tables = DecodeTables::collect(&image, scan_offset)?;
    reconstruct(
        frame,
        scan,
        entropy_bytes,
        entropy.data_offset,
        &tables,
        color_model,
    )
}

fn infer_color_model(image: &JpegImage, frame: &FrameHeader) -> JpegColorModel {
    if frame.components.len() == 1 {
        return JpegColorModel::Grayscale;
    }
    let adobe_transform = image
        .segments
        .iter()
        .find_map(|segment| match &segment.data {
            SegmentData::Application(application)
                if application.number == 14
                    && application.data.len() >= 12
                    && application.data.starts_with(b"Adobe") =>
            {
                Some(application.data[11])
            }
            _ => None,
        });
    match (frame.components.len(), adobe_transform) {
        (3, Some(0)) => JpegColorModel::Rgb,
        (3, Some(1)) => JpegColorModel::YCbCr,
        (4, Some(0)) => JpegColorModel::Cmyk,
        (4, Some(2)) => JpegColorModel::Ycck,
        (3, None) if image.jfif_header().is_some() => JpegColorModel::YCbCr,
        _ => JpegColorModel::Unknown,
    }
}

fn single_scan(image: &JpegImage) -> Result<(&ScanHeader, usize)> {
    if image.entropy_scans.len() != 1 {
        return Err(Error::Unsupported(format!(
            "baseline decoder currently requires exactly one scan, found {}",
            image.entropy_scans.len()
        )));
    }
    let mut scans = image
        .segments
        .iter()
        .filter_map(|segment| match &segment.data {
            SegmentData::Scan(scan) => Some((scan, segment.offset)),
            _ => None,
        });
    let first = scans
        .next()
        .ok_or_else(|| Error::InvalidData("JPEG has entropy data without an SOS header".into()))?;
    if scans.next().is_some() {
        return Err(Error::Unsupported(
            "baseline decoder currently supports only one SOS header".into(),
        ));
    }
    Ok(first)
}

fn validate_baseline_subset(frame: &FrameHeader, scan: &ScanHeader) -> Result<()> {
    if frame.height == 0 {
        return Err(Error::Unsupported(
            "JPEG height supplied by a DNL marker is not supported yet".into(),
        ));
    }
    if scan.components.len() != frame.components.len() {
        return Err(Error::Unsupported(
            "non-interleaved or partial-component JPEG scans are not supported yet".into(),
        ));
    }
    if scan.spectral_start != 0
        || scan.spectral_end != 63
        || scan.successive_approximation_high != 0
        || scan.successive_approximation_low != 0
    {
        return Err(Error::Unsupported(
            "scan parameters are not baseline sequential".into(),
        ));
    }
    for component in &frame.components {
        let occurrences = scan
            .components
            .iter()
            .filter(|scan_component| scan_component.selector == component.id)
            .count();
        if occurrences != 1 {
            return Err(Error::InvalidData(format!(
                "JPEG scan must select frame component {} exactly once",
                component.id
            )));
        }
    }
    Ok(())
}

struct DecodeTables {
    quantization: [Option<QuantizationTable>; 4],
    dc: [Option<HuffmanDecoder>; 4],
    ac: [Option<HuffmanDecoder>; 4],
    restart_interval: u16,
}

impl DecodeTables {
    fn collect(image: &JpegImage, scan_offset: usize) -> Result<Self> {
        let mut tables = Self {
            quantization: std::array::from_fn(|_| None),
            dc: std::array::from_fn(|_| None),
            ac: std::array::from_fn(|_| None),
            restart_interval: 0,
        };
        for segment in image
            .segments
            .iter()
            .take_while(|segment| segment.offset < scan_offset)
        {
            match &segment.data {
                SegmentData::QuantizationTables(definitions) => {
                    for table in definitions {
                        tables.quantization[usize::from(table.id)] = Some(table.clone());
                    }
                }
                SegmentData::HuffmanTables(definitions) => {
                    for table in definitions {
                        let decoder = HuffmanDecoder::new(table)?;
                        let destination = match table.class {
                            HuffmanTableClass::Dc => &mut tables.dc,
                            HuffmanTableClass::Ac => &mut tables.ac,
                        };
                        destination[usize::from(table.id)] = Some(decoder);
                    }
                }
                SegmentData::RestartInterval(interval) => tables.restart_interval = *interval,
                _ => {}
            }
        }
        Ok(tables)
    }
}

struct ComponentState {
    plane: DecodedComponent,
    quantization: QuantizationTable,
    dc_prediction: i32,
}

struct ScanDecoder {
    component_index: usize,
    dc: HuffmanDecoder,
    ac: HuffmanDecoder,
}

fn reconstruct(
    frame: &FrameHeader,
    scan: &ScanHeader,
    entropy: &[u8],
    entropy_offset: usize,
    tables: &DecodeTables,
    color_model: JpegColorModel,
) -> Result<DecodedJpeg> {
    let width = usize::from(frame.width);
    let height = usize::from(frame.height);
    let maximum_horizontal = frame
        .components
        .iter()
        .map(|component| component.horizontal_sampling)
        .max()
        .expect("SOF0 parser requires components");
    let maximum_vertical = frame
        .components
        .iter()
        .map(|component| component.vertical_sampling)
        .max()
        .expect("SOF0 parser requires components");

    let mut states = make_component_states(
        frame,
        width,
        height,
        maximum_horizontal,
        maximum_vertical,
        tables,
    )?;
    let decoders = make_scan_decoders(frame, scan, tables)?;
    let interleaved = frame.components.len() > 1;
    let mcu_width = if interleaved {
        usize::from(maximum_horizontal) * BLOCK_SIDE
    } else {
        BLOCK_SIDE
    };
    let mcu_height = if interleaved {
        usize::from(maximum_vertical) * BLOCK_SIDE
    } else {
        BLOCK_SIDE
    };
    let mcu_columns = width.div_ceil(mcu_width);
    let mcu_rows = height.div_ceil(mcu_height);
    let mut reader = EntropyReader::new(entropy, entropy_offset);
    let mut restart_number = 0_u8;

    for mcu_index in 0..mcu_columns * mcu_rows {
        if mcu_index > 0
            && tables.restart_interval != 0
            && mcu_index.is_multiple_of(usize::from(tables.restart_interval))
        {
            reader.consume_restart(restart_number)?;
            restart_number = (restart_number + 1) & 7;
            for state in &mut states {
                state.dc_prediction = 0;
            }
        }
        let mcu_x = mcu_index % mcu_columns;
        let mcu_y = mcu_index / mcu_columns;
        for decoder in &decoders {
            let state = &mut states[decoder.component_index];
            let block_rows = if interleaved {
                usize::from(state.plane.vertical_sampling)
            } else {
                1
            };
            let block_columns = if interleaved {
                usize::from(state.plane.horizontal_sampling)
            } else {
                1
            };
            for block_y in 0..block_rows {
                for block_x in 0..block_columns {
                    let coefficients = decode_block(
                        &mut reader,
                        &decoder.dc,
                        &decoder.ac,
                        &state.quantization,
                        &mut state.dc_prediction,
                    )?;
                    let samples = inverse_dct(&coefficients);
                    let destination_x = (mcu_x * block_columns + block_x) * BLOCK_SIDE;
                    let destination_y = (mcu_y * block_rows + block_y) * BLOCK_SIDE;
                    write_block(&mut state.plane, destination_x, destination_y, &samples);
                }
            }
        }
    }

    Ok(DecodedJpeg {
        width,
        height,
        color_model,
        components: states.into_iter().map(|state| state.plane).collect(),
    })
}

fn make_component_states(
    frame: &FrameHeader,
    width: usize,
    height: usize,
    maximum_horizontal: u8,
    maximum_vertical: u8,
    tables: &DecodeTables,
) -> Result<Vec<ComponentState>> {
    frame
        .components
        .iter()
        .map(|component| {
            let quantization = tables.quantization[usize::from(component.quantization_table)]
                .clone()
                .ok_or_else(|| {
                    Error::InvalidData(format!(
                        "JPEG component {} references missing quantization table {}",
                        component.id, component.quantization_table
                    ))
                })?;
            if quantization.precision != QuantizationPrecision::EightBit {
                return Err(Error::Unsupported(
                    "16-bit quantization tables are not allowed in baseline JPEG".into(),
                ));
            }
            let component_width = (width * usize::from(component.horizontal_sampling))
                .div_ceil(usize::from(maximum_horizontal));
            let component_height = (height * usize::from(component.vertical_sampling))
                .div_ceil(usize::from(maximum_vertical));
            Ok(ComponentState {
                plane: DecodedComponent {
                    id: component.id,
                    horizontal_sampling: component.horizontal_sampling,
                    vertical_sampling: component.vertical_sampling,
                    width: component_width,
                    height: component_height,
                    samples: vec![0; component_width * component_height],
                },
                quantization,
                dc_prediction: 0,
            })
        })
        .collect()
}

fn make_scan_decoders(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &DecodeTables,
) -> Result<Vec<ScanDecoder>> {
    scan.components
        .iter()
        .map(|component| {
            let component_index = frame
                .components
                .iter()
                .position(|frame_component| frame_component.id == component.selector)
                .expect("baseline subset validation matches scan components");
            let dc = tables.dc[usize::from(component.dc_table)]
                .clone()
                .ok_or_else(|| {
                    Error::InvalidData(format!(
                        "JPEG scan references missing DC Huffman table {}",
                        component.dc_table
                    ))
                })?;
            let ac = tables.ac[usize::from(component.ac_table)]
                .clone()
                .ok_or_else(|| {
                    Error::InvalidData(format!(
                        "JPEG scan references missing AC Huffman table {}",
                        component.ac_table
                    ))
                })?;
            Ok(ScanDecoder {
                component_index,
                dc,
                ac,
            })
        })
        .collect()
}

fn decode_block(
    reader: &mut EntropyReader<'_>,
    dc_decoder: &HuffmanDecoder,
    ac_decoder: &HuffmanDecoder,
    quantization: &QuantizationTable,
    dc_prediction: &mut i32,
) -> Result<[i32; 64]> {
    let mut coefficients = [0_i32; 64];
    let dc_size = dc_decoder.decode(reader)?;
    if dc_size > 11 {
        return Err(reader.error("baseline DC coefficient category exceeds 11"));
    }
    let difference = receive_extend(reader, dc_size)?;
    *dc_prediction = dc_prediction
        .checked_add(difference)
        .ok_or_else(|| reader.error("DC prediction overflow"))?;
    coefficients[0] = dc_prediction
        .checked_mul(i32::from(quantization.values_in_zigzag_order[0]))
        .ok_or_else(|| reader.error("dequantized DC coefficient overflow"))?;

    let mut zigzag_index = 1;
    while zigzag_index < 64 {
        let symbol = ac_decoder.decode(reader)?;
        let run = usize::from(symbol >> 4);
        let size = symbol & 0x0f;
        if size == 0 {
            if run == 0 {
                break;
            }
            if run != 15 {
                return Err(reader.error("invalid zero-size AC Huffman symbol"));
            }
            zigzag_index += 16;
            if zigzag_index > 64 {
                return Err(reader.error("AC zero run exceeds the end of the block"));
            }
            continue;
        }
        if size > 10 {
            return Err(reader.error("baseline AC coefficient category exceeds 10"));
        }
        zigzag_index += run;
        if zigzag_index >= 64 {
            return Err(reader.error("AC coefficient run exceeds the end of the block"));
        }
        let value = receive_extend(reader, size)?;
        let dequantized = value
            .checked_mul(i32::from(quantization.values_in_zigzag_order[zigzag_index]))
            .ok_or_else(|| reader.error("dequantized AC coefficient overflow"))?;
        coefficients[ZIGZAG_TO_NATURAL[zigzag_index]] = dequantized;
        zigzag_index += 1;
    }
    Ok(coefficients)
}

fn write_block(plane: &mut DecodedComponent, x: usize, y: usize, block: &[u8; 64]) {
    for block_y in 0..BLOCK_SIDE {
        let destination_y = y + block_y;
        if destination_y >= plane.height {
            break;
        }
        for block_x in 0..BLOCK_SIDE {
            let destination_x = x + block_x;
            if destination_x >= plane.width {
                break;
            }
            plane.samples[destination_y * plane.width + destination_x] =
                block[block_y * BLOCK_SIDE + block_x];
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::JpegColorModel;
    use super::decode_jpeg;

    const BASELINE_420_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/baseline-420.jpg");
    const MINIMAL_GRAY_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/minimal-gray.jpg");
    const UNKNOWN_APP_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/unknown-app-marker.jpg");
    const RESTART_INTERVAL_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/restart-interval.jpg");

    #[test]
    fn decodes_checked_in_baseline_vectors() {
        for (bytes, dimensions, component_count) in [
            (BASELINE_420_JPEG, (16, 16), 3),
            (MINIMAL_GRAY_JPEG, (8, 8), 3),
            (UNKNOWN_APP_JPEG, (16, 16), 3),
            (RESTART_INTERVAL_JPEG, (16, 8), 1),
        ] {
            let image = decode_jpeg(bytes).expect("decode baseline vector");
            assert_eq!((image.width, image.height), dimensions);
            assert_eq!(image.components.len(), component_count);
        }

        let image = decode_jpeg(BASELINE_420_JPEG).expect("decode 4:2:0 vector");
        assert_eq!(image.color_model, JpegColorModel::YCbCr);
        assert_eq!(
            (image.components[0].width, image.components[0].height),
            (16, 16)
        );
        assert_eq!(
            (image.components[1].width, image.components[1].height),
            (8, 8)
        );
        assert_eq!(
            (image.components[2].width, image.components[2].height),
            (8, 8)
        );

        let restart = decode_jpeg(RESTART_INTERVAL_JPEG).expect("decode restart vector");
        assert_eq!(restart.color_model, JpegColorModel::Grayscale);
        assert!(
            restart.components[0]
                .samples
                .iter()
                .all(|&sample| sample == 128)
        );
    }

    #[test]
    fn reconstruction_is_close_to_ffmpeg() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping FFmpeg comparison: ffmpeg is unavailable");
            return;
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/jpeg/valid/baseline-420.jpg");
        let output = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-i"])
            .arg(path)
            .args(["-f", "rawvideo", "-pix_fmt", "yuvj420p", "-"])
            .output()
            .expect("run FFmpeg reference decoder");
        assert!(output.status.success());

        let decoded = decode_jpeg(BASELINE_420_JPEG).expect("decode baseline vector");
        let candidate: Vec<u8> = decoded
            .components
            .iter()
            .flat_map(|component| component.samples.iter().copied())
            .collect();
        assert_eq!(candidate.len(), output.stdout.len());
        let maximum_difference = candidate
            .iter()
            .zip(&output.stdout)
            .map(|(&candidate, &reference)| candidate.abs_diff(reference))
            .max()
            .unwrap_or(0);
        assert!(
            maximum_difference <= 2,
            "maximum sample difference from FFmpeg is {maximum_difference}"
        );
    }
}
