#![allow(missing_docs)]
#![allow(clippy::cast_precision_loss)]

use mmrecode_mpeg2::{
    ChromaFormat, Mpeg2EncodeOptions, PictureStructure, PictureType, SmartRenderDisposition,
    analyze_dependencies, decode_stream, encode_stream, parse_stream, plan_smart_render,
};

const PROGRESSIVE: &[u8] =
    include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
const INTERLACED: &[u8] =
    include_bytes!("../../../../testdata/mpeg2/valid/main-ml-interlaced-ibp.m2v");
const OPEN_GOP: &[u8] =
    include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-open-gop.m2v");

#[test]
fn parses_progressive_main_profile_ibp_vector() {
    let stream = parse_stream(PROGRESSIVE).expect("parse progressive MPEG-2 vector");
    assert_eq!(stream.pictures().len(), 12);
    assert_eq!(stream.groups().len(), 3);
    let sequence = &stream.pictures()[0].sequence;
    assert_eq!((sequence.width, sequence.height), (96, 64));
    assert_eq!(sequence.chroma_format, ChromaFormat::Yuv420);
    assert!(sequence.progressive_sequence);
    assert_eq!(sequence.profile_and_level_indication, 0x48);
    assert_eq!(sequence.frame_rate.numerator(), 25);
    assert_eq!(sequence.frame_rate.denominator(), 1);
    assert!(
        stream
            .pictures()
            .iter()
            .any(|picture| { picture.header.picture_coding_type == PictureType::I })
    );
    assert!(
        stream
            .pictures()
            .iter()
            .any(|picture| { picture.header.picture_coding_type == PictureType::P })
    );
    assert!(
        stream
            .pictures()
            .iter()
            .any(|picture| { picture.header.picture_coding_type == PictureType::B })
    );
    assert!(
        stream
            .pictures()
            .iter()
            .all(|picture| picture.coding_extension.picture_structure == PictureStructure::Frame)
    );
    assert!(
        stream
            .pictures()
            .iter()
            .all(|picture| picture.coding_extension.progressive_frame)
    );
}

#[test]
fn parses_interlaced_main_profile_ibp_vector() {
    let stream = parse_stream(INTERLACED).expect("parse interlaced MPEG-2 vector");
    assert_eq!(stream.pictures().len(), 12);
    assert!(!stream.pictures()[0].sequence.progressive_sequence);
    assert!(
        stream
            .pictures()
            .iter()
            .all(|picture| picture.coding_extension.picture_structure == PictureStructure::Frame)
    );
    assert!(
        stream
            .pictures()
            .iter()
            .all(|picture| !picture.coding_extension.progressive_frame)
    );
    assert!(
        stream
            .pictures()
            .iter()
            .any(|picture| !picture.coding_extension.frame_pred_frame_dct)
    );
}

#[test]
fn analyzes_open_gop_cross_dependencies_and_recovery_points() {
    let stream = parse_stream(OPEN_GOP).expect("parse open-GOP MPEG-2 vector");
    assert_eq!(stream.pictures().len(), 24);
    assert!(stream.groups()[0].closed_gop);
    assert!(stream.groups()[1..].iter().all(|group| !group.closed_gop));
    let access = analyze_dependencies(&stream).expect("analyze open GOPs");
    assert_eq!(
        access[0].random_access,
        mmrecode_core::RandomAccessKind::Clean
    );
    let recovery_i = access
        .iter()
        .find(|picture| picture.presentation_order == 12)
        .expect("second GOP I picture");
    assert_eq!(
        recovery_i.random_access,
        mmrecode_core::RandomAccessKind::Recovery
    );
    let leading_b = access
        .iter()
        .find(|picture| picture.presentation_order == 10)
        .expect("open-GOP leading B picture");
    assert_eq!(leading_b.references.len(), 2);
    assert!(leading_b.references[0].0 < recovery_i.picture_id.0);
    assert_eq!(leading_b.references[1], recovery_i.picture_id);

    let plan = plan_smart_render(&stream, 9..10).expect("plan across open GOP");
    assert_eq!(plan.encode_presentation_ranges.len(), 1);
    assert_eq!(plan.encode_presentation_ranges[0], 7..12);
    assert!(plan.pictures.iter().any(|picture| {
        picture.presentation_order == 10
            && matches!(
                picture.disposition,
                SmartRenderDisposition::BridgeEncode { .. }
            )
    }));
}

#[test]
fn reconstructs_open_gop_vector_like_ffmpeg() {
    compare_reconstruction_with_ffmpeg(OPEN_GOP);
}

#[test]
fn rejects_truncated_and_missing_picture_extensions() {
    assert!(parse_stream(&PROGRESSIVE[..10]).is_err());

    let picture_extension = PROGRESSIVE
        .windows(4)
        .position(|bytes| bytes == [0, 0, 1, 0xb5])
        .expect("extension start code");
    let mut damaged = PROGRESSIVE.to_vec();
    damaged[picture_extension + 4] &= 0x0f;
    assert!(parse_stream(&damaged).is_err());
}

#[test]
fn rejects_malformed_slice_headers_and_entropy() {
    let first_slice = PROGRESSIVE
        .windows(4)
        .position(|bytes| bytes == [0, 0, 1, 0x01])
        .expect("slice start code");
    let mut zero_qscale = PROGRESSIVE.to_vec();
    zero_qscale[first_slice + 4] &= 0x07;
    assert!(parse_stream(&zero_qscale).is_err());

    let next_start = PROGRESSIVE[first_slice + 4..]
        .windows(3)
        .position(|bytes| bytes == [0, 0, 1])
        .map(|offset| first_slice + 4 + offset)
        .expect("next start code");
    let mut invalid_entropy = PROGRESSIVE.to_vec();
    invalid_entropy[first_slice + 5..next_start].fill(0);
    assert!(parse_stream(&invalid_entropy).is_ok());
    assert!(decode_stream(&invalid_entropy).is_err());
}

#[test]
fn truncated_prefixes_fail_without_panicking() {
    for length in 0..256.min(PROGRESSIVE.len()) {
        let _ = parse_stream(&PROGRESSIVE[..length]);
        let _ = decode_stream(&PROGRESSIVE[..length]);
    }
}

#[test]
fn reconstructs_progressive_main_profile_vector() {
    let decoded = decode_stream(PROGRESSIVE).expect("decode progressive MPEG-2 vector");
    assert_eq!(decoded.len(), 12);
    assert_eq!(decoded[0].presentation_order, 0);
    assert_eq!((decoded[0].frame.width, decoded[0].frame.height), (96, 64));
    assert_eq!(decoded[0].macroblocks.len(), 24);
}

#[test]
fn progressive_reconstruction_matches_ffmpeg() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping FFmpeg MPEG-2 comparison: ffmpeg is unavailable");
        return;
    }
    let decoded = decode_stream(PROGRESSIVE).expect("decode progressive MPEG-2 vector");
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "mpegvideo",
            "-i",
            "pipe:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start FFmpeg MPEG-2 decoder");
    child
        .stdin
        .take()
        .expect("FFmpeg stdin")
        .write_all(PROGRESSIVE)
        .expect("write MPEG-2 stream");
    let output = child
        .wait_with_output()
        .expect("wait for FFmpeg MPEG-2 decoder");
    assert!(
        output.status.success(),
        "FFmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ours: Vec<u8> = decoded
        .iter()
        .flat_map(|picture| picture.frame.planes.iter())
        .flat_map(|plane| plane.data.iter().copied())
        .collect();
    assert_eq!(ours.len(), output.stdout.len());
    let maximum = ours
        .iter()
        .zip(&output.stdout)
        .map(|(&ours, &reference)| ours.abs_diff(reference))
        .max()
        .unwrap_or(0);
    let squared_error: u64 = ours
        .iter()
        .zip(&output.stdout)
        .map(|(&ours, &reference)| u64::from(ours.abs_diff(reference)).pow(2))
        .sum();
    let mse = squared_error as f64 / ours.len() as f64;
    assert!(
        maximum <= 2 && mse <= 0.1,
        "native MPEG-2 reconstruction differs from FFmpeg: max={maximum}, mse={mse}"
    );
}

#[test]
fn reconstructs_interlaced_main_profile_vector() {
    let decoded = decode_stream(INTERLACED).expect("decode interlaced MPEG-2 vector");
    assert_eq!(decoded.len(), 12);
    assert_eq!(decoded[0].macroblocks.len(), 24);
}

#[test]
fn interlaced_reconstruction_matches_ffmpeg() {
    compare_reconstruction_with_ffmpeg(INTERLACED);
}

fn compare_reconstruction_with_ffmpeg(data: &[u8]) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping FFmpeg MPEG-2 comparison: ffmpeg is unavailable");
        return;
    }
    let decoded = decode_stream(data).expect("decode MPEG-2 vector");
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "mpegvideo",
            "-i",
            "pipe:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start FFmpeg MPEG-2 decoder");
    child
        .stdin
        .take()
        .expect("FFmpeg stdin")
        .write_all(data)
        .expect("write MPEG-2 stream");
    let output = child
        .wait_with_output()
        .expect("wait for FFmpeg MPEG-2 decoder");
    assert!(
        output.status.success(),
        "FFmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ours: Vec<u8> = decoded
        .iter()
        .flat_map(|picture| picture.frame.planes.iter())
        .flat_map(|plane| plane.data.iter().copied())
        .collect();
    assert_eq!(ours.len(), output.stdout.len());
    let maximum = ours
        .iter()
        .zip(&output.stdout)
        .map(|(&ours, &reference)| ours.abs_diff(reference))
        .max()
        .unwrap_or(0);
    let squared_error: u64 = ours
        .iter()
        .zip(&output.stdout)
        .map(|(&ours, &reference)| u64::from(ours.abs_diff(reference)).pow(2))
        .sum();
    let mse = squared_error as f64 / ours.len() as f64;
    assert!(
        maximum <= 2 && mse <= 0.1,
        "native MPEG-2 reconstruction differs from FFmpeg: max={maximum}, mse={mse}"
    );
}

#[test]
fn deterministic_native_encoder_emits_ibp_stream() {
    let source: Vec<_> = decode_stream(PROGRESSIVE)
        .expect("decode source MPEG-2")
        .into_iter()
        .map(|picture| picture.frame)
        .collect();
    let options = Mpeg2EncodeOptions::default();
    let first = encode_stream(&source, options).expect("encode MPEG-2 sequence");
    let second = encode_stream(&source, options).expect("repeat MPEG-2 sequence");
    assert_eq!(first.data, second.data);
    assert_eq!(first.reconstructed.len(), source.len());
    assert!(first.picture_types.contains(&PictureType::I));
    assert!(first.picture_types.contains(&PictureType::P));
    assert!(first.picture_types.contains(&PictureType::B));
    let parsed = parse_stream(&first.data).expect("parse native MPEG-2 output");
    assert_eq!(parsed.pictures().len(), source.len());
    assert_eq!(parsed.pictures()[0].sequence.bit_rate, Some(15_000_000));
    assert_eq!(
        parsed.pictures()[0].sequence.profile_and_level_indication,
        0x48
    );

    let mean_psnr = source
        .iter()
        .zip(&first.reconstructed)
        .map(|(source, reconstruction)| {
            mmrecode_quality::compare_video_frames(source, reconstruction)
                .expect("compare encoder reconstruction")
                .psnr
        })
        .sum::<f64>()
        / f64::from(u32::try_from(source.len()).expect("small vector"));
    assert!(
        mean_psnr >= 30.0,
        "native MPEG-2 encoder mean PSNR is {mean_psnr:.3} dB"
    );
    compare_reconstruction_with_ffmpeg(&first.data);
}

#[test]
fn main_level_encoder_rejects_unsupported_frame_rate() {
    let frame = decode_stream(PROGRESSIVE).unwrap().remove(0).frame;
    let error = encode_stream(
        &[frame],
        Mpeg2EncodeOptions {
            frame_rate: mmrecode_mpeg2::FrameRate::Fps60,
            ..Mpeg2EncodeOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("Main Level"));
}

#[test]
fn native_encoder_motion_vectors_interoperate_with_ffmpeg() {
    let mut reference = decode_stream(PROGRESSIVE).unwrap().remove(0).frame;
    for (plane_index, plane) in reference.planes.iter_mut().enumerate() {
        for y in 0..plane.height {
            for x in 0..plane.width {
                plane.data[y * plane.stride + x] = if plane_index == 0 {
                    u8::try_from((x * 17 + y * 29 + x * y * 3) & 0xff).unwrap()
                } else {
                    128
                };
            }
        }
    }
    let mut shifted = reference.clone();
    let luma = &reference.planes[0];
    for y in 0..luma.height {
        for x in 0..luma.width {
            shifted.planes[0].data[y * luma.stride + x] =
                luma.data[y * luma.stride + (x + 1).min(luma.width - 1)];
        }
    }
    let encoded = encode_stream(
        &[reference, shifted],
        Mpeg2EncodeOptions {
            b_frames: 0,
            motion_search_range: 1,
            ..Mpeg2EncodeOptions::default()
        },
    )
    .unwrap();
    let decoded = decode_stream(&encoded.data).unwrap();
    let p_picture = decoded
        .iter()
        .find(|picture| picture.picture_type == PictureType::P)
        .unwrap();
    assert!(
        p_picture
            .macroblocks
            .iter()
            .any(|macroblock| macroblock.forward_motion == Some([2, 0]))
    );
    compare_reconstruction_with_ffmpeg(&encoded.data);
}
