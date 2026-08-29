use mmrecode_core::{Error, Result};

use crate::syntax::{
    ApplicationSegment, EntropyScan, FrameComponent, FrameHeader, HuffmanTable, HuffmanTableClass,
    JfifHeader, JpegImage, JpegSegment, Marker, QuantizationPrecision, QuantizationTable,
    RestartMarker, ScanComponent, ScanHeader, SegmentData,
};

/// Parses and indexes one complete JPEG image without decoding its pixels.
///
/// Unknown and application-specific payloads are preserved. Malformed input is
/// reported with the absolute source byte offset at which it was detected.
///
/// # Errors
///
/// Returns [`Error::InvalidData`] when the input is truncated or violates the
/// JPEG marker and supported baseline-header syntax.
pub fn parse_jpeg(input: &[u8]) -> Result<JpegImage> {
    let (soi_offset, soi, mut position) = read_marker(input, 0)?;
    if soi != Marker::StartOfImage || soi_offset != 0 {
        return invalid(soi_offset, "expected SOI as the first marker");
    }

    let mut image = JpegImage {
        segments: vec![empty_segment(soi_offset, soi)],
        entropy_scans: Vec::new(),
        trailing_data: Vec::new(),
    };
    let mut pending_marker = None;

    loop {
        let (offset, marker, after_marker) = match pending_marker.take() {
            Some(marker) => marker,
            None => read_marker(input, position)?,
        };
        position = after_marker;

        if marker == Marker::EndOfImage {
            image.segments.push(empty_segment(offset, marker));
            image.trailing_data.extend_from_slice(&input[position..]);
            return Ok(image);
        }

        if !marker.has_length() {
            image.segments.push(empty_segment(offset, marker));
            continue;
        }

        let length = read_u16(input, position, "segment length")?;
        if length < 2 {
            return invalid(position, "segment length must include its two length bytes");
        }
        let payload_offset = position + 2;
        let payload_length = usize::from(length) - 2;
        let payload_end = payload_offset
            .checked_add(payload_length)
            .ok_or_else(|| jpeg_error(position, "segment size overflows address space"))?;
        let payload = input.get(payload_offset..payload_end).ok_or_else(|| {
            jpeg_error(
                payload_offset,
                &format!("truncated {} payload", marker.name()),
            )
        })?;
        let data = parse_segment_data(marker, payload, payload_offset)?;
        image.segments.push(JpegSegment {
            offset,
            marker,
            payload_offset: Some(payload_offset),
            payload_length,
            data,
        });
        position = payload_end;

        if marker == Marker::StartOfScan {
            let (scan, next_marker) = scan_entropy_data(input, position)?;
            position = next_marker.2;
            pending_marker = Some(next_marker);
            image.entropy_scans.push(scan);
        }
    }
}

fn empty_segment(offset: usize, marker: Marker) -> JpegSegment {
    JpegSegment {
        offset,
        marker,
        payload_offset: None,
        payload_length: 0,
        data: SegmentData::Empty,
    }
}

fn read_marker(input: &[u8], position: usize) -> Result<(usize, Marker, usize)> {
    if input.get(position) != Some(&0xff) {
        return invalid(position, "expected JPEG marker prefix 0xff");
    }

    let offset = position;
    let mut code_offset = position + 1;
    while input.get(code_offset) == Some(&0xff) {
        code_offset += 1;
    }
    let code = *input
        .get(code_offset)
        .ok_or_else(|| jpeg_error(code_offset, "truncated marker"))?;
    if code == 0x00 {
        return invalid(code_offset, "stuffed 0xff byte outside entropy-coded data");
    }
    Ok((offset, Marker::from_code(code), code_offset + 1))
}

fn read_u16(input: &[u8], offset: usize, what: &str) -> Result<u16> {
    let bytes = input
        .get(offset..offset + 2)
        .ok_or_else(|| jpeg_error(offset, &format!("truncated {what}")))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn parse_segment_data(marker: Marker, payload: &[u8], base: usize) -> Result<SegmentData> {
    match marker {
        Marker::StartOfFrameBaseline => parse_frame_header(payload, base).map(SegmentData::Frame),
        Marker::DefineQuantizationTables => {
            parse_quantization_tables(payload, base).map(SegmentData::QuantizationTables)
        }
        Marker::DefineHuffmanTables => {
            parse_huffman_tables(payload, base).map(SegmentData::HuffmanTables)
        }
        Marker::DefineRestartInterval => {
            if payload.len() != 2 {
                return invalid(base, "DRI payload must contain exactly two bytes");
            }
            Ok(SegmentData::RestartInterval(u16::from_be_bytes([
                payload[0], payload[1],
            ])))
        }
        Marker::StartOfScan => parse_scan_header(payload, base).map(SegmentData::Scan),
        Marker::Application(0) if payload.starts_with(b"JFIF\0") => {
            parse_jfif(payload, base).map(SegmentData::Jfif)
        }
        Marker::Application(number) => Ok(SegmentData::Application(ApplicationSegment {
            number,
            data: payload.to_vec(),
        })),
        Marker::Comment => Ok(SegmentData::Comment(payload.to_vec())),
        _ => Ok(SegmentData::Unknown(payload.to_vec())),
    }
}

fn parse_frame_header(payload: &[u8], base: usize) -> Result<FrameHeader> {
    if payload.len() < 6 {
        return invalid(base, "SOF0 payload is too short");
    }
    let component_count = usize::from(payload[5]);
    if component_count == 0 {
        return invalid(base + 5, "SOF0 must declare at least one component");
    }
    let expected = 6 + 3 * component_count;
    if payload.len() != expected {
        return invalid(
            base + 5,
            &format!(
                "SOF0 declares {component_count} components but has {} bytes",
                payload.len()
            ),
        );
    }
    if payload[0] != 8 {
        return invalid(base, "baseline SOF0 sample precision must be 8 bits");
    }

    let height = u16::from_be_bytes([payload[1], payload[2]]);
    let width = u16::from_be_bytes([payload[3], payload[4]]);
    if width == 0 {
        return invalid(base + 3, "SOF0 width must be non-zero");
    }

    let mut components = Vec::with_capacity(component_count);
    for index in 0..component_count {
        let offset = 6 + index * 3;
        let sampling = payload[offset + 1];
        let horizontal_sampling = sampling >> 4;
        let vertical_sampling = sampling & 0x0f;
        if !(1..=4).contains(&horizontal_sampling) || !(1..=4).contains(&vertical_sampling) {
            return invalid(
                base + offset + 1,
                "sampling factors must be between 1 and 4",
            );
        }
        if payload[offset + 2] > 3 {
            return invalid(
                base + offset + 2,
                "quantization table selector must be 0 through 3",
            );
        }
        components.push(FrameComponent {
            id: payload[offset],
            horizontal_sampling,
            vertical_sampling,
            quantization_table: payload[offset + 2],
        });
    }

    Ok(FrameHeader {
        sample_precision: payload[0],
        width,
        height,
        components,
    })
}

fn parse_quantization_tables(payload: &[u8], base: usize) -> Result<Vec<QuantizationTable>> {
    let mut tables = Vec::new();
    let mut position = 0;
    while position < payload.len() {
        let specification = payload[position];
        position += 1;
        let precision = match specification >> 4 {
            0 => QuantizationPrecision::EightBit,
            1 => QuantizationPrecision::SixteenBit,
            _ => return invalid(base + position - 1, "DQT precision must be 0 or 1"),
        };
        let id = specification & 0x0f;
        if id > 3 {
            return invalid(
                base + position - 1,
                "DQT table identifier must be 0 through 3",
            );
        }
        let bytes_per_value = match precision {
            QuantizationPrecision::EightBit => 1,
            QuantizationPrecision::SixteenBit => 2,
        };
        let table_bytes = 64 * bytes_per_value;
        if payload.len().saturating_sub(position) < table_bytes {
            return invalid(base + position, "truncated DQT values");
        }
        let mut values = [0_u16; 64];
        for value in &mut values {
            *value = if bytes_per_value == 1 {
                let result = u16::from(payload[position]);
                position += 1;
                result
            } else {
                let result = u16::from_be_bytes([payload[position], payload[position + 1]]);
                position += 2;
                result
            };
        }
        tables.push(QuantizationTable {
            id,
            precision,
            values_in_zigzag_order: values,
        });
    }
    if tables.is_empty() {
        return invalid(base, "DQT must contain at least one table");
    }
    Ok(tables)
}

fn parse_huffman_tables(payload: &[u8], base: usize) -> Result<Vec<HuffmanTable>> {
    let mut tables = Vec::new();
    let mut position = 0;
    while position < payload.len() {
        let specification = payload[position];
        position += 1;
        let class = match specification >> 4 {
            0 => HuffmanTableClass::Dc,
            1 => HuffmanTableClass::Ac,
            _ => return invalid(base + position - 1, "DHT class must be 0 or 1"),
        };
        let id = specification & 0x0f;
        if id > 3 {
            return invalid(
                base + position - 1,
                "DHT table identifier must be 0 through 3",
            );
        }
        let counts_slice = payload
            .get(position..position + 16)
            .ok_or_else(|| jpeg_error(base + position, "truncated DHT code counts"))?;
        let mut code_counts = [0_u8; 16];
        code_counts.copy_from_slice(counts_slice);
        position += 16;
        let symbol_count: usize = code_counts.iter().map(|&count| usize::from(count)).sum();
        let symbols = payload
            .get(position..position + symbol_count)
            .ok_or_else(|| jpeg_error(base + position, "truncated DHT symbols"))?
            .to_vec();
        position += symbol_count;
        tables.push(HuffmanTable {
            class,
            id,
            code_counts,
            symbols,
        });
    }
    if tables.is_empty() {
        return invalid(base, "DHT must contain at least one table");
    }
    Ok(tables)
}

fn parse_scan_header(payload: &[u8], base: usize) -> Result<ScanHeader> {
    let Some(&component_count_byte) = payload.first() else {
        return invalid(base, "SOS payload is empty");
    };
    let component_count = usize::from(component_count_byte);
    if component_count == 0 {
        return invalid(base, "SOS must select at least one component");
    }
    let expected = 1 + 2 * component_count + 3;
    if payload.len() != expected {
        return invalid(
            base,
            &format!(
                "SOS declares {component_count} components but has {} bytes",
                payload.len()
            ),
        );
    }
    let mut components = Vec::with_capacity(component_count);
    for index in 0..component_count {
        let offset = 1 + index * 2;
        let selectors = payload[offset + 1];
        if selectors >> 4 > 3 || selectors & 0x0f > 3 {
            return invalid(
                base + offset + 1,
                "Huffman table selectors must be 0 through 3",
            );
        }
        components.push(ScanComponent {
            selector: payload[offset],
            dc_table: selectors >> 4,
            ac_table: selectors & 0x0f,
        });
    }
    let tail = 1 + 2 * component_count;
    Ok(ScanHeader {
        components,
        spectral_start: payload[tail],
        spectral_end: payload[tail + 1],
        successive_approximation_high: payload[tail + 2] >> 4,
        successive_approximation_low: payload[tail + 2] & 0x0f,
    })
}

fn parse_jfif(payload: &[u8], base: usize) -> Result<JfifHeader> {
    if payload.len() < 14 {
        return invalid(base, "JFIF APP0 payload is too short");
    }
    let thumbnail_width = payload[12];
    let thumbnail_height = payload[13];
    let thumbnail_size = 3 * usize::from(thumbnail_width) * usize::from(thumbnail_height);
    if payload.len() != 14 + thumbnail_size {
        return invalid(
            base + 12,
            "JFIF thumbnail dimensions do not match payload size",
        );
    }
    Ok(JfifHeader {
        version_major: payload[5],
        version_minor: payload[6],
        density_units: payload[7],
        density_x: u16::from_be_bytes([payload[8], payload[9]]),
        density_y: u16::from_be_bytes([payload[10], payload[11]]),
        thumbnail_width,
        thumbnail_height,
        thumbnail_rgb: payload[14..].to_vec(),
    })
}

fn scan_entropy_data(
    input: &[u8],
    data_offset: usize,
) -> Result<(EntropyScan, (usize, Marker, usize))> {
    let mut position = data_offset;
    let mut restart_markers = Vec::new();
    while position < input.len() {
        if input[position] != 0xff {
            position += 1;
            continue;
        }
        let marker_offset = position;
        position += 1;
        while input.get(position) == Some(&0xff) {
            position += 1;
        }
        let code = *input
            .get(position)
            .ok_or_else(|| jpeg_error(position, "truncated marker in entropy-coded data"))?;
        position += 1;
        if code == 0x00 {
            continue;
        }
        let marker = Marker::from_code(code);
        if let Marker::Restart(number) = marker {
            restart_markers.push(RestartMarker {
                offset: marker_offset,
                number,
            });
            continue;
        }
        return Ok((
            EntropyScan {
                data_offset,
                data_length: marker_offset - data_offset,
                restart_markers,
            },
            (marker_offset, marker, position),
        ));
    }
    invalid(input.len(), "entropy-coded scan has no terminating marker")
}

fn jpeg_error(offset: usize, message: &str) -> Error {
    Error::InvalidData(format!("JPEG at byte 0x{offset:08x}: {message}"))
}

fn invalid<T>(offset: usize, message: &str) -> Result<T> {
    Err(jpeg_error(offset, message))
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use super::*;

    const BASELINE_420_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/baseline-420.jpg");
    const MINIMAL_GRAY_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/minimal-gray.jpg");
    const UNKNOWN_APP_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/unknown-app-marker.jpg");
    const RESTART_INTERVAL_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/restart-interval.jpg");
    const RESTART_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/parser/stuffed-restarts.jpg");
    const INVALID_LENGTH_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/invalid/invalid-length.jpg");
    const MISSING_EOI_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/invalid/missing-eoi.jpg");
    const TRUNCATED_MARKER_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/invalid/truncated-marker.jpg");

    #[test]
    fn parses_all_permanent_valid_vectors() {
        for (bytes, expected_dimensions) in [
            (BASELINE_420_JPEG, (16, 16)),
            (MINIMAL_GRAY_JPEG, (8, 8)),
            (UNKNOWN_APP_JPEG, (16, 16)),
            (RESTART_INTERVAL_JPEG, (16, 8)),
        ] {
            let image = parse_jpeg(bytes).expect("valid permanent vector");
            let frame = image.frame_header().expect("frame header");
            assert_eq!((frame.width, frame.height), expected_dimensions);
            assert_eq!(
                image.segments.last().map(|segment| segment.marker),
                Some(Marker::EndOfImage)
            );
        }
    }

    #[test]
    fn parses_baseline_headers_and_preserves_unknown_app_data() {
        let image = parse_jpeg(UNKNOWN_APP_JPEG).expect("valid fixture");
        let frame = image.frame_header().expect("frame header");
        assert_eq!((frame.width, frame.height), (16, 16));
        assert_eq!(frame.components.len(), 3);
        assert_eq!(image.jfif_header().expect("JFIF").version_major, 1);
        assert!(image.segments.iter().any(|segment| {
            matches!(
                &segment.data,
                SegmentData::Application(ApplicationSegment { number: 2, data })
                    if data == &[0xde, 0xad, 0xbe, 0xef]
            )
        }));
        assert!(!image.entropy_scans.is_empty());
    }

    #[test]
    fn handles_byte_stuffing_and_restart_markers() {
        let image = parse_jpeg(RESTART_JPEG).expect("valid entropy data");
        let scan = &image.entropy_scans[0];
        assert_eq!(scan.data_length, 9);
        assert_eq!(scan.restart_markers.len(), 2);
        assert_eq!(scan.restart_markers[0].number, 0);
        assert_eq!(scan.restart_markers[1].number, 7);
    }

    #[test]
    fn malformed_length_reports_absolute_offset() {
        let error = parse_jpeg(INVALID_LENGTH_JPEG).expect_err("invalid segment length");
        assert!(error.to_string().contains("0x00000004"));
    }

    #[test]
    fn rejects_other_permanent_invalid_vectors() {
        assert!(parse_jpeg(MISSING_EOI_JPEG).is_err());
        assert!(parse_jpeg(TRUNCATED_MARKER_JPEG).is_err());
    }

    #[test]
    fn parses_jpeg_generated_by_ffmpeg_when_available() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping FFmpeg interoperability test: ffmpeg is unavailable");
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "mmrecode-parser-{}-{}.jpg",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let output = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=16x16:d=0.04",
                "-frames:v",
                "1",
                "-c:v",
                "mjpeg",
                "-y",
            ])
            .arg(&path)
            .output()
            .expect("run ffmpeg");
        assert!(
            output.status.success(),
            "FFmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = fs::read(&path).expect("read generated JPEG");
        let _ = fs::remove_file(&path);
        let image = parse_jpeg(&bytes).expect("parse FFmpeg JPEG");
        let frame = image.frame_header().expect("frame header");
        assert_eq!((frame.width, frame.height), (16, 16));
        assert_eq!(
            image.segments.first().map(|segment| segment.marker),
            Some(Marker::StartOfImage)
        );
        assert_eq!(
            image.segments.last().map(|segment| segment.marker),
            Some(Marker::EndOfImage)
        );
    }
}
