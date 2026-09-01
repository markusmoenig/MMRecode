//! Real-MJPEG packet-copy integration coverage.

use mmrecode_core::{
    CodecDescriptor, CodecId, DependencyAnalyzer, MediaType, Packet, PacketFlags, PacketSideData,
    Rational, StreamDescriptor, StreamId, Timestamp,
};
use mmrecode_edit::{
    Clip, ClipId, EditSequence, MediaSource, OutputIntent, SourceId, TimeRange, Track, TrackId,
};
use mmrecode_mjpeg::{CODEC_NAME, MjpegDependencyAnalyzer};
use mmrecode_render::{
    AnalyzedPacket, PacketSource, RenderDisposition, execute_packet_plan, plan_independent_video,
};

const JPEG_FRAMES: [&[u8]; 3] = [
    include_bytes!("../../../testdata/jpeg/valid/baseline-420.jpg"),
    include_bytes!("../../../testdata/jpeg/valid/unknown-app-marker.jpg"),
    include_bytes!("../../../testdata/jpeg/encoded/mmrecode-q85-420.mjpg"),
];

fn time_base() -> Rational {
    Rational::new(1, 25).unwrap()
}

fn timestamp(value: i64) -> Timestamp {
    Timestamp {
        value,
        time_base: time_base(),
    }
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(timestamp(start), timestamp(end)).unwrap()
}

#[test]
fn reorders_distinct_jpeg_frames_without_reencoding() {
    let sources = vec![packet_source()];
    let plan = plan_independent_video(&sequence(), &sources).unwrap();
    assert_eq!(plan.summary.copied_packets, 3);
    assert_eq!(plan.summary.decoded_frames, 0);
    assert_eq!(plan.summary.encoded_frames, 0);
    assert!(
        plan.decisions
            .iter()
            .all(|decision| decision.disposition == RenderDisposition::Copy)
    );

    let output = execute_packet_plan(&plan, &sources).unwrap();
    assert_eq!(
        output
            .iter()
            .map(|packet| packet.side_data[0].data[0])
            .collect::<Vec<_>>(),
        vec![2, 0, 1]
    );
    assert_eq!(output[0].data, JPEG_FRAMES[2]);
    assert_eq!(output[1].data, JPEG_FRAMES[0]);
    assert_eq!(output[2].data, JPEG_FRAMES[1]);
    assert_eq!(
        output
            .iter()
            .map(|packet| packet.pts.unwrap().value)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

fn packet_source() -> PacketSource {
    let mut analyzer = MjpegDependencyAnalyzer::default();
    let packets: Vec<_> = JPEG_FRAMES
        .iter()
        .enumerate()
        .map(|(index, &data)| {
            let packet = Packet {
                stream_id: StreamId(5),
                data: data.to_vec(),
                pts: Some(timestamp(i64::try_from(index).unwrap())),
                dts: Some(timestamp(i64::try_from(index).unwrap())),
                duration: Some(timestamp(1)),
                flags: PacketFlags::KEY,
                side_data: vec![PacketSideData {
                    kind: "test/source-index".into(),
                    data: vec![u8::try_from(index).unwrap()],
                }],
            };
            let access_unit = analyzer.analyze_access_unit(&packet).unwrap();
            AnalyzedPacket {
                packet,
                access_unit,
            }
        })
        .collect();
    PacketSource {
        source_id: SourceId(9),
        stream_id: StreamId(5),
        packets,
    }
}

fn sequence() -> EditSequence {
    EditSequence {
        time_base: time_base(),
        sources: vec![MediaSource {
            id: SourceId(9),
            locator: "three-frames.mjpg".into(),
            streams: vec![StreamDescriptor {
                id: StreamId(5),
                codec: CodecDescriptor {
                    codec_id: CodecId::new(CODEC_NAME),
                    codec_tag: None,
                    media_type: MediaType::Video,
                    configuration: Vec::new(),
                },
                time_base: time_base(),
            }],
        }],
        tracks: vec![Track {
            id: TrackId(1),
            media_type: MediaType::Video,
            clips: vec![
                Clip {
                    id: ClipId(20),
                    source_id: SourceId(9),
                    source_stream_id: StreamId(5),
                    source_range: range(2, 3),
                    timeline_range: range(0, 1),
                    effects: Vec::new(),
                },
                Clip {
                    id: ClipId(21),
                    source_id: SourceId(9),
                    source_stream_id: StreamId(5),
                    source_range: range(0, 2),
                    timeline_range: range(1, 3),
                    effects: Vec::new(),
                },
            ],
            transitions: Vec::new(),
        }],
        output: OutputIntent {
            time_base: time_base(),
            container: Some("container/raw-mjpeg".into()),
            video_codec: Some(CodecId::new(CODEC_NAME)),
            audio_codec: None,
        },
    }
}
