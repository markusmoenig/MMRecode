//! `MMRecode` command-line entry point.

fn main() {
    if let Err(error) = run() {
        eprintln!("mmrecode: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let command = arguments.next();
    match command.as_deref().and_then(std::ffi::OsStr::to_str) {
        None | Some("help" | "--help" | "-h") => {
            print_help();
            Ok(())
        }
        Some("version" | "--version" | "-V") => {
            println!("mmrecode {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("inspect") => {
            let path = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode inspect <jpeg-file>".to_owned())?;
            if arguments.next().is_some() {
                return Err("usage: mmrecode inspect <jpeg-file>".to_owned());
            }
            inspect(std::path::Path::new(&path))
        }
        Some("decode") => {
            let input = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode decode <jpeg-file> <output.y4m>".to_owned())?;
            let output = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode decode <jpeg-file> <output.y4m>".to_owned())?;
            if arguments.next().is_some() {
                return Err("usage: mmrecode decode <jpeg-file> <output.y4m>".to_owned());
            }
            decode(std::path::Path::new(&input), std::path::Path::new(&output))
        }
        Some("encode") => {
            let input = arguments.next().ok_or_else(|| {
                "usage: mmrecode encode <input.y4m> <output.mjpg> [quality]".to_owned()
            })?;
            let output = arguments.next().ok_or_else(|| {
                "usage: mmrecode encode <input.y4m> <output.mjpg> [quality]".to_owned()
            })?;
            let quality = arguments
                .next()
                .map(|value| {
                    value
                        .to_str()
                        .ok_or_else(|| "quality must be valid UTF-8".to_owned())?
                        .parse::<u8>()
                        .map_err(|_| "quality must be an integer from 1 through 100".to_owned())
                })
                .transpose()?
                .unwrap_or(75);
            if arguments.next().is_some() {
                return Err("usage: mmrecode encode <input.y4m> <output.mjpg> [quality]".to_owned());
            }
            encode_y4m(
                std::path::Path::new(&input),
                std::path::Path::new(&output),
                quality,
            )
        }
        Some("verify") => {
            let input = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode verify <input.mjpg> [reference.y4m]".to_owned())?;
            let reference = arguments.next();
            if arguments.next().is_some() {
                return Err("usage: mmrecode verify <input.mjpg> [reference.y4m]".to_owned());
            }
            verify(
                std::path::Path::new(&input),
                reference.as_deref().map(std::path::Path::new),
            )
        }
        Some("compare") => {
            let reference = arguments.next().ok_or_else(|| {
                "usage: mmrecode compare <reference.y4m> <candidate.y4m>".to_owned()
            })?;
            let candidate = arguments.next().ok_or_else(|| {
                "usage: mmrecode compare <reference.y4m> <candidate.y4m>".to_owned()
            })?;
            if arguments.next().is_some() {
                return Err("usage: mmrecode compare <reference.y4m> <candidate.y4m>".to_owned());
            }
            compare_y4m(
                std::path::Path::new(&reference),
                std::path::Path::new(&candidate),
            )
        }
        Some(other) => Err(format!(
            "command '{other}' is not implemented; run 'mmrecode help' for available commands"
        )),
    }
}

fn decode(input: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
    use std::io::Write as _;

    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    if bytes.is_empty() {
        return Err("input contains no JPEG frames".to_owned());
    }
    let file = std::fs::File::create(output)
        .map_err(|error| format!("cannot create '{}': {error}", output.display()))?;
    let mut y4m = mmrecode_y4m::Y4mWriter::new(std::io::BufWriter::new(file));
    let mut remaining = bytes.as_slice();
    let mut frame_count = 0_usize;
    while !remaining.is_empty() {
        let structure = mmrecode_mjpeg::parse_jpeg(remaining).map_err(|error| error.to_string())?;
        let consumed = remaining.len() - structure.trailing_data.len();
        if consumed == 0 {
            return Err("JPEG parser did not consume input".to_owned());
        }
        let frame = mmrecode_mjpeg::decode_jpeg(&remaining[..consumed])
            .and_then(mmrecode_mjpeg::DecodedJpeg::into_video_frame)
            .map_err(|error| error.to_string())?;
        y4m.write_frame(&frame).map_err(|error| error.to_string())?;
        frame_count += 1;
        remaining = &remaining[consumed..];
    }
    y4m.into_inner()
        .flush()
        .map_err(|error| format!("cannot finish '{}': {error}", output.display()))?;
    println!(
        "Decoded {frame_count} JPEG frame(s) to {}",
        output.display()
    );
    Ok(())
}

fn encode_y4m(
    input: &std::path::Path,
    output: &std::path::Path,
    quality: u8,
) -> Result<(), String> {
    use std::io::Write as _;

    let file = std::fs::File::open(input)
        .map_err(|error| format!("cannot open '{}': {error}", input.display()))?;
    let mut reader = mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file));
    let mut encoded_stream = Vec::new();
    let mut frame_count = 0_usize;
    while let Some(frame) = reader.read_frame().map_err(|error| error.to_string())? {
        let encoded =
            mmrecode_mjpeg::encode_jpeg(&frame, mmrecode_mjpeg::JpegEncodeOptions { quality })
                .map_err(|error| error.to_string())?;
        let report = mmrecode_quality::compare_video_frames(&frame, &encoded.reconstructed)
            .map_err(|error| error.to_string())?;
        println!("Frame {}: {}", frame_count + 1, quality_summary(&report));
        encoded_stream.extend_from_slice(&encoded.data);
        frame_count += 1;
    }
    if frame_count == 0 {
        return Err("Y4M input contains no frames".to_owned());
    }
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(output)
            .map_err(|error| format!("cannot create '{}': {error}", output.display()))?,
    );
    file.write_all(&encoded_stream)
        .and_then(|()| file.flush())
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Encoded {frame_count} Y4M frame(s) at quality {quality} to {} ({} bytes)",
        output.display(),
        encoded_stream.len()
    );
    Ok(())
}

fn verify(input: &std::path::Path, reference: Option<&std::path::Path>) -> Result<(), String> {
    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let mut reference_reader = reference
        .map(|path| {
            let file = std::fs::File::open(path)
                .map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
            Ok::<_, String>(mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file)))
        })
        .transpose()?;
    let mut remaining = bytes.as_slice();
    let mut frame_count = 0_usize;
    while !remaining.is_empty() {
        let structure = mmrecode_mjpeg::parse_jpeg(remaining).map_err(|error| error.to_string())?;
        let consumed = remaining.len() - structure.trailing_data.len();
        if consumed == 0 {
            return Err("JPEG parser did not consume input".to_owned());
        }
        let frame = mmrecode_mjpeg::decode_jpeg(&remaining[..consumed])
            .and_then(mmrecode_mjpeg::DecodedJpeg::into_video_frame)
            .map_err(|error| error.to_string())?;
        frame_count += 1;
        println!(
            "Frame {frame_count}: {}x{} {:?}, {consumed} bytes, {} segment(s)",
            frame.width,
            frame.height,
            frame.format,
            structure.segments.len()
        );
        if let Some(reader) = &mut reference_reader {
            let expected = reader
                .read_frame()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("reference Y4M has fewer than {frame_count} frames"))?;
            let report = mmrecode_quality::compare_video_frames(&expected, &frame)
                .map_err(|error| error.to_string())?;
            print_quality_report(frame_count, &report);
        }
        remaining = &remaining[consumed..];
    }
    if frame_count == 0 {
        return Err("input contains no JPEG frames".to_owned());
    }
    if let Some(reader) = &mut reference_reader
        && reader
            .read_frame()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("reference Y4M has more frames than the JPEG input".to_owned());
    }
    println!("Verification passed for {frame_count} frame(s)");
    Ok(())
}

fn compare_y4m(reference: &std::path::Path, candidate: &std::path::Path) -> Result<(), String> {
    let reference_file = std::fs::File::open(reference)
        .map_err(|error| format!("cannot open '{}': {error}", reference.display()))?;
    let candidate_file = std::fs::File::open(candidate)
        .map_err(|error| format!("cannot open '{}': {error}", candidate.display()))?;
    let mut reference_reader =
        mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(reference_file));
    let mut candidate_reader =
        mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(candidate_file));
    let mut frame_count = 0_usize;
    loop {
        let reference_frame = reference_reader
            .read_frame()
            .map_err(|error| error.to_string())?;
        let candidate_frame = candidate_reader
            .read_frame()
            .map_err(|error| error.to_string())?;
        match (reference_frame, candidate_frame) {
            (Some(reference_frame), Some(candidate_frame)) => {
                frame_count += 1;
                let report =
                    mmrecode_quality::compare_video_frames(&reference_frame, &candidate_frame)
                        .map_err(|error| error.to_string())?;
                print_quality_report(frame_count, &report);
            }
            (None, None) => break,
            (Some(_), None) => return Err("candidate Y4M has fewer frames".to_owned()),
            (None, Some(_)) => return Err("candidate Y4M has more frames".to_owned()),
        }
    }
    if frame_count == 0 {
        return Err("Y4M inputs contain no frames".to_owned());
    }
    println!("Compared {frame_count} frame(s)");
    Ok(())
}

fn print_quality_report(frame_number: usize, report: &mmrecode_quality::FrameQualityReport) {
    println!(
        "  Frame {frame_number} quality: {}",
        quality_summary(report)
    );
    for plane in &report.planes {
        let psnr = if plane.psnr.is_infinite() {
            "exact".to_owned()
        } else {
            format!("{:.3} dB", plane.psnr)
        };
        println!(
            "    Plane {}: PSNR {psnr}, MSE {:.4}, max error {}",
            plane.plane_index, plane.mean_squared_error, plane.maximum_absolute_error
        );
    }
}

fn quality_summary(report: &mmrecode_quality::FrameQualityReport) -> String {
    let psnr = if report.psnr.is_infinite() {
        "exact".to_owned()
    } else {
        format!("{:.3} dB", report.psnr)
    };
    format!(
        "PSNR {psnr}, MSE {:.4}, max error {}",
        report.mean_squared_error, report.maximum_absolute_error
    )
}

fn inspect(path: &std::path::Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if bytes.is_empty() {
        return Err("input contains no JPEG frames".to_owned());
    }
    let multiple_frames = !mmrecode_mjpeg::parse_jpeg(&bytes)
        .map_err(|error| error.to_string())?
        .trailing_data
        .is_empty();
    let mut remaining = bytes.as_slice();
    let mut frame_count = 0_usize;
    let mut file_offset = 0_usize;
    while !remaining.is_empty() {
        let boundary = mmrecode_mjpeg::parse_jpeg(remaining).map_err(|error| error.to_string())?;
        let consumed = remaining.len() - boundary.trailing_data.len();
        let image = mmrecode_mjpeg::parse_jpeg(&remaining[..consumed])
            .map_err(|error| error.to_string())?;
        frame_count += 1;
        if multiple_frames {
            println!(
                "Motion JPEG frame {frame_count} at file offset 0x{file_offset:08x} (marker offsets below are frame-relative)"
            );
        }
        print!("{}", inspection_report(path, consumed, &image));
        remaining = &remaining[consumed..];
        file_offset += consumed;
    }
    if multiple_frames {
        println!("Motion JPEG frames: {frame_count}");
    }
    Ok(())
}

fn inspection_report(
    path: &std::path::Path,
    byte_length: usize,
    image: &mmrecode_mjpeg::JpegImage,
) -> String {
    use std::fmt::Write as _;

    let mut report = String::new();
    let _ = writeln!(report, "JPEG: {}", path.display());
    let _ = writeln!(report, "File size: {byte_length} bytes");
    if let Some(frame) = image.frame_header() {
        let _ = writeln!(
            report,
            "Frame: {}x{}, {}-bit baseline sequential DCT, {} component(s)",
            frame.width,
            frame.height,
            frame.sample_precision,
            frame.components.len()
        );
        for component in &frame.components {
            let _ = writeln!(
                report,
                "  Component {}: sampling {}x{}, quantization table {}",
                component.id,
                component.horizontal_sampling,
                component.vertical_sampling,
                component.quantization_table
            );
        }
    } else {
        let _ = writeln!(report, "Frame: no baseline SOF0 header");
    }
    if let Some(jfif) = image.jfif_header() {
        let _ = writeln!(
            report,
            "JFIF: {}.{:02}, density {}x{} (unit {})",
            jfif.version_major,
            jfif.version_minor,
            jfif.density_x,
            jfif.density_y,
            jfif.density_units
        );
    }

    let _ = writeln!(report, "Segments:");
    for segment in &image.segments {
        append_segment(&mut report, segment);
    }

    let _ = writeln!(report, "Entropy scans:");
    for (index, scan) in image.entropy_scans.iter().enumerate() {
        let _ = writeln!(
            report,
            "  {}: offset 0x{:08x}, {} source bytes, {} restart marker(s)",
            index + 1,
            scan.data_offset,
            scan.data_length,
            scan.restart_markers.len()
        );
    }
    if !image.trailing_data.is_empty() {
        let _ = writeln!(
            report,
            "Trailing data: {} byte(s) after EOI",
            image.trailing_data.len()
        );
    }
    report
}

fn append_segment(report: &mut String, segment: &mmrecode_mjpeg::JpegSegment) {
    use std::fmt::Write as _;

    use mmrecode_mjpeg::{HuffmanTableClass, Marker, QuantizationPrecision, SegmentData};

    let marker_label = match segment.marker {
        Marker::Application(number) => format!("APP{number}"),
        Marker::Restart(number) => format!("RST{number}"),
        Marker::Other(code) => format!("0x{code:02x}"),
        marker => marker.name().to_owned(),
    };
    let _ = write!(report, "  0x{:08x}  {marker_label}", segment.offset);
    if segment.payload_offset.is_some() {
        let _ = write!(report, "  payload {} bytes", segment.payload_length);
    }
    match &segment.data {
        SegmentData::QuantizationTables(tables) => {
            let details = tables
                .iter()
                .map(|table| {
                    let bits = match table.precision {
                        QuantizationPrecision::EightBit => 8,
                        QuantizationPrecision::SixteenBit => 16,
                    };
                    format!("Q{} ({bits}-bit)", table.id)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(report, "  [{details}]");
        }
        SegmentData::HuffmanTables(tables) => {
            let details = tables
                .iter()
                .map(|table| {
                    let class = match table.class {
                        HuffmanTableClass::Dc => "DC",
                        HuffmanTableClass::Ac => "AC",
                    };
                    format!("{class}{} ({} symbols)", table.id, table.symbols.len())
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(report, "  [{details}]");
        }
        SegmentData::RestartInterval(interval) => {
            let _ = write!(report, "  {interval} MCU(s)");
        }
        SegmentData::Scan(scan) => {
            let _ = write!(report, "  {} component(s)", scan.components.len());
        }
        SegmentData::Comment(comment) => {
            let _ = write!(report, "  {} comment byte(s)", comment.len());
        }
        SegmentData::Empty
        | SegmentData::Frame(_)
        | SegmentData::Jfif(_)
        | SegmentData::Application(_)
        | SegmentData::Unknown(_) => {}
    }
    report.push('\n');
}

fn print_help() {
    println!(
        "MMRecode media-codec tools\n\n\
         Usage: mmrecode <command> [arguments]\n\n\
         Available commands:\n  inspect <jpeg-file>  Inspect JPEG syntax without decoding pixels\n  \
         decode <mjpg> <y4m>  Decode baseline JPEG frame(s) to YUV4MPEG2\n  \
         encode <y4m> <mjpg> [quality]  Encode Y4M frame(s) as baseline JPEG\n  \
         verify <mjpg> [reference.y4m]  Verify syntax, reconstruction, and optional quality\n  \
         compare <reference.y4m> <candidate.y4m>  Compare decoded frame quality\n  \
         help                 Show this help\n  version              Show the version\n\n\
         Planned commands:\n  edit\n  benchmark"
    );
}

#[cfg(test)]
mod tests {
    use mmrecode_mjpeg::parse_jpeg;

    use super::inspection_report;

    #[test]
    fn report_includes_frame_and_marker_offsets() {
        let bytes = include_bytes!("../../../testdata/jpeg/valid/baseline-420.jpg");
        let image = parse_jpeg(bytes).expect("valid checked-in JPEG");
        let report = inspection_report(std::path::Path::new("sample.jpg"), bytes.len(), &image);
        assert!(report.contains("Frame: 16x16, 8-bit baseline sequential DCT"));
        assert!(report.contains("SOF0"));
    }
}
