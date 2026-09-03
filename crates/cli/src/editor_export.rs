//! Project-aware export planning and initial MPEG-2/TS delivery.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use mmrecode_core::{
    CodecDescriptor, CodecId, ColorDescription, ColorRange, FieldOrder, FrameTiming, MediaType,
    Muxer, PixelFormat, Plane, Rational, StreamDescriptor, StreamId, Timestamp, TimestampRounding,
    VideoFrame,
};
use mmrecode_edit::{
    Clip, ClipId, EditSequence, MediaOrigin, MediaPath, MediaSource, OutputIntent, SourceId,
    TimeRange, Track, TrackId, VisualScaleMode,
};
use mmrecode_mpeg2::{FrameRate, Mpeg2EncodeOptions, Mpeg2PictureDecoder, PictureType};

#[allow(clippy::too_many_lines)]
pub(crate) fn export_project(
    session: &mmrecode_edit::EditorSession,
    output: Option<&Path>,
    requested_preset: Option<&str>,
) -> Result<String, String> {
    let preset = requested_preset
        .map(str::to_owned)
        .or_else(|| infer_preset(output))
        .unwrap_or_else(|| "mpeg2-ts".into());
    if preset != "mpeg2-ts" {
        return Err(format!(
            "export preset '{preset}' is not executable yet; available now: mpeg2-ts (YouTube delivery needs the future H.264/MP4 slice)"
        ));
    }

    let entries = session
        .project()
        .list(&MediaPath::root())
        .map_err(|error| error.to_string())?;
    if entries.is_empty() {
        return Err("the project timeline is empty; there is nothing to export".into());
    }
    if entries.len() != 1 || entries[0].timeline_range.start.value != 0 {
        return export_root_timeline(session, &entries, output);
    }
    let link = session
        .project()
        .link(entries[0].link_id)
        .ok_or_else(|| "root media placement disappeared".to_owned())?;
    let media = session
        .project()
        .media(link.media_id)
        .ok_or_else(|| "root media definition disappeared".to_owned())?;
    if media.kind.as_str() != "video/mpeg2" {
        return Err(format!(
            "mpeg2-ts export requires video/mpeg2, found {}",
            media.kind.as_str()
        ));
    }
    let project_time_base = session
        .project()
        .settings()
        .time_base()
        .map_err(|error| error.to_string())?;
    let source_path =
        resolve_media_path(&media.origin, session.project_file().and_then(Path::parent))?;
    let elementary = read_elementary_video(&source_path)?;
    let stream = mmrecode_mpeg2::parse_stream(&elementary).map_err(|error| error.to_string())?;
    let sequence = &stream
        .pictures()
        .first()
        .ok_or_else(|| "MPEG-2 source contains no pictures".to_owned())?
        .sequence;
    let settings = session.project().settings();
    let project_progressive = match settings.scan_mode {
        mmrecode_edit::ProjectScanMode::Progressive => true,
        mmrecode_edit::ProjectScanMode::Interlaced => false,
        _ => {
            return Err("mpeg2-ts export does not understand this project scan mode".into());
        }
    };
    let dimensions_match =
        sequence.width == settings.width as usize && sequence.height == settings.height as usize;
    let rate_matches = media.time_base == project_time_base;
    let scan_matches = sequence.progressive_sequence == project_progressive;
    if !(dimensions_match && rate_matches && scan_matches) {
        return export_full_render(
            session,
            link,
            media,
            &source_path,
            &elementary,
            &stream,
            output,
            dimensions_match,
            rate_matches,
            scan_matches,
        );
    }
    let source_id = SourceId(0);
    let stream_id = StreamId(0);
    let clip_id = ClipId(0);
    let packet_source = mmrecode_render::analyze_mpeg2_source(&elementary, source_id, stream_id)
        .map_err(|error| error.to_string())?;
    let picture_count = i64::try_from(packet_source.packets.len())
        .map_err(|_| "MPEG-2 picture count exceeds editor limits".to_owned())?;
    if link.source_range.end.value > picture_count {
        return Err("project source range exceeds the MPEG-2 picture count".into());
    }
    let duration = link
        .source_range
        .duration()
        .map_err(|error| error.to_string())?;
    let timeline_range = time_range(0, duration.value, project_time_base)?;
    let sequence = EditSequence {
        time_base: project_time_base,
        sources: vec![MediaSource {
            id: source_id,
            locator: source_path.to_string_lossy().into_owned(),
            streams: vec![StreamDescriptor {
                id: stream_id,
                codec: CodecDescriptor {
                    codec_id: CodecId::new("video/mpeg2"),
                    codec_tag: None,
                    media_type: MediaType::Video,
                    configuration: Vec::new(),
                },
                time_base: media.time_base,
            }],
        }],
        tracks: vec![Track {
            id: TrackId(0),
            media_type: MediaType::Video,
            clips: vec![Clip {
                id: clip_id,
                source_id,
                source_stream_id: stream_id,
                source_range: link.source_range,
                timeline_range,
                effects: Vec::new(),
            }],
            transitions: Vec::new(),
        }],
        output: OutputIntent {
            time_base: project_time_base,
            container: Some("container/mpegts".into()),
            video_codec: Some(CodecId::new("video/mpeg2")),
            audio_codec: None,
        },
    };
    let render_plan = mmrecode_render::plan_interframe_video(
        &sequence,
        std::slice::from_ref(&packet_source),
        &[],
    )
    .map_err(|error| error.to_string())?;
    let source_start = mmrecode_edit::format_compact_timecode(
        link.source_range.start.value,
        link.source_range.start.time_base,
    )
    .map_err(|error| error.to_string())?;
    let source_end = mmrecode_edit::format_compact_timecode(
        link.source_range.end.value,
        link.source_range.end.time_base,
    )
    .map_err(|error| error.to_string())?;
    let timeline_end =
        mmrecode_edit::format_compact_timecode(timeline_range.end.value, project_time_base)
            .map_err(|error| error.to_string())?;
    let mut report = format!(
        "Export preset: mpeg2-ts\nTimeline: 0:00..{timeline_end}\nPath: packet-preserving project-timeline optimization\nPlacement: {} <- {}\nSource range: {source_start}..{source_end}\nWork: {} decode, {} encode, {} copy packet(s)\nDelivery: MPEG-TS, MPEG-2 Video, no audio",
        link.alias,
        source_path.display(),
        render_plan.summary.decoded_frames,
        render_plan.summary.encoded_frames,
        render_plan.summary.copied_packets,
    );
    let Some(output) = output else {
        return Ok(report);
    };
    let mpeg2 = mmrecode_render::execute_mpeg2_plan_with_report(
        &render_plan,
        std::slice::from_ref(&packet_source),
        &[],
        mmrecode_render::Mpeg2BridgeOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    let delivery = mmrecode_render::plan_mpeg2_mpegts(
        &render_plan,
        &mpeg2.packets,
        None,
        mmrecode_render::MpegTsRenderOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    let rendered =
        mmrecode_render::execute_mpeg2_mpegts(&delivery).map_err(|error| error.to_string())?;
    std::fs::write(output, &rendered.data)
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    let _ = write!(
        report,
        "\n{}\n{}\nWrote {} bytes to {}",
        mpeg2.splice.explanation(),
        rendered.report.explanation(),
        rendered.data.len(),
        output.display()
    );
    Ok(report)
}

struct TimelinePlacement {
    alias: String,
    link: mmrecode_edit::MediaLink,
    media: mmrecode_edit::MediaNode,
    source_path: PathBuf,
    elementary: Vec<u8>,
}

#[derive(Default)]
struct TimelineDecodeState {
    decoder: Mpeg2PictureDecoder,
    decode_cursor: usize,
    decoded_by_presentation: BTreeMap<i64, VideoFrame>,
    last_source: Option<(i64, VideoFrame)>,
}

#[allow(clippy::too_many_lines)]
fn export_root_timeline(
    session: &mmrecode_edit::EditorSession,
    entries: &[mmrecode_edit::MediaListing],
    output: Option<&Path>,
) -> Result<String, String> {
    let settings = session.project().settings();
    if !matches!(
        settings.scan_mode,
        mmrecode_edit::ProjectScanMode::Progressive
    ) {
        return Err(
            "full timeline rendering currently requires a progressive project; a single compatible interlaced placement can still be packet copied"
                .into(),
        );
    }
    let canvas_width = usize::try_from(settings.width)
        .map_err(|_| "project canvas width exceeds platform limits".to_owned())?;
    let canvas_height = usize::try_from(settings.height)
        .map_err(|_| "project canvas height exceeds platform limits".to_owned())?;
    validate_render_canvas(canvas_width, canvas_height)?;
    let project_time_base = settings.time_base().map_err(|error| error.to_string())?;
    let frame_rate = mpeg_frame_rate(project_time_base)?;
    let output_frames = entries
        .iter()
        .map(|entry| entry.timeline_range.end.value)
        .max()
        .ok_or_else(|| "the project timeline is empty".to_owned())?;
    if output_frames <= 0 {
        return Err("the project timeline has no frames to export".into());
    }

    let mut placements = Vec::with_capacity(entries.len());
    for entry in entries {
        let link = session
            .project()
            .link(entry.link_id)
            .ok_or_else(|| "root media placement disappeared".to_owned())?
            .clone();
        let media = session
            .project()
            .media(link.media_id)
            .ok_or_else(|| "root media definition disappeared".to_owned())?
            .clone();
        if media.kind.as_str() != "video/mpeg2" {
            return Err(format!(
                "timeline placement '{}' is {}; the current mpeg2-ts timeline renderer supports video/mpeg2 placements only",
                entry.alias,
                media.kind.as_str()
            ));
        }
        let source_path =
            resolve_media_path(&media.origin, session.project_file().and_then(Path::parent))?;
        let elementary = read_elementary_video(&source_path)?;
        placements.push(TimelinePlacement {
            alias: entry.alias.clone(),
            link,
            media,
            source_path,
            elementary,
        });
    }
    let streams = placements
        .iter()
        .map(|placement| {
            mmrecode_mpeg2::parse_stream(&placement.elementary)
                .map_err(|error| format!("{}: {error}", placement.source_path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (placement, stream) in placements.iter().zip(&streams) {
        let picture_count = i64::try_from(stream.pictures().len())
            .map_err(|_| "MPEG-2 picture count exceeds editor limits".to_owned())?;
        if stream.pictures().is_empty() {
            return Err(format!(
                "timeline placement '{}' contains no MPEG-2 pictures",
                placement.alias
            ));
        }
        if !stream.pictures()[0].sequence.progressive_sequence {
            return Err(format!(
                "timeline placement '{}' is interlaced; full timeline rendering currently supports progressive MPEG-2 sources",
                placement.alias
            ));
        }
        if placement.link.source_range.start.value < 0
            || placement.link.source_range.end.value > picture_count
        {
            return Err(format!(
                "timeline placement '{}' source range exceeds its MPEG-2 picture count",
                placement.alias
            ));
        }
    }
    let dependencies = streams
        .iter()
        .map(|stream| {
            mmrecode_mpeg2::analyze_dependencies(stream).map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let timeline_end = mmrecode_edit::format_compact_timecode(output_frames, project_time_base)
        .map_err(|error| error.to_string())?;
    let mut report = format!(
        "Export preset: mpeg2-ts\nTimeline: 0:00..{timeline_end}\nPath: full project-timeline render\nPlacements: {} in project composition order",
        placements.len()
    );
    for placement in &placements {
        let timeline_start = mmrecode_edit::format_compact_timecode(
            placement.link.timeline_range.start.value,
            project_time_base,
        )
        .map_err(|error| error.to_string())?;
        let timeline_end = mmrecode_edit::format_compact_timecode(
            placement.link.timeline_range.end.value,
            project_time_base,
        )
        .map_err(|error| error.to_string())?;
        let _ = write!(
            report,
            "\n  {}: {timeline_start}..{timeline_end}, {}/{} fps, scale {}, source {}",
            placement.alias,
            placement.media.time_base.denominator(),
            placement.media.time_base.numerator(),
            placement.link.scale_mode.as_str(),
            placement.source_path.display()
        );
    }
    let _ = write!(
        report,
        "\nWork: render {output_frames} project frame(s), including timeline gaps; 0 copy packet(s)\nDelivery: MPEG-TS, MPEG-2 Video, no audio"
    );
    let Some(output) = output else {
        return Ok(report);
    };

    let (encoded, decoded_pictures) = render_timeline_and_encode(
        &placements,
        &streams,
        &dependencies,
        project_time_base,
        output_frames,
        canvas_width,
        canvas_height,
        frame_rate,
    )?;
    let rendered = mux_mpeg2(&encoded, project_time_base)?;
    std::fs::write(output, &rendered)
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    let _ = write!(
        report,
        "\nDecoded {decoded_pictures} required source picture(s) and rendered {output_frames} timeline frame(s)\nWrote {} bytes to {}",
        rendered.len(),
        output.display()
    );
    Ok(report)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_timeline_and_encode(
    placements: &[TimelinePlacement],
    streams: &[mmrecode_mpeg2::Mpeg2Stream<'_>],
    dependencies: &[Vec<mmrecode_core::AccessUnitInfo>],
    project_time_base: Rational,
    output_frames: i64,
    canvas_width: usize,
    canvas_height: usize,
    frame_rate: FrameRate,
) -> Result<(Vec<u8>, usize), String> {
    let output_frames_usize = usize::try_from(output_frames)
        .map_err(|_| "output frame count exceeds platform limits".to_owned())?;
    let mut states: Vec<TimelineDecodeState> = (0..placements.len())
        .map(|_| TimelineDecodeState::default())
        .collect();
    let mut decoded_pictures = 0_usize;
    let mut chunk = Vec::with_capacity(12);
    let mut elementary_output = Vec::new();

    for output_index in 0..output_frames_usize {
        let output_index_i64 = i64::try_from(output_index)
            .map_err(|_| "output frame index exceeds editor limits".to_owned())?;
        let active = placements.iter().rposition(|placement| {
            output_index_i64 >= placement.link.timeline_range.start.value
                && output_index_i64 < placement.link.timeline_range.end.value
        });
        let mut frame = if let Some(index) = active {
            let placement = &placements[index];
            let local_timeline = output_index_i64 - placement.link.timeline_range.start.value;
            let local_source = Timestamp {
                value: local_timeline,
                time_base: project_time_base,
            }
            .rescale(placement.media.time_base, TimestampRounding::Floor)
            .map_err(|error| error.to_string())?
            .value;
            let source_index = placement
                .link
                .source_range
                .start
                .value
                .checked_add(local_source)
                .ok_or_else(|| "source frame index overflows".to_owned())?
                .min(placement.link.source_range.end.value - 1);
            let state = &mut states[index];
            let source_frame = if let Some((last_index, frame)) = &state.last_source {
                if *last_index == source_index {
                    frame.clone()
                } else {
                    decode_timeline_source_frame(
                        placement,
                        &streams[index],
                        &dependencies[index],
                        state,
                        source_index,
                        &mut decoded_pictures,
                    )?
                }
            } else {
                decode_timeline_source_frame(
                    placement,
                    &streams[index],
                    &dependencies[index],
                    state,
                    source_index,
                    &mut decoded_pictures,
                )?
            };
            state.last_source = Some((source_index, source_frame.clone()));
            if source_frame.width == canvas_width && source_frame.height == canvas_height {
                source_frame
            } else {
                mmrecode_render::scale_yuv420_to_canvas(
                    &source_frame,
                    canvas_width,
                    canvas_height,
                    placement.link.scale_mode,
                )
                .map_err(|error| error.to_string())?
            }
        } else {
            black_project_frame(canvas_width, canvas_height)
        };
        frame.timing.pts = Some(Timestamp {
            value: output_index_i64,
            time_base: project_time_base,
        });
        frame.timing.duration = Some(Timestamp {
            value: 1,
            time_base: project_time_base,
        });
        chunk.push(frame);
        if chunk.len() == 12 || output_index + 1 == output_frames_usize {
            encode_chunk(
                &chunk,
                output_index + 1 - chunk.len(),
                frame_rate,
                &mut elementary_output,
            )?;
            chunk.clear();
        }
    }
    Ok((elementary_output, decoded_pictures))
}

fn decode_timeline_source_frame(
    placement: &TimelinePlacement,
    stream: &mmrecode_mpeg2::Mpeg2Stream<'_>,
    dependencies: &[mmrecode_core::AccessUnitInfo],
    state: &mut TimelineDecodeState,
    source_index: i64,
    decoded_pictures: &mut usize,
) -> Result<VideoFrame, String> {
    decode_source_frame(
        &placement.elementary,
        stream,
        dependencies,
        &mut state.decoder,
        &mut state.decode_cursor,
        &mut state.decoded_by_presentation,
        source_index,
        decoded_pictures,
    )
}

fn black_project_frame(width: usize, height: usize) -> VideoFrame {
    VideoFrame {
        format: PixelFormat::Yuv420p8,
        width,
        height,
        planes: vec![
            Plane {
                data: vec![16; width * height],
                stride: width,
                width,
                height,
            },
            Plane {
                data: vec![128; width * height / 4],
                stride: width / 2,
                width: width / 2,
                height: height / 2,
            },
            Plane {
                data: vec![128; width * height / 4],
                stride: width / 2,
                width: width / 2,
                height: height / 2,
            },
        ],
        timing: FrameTiming::default(),
        color: ColorDescription {
            range: ColorRange::Limited,
            ..ColorDescription::default()
        },
        field_order: FieldOrder::Progressive,
    }
}

fn validate_render_canvas(width: usize, height: usize) -> Result<(), String> {
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || width > 1_920
        || height > 1_152
    {
        return Err(
            "full-render MPEG-2 export requires an even project canvas through 1920x1152".into(),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn export_full_render(
    session: &mmrecode_edit::EditorSession,
    link: &mmrecode_edit::MediaLink,
    media: &mmrecode_edit::MediaNode,
    source_path: &Path,
    elementary: &[u8],
    stream: &mmrecode_mpeg2::Mpeg2Stream<'_>,
    output: Option<&Path>,
    dimensions_match: bool,
    rate_matches: bool,
    scan_matches: bool,
) -> Result<String, String> {
    let settings = session.project().settings();
    if !matches!(
        settings.scan_mode,
        mmrecode_edit::ProjectScanMode::Progressive
    ) || !stream
        .pictures()
        .first()
        .is_some_and(|picture| picture.sequence.progressive_sequence)
    {
        return Err(
            "full-render MPEG-2 export currently requires progressive source and project video; packet-preserving export still supports matching interlaced material"
                .into(),
        );
    }
    let canvas_width = usize::try_from(settings.width)
        .map_err(|_| "project canvas width exceeds platform limits".to_owned())?;
    let canvas_height = usize::try_from(settings.height)
        .map_err(|_| "project canvas height exceeds platform limits".to_owned())?;
    validate_render_canvas(canvas_width, canvas_height)?;
    let project_time_base = settings.time_base().map_err(|error| error.to_string())?;
    let frame_rate = mpeg_frame_rate(project_time_base)?;
    let output_frames = link
        .timeline_range
        .duration()
        .map_err(|error| error.to_string())?
        .value;
    if output_frames <= 0 {
        return Err("full-render MPEG-2 export has no project frames to write".into());
    }
    if link.source_range.end.value
        > i64::try_from(stream.pictures().len())
            .map_err(|_| "MPEG-2 picture count exceeds editor limits".to_owned())?
    {
        return Err("project source range exceeds the MPEG-2 picture count".into());
    }

    let first = &stream
        .pictures()
        .first()
        .ok_or_else(|| "MPEG-2 source contains no pictures".to_owned())?
        .sequence;
    let mut reasons = Vec::new();
    if !dimensions_match {
        reasons.push(format!(
            "canvas {}x{} -> {}x{} ({})",
            first.width,
            first.height,
            canvas_width,
            canvas_height,
            link.scale_mode.as_str()
        ));
    }
    if !rate_matches {
        reasons.push(format!(
            "rate {}/{} -> {}/{} fps",
            media.time_base.denominator(),
            media.time_base.numerator(),
            settings.frame_rate.numerator(),
            settings.frame_rate.denominator()
        ));
    }
    if !scan_matches {
        reasons.push("scan conversion".into());
    }
    let source_start = mmrecode_edit::format_compact_timecode(
        link.source_range.start.value,
        link.source_range.start.time_base,
    )
    .map_err(|error| error.to_string())?;
    let source_end = mmrecode_edit::format_compact_timecode(
        link.source_range.end.value,
        link.source_range.end.time_base,
    )
    .map_err(|error| error.to_string())?;
    let timeline_end = mmrecode_edit::format_compact_timecode(output_frames, project_time_base)
        .map_err(|error| error.to_string())?;
    let mut report = format!(
        "Export preset: mpeg2-ts\nTimeline: 0:00..{timeline_end}\nPath: full project-timeline render ({})\nPlacement: {} <- {}\nSource range: {source_start}..{source_end}\nWork: decode dependencies, {} scale/composite, {} encode, 0 copy packet(s)\nDelivery: MPEG-TS, MPEG-2 Video, no audio",
        reasons.join(", "),
        link.alias,
        source_path.display(),
        output_frames,
        output_frames,
    );
    let Some(output) = output else {
        return Ok(report);
    };

    let (encoded, decoded_pictures) = render_and_encode_mpeg2(
        elementary,
        stream,
        link.source_range.start.value,
        link.source_range.end.value,
        media.time_base,
        project_time_base,
        output_frames,
        canvas_width,
        canvas_height,
        link.scale_mode,
        frame_rate,
    )?;
    let rendered = mux_mpeg2(&encoded, project_time_base)?;
    std::fs::write(output, &rendered)
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    let _ = write!(
        report,
        "\nRendered {decoded_pictures} required source picture(s) into {output_frames} project frame(s) with Lanczos scaling\nWrote {} bytes to {}",
        rendered.len(),
        output.display()
    );
    Ok(report)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_and_encode_mpeg2(
    elementary: &[u8],
    stream: &mmrecode_mpeg2::Mpeg2Stream<'_>,
    source_start: i64,
    source_end: i64,
    source_time_base: Rational,
    project_time_base: Rational,
    output_frames: i64,
    canvas_width: usize,
    canvas_height: usize,
    scale_mode: VisualScaleMode,
    frame_rate: FrameRate,
) -> Result<(Vec<u8>, usize), String> {
    let dependencies =
        mmrecode_mpeg2::analyze_dependencies(stream).map_err(|error| error.to_string())?;
    let mut decoder = Mpeg2PictureDecoder::default();
    let mut decode_cursor = 0_usize;
    let mut decoded_by_presentation = BTreeMap::<i64, VideoFrame>::new();
    let mut decoded_pictures = 0_usize;
    let mut last_source = None::<(i64, VideoFrame)>;
    let mut chunk = Vec::with_capacity(12);
    let mut elementary_output = Vec::new();
    let output_frames_usize = usize::try_from(output_frames)
        .map_err(|_| "output frame count exceeds platform limits".to_owned())?;

    for output_index in 0..output_frames_usize {
        let output_index_i64 = i64::try_from(output_index)
            .map_err(|_| "output frame index exceeds editor limits".to_owned())?;
        let local_source = Timestamp {
            value: output_index_i64,
            time_base: project_time_base,
        }
        .rescale(source_time_base, TimestampRounding::Floor)
        .map_err(|error| error.to_string())?
        .value;
        let source_index = source_start
            .checked_add(local_source)
            .ok_or_else(|| "source frame index overflows".to_owned())?
            .min(source_end - 1);
        let source_frame = if let Some((last_index, frame)) = &last_source {
            if *last_index == source_index {
                frame.clone()
            } else {
                let frame = decode_source_frame(
                    elementary,
                    stream,
                    &dependencies,
                    &mut decoder,
                    &mut decode_cursor,
                    &mut decoded_by_presentation,
                    source_index,
                    &mut decoded_pictures,
                )?;
                last_source = Some((source_index, frame.clone()));
                frame
            }
        } else {
            let frame = decode_source_frame(
                elementary,
                stream,
                &dependencies,
                &mut decoder,
                &mut decode_cursor,
                &mut decoded_by_presentation,
                source_index,
                &mut decoded_pictures,
            )?;
            last_source = Some((source_index, frame.clone()));
            frame
        };
        let mut frame =
            if source_frame.width == canvas_width && source_frame.height == canvas_height {
                source_frame
            } else {
                mmrecode_render::scale_yuv420_to_canvas(
                    &source_frame,
                    canvas_width,
                    canvas_height,
                    scale_mode,
                )
                .map_err(|error| error.to_string())?
            };
        frame.timing.pts = Some(Timestamp {
            value: output_index_i64,
            time_base: project_time_base,
        });
        frame.timing.duration = Some(Timestamp {
            value: 1,
            time_base: project_time_base,
        });
        chunk.push(frame);
        if chunk.len() == 12 || output_index + 1 == output_frames_usize {
            encode_chunk(
                &chunk,
                output_index + 1 - chunk.len(),
                frame_rate,
                &mut elementary_output,
            )?;
            chunk.clear();
        }
    }
    Ok((elementary_output, decoded_pictures))
}

#[allow(clippy::too_many_arguments)]
fn decode_source_frame(
    elementary: &[u8],
    stream: &mmrecode_mpeg2::Mpeg2Stream<'_>,
    dependencies: &[mmrecode_core::AccessUnitInfo],
    decoder: &mut Mpeg2PictureDecoder,
    decode_cursor: &mut usize,
    decoded_by_presentation: &mut BTreeMap<i64, VideoFrame>,
    target: i64,
    decoded_pictures: &mut usize,
) -> Result<VideoFrame, String> {
    while !decoded_by_presentation.contains_key(&target) {
        let picture = stream
            .pictures()
            .get(*decode_cursor)
            .ok_or_else(|| format!("MPEG-2 source ended before presentation frame {target}"))?;
        let dependency = dependencies
            .get(*decode_cursor)
            .ok_or_else(|| "MPEG-2 dependency index is incomplete".to_owned())?;
        let required = matches!(
            picture.header.picture_coding_type,
            PictureType::I | PictureType::P
        ) || dependency.presentation_order == target;
        if required {
            let decoded_picture = decoder
                .decode_picture(
                    elementary,
                    picture,
                    dependency.decode_order,
                    dependency.presentation_order,
                )
                .map_err(|error| error.to_string())?;
            *decoded_pictures = decoded_pictures.saturating_add(1);
            if decoded_picture.presentation_order >= target {
                decoded_by_presentation
                    .insert(decoded_picture.presentation_order, decoded_picture.frame);
            }
        }
        *decode_cursor = decode_cursor.saturating_add(1);
    }
    let frame = decoded_by_presentation
        .remove(&target)
        .ok_or_else(|| format!("decoded MPEG-2 frame {target} disappeared"))?;
    decoded_by_presentation.retain(|presentation, _| *presentation > target);
    Ok(frame)
}

fn encode_chunk(
    frames: &[VideoFrame],
    first_output_frame: usize,
    frame_rate: FrameRate,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    let mut options = Mpeg2EncodeOptions {
        frame_rate,
        gop_size: frames.len(),
        b_frames: 2_usize.min(frames.len().saturating_sub(1)),
        quantiser_scale_code: 4,
        motion_search_range: 2,
        ..Mpeg2EncodeOptions::default()
    };
    options.sequence.timecode_start_frame = u64::try_from(first_output_frame)
        .map_err(|_| "output timecode exceeds MPEG-2 limits".to_owned())?;
    options.sequence.aspect_ratio_information =
        aspect_ratio_code(frames[0].width, frames[0].height);
    let rate = frame_rate.rational();
    let high_level = frames[0].width > 720
        || frames[0].height > 576
        || rate.numerator() > 30_i64.saturating_mul(rate.denominator());
    if high_level {
        options.sequence.profile_and_level_indication = 0x44;
        options.sequence.bit_rate = 50_000_000;
    }
    let encoded =
        mmrecode_mpeg2::encode_stream(frames, options).map_err(|error| error.to_string())?;
    output.extend_from_slice(&encoded.data);
    Ok(())
}

fn mux_mpeg2(elementary: &[u8], time_base: Rational) -> Result<Vec<u8>, String> {
    let packet_source = mmrecode_render::analyze_mpeg2_source(elementary, SourceId(0), StreamId(0))
        .map_err(|error| error.to_string())?;
    let mut muxer = mmrecode_mpegts::MpegTsMuxer::new();
    let stream_id = muxer
        .add_stream(StreamDescriptor {
            id: StreamId(0),
            codec: CodecDescriptor {
                codec_id: CodecId::new("video/mpeg2"),
                codec_tag: None,
                media_type: MediaType::Video,
                configuration: Vec::new(),
            },
            time_base,
        })
        .map_err(|error| error.to_string())?;
    for analyzed in packet_source.packets {
        let mut packet = analyzed.packet;
        packet.stream_id = stream_id;
        muxer
            .write_packet(packet)
            .map_err(|error| error.to_string())?;
    }
    muxer.finalize().map_err(|error| error.to_string())?;
    muxer.into_bytes().map_err(|error| error.to_string())
}

fn mpeg_frame_rate(time_base: Rational) -> Result<FrameRate, String> {
    match (time_base.numerator(), time_base.denominator()) {
        (1_001, 24_000) => Ok(FrameRate::Fps23_976),
        (1, 24) => Ok(FrameRate::Fps24),
        (1, 25) => Ok(FrameRate::Fps25),
        (1_001, 30_000) => Ok(FrameRate::Fps29_97),
        (1, 30) => Ok(FrameRate::Fps30),
        (1, 50) => Ok(FrameRate::Fps50),
        (1_001, 60_000) => Ok(FrameRate::Fps59_94),
        (1, 60) => Ok(FrameRate::Fps60),
        _ => Err(format!(
            "full-render MPEG-2 export does not support project frame time base {}/{}; use 24000/1001, 24, 25, 30000/1001, 30, 50, 60000/1001, or 60 fps",
            time_base.numerator(),
            time_base.denominator()
        )),
    }
}

fn aspect_ratio_code(width: usize, height: usize) -> u8 {
    if width.saturating_mul(9) == height.saturating_mul(16) {
        3
    } else if width.saturating_mul(3) == height.saturating_mul(4) {
        2
    } else {
        1
    }
}

fn infer_preset(output: Option<&Path>) -> Option<String> {
    let extension = output?.extension()?.to_str()?;
    extension
        .eq_ignore_ascii_case("ts")
        .then(|| "mpeg2-ts".into())
}

fn resolve_media_path(
    origin: &MediaOrigin,
    project_directory: Option<&Path>,
) -> Result<PathBuf, String> {
    match origin {
        MediaOrigin::Managed { path } => project_directory
            .map(|directory| directory.join(path))
            .ok_or_else(|| "managed media requires the project to be saved first".into()),
        MediaOrigin::External { path } => Ok(path.clone()),
        MediaOrigin::Generated => Err("generated media cannot be exported by mpeg2-ts yet".into()),
        _ => Err("this media origin is not supported by mpeg2-ts export".into()),
    }
}

fn read_elementary_video(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE && bytes[0] == 0x47 {
        return mmrecode_mpegts::demux_transport_stream(&bytes)
            .map_err(|error| error.to_string())?
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string());
    }
    Ok(bytes)
}

fn time_range(
    start: i64,
    end: i64,
    time_base: mmrecode_core::Rational,
) -> Result<TimeRange, String> {
    TimeRange::new(
        Timestamp {
            value: start,
            time_base,
        },
        Timestamp {
            value: end,
            time_base,
        },
    )
    .map_err(|error| error.to_string())
}
