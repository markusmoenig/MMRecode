//! Generates the small JPEG regression corpus.

use std::{fs, io, path::Path, process::Command};

fn main() -> io::Result<()> {
    let root = Path::new("testdata/jpeg");
    fs::create_dir_all(root.join("valid"))?;
    fs::create_dir_all(root.join("parser"))?;
    fs::create_dir_all(root.join("invalid"))?;
    fs::create_dir_all(root.join("encoded"))?;
    fs::create_dir_all("testdata/y4m/valid")?;

    generate_with_ffmpeg(
        &root.join("valid/baseline-420.jpg"),
        "testsrc2=size=16x16:rate=1",
        "yuvj420p",
    )?;
    generate_with_ffmpeg(
        &root.join("valid/minimal-gray.jpg"),
        "color=c=gray:size=8x8:rate=1",
        "yuvj444p",
    )?;

    let baseline = fs::read(root.join("valid/baseline-420.jpg"))?;
    let mut app_marker = Vec::with_capacity(baseline.len() + 8);
    app_marker.extend_from_slice(&baseline[..2]);
    app_marker.extend_from_slice(&[0xff, 0xe2, 0x00, 0x06, 0xde, 0xad, 0xbe, 0xef]);
    app_marker.extend_from_slice(&baseline[2..]);
    fs::write(root.join("valid/unknown-app-marker.jpg"), app_marker)?;
    fs::write(
        root.join("valid/restart-interval.jpg"),
        baseline_restart_vector(),
    )?;

    fs::write(
        root.join("parser/stuffed-restarts.jpg"),
        parser_restart_vector(),
    )?;
    fs::write(
        root.join("invalid/invalid-length.jpg"),
        [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x01],
    )?;
    fs::write(
        root.join("invalid/truncated-marker.jpg"),
        [0xff, 0xd8, 0xff],
    )?;
    fs::write(
        root.join("invalid/missing-eoi.jpg"),
        baseline
            .strip_suffix(&[0xff, 0xd9])
            .expect("FFmpeg JPEG should end with EOI"),
    )?;
    generate_y4m(Path::new("testdata/y4m/valid/two-frame-420.y4m"))?;
    generate_with_mmrecode(
        Path::new("testdata/y4m/valid/two-frame-420.y4m"),
        &root.join("encoded/mmrecode-q85-420.mjpg"),
    )?;
    Ok(())
}

fn generate_with_ffmpeg(path: &Path, source: &str, pixel_format: &str) -> io::Result<()> {
    let status = Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            source,
            "-frames:v",
            "1",
            "-c:v",
            "mjpeg",
            "-pix_fmt",
            pixel_format,
            "-y",
        ])
        .arg(path)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "FFmpeg failed while generating {}",
            path.display()
        )))
    }
}

fn generate_y4m(path: &Path) -> io::Result<()> {
    command_result(
        Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=16x16:rate=1",
                "-frames:v",
                "2",
                "-pix_fmt",
                "yuvj420p",
                "-color_range",
                "pc",
                "-f",
                "yuv4mpegpipe",
                "-y",
            ])
            .arg(path),
        "FFmpeg Y4M generation",
    )
}

fn generate_with_mmrecode(input: &Path, output: &Path) -> io::Result<()> {
    command_result(
        Command::new("cargo")
            .args(["run", "--quiet", "-p", "mmrecode", "--", "encode"])
            .arg(input)
            .arg(output)
            .arg("85"),
        "MMRecode JPEG generation",
    )
}

fn command_result(command: &mut Command, description: &str) -> io::Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{description} failed")))
    }
}

fn parser_restart_vector() -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8];
    segment(
        0xc0,
        &[8, 0, 8, 0, 16, 3, 1, 0x21, 0, 2, 0x11, 0, 3, 0x11, 0],
        &mut jpeg,
    );
    segment(0xda, &[3, 1, 0, 2, 0, 3, 0, 0, 63, 0], &mut jpeg);
    jpeg.extend_from_slice(&[0x12, 0xff, 0x00, 0x34, 0xff, 0xd0, 0x56, 0xff, 0xd7]);
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    jpeg
}

fn baseline_restart_vector() -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8];

    let mut quantization = vec![0];
    quantization.extend([1; 64]);
    segment(0xdb, &quantization, &mut jpeg);
    segment(0xc0, &[8, 0, 8, 0, 16, 1, 1, 0x11, 0], &mut jpeg);

    let mut huffman = vec![0x00, 1];
    huffman.extend([0; 15]);
    huffman.push(0);
    huffman.extend([0x10, 1]);
    huffman.extend([0; 15]);
    huffman.push(0);
    segment(0xc4, &huffman, &mut jpeg);
    segment(0xdd, &[0, 1], &mut jpeg);
    segment(0xda, &[1, 1, 0, 0, 63, 0], &mut jpeg);

    // Each MCU contains one zero-DC block followed by EOB (`00`) and one-bit padding.
    jpeg.extend_from_slice(&[0x3f, 0xff, 0xd0, 0x3f, 0xff, 0xd9]);
    jpeg
}

fn segment(marker: u8, payload: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(&[0xff, marker]);
    let length = u16::try_from(payload.len() + 2).expect("test segment fits in JPEG length");
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(payload);
}
