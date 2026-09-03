#![allow(missing_docs)]

use std::process::Command;

use mmrecode_core::{Decoder, Packet, PacketFlags, Timestamp, VideoFrame};
use mmrecode_h264::H264Decoder;
use mmrecode_isobmff::IsoBmffFile;

#[test]
fn native_decoder_accepts_a_real_x264_zero_residual_idr() {
    let Some(frame) = decode_x264_frame(
        "nullsrc=size=50x34:rate=1,geq=lum=128:cb=128:cr=128",
        "flat",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (50, 34));
    assert!(
        frame
            .planes
            .iter()
            .all(|plane| plane.data.iter().all(|&sample| sample == 128))
    );
}

#[test]
fn native_decoder_accepts_real_x264_intra16_luma_residuals() {
    let Some(frame) = decode_x264_frame(
        "nullsrc=size=64x32:rate=1,geq=lum=64+64*gte(X\\,32):cb=128:cr=128",
        "luma-residual",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 32));
}

#[test]
fn native_decoder_accepts_real_x264_intra16_chroma_residuals() {
    let Some(frame) = decode_x264_frame(
        "nullsrc=size=64x32:rate=1,geq=lum=128:cb=64+64*gte(X\\,16):cr=64+64*gte(Y\\,8)",
        "chroma-residual",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 32));
}

#[test]
fn native_decoder_accepts_real_x264_intra4x4_texture() {
    let Some(frame) = decode_x264_frame(
        "nullsrc=size=64x32:rate=1,geq=lum=64+2*X:cb=128:cr=128",
        "intra4-texture",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 32));
}

#[test]
fn native_decoder_accepts_real_x264_multicolour_texture() {
    let Some(frame) = decode_x264_frame("testsrc2=size=64x48:rate=1", "multicolour-texture") else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 48));
}

#[test]
fn native_decoder_applies_real_x264_intra_deblocking() {
    let Some(frame) = decode_x264_frame_with_params(
        "testsrc2=size=64x48:rate=1",
        "intra-deblocking",
        "cabac=0:8x8dct=0:analyse=0",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 48));
}

#[test]
fn native_decoder_applies_real_x264_deblocking_offsets() {
    let Some(frame) = decode_x264_frame_with_params(
        "testsrc2=size=64x48:rate=1",
        "intra-deblocking-offsets",
        "cabac=0:8x8dct=0:analyse=0:deblock=2,-2",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 48));
}

#[test]
fn native_decoder_retains_a_reference_for_real_x264_p_skip() {
    let Some(frames) = decode_x264_two_frames(
        "color=c=gray:size=64x48:rate=2",
        "p-skip",
        "cabac=0:8x8dct=0:no-deblock=1:analyse=0",
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_cabac_p_skip() {
    let Some(frames) = decode_x264_frames_with_profile(
        "color=c=gray:size=64x48:rate=2",
        "cabac-p-skip",
        "cabac=1:8x8dct=0:no-deblock=1:analyse=0",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_cabac_p16_motion() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=64x48:rate=2,geq=lum='mod(max(0,X-2*N)*13+Y*7,256)':cb='mod(max(0,X-N)*9+Y*5,256)':cr='mod(max(0,X-N)*3+Y*11,256)'",
        "cabac-p16-motion",
        "cabac=1:qp=1:8x8dct=0:no-deblock=1:partitions=none:subme=7:ref=1",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_cabac_p16_residuals() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=64x48:rate=2,geq=lum=64+2*X+8*N:cb=96+N:cr=160-N",
        "cabac-p16-residuals",
        "cabac=1:8x8dct=0:partitions=none:subme=0:ref=1",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_cabac_p16x8_motion() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=64x48:rate=2,geq=lum='mod(if(lt(mod(Y,16),8),max(0,X-2*N),min(63,X+2*N))*13+Y*7,256)':cb=128:cr=128",
        "cabac-p16x8-motion",
        "cabac=1:qp=1:8x8dct=0:no-deblock=1:subme=7:ref=1:scenecut=0",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_partitioned_cabac_p_picture() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=64x48:rate=2,geq=lum='mod(X*13+if(lt(mod(X,16),8),max(0,Y-2*N),min(47,Y+2*N))*7,256)':cb=128:cr=128",
        "cabac-partitioned-p",
        "cabac=1:qp=1:8x8dct=0:no-deblock=1:subme=7:ref=1:scenecut=0",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_p16_motion_and_residuals() {
    let Some(frames) = decode_x264_two_frames(
        "nullsrc=size=64x48:rate=2,geq=lum=64+2*X+8*N:cb=96+N:cr=160-N",
        "p16-motion-residual",
        "cabac=0:8x8dct=0:no-deblock=1:partitions=none:subme=0",
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_p16_fractional_motion() {
    let Some(frames) = decode_x264_two_frames(
        "testsrc2=size=64x48:rate=2",
        "p16-fractional-motion",
        "cabac=0:8x8dct=0:no-deblock=1:partitions=none:subme=7",
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_deblocks_real_x264_mixed_p16_picture() {
    let Some(frames) = decode_x264_two_frames(
        "testsrc2=size=64x48:rate=2",
        "p16-deblocking",
        "cabac=0:8x8dct=0:partitions=none:subme=7",
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_real_x264_partitioned_p_picture() {
    let Some(frames) = decode_x264_two_frames(
        "testsrc2=size=96x64:rate=5",
        "partitioned-p",
        "cabac=0:8x8dct=0:ref=1:subme=7",
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_a_real_baseline_gop() {
    let Some(frames) = decode_x264_frames(
        "testsrc2=size=96x64:rate=12",
        "baseline-gop",
        "cabac=0:8x8dct=0:ref=1:subme=7",
        12,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_reconstructs_a_high_profile_cavlc_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=12",
        "high-cavlc-gop",
        "cabac=0:8x8dct=0:ref=1:subme=7",
        "high",
        12,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_reconstructs_a_cabac_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=12",
        "cabac-gop",
        "cabac=1:8x8dct=0:ref=1:subme=7",
        "high",
        12,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_reconstructs_a_cabac_ipcm_picture() {
    let Some(frame) = decode_x264_frame_with_profile(
        "nullsrc=size=16x16,geq=random(1)*255:random(2)*255:random(3)*255",
        "cabac-ipcm",
        "cabac=1:qp=0:8x8dct=0:no-deblock=1:keyint=1",
        "high",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (16, 16));
}

#[test]
fn native_decoder_reconstructs_mixed_cabac_intra_and_ipcm() {
    let Some(frame) = decode_x264_frame_with_profile(
        "nullsrc=size=64x48,geq=lum='mod(X*13+Y*7,256)':cb='mod(X*9+Y*5,256)':cr='mod(X*3+Y*11,256)'",
        "cabac-mixed-intra-ipcm",
        "cabac=1:qp=0:8x8dct=0:no-deblock=1:keyint=1",
        "high",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 48));
}

#[test]
fn native_decoder_reconstructs_a_lossless_cabac_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=64x48:rate=2,geq=lum='mod(X*13+Y*7+N*5,256)':cb='mod(X*9+Y*5+N*3,256)':cr='mod(X*3+Y*11-N*2,256)'",
        "cabac-lossless-gop",
        "cabac=1:qp=0:8x8dct=0:no-deblock=1:ref=1:scenecut=0",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_a_cabac_intra16_picture() {
    let Some(frame) = decode_x264_frame_with_profile(
        "nullsrc=size=32x16,geq=lum=128:cb=128:cr=128",
        "cabac-intra16-zero",
        "cabac=1:8x8dct=0:analyse=0:no-deblock=1:keyint=1",
        "high",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (32, 16));
}

#[test]
fn native_decoder_reconstructs_cabac_intra16_ac_residuals() {
    let Some(frame) = decode_x264_frame_with_profile(
        "nullsrc=size=32x32,geq=lum='128+8*lt(mod(X+Y,16),8)':cb='128+4*lt(mod(Y,8),4)':cr='128+4*lt(mod(X+Y,8),4)'",
        "cabac-intra16-ac",
        "cabac=1:qp=20:8x8dct=0:analyse=0:no-deblock=1:keyint=1",
        "high",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (32, 32));
}

#[test]
fn native_decoder_reconstructs_cabac_intra4_texture() {
    let Some(frame) = decode_x264_frame_with_profile(
        "nullsrc=size=32x32,geq=lum='mod(X*13+Y*7,256)':cb='mod(X*9+Y*5,256)':cr='mod(X*3+Y*11,256)'",
        "cabac-intra4",
        "cabac=1:qp=20:8x8dct=0:keyint=1",
        "high",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (32, 32));
}

#[test]
fn native_decoder_reconstructs_cabac_intra8_texture() {
    let Some(frame) = decode_x264_frame_with_profile(
        "testsrc2=size=64x48:rate=1",
        "cabac-intra8",
        "cabac=1:qp=20:8x8dct=1:keyint=1",
        "high",
    ) else {
        return;
    };
    assert_eq!((frame.width, frame.height), (64, 48));
}

#[test]
fn native_decoder_reconstructs_a_cabac_8x8_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=12",
        "cabac-8x8-gop",
        "cabac=1:8x8dct=1:ref=1:subme=7",
        "high",
        12,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_reconstructs_a_cabac_jvt_scaling_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=12",
        "cabac-jvt-scaling-gop",
        "cabac=1:8x8dct=1:ref=1:subme=7:cqm=jvt:no-deblock=1",
        "high",
        12,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

fn decode_x264_two_frames(
    filter: &str,
    suffix: &str,
    encoder_params: &str,
) -> Option<Vec<VideoFrame>> {
    decode_x264_frames(filter, suffix, encoder_params, 2)
}

fn decode_x264_frames(
    filter: &str,
    suffix: &str,
    encoder_params: &str,
    frame_count: usize,
) -> Option<Vec<VideoFrame>> {
    decode_x264_frames_with_profile(filter, suffix, encoder_params, "baseline", frame_count)
}

fn decode_x264_frames_with_profile(
    filter: &str,
    suffix: &str,
    encoder_params: &str,
    profile: &str,
    frame_count: usize,
) -> Option<Vec<VideoFrame>> {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping native H.264 interoperability test: ffmpeg is unavailable");
        return None;
    }
    let movie_path = std::env::temp_dir().join(format!(
        "mmrecode-native-h264-{suffix}-{}.mp4",
        std::process::id()
    ));
    let encoded = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            filter,
            "-frames:v",
        ])
        .arg(frame_count.to_string())
        .args([
            "-c:v",
            "libx264",
            "-profile:v",
            profile,
            "-pix_fmt",
            "yuv420p",
            "-bf",
            "0",
            "-g",
            "30",
            "-x264-params",
            encoder_params,
        ])
        .arg("-y")
        .arg(&movie_path)
        .status()
        .expect("run x264 P-picture interoperability encoder");
    if !encoded.success() {
        eprintln!("skipping native H.264 interoperability test: libx264 is unavailable");
        return None;
    }

    let movie = IsoBmffFile::open(&movie_path).unwrap();
    let track = movie.h264_track().unwrap();
    assert_eq!(track.samples.len(), frame_count);
    let mut decoder = H264Decoder::default();
    decoder.configure(&track.descriptor.codec).unwrap();
    let mut frames = Vec::new();
    for sample in &track.samples {
        decoder
            .send_packet(Packet {
                stream_id: track.descriptor.id,
                data: movie.sample_data(sample).unwrap().to_vec(),
                pts: Some(Timestamp {
                    value: sample.pts,
                    time_base: track.descriptor.time_base,
                }),
                dts: Some(Timestamp {
                    value: sample.dts,
                    time_base: track.descriptor.time_base,
                }),
                duration: Some(Timestamp {
                    value: i64::from(sample.duration),
                    time_base: track.descriptor.time_base,
                }),
                flags: if sample.is_sync {
                    PacketFlags::KEY
                } else {
                    PacketFlags::empty()
                },
                side_data: Vec::new(),
            })
            .unwrap();
        frames.push(decoder.receive_frame().unwrap().unwrap());
    }
    let independent = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&movie_path)
        .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "pipe:1"])
        .output()
        .expect("decode P-skip vector independently");
    assert!(independent.status.success());
    let native = frames
        .iter()
        .flat_map(|frame| frame.planes.iter())
        .flat_map(|plane| plane.data.iter().copied())
        .collect::<Vec<_>>();
    assert_frame_bytes_match(&native, &independent.stdout);
    let _ = std::fs::remove_file(movie_path);
    Some(frames)
}

fn assert_frame_bytes_match(native: &[u8], independent: &[u8]) {
    if native == independent {
        return;
    }
    let mismatch = native
        .iter()
        .zip(independent)
        .position(|(native, independent)| native != independent)
        .expect("different frame buffers have a mismatch");
    panic!(
        "native H.264 sequence mismatch at byte {mismatch}: native={}, independent={}, nearby native={:?}, independent={:?}",
        native[mismatch],
        independent[mismatch],
        &native[mismatch.saturating_sub(8)..(mismatch + 16).min(native.len())],
        &independent[mismatch.saturating_sub(8)..(mismatch + 16).min(independent.len())]
    );
}

fn decode_x264_frame(filter: &str, suffix: &str) -> Option<VideoFrame> {
    decode_x264_frame_with_params(filter, suffix, "cabac=0:8x8dct=0:no-deblock=1:analyse=0")
}

fn decode_x264_frame_with_params(
    filter: &str,
    suffix: &str,
    encoder_params: &str,
) -> Option<VideoFrame> {
    decode_x264_frame_with_profile(filter, suffix, encoder_params, "baseline")
}

fn decode_x264_frame_with_profile(
    filter: &str,
    suffix: &str,
    encoder_params: &str,
    profile: &str,
) -> Option<VideoFrame> {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping native H.264 interoperability test: ffmpeg is unavailable");
        return None;
    }
    let movie_path = std::env::temp_dir().join(format!(
        "mmrecode-native-h264-{suffix}-{}.mp4",
        std::process::id()
    ));
    let encoded = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            filter,
            "-frames:v",
            "1",
            "-c:v",
            "libx264",
            "-profile:v",
            profile,
            "-pix_fmt",
            "yuv420p",
            "-bf",
            "0",
            "-g",
            "1",
            "-x264-params",
            encoder_params,
        ])
        .arg("-y")
        .arg(&movie_path)
        .status()
        .expect("run x264 interoperability encoder");
    if !encoded.success() {
        eprintln!("skipping native H.264 interoperability test: libx264 is unavailable");
        return None;
    }

    let movie = IsoBmffFile::open(&movie_path).unwrap();
    let track = movie.h264_track().unwrap();
    let sample = &track.samples[0];
    let time_base = track.descriptor.time_base;
    let mut decoder = H264Decoder::default();
    decoder.configure(&track.descriptor.codec).unwrap();
    decoder
        .send_packet(Packet {
            stream_id: track.descriptor.id,
            data: movie.sample_data(sample).unwrap().to_vec(),
            pts: Some(Timestamp {
                value: sample.pts,
                time_base,
            }),
            dts: Some(Timestamp {
                value: sample.dts,
                time_base,
            }),
            duration: Some(Timestamp {
                value: i64::from(sample.duration),
                time_base,
            }),
            flags: PacketFlags::KEY,
            side_data: Vec::new(),
        })
        .unwrap();
    let frame = decoder.receive_frame().unwrap().unwrap();
    let independent = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&movie_path)
        .args([
            "-frames:v",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .output()
        .expect("decode native H.264 vector independently");
    assert!(independent.status.success());
    assert_frame_matches(&frame, &independent.stdout);
    let _ = std::fs::remove_file(movie_path);
    Some(frame)
}

fn assert_frame_matches(frame: &VideoFrame, independent: &[u8]) {
    let native = frame
        .planes
        .iter()
        .flat_map(|plane| plane.data.iter().copied())
        .collect::<Vec<_>>();
    if native == independent {
        return;
    }
    let mismatch = native
        .iter()
        .zip(independent)
        .position(|(native, independent)| native != independent)
        .expect("different frame buffers have a mismatch");
    panic!(
        "native H.264 mismatch at planar byte {mismatch}: native={}, independent={}, nearby native={:?}, independent={:?}",
        native[mismatch],
        independent[mismatch],
        &native[mismatch.saturating_sub(8)..(mismatch + 16).min(native.len())],
        &independent[mismatch.saturating_sub(8)..(mismatch + 16).min(independent.len())]
    );
}
