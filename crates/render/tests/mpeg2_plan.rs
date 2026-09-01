//! Real MPEG-2 dependency-plan integration coverage.

#[cfg(feature = "mpeg2")]
use std::{io::Write as _, process::Stdio};

use mmrecode_core::{
    CodecDescriptor, CodecId, MediaType, Packet, PacketFlags, RandomAccessKind, Rational,
    StreamDescriptor, StreamId, Timestamp,
};
use mmrecode_edit::{
    Clip, ClipId, EditSequence, MediaSource, OutputIntent, SourceId, TimeRange, Track, TrackId,
};
use mmrecode_mpeg2::{CODEC_NAME, analyze_dependencies, parse_stream, plan_smart_render};
#[cfg(feature = "mpeg2")]
use mmrecode_mpeg2::{
    ColourDescription, Mpeg2EncodeOptions, Mpeg2QuantMatrices, Mpeg2SequenceSettings, Mpeg2Stream,
    SequenceDisplayExtension, decode_stream, encode_stream,
};
#[cfg(feature = "mpegts")]
use mmrecode_mpegts::{TS_PACKET_SIZE, demux_transport_stream};
use mmrecode_render::{
    AnalyzedPacket, PacketSource, RenderDisposition, RenderOperation, VideoChange,
    execute_packet_plan, plan_interframe_video,
};
#[cfg(feature = "mpegts")]
use mmrecode_render::{
    AudioBoundaryPolicy, Layer2AudioInput, MpegTsRenderOptions, execute_mpeg2_mpegts,
    plan_mpeg2_mpegts,
};
#[cfg(feature = "mpeg2")]
use mmrecode_render::{Mpeg2BridgeOptions, Mpeg2FrameReplacement, execute_mpeg2_plan};
#[cfg(feature = "mpeg2")]
use mmrecode_render::{Mpeg2SpliceAction, Mpeg2SpliceReport, execute_mpeg2_plan_with_report};

const MPEG2: &[u8] = include_bytes!("../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
#[cfg(feature = "mpegts")]
const MP2: &[u8] = include_bytes!("../../../testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2");

fn timestamp(value: i64, time_base: Rational) -> Timestamp {
    Timestamp { value, time_base }
}

fn range(start: i64, end: i64, time_base: Rational) -> TimeRange {
    TimeRange::new(timestamp(start, time_base), timestamp(end, time_base)).unwrap()
}

fn fixture() -> (EditSequence, Vec<PacketSource>) {
    fixture_from_bytes(MPEG2)
}

fn fixture_from_bytes(mpeg2: &[u8]) -> (EditSequence, Vec<PacketSource>) {
    let stream = parse_stream(mpeg2).unwrap();
    let dependencies = analyze_dependencies(&stream).unwrap();
    let frame_rate = stream.pictures()[0].sequence.frame_rate;
    let time_base = Rational::new(frame_rate.denominator(), frame_rate.numerator()).unwrap();
    let packets = stream
        .pictures()
        .iter()
        .zip(dependencies)
        .enumerate()
        .map(|(index, (picture, access_unit))| {
            let start = if index == 0 {
                0
            } else {
                stream.pictures()[index - 1].source_range.end
            };
            let end = if index + 1 == stream.pictures().len() {
                mpeg2.len()
            } else {
                picture.source_range.end
            };
            let mut flags = PacketFlags::empty();
            if access_unit.random_access == RandomAccessKind::Clean {
                flags.insert(PacketFlags::KEY);
            }
            AnalyzedPacket {
                packet: Packet {
                    stream_id: StreamId(6),
                    data: mpeg2[start..end].to_vec(),
                    pts: Some(timestamp(access_unit.presentation_order, time_base)),
                    dts: Some(timestamp(access_unit.decode_order, time_base)),
                    duration: Some(timestamp(1, time_base)),
                    flags,
                    side_data: Vec::new(),
                },
                access_unit,
            }
        })
        .collect::<Vec<_>>();
    let picture_count = i64::try_from(packets.len()).unwrap();
    let sequence = EditSequence {
        time_base,
        sources: vec![MediaSource {
            id: SourceId(3),
            locator: "main-ml-progressive-ibp.m2v".into(),
            streams: vec![StreamDescriptor {
                id: StreamId(6),
                codec: CodecDescriptor {
                    codec_id: CodecId::new(CODEC_NAME),
                    codec_tag: None,
                    media_type: MediaType::Video,
                    configuration: Vec::new(),
                },
                time_base,
            }],
        }],
        tracks: vec![Track {
            id: TrackId(2),
            media_type: MediaType::Video,
            clips: vec![Clip {
                id: ClipId(7),
                source_id: SourceId(3),
                source_stream_id: StreamId(6),
                source_range: range(0, picture_count, time_base),
                timeline_range: range(0, picture_count, time_base),
                effects: Vec::new(),
            }],
            transitions: Vec::new(),
        }],
        output: OutputIntent {
            time_base,
            container: Some("container/mpeg2-es".into()),
            video_codec: Some(CodecId::new(CODEC_NAME)),
            audio_codec: None,
        },
    };
    (
        sequence,
        vec![PacketSource {
            source_id: SourceId(3),
            stream_id: StreamId(6),
            packets,
        }],
    )
}

#[test]
fn generic_plan_matches_mpeg2_dependency_propagation() {
    let (sequence, sources) = fixture();
    let time_base = sequence.time_base;
    let plan = plan_interframe_video(
        &sequence,
        &sources,
        &[VideoChange {
            clip_id: ClipId(7),
            timeline_range: range(0, 1, time_base),
        }],
    )
    .unwrap();
    let stream = parse_stream(MPEG2).unwrap();
    let codec_plan = plan_smart_render(&stream, 0..1).unwrap();
    let codec_encoded = codec_plan
        .pictures
        .iter()
        .filter(|picture| picture.disposition != mmrecode_mpeg2::SmartRenderDisposition::Copy)
        .count();

    assert_eq!(plan.summary.encoded_frames, codec_encoded);
    assert_eq!(plan.summary.encoded_frames, 4);
    assert_eq!(plan.summary.decoded_frames, 4);
    assert_eq!(plan.summary.copied_packets, 8);
    assert_eq!(
        plan.decisions[0].disposition,
        RenderDisposition::BridgeEncode
    );
    assert!(plan.decisions[0].reason.contains("3 dependent"));
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::ApplyEffects { timeline_range, .. }
            if *timeline_range == range(0, 1, time_base)
    )));
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::BridgeEncode {
            source_range,
            timeline_range,
            output_packet_range,
            ..
        } if *source_range == range(0, 4, time_base)
            && *timeline_range == range(0, 4, time_base)
            && *output_packet_range == (0..4)
    )));
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::CopyPackets {
            source_packet_range,
            output_packet_range,
            ..
        } if *source_packet_range == (4..12) && *output_packet_range == (4..12)
    )));
}

#[test]
fn decode_plan_includes_unchanged_reference_preroll() {
    let (sequence, sources) = fixture();
    let time_base = sequence.time_base;
    let plan = plan_interframe_video(
        &sequence,
        &sources,
        &[VideoChange {
            clip_id: ClipId(7),
            timeline_range: range(3, 4, time_base),
        }],
    )
    .unwrap();

    assert_eq!(plan.summary.encoded_frames, 3);
    assert_eq!(plan.summary.decoded_frames, 4);
    assert_eq!(plan.summary.copied_packets, 9);
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::Decode { source_range, .. }
            if *source_range == range(0, 4, time_base)
    )));
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::BridgeEncode {
            timeline_range,
            output_packet_range,
            ..
        } if *timeline_range == range(1, 4, time_base)
            && *output_packet_range == (1..4)
    )));
}

#[test]
fn unchanged_mpeg2_stream_uses_the_existing_packet_executor_losslessly() {
    let (sequence, sources) = fixture();
    let plan = plan_interframe_video(&sequence, &sources, &[]).unwrap();
    assert_eq!(plan.decisions[0].disposition, RenderDisposition::Copy);
    assert_eq!(plan.summary.copied_packets, 12);
    assert_eq!(plan.summary.decoded_frames, 0);
    assert_eq!(plan.summary.encoded_frames, 0);

    let output = execute_packet_plan(&plan, &sources).unwrap();
    let bytes = output
        .iter()
        .flat_map(|packet| packet.data.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(bytes, MPEG2);
    assert_eq!(
        output
            .iter()
            .map(|packet| packet.pts.unwrap().value)
            .collect::<Vec<_>>(),
        vec![0, 3, 1, 2, 4, 7, 5, 6, 8, 11, 9, 10]
    );
}

#[cfg(feature = "mpeg2")]
#[test]
fn executes_and_validates_a_middle_of_gop_pixel_change() {
    let (sequence, sources) = fixture();
    let time_base = sequence.time_base;
    let plan = plan_interframe_video(
        &sequence,
        &sources,
        &[VideoChange {
            clip_id: ClipId(7),
            timeline_range: range(3, 4, time_base),
        }],
    )
    .unwrap();
    let original = decode_stream(MPEG2).unwrap();
    let mut replacement = original[3].frame.clone();
    replacement.planes[0].data.fill(32);
    replacement.planes[1].data.fill(128);
    replacement.planes[2].data.fill(128);

    let output = execute_mpeg2_plan(
        &plan,
        &sources,
        &[Mpeg2FrameReplacement {
            timeline_pts: timestamp(3, time_base),
            frame: replacement,
        }],
        Mpeg2BridgeOptions::default(),
    )
    .unwrap();
    let output_bytes = output
        .iter()
        .flat_map(|packet| packet.data.iter().copied())
        .collect::<Vec<_>>();
    let reconstructed = decode_stream(&output_bytes).unwrap();

    assert_eq!(output.len(), 12);
    assert_eq!(reconstructed.len(), 12);
    assert_eq!(output[0].data, sources[0].packets[0].packet.data);
    for index in 4..12 {
        assert_eq!(output[index].data, sources[0].packets[index].packet.data);
        assert_eq!(
            reconstructed[index].frame.planes,
            original[index].frame.planes
        );
    }
    assert_ne!(reconstructed[3].frame.planes, original[3].frame.planes);
    assert_eq!(
        output
            .iter()
            .map(|packet| packet.pts.unwrap().value)
            .collect::<Vec<_>>(),
        vec![0, 1, 3, 2, 4, 7, 5, 6, 8, 11, 9, 10]
    );
    assert_eq!(
        output
            .iter()
            .map(|packet| packet.dts.unwrap().value)
            .collect::<Vec<_>>(),
        (0..12).collect::<Vec<_>>()
    );
    assert_ffmpeg_decodes(&output_bytes);
}

#[cfg(feature = "mpeg2")]
#[test]
fn bridge_executor_requires_every_directly_changed_frame() {
    let (sequence, sources) = fixture();
    let plan = plan_interframe_video(
        &sequence,
        &sources,
        &[VideoChange {
            clip_id: ClipId(7),
            timeline_range: range(3, 4, sequence.time_base),
        }],
    )
    .unwrap();
    let error =
        execute_mpeg2_plan(&plan, &sources, &[], Mpeg2BridgeOptions::default()).unwrap_err();
    assert!(error.to_string().contains("no replacement frame"));
}

#[cfg(feature = "mpeg2")]
#[test]
fn cuts_and_concatenates_compatible_mpeg2_sources_with_a_boundary_bridge() {
    let (mut sequence, mut sources) = fixture();
    let time_base = sequence.time_base;
    let mut second_media_source = sequence.sources[0].clone();
    second_media_source.id = SourceId(4);
    second_media_source.locator = "second-main-ml-progressive-ibp.m2v".into();
    sequence.sources.push(second_media_source);
    let mut second_packet_source = sources[0].clone();
    second_packet_source.source_id = SourceId(4);
    sources.push(second_packet_source);
    sequence.tracks[0].clips = vec![
        Clip {
            id: ClipId(7),
            source_id: SourceId(3),
            source_stream_id: StreamId(6),
            source_range: range(1, 7, time_base),
            timeline_range: range(0, 6, time_base),
            effects: Vec::new(),
        },
        Clip {
            id: ClipId(8),
            source_id: SourceId(4),
            source_stream_id: StreamId(6),
            source_range: range(4, 12, time_base),
            timeline_range: range(6, 14, time_base),
            effects: Vec::new(),
        },
    ];

    let plan = plan_interframe_video(&sequence, &sources, &[]).unwrap();
    assert_eq!(plan.summary.encoded_frames, 5);
    assert_eq!(plan.summary.copied_packets, 9);
    assert_eq!(plan.summary.decoded_frames, 8);
    assert_eq!(
        plan.decisions[0].disposition,
        RenderDisposition::BridgeEncode
    );
    assert_eq!(plan.decisions[1].disposition, RenderDisposition::Copy);
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::BridgeEncode {
            source_id: SourceId(3),
            source_packet_indices,
            output_packet_range,
            ..
        } if source_packet_indices == &[1, 2, 3] && *output_packet_range == (0..3)
    )));
    assert!(plan.operations.iter().any(|operation| matches!(
        operation,
        RenderOperation::CopyPackets {
            source_id: SourceId(4),
            source_packet_range,
            output_packet_range,
            ..
        } if *source_packet_range == (4..12) && *output_packet_range == (6..14)
    )));

    let rendered =
        execute_mpeg2_plan_with_report(&plan, &sources, &[], Mpeg2BridgeOptions::default())
            .unwrap();
    assert_eq!(rendered.splice.regenerated_runs, 2);
    assert_eq!(rendered.packets.len(), 14);
    for (output, source) in rendered.packets[6..].iter().zip(&sources[1].packets[4..]) {
        assert_eq!(output.data, source.packet.data);
    }
    let pts = rendered
        .packets
        .iter()
        .map(|packet| packet.pts.unwrap().value)
        .collect::<Vec<_>>();
    let mut presentation_pts = pts;
    presentation_pts.sort_unstable();
    assert_eq!(presentation_pts, (0..14).collect::<Vec<_>>());
    assert_eq!(
        rendered
            .packets
            .iter()
            .map(|packet| packet.dts.unwrap().value)
            .collect::<Vec<_>>(),
        (0..14).collect::<Vec<_>>()
    );
    let output_bytes = rendered
        .packets
        .iter()
        .flat_map(|packet| packet.data.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(decode_stream(&output_bytes).unwrap().len(), 14);
    assert_ffmpeg_decodes(&output_bytes);
}

#[cfg(feature = "mpeg2")]
fn assert_splice_report(report: &Mpeg2SpliceReport) {
    assert_eq!(report.regenerated_runs, 1);
    assert_eq!(report.aspect_ratio, Mpeg2SpliceAction::Preserved);
    assert_eq!(report.display_metadata, Mpeg2SpliceAction::Preserved);
    assert_eq!(report.quantizer_matrices, Mpeg2SpliceAction::Preserved);
    assert_eq!(report.profile_and_level, Mpeg2SpliceAction::Preserved);
    assert_eq!(report.bit_rate, Mpeg2SpliceAction::Rewritten);
    assert_eq!(report.source_bit_rate, Some(8_000_000));
    assert_eq!(report.encoded_bit_rate, 15_000_000);
    assert_eq!(report.vbv_buffer_size, Mpeg2SpliceAction::Rewritten);
    assert!(report.explanation().contains("timecode Recomputed"));
}

#[cfg(feature = "mpeg2")]
fn assert_splice_metadata(
    output: &Mpeg2Stream<'_>,
    display: SequenceDisplayExtension,
    matrices: Mpeg2QuantMatrices,
) {
    assert!(
        output
            .sequence_headers()
            .iter()
            .all(|header| header.aspect_ratio_information == 3)
    );
    for picture in output.pictures() {
        assert_eq!(picture.sequence.display, Some(display));
        assert_eq!(picture.sequence.intra_quantizer_matrix, matrices.intra);
        assert_eq!(
            picture.sequence.non_intra_quantizer_matrix,
            matrices.non_intra
        );
        assert_eq!(
            picture.sequence.chroma_intra_quantizer_matrix,
            matrices.chroma_intra
        );
        assert_eq!(
            picture.sequence.chroma_non_intra_quantizer_matrix,
            matrices.chroma_non_intra
        );
    }
    assert!(output.groups().iter().any(|group| {
        group.closed_gop
            && group.hours == 2
            && group.minutes == 0
            && group.seconds == 0
            && group.pictures == 1
    }));
}

#[cfg(feature = "mpeg2")]
#[test]
fn bridge_preserves_sequence_metadata_and_reports_rate_rewrites() {
    let frames = decode_stream(MPEG2)
        .unwrap()
        .into_iter()
        .map(|picture| picture.frame)
        .collect::<Vec<_>>();
    let mut matrices = Mpeg2QuantMatrices::default();
    matrices.intra[1] = 23;
    matrices.non_intra[2] = 19;
    matrices.chroma_intra[3] = 31;
    matrices.chroma_non_intra[4] = 21;
    let display = SequenceDisplayExtension {
        video_format: 5,
        colour_description: Some(ColourDescription {
            colour_primaries: 5,
            transfer_characteristics: 6,
            matrix_coefficients: 6,
        }),
        display_horizontal_size: 90,
        display_vertical_size: 60,
    };
    let source = encode_stream(
        &frames,
        Mpeg2EncodeOptions {
            gop_size: 4,
            sequence: Mpeg2SequenceSettings {
                aspect_ratio_information: 3,
                bit_rate: 8_000_000,
                vbv_buffer_size_bits: 224 * 16_384,
                display: Some(display),
                quant_matrices: matrices,
                timecode_start_frame: 2 * 3_600 * 25,
                ..Mpeg2SequenceSettings::default()
            },
            ..Mpeg2EncodeOptions::default()
        },
    )
    .unwrap();
    let (sequence, sources) = fixture_from_bytes(&source.data);
    let time_base = sequence.time_base;
    let plan = plan_interframe_video(
        &sequence,
        &sources,
        &[VideoChange {
            clip_id: ClipId(7),
            timeline_range: range(3, 4, time_base),
        }],
    )
    .unwrap();
    let mut replacement = frames[3].clone();
    replacement.planes[0].data.fill(32);
    replacement.planes[1].data.fill(128);
    replacement.planes[2].data.fill(128);
    let rendered = execute_mpeg2_plan_with_report(
        &plan,
        &sources,
        &[Mpeg2FrameReplacement {
            timeline_pts: timestamp(3, time_base),
            frame: replacement,
        }],
        Mpeg2BridgeOptions::default(),
    )
    .unwrap();
    let output_bytes = rendered
        .packets
        .iter()
        .flat_map(|packet| packet.data.iter().copied())
        .collect::<Vec<_>>();
    let output = parse_stream(&output_bytes).unwrap();

    assert_splice_report(&rendered.splice);
    assert_splice_metadata(&output, display, matrices);
    assert_ffmpeg_decodes(&output_bytes);
}

#[cfg(feature = "mpegts")]
#[test]
fn plans_and_muxes_smart_rendered_mpeg2_with_exact_layer2_audio() {
    let (sequence, sources) = fixture();
    let time_base = sequence.time_base;
    let plan = plan_interframe_video(
        &sequence,
        &sources,
        &[VideoChange {
            clip_id: ClipId(7),
            timeline_range: range(3, 4, time_base),
        }],
    )
    .unwrap();
    let mut replacement = decode_stream(MPEG2).unwrap()[3].frame.clone();
    replacement.planes[0].data.fill(32);
    replacement.planes[1].data.fill(128);
    replacement.planes[2].data.fill(128);
    let video_packets = execute_mpeg2_plan(
        &plan,
        &sources,
        &[Mpeg2FrameReplacement {
            timeline_pts: timestamp(3, time_base),
            frame: replacement,
        }],
        Mpeg2BridgeOptions::default(),
    )
    .unwrap();
    let delivery = plan_mpeg2_mpegts(
        &plan,
        &video_packets,
        Some(Layer2AudioInput {
            data: MP2,
            start: timestamp(0, Rational::new(1, 48_000).unwrap()),
        }),
        MpegTsRenderOptions::default(),
    )
    .unwrap();

    assert_eq!(delivery.report().copied_video_packets, 9);
    assert_eq!(delivery.report().regenerated_video_packets, 3);
    assert_eq!(delivery.report().available_audio_frames, 20);
    assert_eq!(delivery.report().selected_audio_frames, 20);
    assert_eq!(delivery.report().audio_bytes, MP2.len());
    assert_eq!(delivery.report().av_end_delta_micros, Some(0));
    assert_eq!(delivery.report().transport_packets, None);
    assert_eq!(delivery.packets().len(), 32);
    assert!(delivery.report().explanation().contains("dry run"));
    assert!(
        delivery
            .report()
            .explanation()
            .contains("9 copied + 3 regenerated")
    );

    let output = execute_mpeg2_mpegts(&delivery).unwrap();
    assert_eq!(output.data.len() % TS_PACKET_SIZE, 0);
    assert_eq!(
        output.report.transport_packets,
        Some(output.data.len() / TS_PACKET_SIZE)
    );
    let transport = demux_transport_stream(&output.data).unwrap();
    let expected_video = video_packets
        .iter()
        .flat_map(|packet| packet.data.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(transport.mpeg2_video_bytes().unwrap(), expected_video);
    assert_eq!(transport.mpeg1_audio_bytes().unwrap(), MP2);
    assert_eq!(decode_stream(&expected_video).unwrap().len(), 12);
    assert_ffmpeg_decodes_mpegts(&output.data);
}

#[cfg(feature = "mpegts")]
#[test]
fn layer2_end_policy_is_explicit_at_subframe_boundaries() {
    let (sequence, sources) = fixture();
    let plan = plan_interframe_video(&sequence, &sources, &[]).unwrap();
    let video_packets = execute_packet_plan(&plan, &sources).unwrap();
    let audio = Layer2AudioInput {
        data: MP2,
        start: timestamp(1, Rational::new(1, 48_000).unwrap()),
    };

    let exact = plan_mpeg2_mpegts(
        &plan,
        &video_packets,
        Some(audio),
        MpegTsRenderOptions::default(),
    )
    .unwrap_err();
    assert!(exact.to_string().contains("cannot end exactly"));

    let contained = plan_mpeg2_mpegts(
        &plan,
        &video_packets,
        Some(audio),
        MpegTsRenderOptions {
            audio_boundary: AudioBoundaryPolicy::Contained,
            ..MpegTsRenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(contained.report().selected_audio_frames, 19);
    assert_eq!(contained.report().av_end_delta_micros, Some(-23_979));

    let cover = plan_mpeg2_mpegts(
        &plan,
        &video_packets,
        Some(audio),
        MpegTsRenderOptions {
            audio_boundary: AudioBoundaryPolicy::Cover,
            ..MpegTsRenderOptions::default()
        },
    )
    .unwrap();
    assert_eq!(cover.report().selected_audio_frames, 20);
    assert_eq!(cover.report().av_end_delta_micros, Some(21));
}

#[cfg(feature = "mpeg2")]
fn assert_ffmpeg_decodes(data: &[u8]) {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping FFmpeg MPEG-2 bridge check: ffmpeg is unavailable");
        return;
    }
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "mpegvideo",
            "-i",
            "pipe:0",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "FFmpeg rejected MPEG-2 bridge: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "mpegts")]
fn assert_ffmpeg_decodes_mpegts(data: &[u8]) {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("skipping FFmpeg MPEG-TS A/V check: ffmpeg is unavailable");
        return;
    }
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-f", "mpegts", "-i", "pipe:0", "-map", "0:v:0", "-map", "0:a:0", "-f",
            "null", "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "FFmpeg rejected MPEG-TS A/V render: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
