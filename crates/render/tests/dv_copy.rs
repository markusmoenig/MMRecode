//! Real-DV packet-copy integration coverage.

use mmrecode_core::{
    CodecDescriptor, CodecId, DependencyAnalyzer, MediaType, Packet, PacketFlags, PacketSideData,
    Rational, StreamDescriptor, StreamId, Timestamp,
};
use mmrecode_dv::{CODEC_NAME, DvDependencyAnalyzer};
use mmrecode_edit::{
    Clip, ClipId, EditSequence, MediaSource, OutputIntent, SourceId, TimeRange, Track, TrackId,
};
use mmrecode_render::{
    AnalyzedPacket, PacketSource, RenderDisposition, execute_packet_plan, plan_independent_video,
};

const DV_FRAME: &[u8] = include_bytes!("../../../testdata/dv/valid/dv25-525-60-one-frame.dv");

fn time_base() -> Rational {
    Rational::new(1_001, 30_000).unwrap()
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
fn reorders_real_dv_frames_without_touching_payload_or_side_data() {
    let mut analyzer = DvDependencyAnalyzer::default();
    let packets: Vec<_> = (0_u8..3)
        .map(|index| {
            let packet = Packet {
                stream_id: StreamId(4),
                data: DV_FRAME.to_vec(),
                pts: Some(timestamp(i64::from(index))),
                dts: Some(timestamp(i64::from(index))),
                duration: Some(timestamp(1)),
                flags: PacketFlags::KEY,
                side_data: vec![PacketSideData {
                    kind: "test/source-index".into(),
                    data: vec![index],
                }],
            };
            let access_unit = analyzer.analyze_access_unit(&packet).unwrap();
            AnalyzedPacket {
                packet,
                access_unit,
            }
        })
        .collect();
    let sources = vec![PacketSource {
        source_id: SourceId(8),
        stream_id: StreamId(4),
        packets,
    }];
    let sequence = EditSequence {
        time_base: time_base(),
        sources: vec![MediaSource {
            id: SourceId(8),
            locator: "three-frames.dv".into(),
            streams: vec![StreamDescriptor {
                id: StreamId(4),
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
                    id: ClipId(10),
                    source_id: SourceId(8),
                    source_stream_id: StreamId(4),
                    source_range: range(1, 3),
                    timeline_range: range(0, 2),
                    effects: Vec::new(),
                },
                Clip {
                    id: ClipId(11),
                    source_id: SourceId(8),
                    source_stream_id: StreamId(4),
                    source_range: range(0, 1),
                    timeline_range: range(2, 3),
                    effects: Vec::new(),
                },
            ],
            transitions: Vec::new(),
        }],
        output: OutputIntent {
            time_base: time_base(),
            container: Some("container/raw-dv".into()),
            video_codec: Some(CodecId::new(CODEC_NAME)),
            audio_codec: None,
        },
    };

    let plan = plan_independent_video(&sequence, &sources).unwrap();
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
        vec![1, 2, 0]
    );
    assert!(output.iter().all(|packet| packet.data == DV_FRAME));
    assert_eq!(
        output
            .iter()
            .map(|packet| packet.pts.unwrap().value)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(output.iter().all(|packet| packet.stream_id == StreamId(0)));
}
