use mmrecode_core::{ColorRange, Error, FieldOrder, PixelFormat, Plane, Result, VideoFrame};

use crate::{
    HuffmanTable, HuffmanTableClass, decode_jpeg,
    tables::{CHROMA_QUANTIZATION, LUMA_QUANTIZATION, ZIGZAG_TO_NATURAL},
    transform::forward_dct_quantize,
};

const BLOCK_SIDE: usize = 8;

/// Settings for the constrained baseline JPEG encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JpegEncodeOptions {
    /// JPEG quality from 1 (coarsest) through 100 (finest).
    pub quality: u8,
}

impl Default for JpegEncodeOptions {
    fn default() -> Self {
        Self { quality: 75 }
    }
}

/// Encoded JPEG bytes and the encoder's reconstructed frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedJpeg {
    /// Complete JPEG image from SOI through EOI.
    pub data: Vec<u8>,
    /// Reconstruction produced by the portable reference decoder from the
    /// emitted bitstream, ensuring it reflects quantized encoded coefficients.
    pub reconstructed: VideoFrame,
}

/// Encodes one planar frame as an eight-bit baseline sequential JPEG.
///
/// Grayscale, 4:2:0, 4:2:2, and 4:4:4 input are accepted. The encoder uses
/// scaled Annex K quantization matrices and simple fixed canonical Huffman
/// tables. Input is interpreted as full-range JPEG component samples.
///
/// # Errors
///
/// Returns an error for invalid frame layout, unsupported metadata or pixel
/// formats, invalid quality settings, and internal reconstruction failures.
pub fn encode_jpeg(frame: &VideoFrame, options: JpegEncodeOptions) -> Result<EncodedJpeg> {
    let layout = validate_frame(frame)?;
    if !(1..=100).contains(&options.quality) {
        return Err(Error::InvalidData(
            "JPEG quality must be between 1 and 100".into(),
        ));
    }
    let luma_quantization = scale_quantization(&LUMA_QUANTIZATION, options.quality);
    let chroma_quantization = scale_quantization(&CHROMA_QUANTIZATION, options.quality);
    let dc_table = simple_dc_table();
    let ac_table = simple_ac_table();
    let dc_encoder = HuffmanEncoder::new(&dc_table)?;
    let ac_encoder = HuffmanEncoder::new(&ac_table)?;

    let mut output = Vec::new();
    output.extend_from_slice(&[0xff, 0xd8]);
    write_jfif(&mut output)?;
    write_quantization_tables(
        &mut output,
        &luma_quantization,
        layout.components.len() > 1,
        &chroma_quantization,
    )?;
    write_frame_header(&mut output, frame, &layout)?;
    write_huffman_tables(&mut output, &dc_table, &ac_table)?;
    write_scan_header(&mut output, &layout)?;
    let entropy = encode_scan(
        frame,
        &layout,
        &luma_quantization,
        &chroma_quantization,
        &dc_encoder,
        &ac_encoder,
    )?;
    output.extend_from_slice(&entropy);
    output.extend_from_slice(&[0xff, 0xd9]);

    let mut reconstructed = decode_jpeg(&output)?.into_video_frame()?;
    reconstructed.timing = frame.timing;
    Ok(EncodedJpeg {
        data: output,
        reconstructed,
    })
}

#[derive(Clone, Copy)]
struct ComponentLayout {
    id: u8,
    horizontal_sampling: u8,
    vertical_sampling: u8,
    quantization_table: u8,
}

struct FrameLayout {
    components: Vec<ComponentLayout>,
    maximum_horizontal: u8,
    maximum_vertical: u8,
}

fn validate_frame(frame: &VideoFrame) -> Result<FrameLayout> {
    if frame.width == 0
        || frame.height == 0
        || frame.width > usize::from(u16::MAX)
        || frame.height > usize::from(u16::MAX)
    {
        return Err(Error::InvalidData(
            "JPEG frame dimensions must be between 1 and 65535".into(),
        ));
    }
    if frame.field_order != FieldOrder::Progressive {
        return Err(Error::Unsupported(
            "constrained JPEG encoding currently requires progressive frames".into(),
        ));
    }
    if frame.color.range == ColorRange::Limited {
        return Err(Error::Unsupported(
            "limited-range input requires conversion to full-range JPEG samples".into(),
        ));
    }
    let components = match frame.format {
        PixelFormat::Gray8 => vec![component(1, 1, 1, 0)],
        PixelFormat::Yuv420p8 => vec![
            component(1, 2, 2, 0),
            component(2, 1, 1, 1),
            component(3, 1, 1, 1),
        ],
        PixelFormat::Yuv422p8 => vec![
            component(1, 2, 1, 0),
            component(2, 1, 1, 1),
            component(3, 1, 1, 1),
        ],
        PixelFormat::Yuv444p8 => vec![
            component(1, 1, 1, 0),
            component(2, 1, 1, 1),
            component(3, 1, 1, 1),
        ],
        PixelFormat::Rgb24 => {
            return Err(Error::Unsupported(
                "packed RGB input requires color conversion before JPEG encoding".into(),
            ));
        }
        _ => {
            return Err(Error::Unsupported(
                "pixel format is not supported by JPEG".into(),
            ));
        }
    };
    let maximum_horizontal = components
        .iter()
        .map(|component| component.horizontal_sampling)
        .max()
        .expect("layouts contain components");
    let maximum_vertical = components
        .iter()
        .map(|component| component.vertical_sampling)
        .max()
        .expect("layouts contain components");
    validate_input_planes(frame, &components, maximum_horizontal, maximum_vertical)?;
    Ok(FrameLayout {
        components,
        maximum_horizontal,
        maximum_vertical,
    })
}

const fn component(
    id: u8,
    horizontal_sampling: u8,
    vertical_sampling: u8,
    quantization_table: u8,
) -> ComponentLayout {
    ComponentLayout {
        id,
        horizontal_sampling,
        vertical_sampling,
        quantization_table,
    }
}

fn validate_input_planes(
    frame: &VideoFrame,
    components: &[ComponentLayout],
    maximum_horizontal: u8,
    maximum_vertical: u8,
) -> Result<()> {
    if frame.planes.len() != components.len() {
        return Err(Error::InvalidData(format!(
            "JPEG {:?} input requires {} plane(s), found {}",
            frame.format,
            components.len(),
            frame.planes.len()
        )));
    }
    for (index, (plane, component)) in frame.planes.iter().zip(components).enumerate() {
        let width = (frame.width * usize::from(component.horizontal_sampling))
            .div_ceil(usize::from(maximum_horizontal));
        let height = (frame.height * usize::from(component.vertical_sampling))
            .div_ceil(usize::from(maximum_vertical));
        let storage = plane.stride.checked_mul(plane.height).ok_or_else(|| {
            Error::InvalidData(format!("JPEG input plane {index} storage size overflows"))
        })?;
        if (plane.width, plane.height) != (width, height)
            || plane.stride < width
            || plane.data.len() < storage
        {
            return Err(Error::InvalidData(format!(
                "JPEG input plane {index} has inconsistent dimensions, stride, or storage"
            )));
        }
    }
    Ok(())
}

fn scale_quantization(base: &[u8; 64], quality: u8) -> [u8; 64] {
    let quality = u32::from(quality);
    let scale = if quality < 50 {
        5_000 / quality
    } else {
        200 - quality * 2
    };
    std::array::from_fn(|index| {
        let value = (u32::from(base[index]) * scale + 50) / 100;
        u8::try_from(value.clamp(1, 255)).expect("scaled quantizer fits in u8")
    })
}

fn simple_dc_table() -> HuffmanTable {
    let mut code_counts = [0; 16];
    code_counts[3] = 12;
    HuffmanTable {
        class: HuffmanTableClass::Dc,
        id: 0,
        code_counts,
        symbols: (0..=11).collect(),
    }
}

fn simple_ac_table() -> HuffmanTable {
    let mut code_counts = [0; 16];
    code_counts[7] = 162;
    let mut symbols = vec![0x00, 0xf0];
    for run in 0..=15 {
        for size in 1..=10 {
            symbols.push((run << 4) | size);
        }
    }
    HuffmanTable {
        class: HuffmanTableClass::Ac,
        id: 0,
        code_counts,
        symbols,
    }
}

fn write_jfif(output: &mut Vec<u8>) -> Result<()> {
    write_segment(
        output,
        0xe0,
        &[b'J', b'F', b'I', b'F', 0, 1, 2, 0, 0, 1, 0, 1, 0, 0],
    )
}

fn write_quantization_tables(
    output: &mut Vec<u8>,
    luma: &[u8; 64],
    include_chroma: bool,
    chroma: &[u8; 64],
) -> Result<()> {
    let mut payload = Vec::with_capacity(if include_chroma { 130 } else { 65 });
    payload.push(0);
    payload.extend(ZIGZAG_TO_NATURAL.map(|index| luma[index]));
    if include_chroma {
        payload.push(1);
        payload.extend(ZIGZAG_TO_NATURAL.map(|index| chroma[index]));
    }
    write_segment(output, 0xdb, &payload)
}

fn write_frame_header(
    output: &mut Vec<u8>,
    frame: &VideoFrame,
    layout: &FrameLayout,
) -> Result<()> {
    let mut payload = Vec::with_capacity(6 + layout.components.len() * 3);
    payload.push(8);
    payload.extend_from_slice(
        &u16::try_from(frame.height)
            .expect("validated height")
            .to_be_bytes(),
    );
    payload.extend_from_slice(
        &u16::try_from(frame.width)
            .expect("validated width")
            .to_be_bytes(),
    );
    payload.push(u8::try_from(layout.components.len()).expect("JPEG component count fits"));
    for component in &layout.components {
        payload.push(component.id);
        payload.push((component.horizontal_sampling << 4) | component.vertical_sampling);
        payload.push(component.quantization_table);
    }
    write_segment(output, 0xc0, &payload)
}

fn write_huffman_tables(output: &mut Vec<u8>, dc: &HuffmanTable, ac: &HuffmanTable) -> Result<()> {
    let mut payload = Vec::new();
    for table in [dc, ac] {
        let class = match table.class {
            HuffmanTableClass::Dc => 0,
            HuffmanTableClass::Ac => 1,
        };
        payload.push((class << 4) | table.id);
        payload.extend_from_slice(&table.code_counts);
        payload.extend_from_slice(&table.symbols);
    }
    write_segment(output, 0xc4, &payload)
}

fn write_scan_header(output: &mut Vec<u8>, layout: &FrameLayout) -> Result<()> {
    let mut payload = Vec::with_capacity(1 + layout.components.len() * 2 + 3);
    payload.push(u8::try_from(layout.components.len()).expect("JPEG component count fits"));
    for component in &layout.components {
        payload.extend_from_slice(&[component.id, 0]);
    }
    payload.extend_from_slice(&[0, 63, 0]);
    write_segment(output, 0xda, &payload)
}

fn write_segment(output: &mut Vec<u8>, marker: u8, payload: &[u8]) -> Result<()> {
    let length = u16::try_from(payload.len() + 2)
        .map_err(|_| Error::InvalidData("JPEG segment payload exceeds 65533 bytes".into()))?;
    output.extend_from_slice(&[0xff, marker]);
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(payload);
    Ok(())
}

fn encode_scan(
    frame: &VideoFrame,
    layout: &FrameLayout,
    luma_quantization: &[u8; 64],
    chroma_quantization: &[u8; 64],
    dc_encoder: &HuffmanEncoder,
    ac_encoder: &HuffmanEncoder,
) -> Result<Vec<u8>> {
    let mcu_width = usize::from(layout.maximum_horizontal) * BLOCK_SIDE;
    let mcu_height = usize::from(layout.maximum_vertical) * BLOCK_SIDE;
    let mcu_columns = frame.width.div_ceil(mcu_width);
    let mcu_rows = frame.height.div_ceil(mcu_height);
    let mut dc_predictions = vec![0_i32; layout.components.len()];
    let mut writer = EntropyWriter::default();
    for mcu_y in 0..mcu_rows {
        for mcu_x in 0..mcu_columns {
            for (component_index, component) in layout.components.iter().enumerate() {
                let quantization = if component.quantization_table == 0 {
                    luma_quantization
                } else {
                    chroma_quantization
                };
                for block_y in 0..usize::from(component.vertical_sampling) {
                    for block_x in 0..usize::from(component.horizontal_sampling) {
                        let x = (mcu_x * usize::from(component.horizontal_sampling) + block_x)
                            * BLOCK_SIDE;
                        let y = (mcu_y * usize::from(component.vertical_sampling) + block_y)
                            * BLOCK_SIDE;
                        let samples = read_block(&frame.planes[component_index], x, y);
                        let mut coefficients = forward_dct_quantize(&samples, quantization);
                        for coefficient in &mut coefficients[1..] {
                            *coefficient = (*coefficient).clamp(-1_023, 1_023);
                        }
                        encode_block(
                            &mut writer,
                            &coefficients,
                            &mut dc_predictions[component_index],
                            dc_encoder,
                            ac_encoder,
                        )?;
                    }
                }
            }
        }
    }
    Ok(writer.finish())
}

fn read_block(plane: &Plane, x: usize, y: usize) -> [u8; 64] {
    let mut block = [0; 64];
    for block_y in 0..BLOCK_SIDE {
        let source_y = (y + block_y).min(plane.height - 1);
        for block_x in 0..BLOCK_SIDE {
            let source_x = (x + block_x).min(plane.width - 1);
            block[block_y * BLOCK_SIDE + block_x] = plane.data[source_y * plane.stride + source_x];
        }
    }
    block
}

fn encode_block(
    writer: &mut EntropyWriter,
    coefficients: &[i32; 64],
    dc_prediction: &mut i32,
    dc_encoder: &HuffmanEncoder,
    ac_encoder: &HuffmanEncoder,
) -> Result<()> {
    let difference = coefficients[0] - *dc_prediction;
    *dc_prediction = coefficients[0];
    let dc_size = magnitude_category(difference);
    dc_encoder.write_symbol(writer, dc_size)?;
    write_amplitude(writer, difference, dc_size)?;

    let mut zero_run = 0_u8;
    for &natural_index in &ZIGZAG_TO_NATURAL[1..] {
        let coefficient = coefficients[natural_index];
        if coefficient == 0 {
            zero_run += 1;
            continue;
        }
        while zero_run >= 16 {
            ac_encoder.write_symbol(writer, 0xf0)?;
            zero_run -= 16;
        }
        let size = magnitude_category(coefficient);
        ac_encoder.write_symbol(writer, (zero_run << 4) | size)?;
        write_amplitude(writer, coefficient, size)?;
        zero_run = 0;
    }
    if zero_run != 0 {
        ac_encoder.write_symbol(writer, 0x00)?;
    }
    Ok(())
}

fn magnitude_category(value: i32) -> u8 {
    let magnitude = value.unsigned_abs();
    u8::try_from(u32::BITS - magnitude.leading_zeros()).expect("i32 magnitude category fits in u8")
}

fn write_amplitude(writer: &mut EntropyWriter, value: i32, size: u8) -> Result<()> {
    if size == 0 {
        return Ok(());
    }
    let encoded = if value < 0 {
        value + (1_i32 << size) - 1
    } else {
        value
    };
    writer.write_bits(
        u16::try_from(encoded).map_err(|_| {
            Error::InvalidData(format!("JPEG coefficient {value} cannot be represented"))
        })?,
        size,
    );
    Ok(())
}

#[derive(Clone, Copy)]
struct HuffmanCode {
    bits: u16,
    length: u8,
}

struct HuffmanEncoder {
    codes: [Option<HuffmanCode>; 256],
}

impl HuffmanEncoder {
    fn new(table: &HuffmanTable) -> Result<Self> {
        let mut codes = [None; 256];
        let mut code = 0_u32;
        let mut symbol_index = 0;
        for (length_index, &count) in table.code_counts.iter().enumerate() {
            let length = u8::try_from(length_index + 1).expect("JPEG Huffman length fits in u8");
            if code + u32::from(count) > (1_u32 << length) {
                return Err(Error::InvalidData(
                    "JPEG Huffman table is oversubscribed".into(),
                ));
            }
            for _ in 0..count {
                let symbol = *table.symbols.get(symbol_index).ok_or_else(|| {
                    Error::InvalidData("JPEG Huffman table has too few symbols".into())
                })?;
                codes[usize::from(symbol)] = Some(HuffmanCode {
                    bits: u16::try_from(code).expect("JPEG Huffman code fits in u16"),
                    length,
                });
                symbol_index += 1;
                code += 1;
            }
            code <<= 1;
        }
        if symbol_index != table.symbols.len() {
            return Err(Error::InvalidData(
                "JPEG Huffman table has excess symbols".into(),
            ));
        }
        Ok(Self { codes })
    }

    fn write_symbol(&self, writer: &mut EntropyWriter, symbol: u8) -> Result<()> {
        let code = self.codes[usize::from(symbol)].ok_or_else(|| {
            Error::InvalidData(format!("JPEG Huffman table has no symbol 0x{symbol:02x}"))
        })?;
        writer.write_bits(code.bits, code.length);
        Ok(())
    }
}

#[derive(Default)]
struct EntropyWriter {
    data: Vec<u8>,
    current_byte: u8,
    bits_written: u8,
}

impl EntropyWriter {
    fn write_bits(&mut self, value: u16, count: u8) {
        for bit_index in (0..count).rev() {
            let bit = u8::try_from((value >> bit_index) & 1).expect("single bit fits in u8");
            self.current_byte = (self.current_byte << 1) | bit;
            self.bits_written += 1;
            if self.bits_written == 8 {
                self.push_current_byte();
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        while self.bits_written != 0 {
            self.write_bits(1, 1);
        }
        self.data
    }

    fn push_current_byte(&mut self) {
        self.data.push(self.current_byte);
        if self.current_byte == 0xff {
            self.data.push(0);
        }
        self.current_byte = 0;
        self.bits_written = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use mmrecode_core::{
        ColorDescription, ColorRange, FieldOrder, FrameTiming, PixelFormat, Plane, VideoFrame,
    };
    use mmrecode_y4m::Y4mReader;

    use super::{JpegEncodeOptions, encode_jpeg};

    const TWO_FRAME_Y4M: &[u8] = include_bytes!("../../../../testdata/y4m/valid/two-frame-420.y4m");
    const GOLDEN_MJPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/encoded/mmrecode-q85-420.mjpg");
    const GRAYSCALE_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/restart-interval.jpg");
    const YUV444_JPEG: &[u8] = include_bytes!("../../../../testdata/jpeg/valid/minimal-gray.jpg");

    #[test]
    fn encodes_and_reconstructs_a_y4m_frame() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/jpeg/valid/baseline-420.jpg");
        let reference_jpeg = std::fs::read(path).expect("read vector");
        let decoded = crate::decode_jpeg(&reference_jpeg)
            .and_then(crate::DecodedJpeg::into_video_frame)
            .expect("decode input frame");
        let encoded = encode_jpeg(&decoded, JpegEncodeOptions { quality: 90 }).expect("encode");
        assert!(encoded.data.starts_with(&[0xff, 0xd8]));
        assert!(encoded.data.ends_with(&[0xff, 0xd9]));
        assert_eq!(
            (encoded.reconstructed.width, encoded.reconstructed.height),
            (16, 16)
        );
    }

    #[test]
    fn output_is_decodable_by_ffmpeg_when_available() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("skipping FFmpeg encoder check: ffmpeg is unavailable");
            return;
        }
        let y4m = b"YUV4MPEG2 W8 H8 Ip Cmono XCOLORRANGE=FULL\nFRAME\n\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80\
                    \x80\x80\x80\x80\x80\x80\x80\x80";
        let mut reader = Y4mReader::new(BufReader::new(&y4m[..]));
        let frame = reader.read_frame().unwrap().expect("test frame");
        let encoded = encode_jpeg(&frame, JpegEncodeOptions::default()).expect("encode");
        let path =
            std::env::temp_dir().join(format!("mmrecode-encoder-{}.jpg", std::process::id()));
        std::fs::write(&path, encoded.data).unwrap();
        let output = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"])
            .output()
            .expect("run FFmpeg");
        let _ = std::fs::remove_file(path);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn encoded_output_is_deterministic() {
        let mut reader = Y4mReader::new(BufReader::new(TWO_FRAME_Y4M));
        let mut stream = Vec::new();
        while let Some(frame) = reader.read_frame().unwrap() {
            let encoded = encode_jpeg(&frame, JpegEncodeOptions { quality: 85 }).unwrap();
            stream.extend_from_slice(&encoded.data);
        }
        assert_eq!(stream, GOLDEN_MJPEG);
    }

    #[test]
    fn internal_reconstruction_matches_ffmpeg_when_available() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("skipping FFmpeg reconstruction check: ffmpeg is unavailable");
            return;
        }
        let mut reader = Y4mReader::new(BufReader::new(TWO_FRAME_Y4M));
        let frame = reader.read_frame().unwrap().expect("source frame");
        let encoded = encode_jpeg(&frame, JpegEncodeOptions { quality: 85 }).unwrap();
        let path = std::env::temp_dir().join(format!(
            "mmrecode-reconstruction-{}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, &encoded.data).unwrap();
        let output = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-i"])
            .arg(&path)
            .args(["-f", "rawvideo", "-pix_fmt", "yuvj420p", "-"])
            .output()
            .expect("run FFmpeg");
        let _ = std::fs::remove_file(path);
        assert!(output.status.success());
        let internal: Vec<u8> = encoded
            .reconstructed
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect();
        assert_eq!(internal.len(), output.stdout.len());
        let maximum_difference = internal
            .iter()
            .zip(output.stdout)
            .map(|(&ours, reference)| ours.abs_diff(reference))
            .max()
            .unwrap_or(0);
        assert!(
            maximum_difference <= 2,
            "maximum reconstruction difference from FFmpeg is {maximum_difference}"
        );
    }

    #[test]
    fn encodes_all_supported_sampling_modes() {
        let grayscale = crate::decode_jpeg(GRAYSCALE_JPEG)
            .and_then(crate::DecodedJpeg::into_video_frame)
            .unwrap();
        let yuv444 = crate::decode_jpeg(YUV444_JPEG)
            .and_then(crate::DecodedJpeg::into_video_frame)
            .unwrap();
        let yuv422 = patterned_422_frame();
        for frame in [grayscale, yuv422, yuv444] {
            let encoded = encode_jpeg(&frame, JpegEncodeOptions::default()).unwrap();
            assert_eq!(encoded.reconstructed.format, frame.format);
            assert_eq!(
                (encoded.reconstructed.width, encoded.reconstructed.height),
                (frame.width, frame.height)
            );
        }
    }

    fn patterned_422_frame() -> VideoFrame {
        let plane = |width: usize, height: usize, offset: u8| Plane {
            data: (0..width * height)
                .map(|index| {
                    u8::try_from(index % 128)
                        .expect("pattern value fits")
                        .saturating_add(offset)
                })
                .collect(),
            stride: width,
            width,
            height,
        };
        VideoFrame {
            format: PixelFormat::Yuv422p8,
            width: 10,
            height: 9,
            planes: vec![plane(10, 9, 16), plane(5, 9, 64), plane(5, 9, 96)],
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Full,
                ..ColorDescription::default()
            },
            field_order: FieldOrder::Progressive,
        }
    }
}
