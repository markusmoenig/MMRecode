#![allow(missing_docs)]

use std::{
    io::Write,
    process::{Command, Stdio},
};

use mmrecode_mpeg2::{decode_stream, parse_stream};
use mmrecode_mpegts::{TS_PACKET_SIZE, demux_transport_stream, mux_mpeg2_video};

const FFMPEG_TS: &[u8] =
    include_bytes!("../../../../testdata/mpegts/valid/single-program-mpeg2.ts");
const MPEG2: &[u8] = include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");

#[test]
fn parses_independent_single_program_vector() {
    let transport = demux_transport_stream(FFMPEG_TS).expect("demux FFmpeg transport stream");
    assert!(FFMPEG_TS.len().is_multiple_of(TS_PACKET_SIZE));
    assert!(!transport.packets.is_empty());
    assert_eq!(transport.program_association_tables[0].programs.len(), 1);
    assert_eq!(transport.program_map_tables[0].program_number, 1);
    assert_eq!(transport.program_map_tables[0].pcr_pid, 0x0100);
    assert_eq!(transport.streams.len(), 1);
    assert_eq!(transport.streams[0].codec.codec_id.as_str(), "video/mpeg2");
    assert!(
        transport
            .elementary_packets
            .iter()
            .all(|packet| packet.pts.is_some())
    );
    let elementary = transport.mpeg2_video_bytes().expect("extract video");
    assert_eq!(parse_stream(&elementary).unwrap().pictures().len(), 12);
    assert_eq!(decode_stream(&elementary).unwrap().len(), 12);
}

#[test]
fn rejects_sync_continuity_crc_and_truncation_damage() {
    let mut bad_sync = FFMPEG_TS.to_vec();
    bad_sync[TS_PACKET_SIZE] = 0;
    assert!(demux_transport_stream(&bad_sync).is_err());

    let transport = demux_transport_stream(FFMPEG_TS).unwrap();
    let video_packets: Vec<_> = transport
        .packets
        .iter()
        .filter(|packet| packet.header.pid == 0x0100 && packet.header.has_payload)
        .collect();
    let mut bad_counter = FFMPEG_TS.to_vec();
    let second_offset = video_packets[1].source_range.start;
    bad_counter[second_offset + 3] ^= 1;
    assert!(demux_transport_stream(&bad_counter).is_err());

    let mut bad_crc = FFMPEG_TS.to_vec();
    let pat_payload = transport
        .packets
        .iter()
        .find(|packet| packet.header.pid == 0)
        .unwrap()
        .payload_range
        .clone()
        .unwrap();
    bad_crc[pat_payload.start + 5] ^= 1;
    assert!(demux_transport_stream(&bad_crc).is_err());

    assert!(demux_transport_stream(&FFMPEG_TS[..FFMPEG_TS.len() - 1]).is_err());
}

#[test]
fn native_mux_is_decoded_by_ffmpeg_when_available() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping FFmpeg MPEG-TS check: ffmpeg is unavailable");
        return;
    }
    let transport = mux_mpeg2_video(MPEG2).expect("mux native transport stream");
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "mpegts",
            "-i",
            "pipe:0",
            "-map",
            "0:v:0",
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
        .expect("start FFmpeg MPEG-TS decoder");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&transport)
        .expect("write native TS");
    let output = child.wait_with_output().expect("wait for FFmpeg");
    assert!(
        output.status.success(),
        "FFmpeg rejected native TS: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.len(), 12 * 96 * 64 * 3 / 2);
}
