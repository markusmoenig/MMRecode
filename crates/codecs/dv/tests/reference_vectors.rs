//! Regression coverage using independently encoded raw-DV frames.

use mmrecode_dv::{
    DvProfile, DvVideoDecodeOptions, decode_video, decode_video_with_options, encode_video,
    encode_video_with_audio, extract_audio, parse_frame,
};

const NTSC: &[u8] = include_bytes!("../../../../testdata/dv/valid/dv25-525-60-one-frame.dv");
const PAL: &[u8] = include_bytes!("../../../../testdata/dv/valid/dv25-625-50-one-frame.dv");

#[test]
fn parses_independent_dv25_vectors_without_layout_issues() {
    for (data, profile, samples) in [
        (NTSC, DvProfile::DV25_525_60, 1_600),
        (PAL, DvProfile::DV25_625_50, 1_920),
    ] {
        let frame = parse_frame(data).expect("independent FFmpeg DV frame");
        assert_eq!(frame.profile(), profile);
        assert!(frame.issues().is_empty());
        assert_eq!(frame.blocks().len(), profile.block_count());
        let audio = extract_audio(&frame).expect("embedded audio");
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].sample_rate, 48_000);
        assert_eq!(audio[0].samples_per_channel, samples);
        audio[0].validate().expect("valid interleaved layout");
        assert!(audio[0].samples.iter().any(|&sample| sample != 0));
        let video = decode_video(&frame).expect("native DV picture reconstruction");
        assert_eq!(video.width, profile.width);
        assert_eq!(video.height, profile.height);
        assert_eq!(video.format, profile.pixel_format);
        assert!(video.planes[0].data.iter().any(|&sample| sample != 0));
    }
}

#[test]
fn native_reconstruction_is_close_to_ffmpeg_when_available() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        return;
    }
    compare_with_ffmpeg("dv25-525-60-one-frame.dv", NTSC, DvProfile::DV25_525_60);
    compare_with_ffmpeg("dv25-625-50-one-frame.dv", PAL, DvProfile::DV25_625_50);
}

#[test]
fn damaged_video_segment_can_be_reported_and_concealed() {
    let mut damaged = NTSC.to_vec();
    damaged[7 * 80] = 0x76;
    let parsed = parse_frame(&damaged).unwrap();
    assert!(decode_video(&parsed).is_err());
    let decoded = decode_video_with_options(
        &parsed,
        DvVideoDecodeOptions {
            conceal_errors: true,
        },
    )
    .unwrap();
    assert_eq!(decoded.concealed_segments.len(), 1);
    assert_eq!(decoded.concealed_segments[0].sequence, 0);
    assert_eq!(decoded.concealed_segments[0].slot, 0);
}

#[test]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn encoded_reconstruction_is_deterministic_and_ffmpeg_decodable() {
    use std::io::Write as _;
    use std::process::Stdio;

    for (data, profile, pixel_format) in [
        (NTSC, DvProfile::DV25_525_60, "yuv411p"),
        (PAL, DvProfile::DV25_625_50, "yuv420p"),
    ] {
        let parsed_source = parse_frame(data).unwrap();
        let source = decode_video(&parsed_source).unwrap();
        let source_audio = extract_audio(&parsed_source).unwrap().remove(0);
        let first = encode_video(&source).expect("encode decoded reference frame");
        let second = encode_video(&source).expect("repeat deterministic encode");
        assert_eq!(first.data, second.data);
        assert_eq!(first.data.len(), profile.frame_size);
        let with_audio = encode_video_with_audio(&source, &source_audio).unwrap();
        let round_trip_audio = extract_audio(&parse_frame(&with_audio.data).unwrap()).unwrap();
        assert_eq!(
            round_trip_audio.as_slice(),
            std::slice::from_ref(&source_audio)
        );

        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            continue;
        }
        let mut audio_child = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "dv",
                "-i",
                "pipe:0",
                "-map",
                "0:a:0",
                "-c:a",
                "pcm_s16le",
                "-f",
                "s16le",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start FFmpeg audio decode");
        audio_child
            .stdin
            .take()
            .unwrap()
            .write_all(&with_audio.data)
            .unwrap();
        let audio_output = audio_child
            .wait_with_output()
            .expect("wait for FFmpeg audio");
        assert!(
            audio_output.status.success(),
            "FFmpeg rejected encoded DV audio: {}",
            String::from_utf8_lossy(&audio_output.stderr)
        );
        let expected_audio: Vec<u8> = source_audio
            .samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        assert_eq!(audio_output.stdout, expected_audio);

        let mut child = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "dv",
                "-i",
                "pipe:0",
                "-map",
                "0:v:0",
                "-frames:v",
                "1",
                "-pix_fmt",
                pixel_format,
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start FFmpeg");
        child.stdin.take().unwrap().write_all(&first.data).unwrap();
        let output = child.wait_with_output().expect("wait for FFmpeg");
        assert!(
            output.status.success(),
            "FFmpeg rejected encoded DV: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected_length: usize = first
            .reconstructed
            .planes
            .iter()
            .map(|plane| plane.width * plane.height)
            .sum();
        assert_eq!(output.stdout.len(), expected_length);
        let internal: Vec<u8> = first
            .reconstructed
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect();
        let maximum_decoder_difference = internal
            .iter()
            .zip(&output.stdout)
            .map(|(&ours, &ffmpeg)| ours.abs_diff(ffmpeg))
            .max()
            .unwrap_or(0);
        assert!(
            maximum_decoder_difference <= 1,
            "encoder reconstruction differs from FFmpeg by {maximum_decoder_difference}"
        );
        for (plane_index, (original, reconstructed)) in source
            .planes
            .iter()
            .zip(&first.reconstructed.planes)
            .enumerate()
        {
            let sum_squared: u64 = original
                .data
                .iter()
                .zip(&reconstructed.data)
                .map(|(&a, &b)| {
                    let difference = a.abs_diff(b);
                    u64::from(difference) * u64::from(difference)
                })
                .sum();
            let mse = sum_squared as f64 / original.data.len() as f64;
            eprintln!(
                "encoded {:?} plane {plane_index}: MSE {mse:.3}",
                profile.system
            );
            assert!(mse < 100.0, "encoded plane MSE {mse}");
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn compare_with_ffmpeg(name: &str, data: &[u8], profile: DvProfile) {
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let input = repository.join("testdata/dv/valid").join(name);
    let pixel_format = match profile.system {
        mmrecode_dv::DvSystem::System525_60 => "yuv411p",
        mmrecode_dv::DvSystem::System625_50 => "yuv420p",
    };
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-pix_fmt",
            pixel_format,
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .expect("run FFmpeg DV decoder");
    assert!(
        output.status.success(),
        "FFmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed = parse_frame(data).unwrap();
    let decoded = decode_video(&parsed).unwrap();
    let mut offset = 0;
    for (index, plane) in decoded.planes.iter().enumerate() {
        let length = plane.width * plane.height;
        let reference = &output.stdout[offset..offset + length];
        offset += length;
        let (sum_squared, maximum) = plane.data.iter().zip(reference).fold(
            (0_u64, 0_u8),
            |(sum, maximum), (&actual, &expected)| {
                let difference = actual.abs_diff(expected);
                (
                    sum + u64::from(difference) * u64::from(difference),
                    maximum.max(difference),
                )
            },
        );
        let mse = sum_squared as f64 / length as f64;
        eprintln!("{name} plane {index}: MSE {mse:.6}, max {maximum}");
        assert!(mse < 2.0, "{name} plane {index} MSE {mse}");
        assert!(maximum <= 8, "{name} plane {index} max error {maximum}");
    }
    assert_eq!(offset, output.stdout.len());
}
