//! Independent encoded inputs and PCM comparisons. `FFmpeg` is a test oracle, never the decoder
//! used for actual output. Optional on machines without the reference executable.

#![cfg(not(target_arch = "wasm32"))]

use std::{path::Path, process::Command};

use mmrecode_aac::{AacLcDecoder, AudioSpecificConfig};
use mmrecode_bitstream::BitWriter;
use mmrecode_core::{
    AudioDecoder, CodecDescriptor, CodecId, MediaType, Packet, PacketFlags, StreamId,
};

fn native_pcm(path: &Path, configuration: Vec<u8>) -> Vec<i16> {
    let data = std::fs::read(path).unwrap();
    let mut decoder = AacLcDecoder::default();
    decoder
        .configure(&CodecDescriptor {
            codec_id: CodecId::new("audio/aac"),
            codec_tag: None,
            media_type: MediaType::Audio,
            configuration,
        })
        .unwrap();
    let mut offset = 0;
    let mut output = Vec::new();
    while offset < data.len() {
        let header = &data[offset..offset + 7];
        assert_eq!(header[0], 0xff);
        assert_eq!(header[1] & 0xf7, 0xf1); // unprotected ADTS, no CRC
        assert_eq!(header[6] & 3, 0); // one raw_data_block
        let length = (usize::from(header[3] & 3) << 11)
            | (usize::from(header[4]) << 3)
            | usize::from(header[5] >> 5);
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: data[offset + 7..offset + length].to_vec(),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: vec![],
            })
            .unwrap_or_else(|error| {
                panic!("{} packet {}: {error}", path.display(), output.len() / 1024)
            });
        output.extend(decoder.receive_frame().unwrap().unwrap().samples);
        offset += length;
    }
    decoder.flush().unwrap();
    assert!(decoder.receive_frame().unwrap().is_none());
    output
}

#[test]
#[allow(clippy::too_many_lines)] // The matrix is intentionally kept together for one oracle run.
fn native_nonzero_lc_matches_reference_pcm() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping AAC reference tests: ffmpeg unavailable");
        return;
    }
    let directory = std::env::temp_dir().join(format!(
        "mmrecode-aac-native-vectors-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let mut cases = vec![
        (
            "stereo-transient".to_owned(),
            vec![0x11, 0x90],
            "aevalsrc=0.2*sin(2*PI*440*t)*lt(mod(t\\,0.04)\\,0.02)|0.15*sin(2*PI*733*t)*lt(mod(t\\,0.07)\\,0.03):s=48000:d=0.3".to_owned(),
            "1",
            "none",
        ),
        (
            "stereo-independent".to_owned(),
            vec![0x12, 0x10],
            "aevalsrc=0.2*sin(2*PI*440*t)|0.15*sin(2*PI*1901*t):s=44100:d=0.2".to_owned(),
            "0",
            "none",
        ),
        (
            "stereo-tns".to_owned(),
            vec![0x11, 0x90],
            "aevalsrc=0.12*sin(2*PI*331*t)+0.04*random(0)|0.10*sin(2*PI*997*t)+0.05*random(1):s=48000:d=0.3".to_owned(),
            "1",
            "tns",
        ),
        (
            "stereo-is".to_owned(),
            vec![0x11, 0x90],
            "aevalsrc=0.12*sin(2*PI*331*t)+0.04*random(0)|0.10*sin(2*PI*997*t)+0.05*random(1):s=48000:d=0.3".to_owned(),
            "1",
            "is",
        ),
        (
            "stereo-pns".to_owned(),
            vec![0x11, 0x90],
            "aevalsrc=0.12*sin(2*PI*331*t)+0.04*random(0)|0.10*sin(2*PI*997*t)+0.05*random(1):s=48000:d=0.3".to_owned(),
            "1",
            "pns",
        ),
        (
            "stereo-all-tools".to_owned(),
            vec![0x11, 0x90],
            "aevalsrc=0.12*sin(2*PI*331*t)+0.04*random(0)|0.10*sin(2*PI*997*t)+0.05*random(1):s=48000:d=0.3".to_owned(),
            "1",
            "all",
        ),
    ];
    for (index, rate) in [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ]
    .iter()
    .enumerate()
    {
        let mut writer = BitWriter::new();
        writer.write_bits(2, 5).unwrap();
        writer.write_bits(u64::try_from(index).unwrap(), 4).unwrap();
        writer.write_bits(1, 4).unwrap();
        writer.write_bits(0, 3).unwrap();
        cases.push((
            format!("mono-{rate}"),
            writer.into_bytes(),
            format!("sine=frequency=997:sample_rate={rate}:duration=0.18"),
            "0",
            "none",
        ));
    }
    for (name, configuration, source, mid_side, tools) in cases {
        let path = directory.join(format!("{name}.aac"));
        let mut command = Command::new("ffmpeg");
        command.args([
            "-v", "error", "-f", "lavfi", "-i", &source, "-c:a", "aac", "-b:a", "64k",
        ]);
        command.args([
            "-aac_pns",
            if matches!(tools, "pns" | "all") {
                "1"
            } else {
                "0"
            },
        ]);
        command.args([
            "-aac_tns",
            if matches!(tools, "tns" | "all") {
                "1"
            } else {
                "0"
            },
        ]);
        command.args([
            "-aac_is",
            if matches!(tools, "is" | "all") {
                "1"
            } else {
                "0"
            },
        ]);
        let encoded = command
            .args(["-aac_ms", mid_side, "-f", "adts", "-y"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            encoded.status.success(),
            "{}",
            String::from_utf8_lossy(&encoded.stderr)
        );
        compare_reference(&path, configuration);
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_dir(directory).unwrap();
}

fn compare_reference(path: &Path, configuration: Vec<u8>) {
    let reference = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "pipe:1"])
        .output()
        .unwrap();
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let reference: Vec<i16> = reference
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| i16::from_le_bytes(*bytes))
        .collect();
    let actual = native_pcm(path, configuration);
    assert_eq!(actual.len(), reference.len(), "{}", path.display());
    let max_error = actual
        .iter()
        .zip(&reference)
        .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
        .max()
        .unwrap();
    let energy: i64 = reference.iter().map(|v| i64::from(*v).pow(2)).sum();
    assert!(energy > 1_000_000, "reference must be non-silent");
    eprintln!(
        "{}: {} samples; max error {max_error}",
        path.display(),
        actual.len()
    );
    assert!(
        max_error <= 2,
        "{}: max signed-16 error {max_error}",
        path.display()
    );
}

#[test]
fn long_start_grouped_short_stop_and_window_shape_changes_match_reference() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let path =
        std::env::temp_dir().join(format!("mmrecode-aac-windows-{}.aac", std::process::id()));
    let config = AudioSpecificConfig::parse(&[0x12, 0x08]).unwrap();
    let mut adts = Vec::new();
    for (sequence, shape) in [(0, 0), (1, 1), (2, 0), (2, 1), (3, 1), (0, 0)] {
        let mut writer = BitWriter::new();
        writer.write_bits(0, 3).unwrap(); // SCE
        writer.write_bits(0, 4).unwrap(); // tag
        writer.write_bits(160, 8).unwrap(); // global_gain
        writer.write_bits(0, 1).unwrap(); // ICS reserved
        writer.write_bits(sequence, 2).unwrap();
        writer.write_bits(shape, 1).unwrap();
        writer
            .write_bits(1, if sequence == 2 { 4 } else { 6 })
            .unwrap();
        if sequence == 2 {
            writer.write_bits(0b101_0101, 7).unwrap();
        } else {
            writer.write_bit(false).unwrap();
        }
        let groups = if sequence == 2 { 4 } else { 1 };
        for _ in 0..groups {
            writer.write_bits(5, 4).unwrap(); // signed pair codebook 5
            writer
                .write_bits(1, if sequence == 2 { 3 } else { 5 })
                .unwrap();
        }
        for _ in 0..groups {
            writer.write_bit(false).unwrap();
        } // zero scalefactor delta
        writer.write_bits(0, 3).unwrap(); // pulse, TNS, gain control absent
        for _ in 0..if sequence == 2 { 8 } else { 1 } {
            // First band has four coefficients: (1, 0) then (1, -1).
            writer.write_bits(9, 4).unwrap();
            writer.write_bits(24, 5).unwrap();
        }
        writer.write_bits(7, 3).unwrap();
        let payload = writer.into_bytes();
        adts.extend_from_slice(&config.adts_header(payload.len()).unwrap());
        adts.extend(payload);
    }
    std::fs::write(&path, adts).unwrap();
    compare_reference(&path, vec![0x12, 0x08]);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn pns_state_and_normalization_match_reference() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let path = std::env::temp_dir().join(format!("mmrecode-aac-pns-{}.aac", std::process::id()));
    let config = AudioSpecificConfig::parse(&[0x12, 0x08]).unwrap();
    let mut adts = Vec::new();
    for _ in 0..4 {
        let mut writer = BitWriter::new();
        writer.write_bits(0, 3).unwrap(); // SCE
        writer.write_bits(0, 4).unwrap(); // tag
        writer.write_bits(160, 8).unwrap();
        writer.write_bits(0, 1).unwrap(); // ICS reserved
        writer.write_bits(0, 2).unwrap(); // long
        writer.write_bits(0, 1).unwrap(); // sine
        writer.write_bits(1, 6).unwrap(); // one band
        writer.write_bit(false).unwrap(); // no prediction
        writer.write_bits(13, 4).unwrap(); // PNS
        writer.write_bits(1, 5).unwrap();
        writer.write_bits(256, 9).unwrap(); // first noise energy delta zero
        writer.write_bits(0, 3).unwrap(); // no pulse/TNS/gain
        writer.write_bits(7, 3).unwrap(); // END
        let payload = writer.into_bytes();
        adts.extend_from_slice(&config.adts_header(payload.len()).unwrap());
        adts.extend(payload);
    }
    std::fs::write(&path, adts).unwrap();
    compare_reference(&path, vec![0x12, 0x08]);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn pulse_reconstruction_matches_reference() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let path = std::env::temp_dir().join(format!("mmrecode-aac-pulse-{}.aac", std::process::id()));
    let config = AudioSpecificConfig::parse(&[0x12, 0x08]).unwrap();
    let mut writer = BitWriter::new();
    writer.write_bits(0, 3).unwrap(); // SCE/tag
    writer.write_bits(0, 4).unwrap();
    writer.write_bits(160, 8).unwrap();
    writer.write_bits(0, 1).unwrap(); // long sine, one band, no prediction
    writer.write_bits(0, 2).unwrap();
    writer.write_bits(0, 1).unwrap();
    writer.write_bits(1, 6).unwrap();
    writer.write_bit(false).unwrap();
    writer.write_bits(5, 4).unwrap(); // signed pair codebook 5
    writer.write_bits(1, 5).unwrap();
    writer.write_bit(false).unwrap(); // scalefactor delta zero
    writer.write_bit(true).unwrap(); // one pulse at coefficient zero, amplitude three
    writer.write_bits(0, 2).unwrap();
    writer.write_bits(0, 6).unwrap();
    writer.write_bits(0, 5).unwrap();
    writer.write_bits(3, 4).unwrap();
    writer.write_bits(0, 2).unwrap(); // no TNS/gain
    writer.write_bits(9, 4).unwrap(); // (1, 0), then (1, -1)
    writer.write_bits(24, 5).unwrap();
    writer.write_bits(7, 3).unwrap();
    let payload = writer.into_bytes();
    let mut adts = config.adts_header(payload.len()).unwrap().to_vec();
    adts.extend(payload);
    std::fs::write(&path, adts).unwrap();
    compare_reference(&path, vec![0x12, 0x08]);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn intensity_stereo_reconstruction_matches_reference() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return;
    }
    let path =
        std::env::temp_dir().join(format!("mmrecode-aac-intensity-{}.aac", std::process::id()));
    let config = AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
    let mut writer = BitWriter::new();
    writer.write_bits(1, 3).unwrap(); // CPE/tag/common window
    writer.write_bits(0, 4).unwrap();
    writer.write_bit(true).unwrap();
    writer.write_bits(0, 1).unwrap(); // long sine, one band, no prediction
    writer.write_bits(0, 2).unwrap();
    writer.write_bits(0, 1).unwrap();
    writer.write_bits(1, 6).unwrap();
    writer.write_bit(false).unwrap();
    writer.write_bits(0, 2).unwrap(); // no M/S

    writer.write_bits(160, 8).unwrap(); // left: ordinary spectral band
    writer.write_bits(5, 4).unwrap();
    writer.write_bits(1, 5).unwrap();
    writer.write_bit(false).unwrap();
    writer.write_bits(0, 3).unwrap();
    writer.write_bits(9, 4).unwrap();
    writer.write_bits(24, 5).unwrap();

    writer.write_bits(160, 8).unwrap(); // right: intensity copy of left
    writer.write_bits(14, 4).unwrap();
    writer.write_bits(1, 5).unwrap();
    writer.write_bit(false).unwrap();
    writer.write_bits(0, 3).unwrap();
    writer.write_bits(7, 3).unwrap();
    let payload = writer.into_bytes();
    let mut adts = config.adts_header(payload.len()).unwrap().to_vec();
    adts.extend(payload);
    std::fs::write(&path, adts).unwrap();
    compare_reference(&path, vec![0x12, 0x10]);
    std::fs::remove_file(path).unwrap();
}
