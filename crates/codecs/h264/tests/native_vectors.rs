#![allow(missing_docs)]

use std::process::Command;

use mmrecode_core::{Decoder, FieldOrder, Packet, PacketFlags, Timestamp, VideoFrame};
use mmrecode_h264::{
    AvcDecoderConfigurationRecord, H264Decoder, NalUnitType, length_prefixed_nal_units,
    nal_units_to_annex_b, parse_sps,
};
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
fn native_decoder_reconstructs_a_multislice_cavlc_idr() {
    let frame = decode_x264_frame_with_params(
        "testsrc2=size=96x64:rate=1",
        "cavlc-multislice-idr",
        "cabac=0:slices=4:keyint=1:min-keyint=1:scenecut=0",
    );
    if let Some(frame) = frame {
        assert_eq!((frame.width, frame.height), (96, 64));
    }
}

#[test]
fn native_decoder_reconstructs_a_multislice_cabac_idr() {
    let frame = decode_x264_frame_with_params(
        "testsrc2=size=96x64:rate=1",
        "cabac-multislice-idr",
        "cabac=1:slices=4:keyint=1:min-keyint=1:scenecut=0",
    );
    if let Some(frame) = frame {
        assert_eq!((frame.width, frame.height), (96, 64));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn native_decoder_reconstructs_a_multislice_cabac_open_gop_i_picture() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping native H.264 interoperability test: ffmpeg is unavailable");
        return;
    }
    let movie_path = std::env::temp_dir().join(format!(
        "mmrecode-native-h264-multislice-open-gop-i-{}.mp4",
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
            "testsrc2=size=96x64:rate=8",
            "-frames:v",
            "16",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
            "-bf",
            "2",
            "-g",
            "8",
            "-keyint_min",
            "8",
            "-x264-params",
            "cabac=1:slices=4:open-gop=1:scenecut=0",
            "-y",
        ])
        .arg(&movie_path)
        .status()
        .expect("run x264 open-GOP multi-slice encoder");
    if !encoded.success() {
        eprintln!("skipping native H.264 interoperability test: libx264 is unavailable");
        return;
    }

    let movie = IsoBmffFile::open(&movie_path).unwrap();
    let track = movie.h264_track().unwrap();
    let configuration =
        AvcDecoderConfigurationRecord::parse(&track.descriptor.codec.configuration).unwrap();
    let sample = track
        .samples
        .iter()
        .filter(|sample| sample.is_sync)
        .nth(1)
        .expect("x264 open GOP contains a second intra random-access sample");
    let sample_data = movie.sample_data(sample).unwrap();
    let units = length_prefixed_nal_units(sample_data, configuration.length_size).unwrap();
    let slices = units
        .iter()
        .filter(|unit| unit.header.unit_type == NalUnitType::CodedSlice)
        .count();
    assert_eq!(slices, 4);
    assert!(
        units
            .iter()
            .all(|unit| unit.header.unit_type != NalUnitType::IdrSlice),
        "open-GOP intra vector must exercise non-IDR slices"
    );

    let mut decoder = H264Decoder::default();
    decoder.configure(&track.descriptor.codec).unwrap();
    decoder
        .send_packet(Packet {
            stream_id: track.descriptor.id,
            data: sample_data.to_vec(),
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
            flags: PacketFlags::KEY,
            side_data: Vec::new(),
        })
        .unwrap();
    let frame = decoder.receive_frame().unwrap().unwrap();
    let native = frame
        .planes
        .iter()
        .flat_map(|plane| plane.data.iter().copied())
        .collect::<Vec<_>>();

    let elementary_path = std::env::temp_dir().join(format!(
        "mmrecode-native-h264-multislice-open-gop-i-{}.h264",
        std::process::id()
    ));
    let mut elementary = configuration.parameter_sets_annex_b();
    elementary.extend(nal_units_to_annex_b(sample_data, configuration.length_size).unwrap());
    std::fs::write(&elementary_path, elementary).unwrap();
    let independent = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i"])
        .arg(&elementary_path)
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
        .expect("decode isolated open-GOP I picture independently");
    assert!(independent.status.success());
    assert_frame_bytes_match(&native, &independent.stdout);
    let _ = std::fs::remove_file(elementary_path);
    let _ = std::fs::remove_file(movie_path);
}

#[test]
fn native_decoder_reconstructs_a_multislice_cavlc_p_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=8",
        "cavlc-multislice-p-gop",
        "cabac=0:slices=4:8x8dct=0:ref=1:weightp=0:scenecut=0",
        "main",
        8,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_a_multislice_cabac_p_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=8",
        "cabac-multislice-p-gop",
        "cabac=1:slices=5:8x8dct=1:ref=1:weightp=0:partitions=all:subme=7:scenecut=0",
        "high",
        8,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_a_multislice_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "testsrc2=size=96x64:rate=8",
        "cavlc-multislice-b-gop",
        "cabac=0:slices=4:8x8dct=0:b-adapt=0:direct=spatial:weightb=1:weightp=0:ref=1:partitions=all:subme=7:scenecut=0",
        "main",
        12,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_reconstructs_a_multislice_cabac_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "testsrc2=size=96x64:rate=8",
        "cabac-multislice-b-gop",
        "cabac=1:slices=5:8x8dct=1:b-adapt=0:direct=spatial:weightb=1:weightp=0:ref=1:partitions=all:subme=7:scenecut=0",
        "high",
        12,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
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
fn native_decoder_reconstructs_a_two_reference_cavlc_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=12",
        "two-reference-cavlc-gop",
        "cabac=0:8x8dct=0:ref=2:weightp=0:subme=7:scenecut=0",
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
fn native_decoder_reconstructs_a_two_reference_cabac_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=96x64:rate=12,geq=lum='if(mod(N,2),mod(X*13+Y*7,256),mod(X*3+Y*17,256))':cb='if(mod(N,2),64,192)':cr='if(mod(N,2),192,64)'",
        "two-reference-cabac-gop",
        "cabac=1:8x8dct=0:ref=2:weightp=0:subme=7:scenecut=0",
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

#[test]
fn native_decoder_reconstructs_a_spatial_direct_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "color=c=gray:size=64x48:rate=6",
        "cavlc-spatial-direct-b-gop",
        "cabac=0:8x8dct=0:b-adapt=0:direct=spatial:weightb=0:weightp=0:ref=1:no-deblock=1:analyse=none:scenecut=0",
        "main",
        6,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 6);
}

#[test]
fn native_decoder_reconstructs_a_cabac_b_skip_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "color=c=gray:size=64x48:rate=6",
        "cabac-b-skip-gop",
        "cabac=1:8x8dct=0:b-adapt=0:direct=spatial:weightb=0:weightp=0:ref=1:no-deblock=1:analyse=none:scenecut=0",
        "main",
        6,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 6);
}

#[test]
fn native_decoder_reconstructs_explicit_cabac_b16_motion() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*9,256)':cb='92+N':cr='164-N'",
        "cabac-explicit-b16-gop",
        "cabac=1:8x8dct=0:b-adapt=0:direct=none:weightb=0:weightp=0:ref=1:no-deblock=1:analyse=none:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_partitioned_cabac_b_motion() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*3,256)':cb='96+N':cr='160-N'",
        "cabac-partitioned-b-gop",
        "cabac=1:8x8dct=0:b-adapt=0:direct=none:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_cabac_b_pictures_with_intra_macroblocks() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "testsrc2=size=128x96:rate=8",
        "cabac-b-intra-macroblocks",
        "cabac=1:8x8dct=0:b-adapt=0:direct=none:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        12,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_reconstructs_temporal_direct_cabac_b_pictures() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*3,256)':cb='96+N':cr='160-N'",
        "cabac-temporal-direct-b-gop",
        "cabac=1:8x8dct=0:b-adapt=0:direct=temporal:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_weighted_high_profile_cabac_b_pictures() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "testsrc2=size=96x64:rate=8",
        "cabac-weighted-high-b-gop",
        "cabac=1:8x8dct=1:b-adapt=0:direct=spatial:weightb=1:weightp=0:ref=1:partitions=all:subme=7:scenecut=0",
        "high",
        12,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 12);
}

#[test]
fn native_decoder_deblocks_a_moving_cabac_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*11,256)':cb='84+N*2':cr='172-N*2'",
        "cabac-deblocked-b-gop",
        "cabac=1:8x8dct=0:b-adapt=0:direct=spatial:weightb=0:weightp=0:ref=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_a_moving_spatial_direct_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*17,256)':cb='80+N*3':cr='176-N*2'",
        "cavlc-moving-spatial-direct-b-gop",
        "cabac=0:8x8dct=0:b-adapt=0:direct=spatial:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_deblocks_a_moving_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*11,256)':cb='84+N*2':cr='172-N*2'",
        "cavlc-deblocked-b-gop",
        "cabac=0:8x8dct=0:b-adapt=0:direct=spatial:weightb=0:weightp=0:ref=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_a_temporal_direct_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*3,256)':cb='96+N':cr='160-N'",
        "cavlc-temporal-direct-b-gop",
        "cabac=0:8x8dct=0:b-adapt=0:direct=temporal:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_an_implicitly_weighted_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*17,256)':cb='80+N*3':cr='176-N*2'",
        "cavlc-implicit-weighted-b-gop",
        "cabac=0:8x8dct=0:b-adapt=0:direct=spatial:weightb=1:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_an_explicit_partitioned_cavlc_b_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=96x64:rate=8,geq=lum='mod(X*7+Y*5+N*3,256)':cb='96+N':cr='160-N'",
        "cavlc-partitioned-b-gop",
        "cabac=0:8x8dct=0:b-adapt=0:direct=none:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
}

#[test]
fn native_decoder_reconstructs_frame_pictures_from_an_interlaced_sps() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=64x48:rate=4",
        "frame-pictures-interlaced-sps",
        "cabac=1:fake-interlaced=1:ref=1:weightp=0:scenecut=0",
        "high",
        4,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 4);
    assert!(
        frames
            .iter()
            .all(|frame| frame.field_order == FieldOrder::Unspecified)
    );
}

#[test]
fn native_decoder_reconstructs_a_real_cavlc_mbaff_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "nullsrc=size=64x64:rate=4,geq=lum='if(mod(Y,2),64,192)':cb='if(mod(Y,2),80,176)':cr='if(mod(Y,2),176,80)'",
        "cavlc-mbaff-gop",
        "interlaced=1:tff=1:cabac=0:8x8dct=0:b-adapt=0:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=i16x16,p16x16,b16x16:scenecut=0",
        "main",
        4,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 4);
}

#[test]
fn native_decoder_reconstructs_field_coded_intra4_in_a_cavlc_mbaff_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=16,tinterlace=mode=interleave_top",
        "cavlc-mbaff-intra4",
        "interlaced=1:tff=1:cabac=0:8x8dct=0:keyint=1:min-keyint=1:ref=1:no-deblock=1:partitions=i4x4:scenecut=0",
        "main",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_field_coded_intra8_in_a_cavlc_mbaff_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "nullsrc=size=96x64:rate=16,geq=lum='mod(X*3+Y*2+N*40,256)':cb='96+N*10':cr='160-N*10',tinterlace=mode=interleave_top",
        "cavlc-mbaff-intra8",
        "interlaced=1:tff=1:cabac=0:8x8dct=1:keyint=1:min-keyint=1:ref=1:no-deblock=1:qp=20:scenecut=0",
        "high",
        2,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 2);
}

#[test]
fn native_decoder_reconstructs_partitioned_motion_in_a_cavlc_mbaff_gop() {
    let Some(frames) = decode_x264_frames_with_profile(
        "testsrc2=size=96x64:rate=16,tinterlace=mode=interleave_top",
        "cavlc-mbaff-partitioned-motion",
        "interlaced=1:tff=1:cabac=0:8x8dct=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        6,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 6);
}

#[test]
fn native_decoder_reconstructs_explicit_b_motion_in_a_cavlc_mbaff_gop() {
    let Some(frames) = decode_x264_frames_with_profile_and_bframes(
        "testsrc2=size=96x64:rate=16,tinterlace=mode=interleave_top",
        "cavlc-mbaff-explicit-b-motion",
        "interlaced=1:tff=1:cabac=0:8x8dct=0:b-adapt=0:direct=none:weightb=0:weightp=0:ref=1:no-deblock=1:partitions=all:subme=7:scenecut=0",
        "main",
        8,
        1,
    ) else {
        return;
    };
    assert_eq!(frames.len(), 8);
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
    decode_x264_frames_with_profile_and_bframes(
        filter,
        suffix,
        encoder_params,
        profile,
        frame_count,
        0,
    )
}

fn decode_x264_frames_with_profile_and_bframes(
    filter: &str,
    suffix: &str,
    encoder_params: &str,
    profile: &str,
    frame_count: usize,
    b_frames: usize,
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
        ])
        .arg(b_frames.to_string())
        .args(["-g", "30", "-x264-params", encoder_params])
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
    assert_mbaff_when_requested(encoder_params, track);
    if b_frames != 0 {
        assert!(
            track
                .samples
                .windows(2)
                .any(|samples| samples[1].pts < samples[0].pts),
            "x264 interoperability vector did not contain reordered B pictures"
        );
    }
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
    frames.sort_by_key(|frame| frame.timing.pts.map(|timestamp| timestamp.value));
    let native = frames
        .iter()
        .flat_map(|frame| frame.planes.iter())
        .flat_map(|plane| plane.data.iter().copied())
        .collect::<Vec<_>>();
    assert_frame_bytes_match(&native, &independent.stdout);
    let _ = std::fs::remove_file(movie_path);
    Some(frames)
}

fn assert_mbaff_when_requested(encoder_params: &str, track: &mmrecode_isobmff::Track) {
    if !encoder_params
        .split(':')
        .any(|parameter| parameter.starts_with("interlaced="))
    {
        return;
    }
    let configuration =
        AvcDecoderConfigurationRecord::parse(&track.descriptor.codec.configuration).unwrap();
    let sps = parse_sps(&configuration.sequence_parameter_sets[0]).unwrap();
    assert!(
        sps.mb_adaptive_frame_field,
        "x264 interoperability vector did not signal MBAFF"
    );
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
    let mismatch_indices = native
        .iter()
        .zip(independent)
        .enumerate()
        .filter_map(|(index, (native, independent))| (native != independent).then_some(index))
        .take(32)
        .collect::<Vec<_>>();
    panic!(
        "native H.264 sequence mismatch at byte {mismatch}: native={}, independent={}, first mismatches={mismatch_indices:?}, nearby native={:?}, independent={:?}",
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
