//! Project-aware MPEG-2/TS and YouTube-oriented H.264/MP4 delivery.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
};

use mmrecode_core::{
    AudioEncoder, AudioEncoderSettings, AudioFrame, AudioSampleFormat, CodecDescriptor, CodecId,
    ColorDescription, ColorRange, Decoder, Encoder, FieldOrder, FourCc, FrameTiming, MediaType,
    Muxer, Packet, PacketFlags, PixelFormat, Plane, Rational, StreamDescriptor, StreamId,
    Timestamp, TimestampRounding, VideoEncoderSettings, VideoFrame,
};
use mmrecode_edit::{
    Clip, ClipId, EditSequence, MediaOrigin, MediaPath, MediaSource, OutputIntent, SourceId,
    TimeRange, Track, TrackId, VisualScaleMode,
};
use mmrecode_mpeg2::{FrameRate, Mpeg2EncodeOptions, Mpeg2PictureDecoder, PictureType};
use mmrecode_render::FlattenedProjectPlacement;

#[derive(Clone, Copy)]
struct YoutubeProfile {
    name: &'static str,
    width: usize,
    height: usize,
    standard_bitrate: u64,
    high_frame_rate_bitrate: u64,
}

impl YoutubeProfile {
    const HD: Self = Self {
        name: "youtube-1080p",
        width: 1_920,
        height: 1_080,
        standard_bitrate: 8_000_000,
        high_frame_rate_bitrate: 12_000_000,
    };
    const UHD: Self = Self {
        name: "youtube-2160p",
        width: 3_840,
        height: 2_160,
        standard_bitrate: 40_000_000,
        high_frame_rate_bitrate: 60_000_000,
    };
}

#[derive(Clone, Copy)]
enum TimelineDelivery {
    Mpeg2Ts,
    Youtube(YoutubeProfile),
}

#[allow(clippy::too_many_lines)]
pub(crate) fn export_project(
    session: &mmrecode_edit::EditorSession,
    output: Option<&Path>,
    requested_preset: Option<&str>,
) -> Result<String, String> {
    let preset = requested_preset
        .map(str::to_owned)
        .or_else(|| infer_preset(output, session.project().settings()))
        .unwrap_or_else(|| "mpeg2-ts".into());
    let delivery = match preset.as_str() {
        "mpeg2-ts" => TimelineDelivery::Mpeg2Ts,
        "youtube-1080p" => TimelineDelivery::Youtube(YoutubeProfile::HD),
        "youtube-2160p" => TimelineDelivery::Youtube(YoutubeProfile::UHD),
        _ => return Err(format!("unknown export preset '{preset}'")),
    };

    let entries = session
        .project()
        .list(&MediaPath::root())
        .map_err(|error| error.to_string())?;
    if entries.is_empty() {
        return Err("the project timeline is empty; there is nothing to export".into());
    }
    let flattened =
        mmrecode_render::flatten_project_timeline(session.project(), session.project().root_id())
            .map_err(|error| error.to_string())?;
    if matches!(delivery, TimelineDelivery::Youtube(_)) {
        return export_root_timeline(session, &flattened, output, delivery);
    }
    if flattened.len() != 1
        || entries.len() != 1
        || entries[0].timeline_range.start.value != 0
        || entries[0].kind.as_str() != "video/mpeg2"
    {
        return export_root_timeline(session, &flattened, output, delivery);
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
    path: String,
    mapping: FlattenedProjectPlacement,
    media: mmrecode_edit::MediaNode,
    source_path: PathBuf,
    audio: Option<AudioFrame>,
    elementary: Option<Vec<u8>>,
    h264_frames: Option<Vec<VideoFrame>>,
}

#[derive(Default)]
struct TimelineDecodeState {
    decoder: Mpeg2PictureDecoder,
    decode_cursor: usize,
    decoded_by_presentation: BTreeMap<i64, VideoFrame>,
    last_source: Option<(i64, VideoFrame)>,
}

struct YoutubeAudioOutput {
    track: mmrecode_isobmff::Track,
    packets: Vec<Packet>,
    edit: Option<mmrecode_isobmff::TrackMuxEdit>,
}

#[allow(clippy::too_many_lines)]
fn export_root_timeline(
    session: &mmrecode_edit::EditorSession,
    flattened: &[FlattenedProjectPlacement],
    output: Option<&Path>,
    delivery: TimelineDelivery,
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
    match delivery {
        TimelineDelivery::Mpeg2Ts => validate_render_canvas(canvas_width, canvas_height)?,
        TimelineDelivery::Youtube(profile)
            if (canvas_width, canvas_height) != (profile.width, profile.height) =>
        {
            return Err(format!(
                "{} requires a {}x{} project; current canvas is {}x{}",
                profile.name, profile.width, profile.height, canvas_width, canvas_height
            ));
        }
        TimelineDelivery::Youtube(_) => {}
    }
    let project_time_base = settings.time_base().map_err(|error| error.to_string())?;
    let frame_rate = mpeg_frame_rate(project_time_base)?;
    let output_frames = session
        .project()
        .media(session.project().root_id())
        .ok_or_else(|| "project root disappeared".to_owned())?
        .duration
        .value;
    if output_frames <= 0 {
        return Err("the project timeline has no frames to export".into());
    }

    let mut placements = Vec::with_capacity(flattened.len());
    for mapping in flattened {
        let media = session
            .project()
            .media(mapping.media_id)
            .ok_or_else(|| "flattened media definition disappeared".to_owned())?
            .clone();
        if media.kind.is_mmfx_scene() {
            continue;
        }
        if !matches!(media.kind.as_str(), "video/mpeg2" | "video/h264") {
            return Err(format!(
                "timeline placement '{}' is {}; the current delivery renderer supports video/mpeg2, video/h264, and generated MMFX scene placements",
                mapping.display_path,
                media.kind.as_str()
            ));
        }
        let source_path =
            resolve_media_path(&media.origin, session.project_file().and_then(Path::parent))?;
        let source_bytes = std::fs::read(&source_path)
            .map_err(|error| format!("cannot read '{}': {error}", source_path.display()))?;
        let audio = if matches!(delivery, TimelineDelivery::Youtube(_)) {
            mmrecode_playback::decode_audio_source(&source_bytes)
                .map_err(|error| format!("{}: {error}", source_path.display()))?
        } else {
            None
        };
        let (elementary, h264_frames) = if media.kind.as_str() == "video/mpeg2" {
            (Some(elementary_video_bytes(&source_bytes)?), None)
        } else {
            (None, Some(decode_h264_video(&source_bytes)?))
        };
        placements.push(TimelinePlacement {
            path: mapping.display_path.clone(),
            mapping: mapping.clone(),
            media,
            source_path,
            audio,
            elementary,
            h264_frames,
        });
    }
    let project_directory = session.project_file().and_then(Path::parent);
    let mut compositor = mmrecode_render::ProjectCompositor::new();
    let compositor_sync = compositor.synchronize(
        session.project(),
        session.project().root_id(),
        |_, source, scene| {
            let base_directory = source
                .resource_base
                .as_deref()
                .or(project_directory)
                .unwrap_or_else(|| Path::new("."));
            crate::load_mmfx_resources(scene, base_directory)
        },
    );
    if !compositor_sync.diagnostics.is_empty() {
        return Err(compositor_sync
            .diagnostics
            .into_iter()
            .map(|diagnostic| format!("MMFX {:?}: {}", diagnostic.media_id, diagnostic.message))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let streams = placements
        .iter()
        .map(|placement| {
            placement
                .elementary
                .as_deref()
                .map(|elementary| {
                    mmrecode_mpeg2::parse_stream(elementary)
                        .map_err(|error| format!("{}: {error}", placement.source_path.display()))
                })
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (placement, stream) in placements.iter().zip(&streams) {
        let picture_count = stream.as_ref().map_or_else(
            || placement.h264_frames.as_ref().map_or(0, Vec::len),
            |stream| stream.pictures().len(),
        );
        let picture_count = i64::try_from(picture_count)
            .map_err(|_| "MPEG-2 picture count exceeds editor limits".to_owned())?;
        if picture_count == 0 {
            return Err(format!(
                "timeline placement '{}' contains no video pictures",
                placement.path
            ));
        }
        if stream
            .as_ref()
            .is_some_and(|stream| !stream.pictures()[0].sequence.progressive_sequence)
        {
            return Err(format!(
                "timeline placement '{}' is interlaced; full timeline rendering currently supports progressive MPEG-2 sources",
                placement.path
            ));
        }
        if placement.mapping.source_range.start < 0
            || placement.mapping.source_range.end > picture_count
        {
            return Err(format!(
                "timeline placement '{}' source range exceeds its MPEG-2 picture count",
                placement.path
            ));
        }
    }
    let dependencies = streams
        .iter()
        .map(|stream| {
            stream
                .as_ref()
                .map(|stream| {
                    mmrecode_mpeg2::analyze_dependencies(stream).map_err(|error| error.to_string())
                })
                .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;

    let timeline_end = mmrecode_edit::format_compact_timecode(output_frames, project_time_base)
        .map_err(|error| error.to_string())?;
    let preset_name = match delivery {
        TimelineDelivery::Mpeg2Ts => "mpeg2-ts",
        TimelineDelivery::Youtube(profile) => profile.name,
    };
    let mut report = format!(
        "Export preset: {preset_name}\nTimeline: 0:00..{timeline_end}\nPath: full project-timeline render\nPlacements: {} object(s) in project composition order; {} cached MMFX asset(s)",
        flattened.len(),
        compositor_sync.compiled_assets,
    );
    for placement in &placements {
        let timeline_start = mmrecode_edit::format_compact_timecode(
            placement.mapping.timeline_range.start,
            project_time_base,
        )
        .map_err(|error| error.to_string())?;
        let timeline_end = mmrecode_edit::format_compact_timecode(
            placement.mapping.timeline_range.end,
            project_time_base,
        )
        .map_err(|error| error.to_string())?;
        let _ = write!(
            report,
            "\n  {}: {timeline_start}..{timeline_end}, {}/{} fps, scale {}, source {}",
            placement.path,
            placement.media.time_base.denominator(),
            placement.media.time_base.numerator(),
            placement.mapping.scale_mode.as_str(),
            placement.source_path.display()
        );
    }
    for mapping in flattened {
        let Some(media) = session.project().media(mapping.media_id) else {
            continue;
        };
        if !media.kind.is_mmfx_scene() {
            continue;
        }
        let timeline_start =
            mmrecode_edit::format_compact_timecode(mapping.timeline_range.start, project_time_base)
                .map_err(|error| error.to_string())?;
        let timeline_end =
            mmrecode_edit::format_compact_timecode(mapping.timeline_range.end, project_time_base)
                .map_err(|error| error.to_string())?;
        let _ = write!(
            report,
            "\n  {}: {timeline_start}..{timeline_end}, MMFX scene, scale {}",
            mapping.display_path,
            mapping.scale_mode.as_str(),
        );
    }
    match delivery {
        TimelineDelivery::Mpeg2Ts => {
            let _ = write!(
                report,
                "\nWork: render {output_frames} project frame(s), including timeline gaps; 0 copy packet(s)\nDelivery: MPEG-TS, MPEG-2 Video, no audio"
            );
        }
        TimelineDelivery::Youtube(profile) => {
            let bitrate = youtube_bitrate(profile, project_time_base)?;
            let gop_size = youtube_gop_size(project_time_base)?;
            let _ = write!(
                report,
                "\nWork: render {output_frames} project frame(s), including timeline gaps; 0 copy packet(s)\nDelivery: Fast Start MP4, H.264 High/CABAC fast analysis, 2 B-frames, closed {gop_size}-frame GOP, {} Mbps VBR, BT.709; native AAC-LC 48 kHz stereo/384 kbps timeline mix from MP4/MOV AAC and MPEG-TS Layer II (silence where absent)",
                bitrate / 1_000_000
            );
        }
    }
    let Some(output) = output else {
        return Ok(report);
    };

    let (rendered, decoded_pictures) = match delivery {
        TimelineDelivery::Mpeg2Ts => {
            let (encoded, decoded_pictures) = render_timeline_and_encode(
                &placements,
                &streams,
                &dependencies,
                project_time_base,
                output_frames,
                canvas_width,
                canvas_height,
                frame_rate,
                &mut compositor,
                session.project(),
            )?;
            (mux_mpeg2(&encoded, project_time_base)?, decoded_pictures)
        }
        TimelineDelivery::Youtube(profile) => render_timeline_and_encode_youtube(
            &placements,
            &streams,
            &dependencies,
            project_time_base,
            output_frames,
            canvas_width,
            canvas_height,
            profile,
            &mut compositor,
            session.project(),
        )?,
    };
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
    streams: &[Option<mmrecode_mpeg2::Mpeg2Stream<'_>>],
    dependencies: &[Option<Vec<mmrecode_core::AccessUnitInfo>>],
    project_time_base: Rational,
    output_frames: i64,
    canvas_width: usize,
    canvas_height: usize,
    frame_rate: FrameRate,
    compositor: &mut mmrecode_render::ProjectCompositor,
    project: &mmrecode_edit::MediaProject,
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
        let active = placements
            .iter()
            .rposition(|placement| placement.mapping.timeline_range.contains(&output_index_i64));
        let first_overlay_order = active.map_or(0, |index| {
            placements[index]
                .mapping
                .composition_order
                .saturating_add(1)
        });
        let mut frame = if let Some(index) = active {
            let placement = &placements[index];
            let source_index = placement
                .mapping
                .source_frame_at(project, output_index_i64)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "active timeline placement '{}' has no source frame at {output_index_i64}",
                        placement.path
                    )
                })?;
            let source_frame = timeline_source_frame(
                placement,
                streams[index].as_ref(),
                dependencies[index].as_deref(),
                &mut states[index],
                source_index,
                &mut decoded_pictures,
            )?;
            if source_frame.width == canvas_width && source_frame.height == canvas_height {
                source_frame
            } else {
                mmrecode_render::scale_yuv420_to_canvas(
                    &source_frame,
                    canvas_width,
                    canvas_height,
                    placement.mapping.scale_mode,
                )
                .map_err(|error| error.to_string())?
            }
        } else {
            black_project_frame(canvas_width, canvas_height)
        };
        compositor
            .composite_yuv420_from(output_index_i64, &mut frame, first_overlay_order)
            .map_err(|error| error.to_string())?;
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

fn youtube_bitrate(profile: YoutubeProfile, time_base: Rational) -> Result<u64, String> {
    let high_frame_rate = time_base.denominator()
        > 30_i64
            .checked_mul(time_base.numerator())
            .ok_or_else(|| "project frame rate overflows".to_owned())?;
    Ok(if high_frame_rate {
        profile.high_frame_rate_bitrate
    } else {
        profile.standard_bitrate
    })
}

fn youtube_gop_size(time_base: Rational) -> Result<usize, String> {
    let frame_duration = u64::try_from(time_base.numerator())
        .map_err(|_| "YouTube export requires a positive project frame rate".to_owned())?;
    let clock = u64::try_from(time_base.denominator())
        .map_err(|_| "YouTube export requires a positive project frame rate".to_owned())?;
    let divisor = frame_duration
        .checked_mul(2)
        .ok_or_else(|| "YouTube GOP calculation overflows".to_owned())?;
    usize::try_from(clock.div_ceil(divisor).max(3))
        .map_err(|_| "YouTube GOP size exceeds platform limits".to_owned())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_timeline_and_encode_youtube(
    placements: &[TimelinePlacement],
    streams: &[Option<mmrecode_mpeg2::Mpeg2Stream<'_>>],
    dependencies: &[Option<Vec<mmrecode_core::AccessUnitInfo>>],
    project_time_base: Rational,
    output_frames: i64,
    canvas_width: usize,
    canvas_height: usize,
    profile: YoutubeProfile,
    compositor: &mut mmrecode_render::ProjectCompositor,
    project: &mmrecode_edit::MediaProject,
) -> Result<(Vec<u8>, usize), String> {
    let output_frames_usize = usize::try_from(output_frames)
        .map_err(|_| "output frame count exceeds platform limits".to_owned())?;
    let frame_duration = u64::try_from(project_time_base.numerator())
        .map_err(|_| "YouTube export requires a positive project frame rate".to_owned())?;
    let encoder_time_base =
        Rational::new(1, project_time_base.denominator()).map_err(|error| error.to_string())?;
    let mut options = BTreeMap::new();
    for (name, value) in [
        ("mode", "inter".to_owned()),
        ("profile", "high".to_owned()),
        ("entropy", "cabac".to_owned()),
        ("b_frames", "2".to_owned()),
        ("b_direct", "spatial".to_owned()),
        ("max_refs", "2".to_owned()),
        ("search_range", "16".to_owned()),
        ("analysis", "fast".to_owned()),
        ("aq_strength", "0".to_owned()),
        ("scaling_matrix", "jvt".to_owned()),
        ("color", "bt709".to_owned()),
        ("gop_size", youtube_gop_size(project_time_base)?.to_string()),
        ("frame_duration_ticks", frame_duration.to_string()),
    ] {
        options.insert(name.into(), value);
    }
    let mut encoder = mmrecode_h264::H264Encoder::default();
    let codec = encoder
        .configure(&VideoEncoderSettings {
            width: canvas_width,
            height: canvas_height,
            pixel_format: PixelFormat::Yuv420p8,
            time_base: encoder_time_base,
            bitrate: Some(youtube_bitrate(profile, project_time_base)?),
            options,
        })
        .map_err(|error| error.to_string())?;
    let mut packets = Vec::with_capacity(output_frames_usize);
    let mut states: Vec<TimelineDecodeState> = (0..placements.len())
        .map(|_| TimelineDecodeState::default())
        .collect();
    let mut decoded_pictures = 0_usize;

    for output_index in 0..output_frames_usize {
        let output_index_i64 = i64::try_from(output_index)
            .map_err(|_| "output frame index exceeds editor limits".to_owned())?;
        let active = placements
            .iter()
            .rposition(|placement| placement.mapping.timeline_range.contains(&output_index_i64));
        let first_overlay_order = active.map_or(0, |index| {
            placements[index]
                .mapping
                .composition_order
                .saturating_add(1)
        });
        let mut frame = if let Some(index) = active {
            let placement = &placements[index];
            let source_index = placement
                .mapping
                .source_frame_at(project, output_index_i64)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "active timeline placement '{}' has no source frame at {output_index_i64}",
                        placement.path
                    )
                })?;
            let source_frame = timeline_source_frame(
                placement,
                streams[index].as_ref(),
                dependencies[index].as_deref(),
                &mut states[index],
                source_index,
                &mut decoded_pictures,
            )?;
            if source_frame.width == canvas_width && source_frame.height == canvas_height {
                source_frame
            } else {
                mmrecode_render::scale_yuv420_to_canvas(
                    &source_frame,
                    canvas_width,
                    canvas_height,
                    placement.mapping.scale_mode,
                )
                .map_err(|error| error.to_string())?
            }
        } else {
            black_project_frame(canvas_width, canvas_height)
        };
        compositor
            .composite_yuv420_from(output_index_i64, &mut frame, first_overlay_order)
            .map_err(|error| error.to_string())?;
        let pts = output_index_i64
            .checked_mul(
                i64::try_from(frame_duration)
                    .map_err(|_| "YouTube frame duration exceeds timestamp limits".to_owned())?,
            )
            .ok_or_else(|| "YouTube presentation timestamp overflows".to_owned())?;
        frame.timing.pts = Some(Timestamp {
            value: pts,
            time_base: encoder_time_base,
        });
        frame.timing.duration = Some(Timestamp {
            value: i64::try_from(frame_duration)
                .map_err(|_| "YouTube frame duration exceeds timestamp limits".to_owned())?,
            time_base: encoder_time_base,
        });
        frame.field_order = FieldOrder::Progressive;
        frame.color = ColorDescription {
            range: ColorRange::Limited,
            primaries: Some("bt709".into()),
            transfer: Some("bt709".into()),
            matrix: Some("bt709".into()),
        };
        encoder
            .send_frame(frame)
            .map_err(|error| error.to_string())?;
        while let Some(packet) = encoder
            .receive_packet()
            .map_err(|error| error.to_string())?
        {
            packets.push(packet);
            let _ = encoder
                .receive_reconstructed_frame()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "H.264 encoder omitted a reconstruction".to_owned())?;
        }
    }
    encoder.flush().map_err(|error| error.to_string())?;
    while let Some(packet) = encoder
        .receive_packet()
        .map_err(|error| error.to_string())?
    {
        packets.push(packet);
        let _ = encoder
            .receive_reconstructed_frame()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "H.264 encoder omitted a reconstruction while draining".to_owned())?;
    }
    if packets.len() != output_frames_usize {
        return Err(format!(
            "H.264 encoder returned {} packets for {output_frames_usize} project frames",
            packets.len()
        ));
    }
    let video_track = mmrecode_isobmff::Track {
        descriptor: StreamDescriptor {
            id: StreamId(0),
            codec,
            time_base: encoder_time_base,
        },
        track_id: 1,
        handler_type: FourCc(*b"vide"),
        width: Some(
            u32::try_from(canvas_width)
                .map_err(|_| "YouTube width exceeds MP4 limits".to_owned())?,
        ),
        height: Some(
            u32::try_from(canvas_height)
                .map_err(|_| "YouTube height exceeds MP4 limits".to_owned())?,
        ),
        pixel_aspect: Some(mmrecode_isobmff::PixelAspectRatio {
            horizontal_spacing: 1,
            vertical_spacing: 1,
        }),
        colour: Some(mmrecode_isobmff::ColourInformation {
            primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            full_range: Some(false),
        }),
        rotation_degrees: 0,
        channel_count: None,
        sample_rate: None,
        presentation_duration: None,
        samples: Vec::new(),
    };
    let audio = encode_youtube_timeline_audio(
        placements,
        output_frames_usize,
        frame_duration,
        project_time_base,
    )?;
    let data = mmrecode_isobmff::mux_tracks(&[
        mmrecode_isobmff::TrackMuxInput {
            track: &video_track,
            packets: &packets,
            edit: None,
        },
        mmrecode_isobmff::TrackMuxInput {
            track: &audio.track,
            packets: &audio.packets,
            edit: audio.edit,
        },
    ])
    .map_err(|error| error.to_string())?;
    Ok((data, decoded_pictures))
}

#[allow(clippy::too_many_lines)]
fn encode_youtube_timeline_audio(
    placements: &[TimelinePlacement],
    video_frames: usize,
    video_frame_duration: u64,
    project_time_base: Rational,
) -> Result<YoutubeAudioOutput, String> {
    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    let audio_samples = youtube_audio_sample_count(
        video_frames,
        video_frame_duration,
        project_time_base.denominator(),
        SAMPLE_RATE,
    )?;
    let mut decoded = Vec::new();
    let mut decoded_mappings = Vec::new();
    for placement in placements {
        if let Some(audio) = placement.audio.as_ref() {
            decoded.push(audio);
            decoded_mappings.push((&placement.mapping, placement.media.time_base));
        }
    }
    if decoded.is_empty() {
        let (track, packets) = encode_youtube_silent_audio(
            video_frames,
            video_frame_duration,
            project_time_base.denominator(),
        )?;
        return Ok(YoutubeAudioOutput {
            track,
            packets,
            edit: None,
        });
    }

    let output_time_base =
        Rational::new(1, i64::from(SAMPLE_RATE)).map_err(|error| error.to_string())?;
    let mut mix_placements = Vec::with_capacity(decoded.len());
    for (audio, (mapping, media_time_base)) in decoded.into_iter().zip(decoded_mappings) {
        let source_time_base =
            Rational::new(1, i64::from(audio.sample_rate)).map_err(|error| error.to_string())?;
        let requested_start = Timestamp {
            value: mapping.source_range.start,
            time_base: media_time_base,
        }
        .rescale(source_time_base, TimestampRounding::Floor)
        .map_err(|error| error.to_string())?
        .value;
        let requested_end = Timestamp {
            value: mapping.source_range.end,
            time_base: media_time_base,
        }
        .rescale(source_time_base, TimestampRounding::Ceiling)
        .map_err(|error| error.to_string())?
        .value;
        let source_origin = audio.timing.pts.map_or(0, |pts| pts.value);
        let available_end = source_origin
            .checked_add(
                i64::try_from(audio.samples_per_channel)
                    .map_err(|_| "decoded audio duration exceeds i64".to_owned())?,
            )
            .ok_or_else(|| "decoded audio end overflows".to_owned())?;
        let selected_start = requested_start.max(source_origin);
        let selected_end = requested_end.min(available_end);
        if selected_start >= selected_end {
            continue;
        }
        let source_start = usize::try_from(selected_start - source_origin)
            .map_err(|_| "decoded audio source start exceeds usize".to_owned())?;
        let source_samples = usize::try_from(selected_end - selected_start)
            .map_err(|_| "decoded audio source duration exceeds usize".to_owned())?;
        let timeline_base = Timestamp {
            value: mapping.timeline_range.start,
            time_base: project_time_base,
        }
        .rescale(output_time_base, TimestampRounding::NearestTiesAway)
        .map_err(|error| error.to_string())?
        .value;
        let leading_source = Timestamp {
            value: selected_start - requested_start,
            time_base: source_time_base,
        }
        .rescale(output_time_base, TimestampRounding::NearestTiesAway)
        .map_err(|error| error.to_string())?
        .value;
        mix_placements.push(mmrecode_render::AudioPlacement {
            source: audio,
            timeline_start: Timestamp {
                value: timeline_base
                    .checked_add(leading_source)
                    .ok_or_else(|| "audio timeline start overflows".to_owned())?,
                time_base: output_time_base,
            },
            source_start,
            source_samples,
            gain: 1.0,
        });
    }
    let mixed = mmrecode_render::mix_audio_timeline(
        &mix_placements,
        mmrecode_render::AudioMixSettings {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            samples_per_channel: audio_samples,
        },
    )
    .map_err(|error| error.to_string())?;
    encode_youtube_program_audio(&mixed)
}

fn encode_youtube_program_audio(audio: &AudioFrame) -> Result<YoutubeAudioOutput, String> {
    const BITRATE: u64 = 384_000;
    if audio.sample_rate != 48_000
        || audio.channels != 2
        || audio.format != AudioSampleFormat::I16Interleaved
        || audio.samples_per_channel == 0
    {
        return Err("YouTube program audio must be non-empty 48 kHz stereo signed-16 PCM".into());
    }
    audio.validate().map_err(|error| error.to_string())?;
    let time_base = Rational::new(1, 48_000).map_err(|error| error.to_string())?;
    let mut encoder = mmrecode_aac::AacLcEncoder::default();
    let codec = encoder
        .configure(&AudioEncoderSettings {
            sample_rate: 48_000,
            channels: 2,
            sample_format: AudioSampleFormat::I16Interleaved,
            bitrate: Some(BITRATE),
            options: BTreeMap::new(),
        })
        .map_err(|error| error.to_string())?;
    let packet_count = audio.samples_per_channel.div_ceil(1_024);
    let mut packets = Vec::with_capacity(packet_count + 1);
    for packet_index in 0..packet_count {
        let sample_start = packet_index
            .checked_mul(1_024)
            .ok_or_else(|| "YouTube audio packet start overflows".to_owned())?;
        let sample_end = (sample_start + 1_024).min(audio.samples_per_channel);
        let mut samples = vec![0; 2_048];
        let available = (sample_end - sample_start) * 2;
        samples[..available]
            .copy_from_slice(&audio.samples[sample_start * 2..sample_start * 2 + available]);
        let pts = i64::try_from(sample_start)
            .map_err(|_| "YouTube audio timestamp exceeds i64".to_owned())?;
        encoder
            .send_frame(AudioFrame {
                format: AudioSampleFormat::I16Interleaved,
                sample_rate: 48_000,
                channels: 2,
                samples_per_channel: 1_024,
                samples,
                timing: FrameTiming {
                    pts: Some(Timestamp {
                        value: pts,
                        time_base,
                    }),
                    duration: Some(Timestamp {
                        value: 1_024,
                        time_base,
                    }),
                },
            })
            .map_err(|error| error.to_string())?;
        packets.push(
            encoder
                .receive_packet()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "AAC-LC encoder omitted a program access unit".to_owned())?,
        );
    }
    encoder.flush().map_err(|error| error.to_string())?;
    packets.push(
        encoder
            .receive_packet()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "AAC-LC encoder omitted its program overlap tail".to_owned())?,
    );
    let duration = u64::try_from(audio.samples_per_channel)
        .map_err(|_| "YouTube audio duration exceeds u64".to_owned())?;
    Ok(YoutubeAudioOutput {
        track: mmrecode_isobmff::Track {
            descriptor: StreamDescriptor {
                id: StreamId(1),
                codec,
                time_base,
            },
            track_id: 2,
            handler_type: FourCc(*b"soun"),
            width: None,
            height: None,
            pixel_aspect: None,
            colour: None,
            rotation_degrees: 0,
            channel_count: Some(2),
            sample_rate: Some(48_000),
            presentation_duration: Some(duration),
            samples: Vec::new(),
        },
        packets,
        edit: Some(mmrecode_isobmff::TrackMuxEdit {
            media_time: u64::from(mmrecode_aac::AAC_LC_PRIMING_SAMPLES),
            presentation_duration: duration,
        }),
    })
}

fn youtube_audio_sample_count(
    video_frames: usize,
    video_frame_duration: u64,
    video_timescale: i64,
    sample_rate: u32,
) -> Result<usize, String> {
    let video_timescale = u64::try_from(video_timescale)
        .map_err(|_| "YouTube video timescale must be positive".to_owned())?;
    let video_ticks = u128::try_from(video_frames)
        .map_err(|_| "YouTube frame count exceeds audio timing limits".to_owned())?
        .checked_mul(u128::from(video_frame_duration))
        .ok_or_else(|| "YouTube audio duration overflows".to_owned())?;
    let audio_samples = video_ticks
        .checked_mul(u128::from(sample_rate))
        .ok_or_else(|| "YouTube audio duration overflows".to_owned())?
        .div_ceil(u128::from(video_timescale));
    usize::try_from(audio_samples)
        .map_err(|_| "YouTube audio duration exceeds platform limits".to_owned())
}

fn encode_youtube_silent_audio(
    video_frames: usize,
    video_frame_duration: u64,
    video_timescale: i64,
) -> Result<(mmrecode_isobmff::Track, Vec<mmrecode_core::Packet>), String> {
    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    const BITRATE: u64 = 384_000;

    let audio_samples = youtube_audio_sample_count(
        video_frames,
        video_frame_duration,
        video_timescale,
        SAMPLE_RATE,
    )?;
    let packet_count = audio_samples.div_ceil(1_024);
    let time_base = Rational::new(1, i64::from(SAMPLE_RATE)).map_err(|error| error.to_string())?;
    let mut encoder = mmrecode_aac::AacLcEncoder::default();
    let codec = encoder
        .configure(&AudioEncoderSettings {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            sample_format: AudioSampleFormat::I16Interleaved,
            bitrate: Some(BITRATE),
            options: BTreeMap::new(),
        })
        .map_err(|error| error.to_string())?;
    let samples_per_packet = 1_024_usize
        .checked_mul(usize::from(CHANNELS))
        .ok_or_else(|| "YouTube audio frame size overflows".to_owned())?;
    let mut packets = Vec::with_capacity(packet_count);
    for packet_index in 0..packet_count {
        let pts = i64::try_from(
            packet_index
                .checked_mul(1_024)
                .ok_or_else(|| "YouTube audio timestamp overflows".to_owned())?,
        )
        .map_err(|_| "YouTube audio timestamp exceeds i64".to_owned())?;
        encoder
            .send_frame(AudioFrame {
                format: AudioSampleFormat::I16Interleaved,
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
                samples_per_channel: 1_024,
                samples: vec![0; samples_per_packet],
                timing: FrameTiming {
                    pts: Some(Timestamp {
                        value: pts,
                        time_base,
                    }),
                    duration: Some(Timestamp {
                        value: 1_024,
                        time_base,
                    }),
                },
            })
            .map_err(|error| error.to_string())?;
        let mut packet = encoder
            .receive_packet()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "AAC-LC encoder omitted a silent access unit".to_owned())?;
        if packet_index + 1 == packet_count {
            let packet_start = packet_index
                .checked_mul(1_024)
                .ok_or_else(|| "YouTube audio timestamp overflows".to_owned())?;
            let final_duration = audio_samples
                .checked_sub(packet_start)
                .ok_or_else(|| "YouTube audio duration underflows".to_owned())?;
            packet.duration = Some(Timestamp {
                value: i64::try_from(final_duration)
                    .map_err(|_| "YouTube audio duration exceeds i64".to_owned())?,
                time_base,
            });
        }
        packets.push(packet);
    }
    Ok((
        mmrecode_isobmff::Track {
            descriptor: StreamDescriptor {
                id: StreamId(1),
                codec,
                time_base,
            },
            track_id: 2,
            handler_type: FourCc(*b"soun"),
            width: None,
            height: None,
            pixel_aspect: None,
            colour: None,
            rotation_degrees: 0,
            channel_count: Some(CHANNELS),
            sample_rate: Some(SAMPLE_RATE),
            presentation_duration: Some(
                u64::try_from(audio_samples)
                    .map_err(|_| "YouTube audio duration exceeds u64".to_owned())?,
            ),
            samples: Vec::new(),
        },
        packets,
    ))
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
        placement
            .elementary
            .as_deref()
            .ok_or_else(|| "MPEG-2 placement lost its elementary stream".to_owned())?,
        stream,
        dependencies,
        &mut state.decoder,
        &mut state.decode_cursor,
        &mut state.decoded_by_presentation,
        source_index,
        decoded_pictures,
    )
}

fn timeline_source_frame(
    placement: &TimelinePlacement,
    stream: Option<&mmrecode_mpeg2::Mpeg2Stream<'_>>,
    dependencies: Option<&[mmrecode_core::AccessUnitInfo]>,
    state: &mut TimelineDecodeState,
    source_index: i64,
    decoded_pictures: &mut usize,
) -> Result<VideoFrame, String> {
    if let Some((last_index, frame)) = &state.last_source
        && *last_index == source_index
    {
        return Ok(frame.clone());
    }
    let frame = if let Some(frames) = &placement.h264_frames {
        let index = usize::try_from(source_index)
            .map_err(|_| "H.264 source frame index is negative".to_owned())?;
        let frame = frames.get(index).cloned().ok_or_else(|| {
            format!(
                "H.264 source frame {source_index} is outside 0..{}",
                frames.len()
            )
        })?;
        *decoded_pictures = decoded_pictures.saturating_add(1);
        frame
    } else {
        decode_timeline_source_frame(
            placement,
            stream.ok_or_else(|| "MPEG-2 placement lost its parsed stream".to_owned())?,
            dependencies.ok_or_else(|| "MPEG-2 placement lost its dependency index".to_owned())?,
            state,
            source_index,
            decoded_pictures,
        )?
    };
    state.last_source = Some((source_index, frame.clone()));
    Ok(frame)
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

fn infer_preset(
    output: Option<&Path>,
    settings: &mmrecode_edit::ProjectSettings,
) -> Option<String> {
    let extension = output?.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("ts") {
        Some("mpeg2-ts".into())
    } else if extension.eq_ignore_ascii_case("mp4") {
        Some(if (settings.width, settings.height) == (3_840, 2_160) {
            "youtube-2160p".into()
        } else {
            "youtube-1080p".into()
        })
    } else {
        None
    }
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
    elementary_video_bytes(&bytes)
}

fn elementary_video_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE && bytes[0] == 0x47 {
        return mmrecode_mpegts::demux_transport_stream(bytes)
            .map_err(|error| error.to_string())?
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string());
    }
    Ok(bytes.to_vec())
}

fn decode_h264_video(bytes: &[u8]) -> Result<Vec<VideoFrame>, String> {
    let movie =
        mmrecode_isobmff::IsoBmffFile::parse(bytes.to_vec()).map_err(|error| error.to_string())?;
    let track = movie
        .h264_track()
        .ok_or_else(|| "ISO-BMFF source has no H.264 video track".to_owned())?;
    let mut decoder = mmrecode_h264::H264Decoder::default();
    decoder
        .configure(&track.descriptor.codec)
        .map_err(|error| error.to_string())?;
    let mut frames = Vec::with_capacity(track.samples.len());
    for sample in &track.samples {
        let timestamp = |value| Timestamp {
            value,
            time_base: track.descriptor.time_base,
        };
        decoder
            .send_packet(Packet {
                stream_id: track.descriptor.id,
                data: movie
                    .sample_data(sample)
                    .map_err(|error| error.to_string())?
                    .to_vec(),
                pts: Some(timestamp(sample.pts)),
                dts: Some(timestamp(sample.dts)),
                duration: Some(timestamp(i64::from(sample.duration))),
                flags: if sample.is_sync {
                    PacketFlags::KEY
                } else {
                    PacketFlags::empty()
                },
                side_data: Vec::new(),
            })
            .map_err(|error| error.to_string())?;
        while let Some(frame) = decoder.receive_frame().map_err(|error| error.to_string())? {
            frames.push(rotate_video_frame(frame, track.rotation_degrees)?);
        }
    }
    decoder.flush().map_err(|error| error.to_string())?;
    while let Some(frame) = decoder.receive_frame().map_err(|error| error.to_string())? {
        frames.push(rotate_video_frame(frame, track.rotation_degrees)?);
    }
    frames.sort_by_key(|frame| {
        frame.timing.pts.map_or((i64::MAX, i64::MAX), |pts| {
            (pts.value, pts.time_base.denominator())
        })
    });
    if frames.len() != track.samples.len() {
        return Err(format!(
            "native H.264 decoder returned {} frame(s) for {} MP4 sample(s)",
            frames.len(),
            track.samples.len()
        ));
    }
    Ok(frames)
}

fn rotate_video_frame(mut frame: VideoFrame, degrees: i16) -> Result<VideoFrame, String> {
    let degrees = degrees.rem_euclid(360);
    if degrees == 0 {
        return Ok(frame);
    }
    if !matches!(degrees, 90 | 180 | 270) {
        return Err(format!("H.264 display rotation {degrees} is unsupported"));
    }
    for plane in &mut frame.planes {
        let (width, height) = if matches!(degrees, 90 | 270) {
            (plane.height, plane.width)
        } else {
            (plane.width, plane.height)
        };
        let mut data = vec![0; width.saturating_mul(height)];
        for source_y in 0..plane.height {
            for source_x in 0..plane.width {
                let source = source_y
                    .checked_mul(plane.stride)
                    .and_then(|offset| offset.checked_add(source_x))
                    .filter(|offset| *offset < plane.data.len())
                    .ok_or_else(|| "native H.264 plane layout is invalid".to_owned())?;
                let (target_x, target_y) = match degrees {
                    90 => (plane.height - 1 - source_y, source_x),
                    180 => (plane.width - 1 - source_x, plane.height - 1 - source_y),
                    270 => (source_y, plane.width - 1 - source_x),
                    _ => unreachable!("validated rotation"),
                };
                data[target_y * width + target_x] = plane.data[source];
            }
        }
        plane.data = data;
        plane.stride = width;
        plane.width = width;
        plane.height = height;
    }
    if matches!(degrees, 90 | 270) {
        (frame.width, frame.height) = (frame.height, frame.width);
    }
    Ok(frame)
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

#[cfg(test)]
mod tests {
    use mmrecode_core::{AudioDecoder, Decoder, Demuxer};
    use mmrecode_edit::{EditorSession, MediaKind, MediaProject, MmfxSource, ProjectSettings};

    use super::*;

    fn fx_only_session(source: &str) -> EditorSession {
        let settings = ProjectSettings {
            width: 16,
            height: 16,
            ..ProjectSettings::default()
        };
        let mut project = MediaProject::with_settings("FX export", settings).unwrap();
        let link_id = project
            .add_generated(
                project.root_id(),
                MediaKind::new("fx").unwrap(),
                "Card",
                0,
                2,
            )
            .unwrap();
        let media_id = project.link(link_id).unwrap().media_id;
        project
            .set_mmfx_source(
                media_id,
                MmfxSource {
                    source: source.into(),
                    resource_base: None,
                    linked_path: None,
                    parameter_bindings: BTreeMap::default(),
                },
            )
            .unwrap();
        EditorSession::new(project)
    }

    fn nested_fx_session(source: &str) -> (EditorSession, PathBuf) {
        let settings = ProjectSettings {
            width: 16,
            height: 16,
            ..ProjectSettings::default()
        };
        let time_base = settings.time_base().unwrap();
        let frames = vec![black_project_frame(16, 16), black_project_frame(16, 16)];
        let encoded = mmrecode_mpeg2::encode_stream(
            &frames,
            Mpeg2EncodeOptions {
                frame_rate: mpeg_frame_rate(time_base).unwrap(),
                gop_size: 2,
                b_frames: 0,
                ..Mpeg2EncodeOptions::default()
            },
        )
        .unwrap();
        let source_path = std::env::temp_dir().join(format!(
            "mmrecode-nested-fx-source-{}.m2v",
            std::process::id()
        ));
        std::fs::write(&source_path, encoded.data).unwrap();

        let mut project = MediaProject::with_settings("Nested FX export", settings).unwrap();
        let video = project
            .create_media(
                "clip",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                2,
                MediaOrigin::External {
                    path: source_path.clone(),
                },
            )
            .unwrap();
        project
            .link_media(
                project.root_id(),
                video,
                "Clip",
                time_range(0, 2, time_base).unwrap(),
                time_range(0, 2, time_base).unwrap(),
            )
            .unwrap();
        let nested_video = project
            .create_media(
                "nested clip",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                2,
                MediaOrigin::External {
                    path: source_path.clone(),
                },
            )
            .unwrap();
        project
            .link_media(
                video,
                nested_video,
                "Nested",
                time_range(0, 1, time_base).unwrap(),
                time_range(1, 2, time_base).unwrap(),
            )
            .unwrap();
        let fx_link = project
            .add_generated(nested_video, MediaKind::new("fx").unwrap(), "Card", 0, 1)
            .unwrap();
        let fx = project.link(fx_link).unwrap().media_id;
        project
            .set_mmfx_source(
                fx,
                MmfxSource {
                    source: source.into(),
                    resource_base: None,
                    linked_path: None,
                    parameter_bindings: BTreeMap::default(),
                },
            )
            .unwrap();
        (EditorSession::new(project), source_path)
    }

    #[test]
    fn export_plan_accepts_an_fx_only_root_timeline() {
        let session =
            fx_only_session("@scene card { width: 16px; height: 16px; background: #336699; }");
        let report = export_project(&session, None, None).unwrap();
        assert!(report.contains("full project-timeline render"));
        assert!(report.contains("1 cached MMFX asset(s)"));
    }

    #[test]
    fn fx_only_timeline_writes_a_decodable_transport_stream() {
        let session =
            fx_only_session("@scene card { width: 16px; height: 16px; background: #336699; }");
        let output =
            std::env::temp_dir().join(format!("mmrecode-fx-only-export-{}.ts", std::process::id()));
        let report = export_project(&session, Some(&output), None).unwrap();
        let bytes = std::fs::read(&output).unwrap();
        let _ = std::fs::remove_file(&output);
        assert!(report.contains("Wrote"));
        assert_eq!(bytes.first(), Some(&0x47));
        let demuxed = mmrecode_mpegts::demux_transport_stream(&bytes).unwrap();
        let video = demuxed.mpeg2_video_bytes().unwrap();
        assert_eq!(
            mmrecode_mpeg2::parse_stream(&video)
                .unwrap()
                .pictures()
                .len(),
            2
        );
    }

    #[test]
    fn nested_fx_forces_recursive_full_render_and_is_reported_by_path() {
        let (session, source_path) =
            nested_fx_session("@scene card { width: 16px; height: 16px; background: #336699; }");
        let output_path = std::env::temp_dir().join(format!(
            "mmrecode-nested-fx-export-{}.ts",
            std::process::id()
        ));
        let report = export_project(&session, Some(&output_path), None).unwrap();
        let bytes = std::fs::read(&output_path).unwrap();
        let _ = std::fs::remove_file(source_path);
        let _ = std::fs::remove_file(output_path);
        assert!(report.contains("full project-timeline render"));
        assert!(report.contains("3 object(s)"));
        assert!(report.contains("1 cached MMFX asset(s)"));
        assert!(report.contains("/Clip/Nested"));
        assert!(report.contains("/Clip/Nested/Card"));
        let demuxed = mmrecode_mpegts::demux_transport_stream(&bytes).unwrap();
        let video = demuxed.mpeg2_video_bytes().unwrap();
        let stream = mmrecode_mpeg2::parse_stream(&video).unwrap();
        assert_eq!(stream.pictures().len(), 2);
        let dependencies = mmrecode_mpeg2::analyze_dependencies(&stream).unwrap();
        let mut decoder = Mpeg2PictureDecoder::default();
        let mut decode_cursor = 0;
        let mut presentation_frames = BTreeMap::new();
        let mut decoded_pictures = 0;
        let first = decode_source_frame(
            &video,
            &stream,
            &dependencies,
            &mut decoder,
            &mut decode_cursor,
            &mut presentation_frames,
            0,
            &mut decoded_pictures,
        );
        let second = decode_source_frame(
            &video,
            &stream,
            &dependencies,
            &mut decoder,
            &mut decode_cursor,
            &mut presentation_frames,
            1,
            &mut decoded_pictures,
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(
            second.planes[0]
                .data
                .iter()
                .map(|value| u64::from(*value))
                .sum::<u64>()
                > first.planes[0]
                    .data
                    .iter()
                    .map(|value| u64::from(*value))
                    .sum::<u64>()
        );
    }

    #[test]
    fn export_plan_rejects_invalid_mmfx_instead_of_using_stale_pixels() {
        let session = fx_only_session("not an MMFX scene");
        let error = export_project(&session, None, None).unwrap_err();
        assert!(error.contains("MMFX"));
        assert!(error.contains("1:1"));
    }

    #[test]
    fn youtube_delivery_writes_fast_start_high_cabac_bt709_mp4() {
        let session =
            fx_only_session("@scene card { width: 16px; height: 16px; background: #336699; }");
        let mut compositor = mmrecode_render::ProjectCompositor::new();
        let sync = compositor.synchronize(
            session.project(),
            session.project().root_id(),
            |_, _, scene| crate::load_mmfx_resources(scene, Path::new(".")),
        );
        assert!(sync.diagnostics.is_empty());
        let project_time_base = session.project().settings().time_base().unwrap();
        let profile = YoutubeProfile {
            name: "youtube-test",
            width: 16,
            height: 16,
            standard_bitrate: 100_000,
            high_frame_rate_bitrate: 150_000,
        };
        let (data, decoded_source_pictures) = render_timeline_and_encode_youtube(
            &[],
            &[],
            &[],
            project_time_base,
            2,
            16,
            16,
            profile,
            &mut compositor,
            session.project(),
        )
        .unwrap();
        assert_eq!(decoded_source_pictures, 0);
        let decoded_h264 = decode_h264_video(&data).unwrap();
        assert_eq!(decoded_h264.len(), 2);
        assert_eq!(&data[4..8], b"ftyp");
        let ftyp_size = usize::try_from(u32::from_be_bytes(data[..4].try_into().unwrap())).unwrap();
        assert_eq!(&data[ftyp_size + 4..ftyp_size + 8], b"moov");
        let mut file = mmrecode_isobmff::IsoBmffFile::parse(data).unwrap();
        assert_eq!(file.tracks().len(), 2);
        let track = file.h264_track().unwrap();
        assert_eq!(track.samples.len(), 2);
        assert_eq!(
            track.colour,
            Some(mmrecode_isobmff::ColourInformation {
                primaries: 1,
                transfer_characteristics: 1,
                matrix_coefficients: 1,
                full_range: Some(false),
            })
        );
        let avcc = mmrecode_h264::AvcDecoderConfigurationRecord::parse(
            &track.descriptor.codec.configuration,
        )
        .unwrap();
        let sps = mmrecode_h264::parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
        let pps = mmrecode_h264::parse_pps(&avcc.picture_parameter_sets[0]).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert!(pps.entropy_coding_mode);
        assert_eq!(sps.vui.unwrap().colour_primaries, Some(1));

        let video_stream_id = track.descriptor.id;
        let video_descriptor = track.descriptor.codec.clone();
        let audio_track = file
            .tracks()
            .iter()
            .find(|track| track.descriptor.codec.media_type == MediaType::Audio)
            .unwrap();
        assert_eq!(audio_track.channel_count, Some(2));
        assert_eq!(audio_track.sample_rate, Some(48_000));
        assert_eq!(audio_track.samples.len(), 4);
        assert_eq!(audio_track.samples[0].pts, 0);
        assert_eq!(audio_track.presentation_duration, None);
        assert_eq!(
            audio_track
                .samples
                .last()
                .map(|sample| sample.pts + i64::from(sample.duration)),
            Some(3_200)
        );
        let audio_stream_id = audio_track.descriptor.id;
        let audio_descriptor = audio_track.descriptor.codec.clone();
        let mut decoder = mmrecode_h264::H264Decoder::default();
        decoder.configure(&video_descriptor).unwrap();
        let mut audio_decoder = mmrecode_aac::AacLcDecoder::default();
        audio_decoder.configure(&audio_descriptor).unwrap();
        let mut decoded_frames = 0;
        let mut decoded_audio_frames = 0;
        while let Some(packet) = file.read_packet().unwrap() {
            if packet.stream_id == video_stream_id {
                decoder.send_packet(packet).unwrap();
                while decoder.receive_frame().unwrap().is_some() {
                    decoded_frames += 1;
                }
            } else if packet.stream_id == audio_stream_id {
                audio_decoder.send_packet(packet).unwrap();
                while let Some(frame) = audio_decoder.receive_frame().unwrap() {
                    assert!(frame.samples.iter().all(|sample| *sample == 0));
                    decoded_audio_frames += 1;
                }
            }
        }
        decoder.flush().unwrap();
        while decoder.receive_frame().unwrap().is_some() {
            decoded_frames += 1;
        }
        assert_eq!(decoded_frames, 2);
        audio_decoder.flush().unwrap();
        assert_eq!(decoded_audio_frames, 4);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn reexports_h264_mp4_aac_program_audio_through_the_timeline_mix() {
        let session =
            fx_only_session("@scene card { width: 16px; height: 16px; background: #336699; }");
        let mut compositor = mmrecode_render::ProjectCompositor::new();
        let sync = compositor.synchronize(
            session.project(),
            session.project().root_id(),
            |_, _, scene| crate::load_mmfx_resources(scene, Path::new(".")),
        );
        assert!(sync.diagnostics.is_empty());
        let time_base = session.project().settings().time_base().unwrap();
        let profile = YoutubeProfile {
            name: "youtube-test",
            width: 16,
            height: 16,
            standard_bitrate: 100_000,
            high_frame_rate_bitrate: 150_000,
        };
        let (silent_movie, _) = render_timeline_and_encode_youtube(
            &[],
            &[],
            &[],
            time_base,
            2,
            16,
            16,
            profile,
            &mut compositor,
            session.project(),
        )
        .unwrap();
        let mut parsed = mmrecode_isobmff::IsoBmffFile::parse(silent_movie).unwrap();
        let mut video_track = parsed.h264_track().unwrap().clone();
        let video_stream_id = video_track.descriptor.id;
        let mut video_packets = Vec::new();
        while let Some(packet) = parsed.read_packet().unwrap() {
            if packet.stream_id == video_stream_id {
                video_packets.push(packet);
            }
        }
        video_track.descriptor.id = StreamId(0);
        for packet in &mut video_packets {
            packet.stream_id = StreamId(0);
        }
        let samples = (0..3_200)
            .flat_map(|sample| {
                let value = (7_000.0
                    * (2.0 * std::f64::consts::PI * 440.0 * sample as f64 / 48_000.0).sin())
                .round() as i16;
                [value, value]
            })
            .collect::<Vec<_>>();
        let program = encode_youtube_program_audio(&AudioFrame {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate: 48_000,
            channels: 2,
            samples_per_channel: 3_200,
            samples,
            timing: FrameTiming::default(),
        })
        .unwrap();
        let source_movie = mmrecode_isobmff::mux_tracks(&[
            mmrecode_isobmff::TrackMuxInput {
                track: &video_track,
                packets: &video_packets,
                edit: None,
            },
            mmrecode_isobmff::TrackMuxInput {
                track: &program.track,
                packets: &program.packets,
                edit: program.edit,
            },
        ])
        .unwrap();
        let source_path = std::env::temp_dir().join(format!(
            "mmrecode-h264-aac-program-source-{}.mp4",
            std::process::id()
        ));
        let output_path = std::env::temp_dir().join(format!(
            "mmrecode-h264-aac-program-output-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&source_path, source_movie).unwrap();

        let settings = ProjectSettings {
            width: 16,
            height: 16,
            ..ProjectSettings::default()
        };
        let mut project = MediaProject::with_settings("H.264 AAC export", settings).unwrap();
        let media = project
            .create_media(
                "H.264 clip",
                MediaKind::new("video/h264").unwrap(),
                time_base,
                2,
                MediaOrigin::External {
                    path: source_path.clone(),
                },
            )
            .unwrap();
        project
            .link_media(
                project.root_id(),
                media,
                "Clip",
                time_range(1, 2, time_base).unwrap(),
                time_range(0, 1, time_base).unwrap(),
            )
            .unwrap();
        let session = EditorSession::new(project);
        let flattened = mmrecode_render::flatten_project_timeline(
            session.project(),
            session.project().root_id(),
        )
        .unwrap();
        let report = export_root_timeline(
            &session,
            &flattened,
            Some(&output_path),
            TimelineDelivery::Youtube(profile),
        )
        .unwrap();
        let output = std::fs::read(&output_path).unwrap();
        let decoded = mmrecode_playback::decode_audio_source(&output)
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_file(source_path);
        let _ = std::fs::remove_file(output_path);
        assert!(report.contains("Decoded 1 required source picture(s)"));
        assert_eq!(decoded.samples_per_channel, 1_600);
        assert!(
            decoded
                .samples
                .iter()
                .any(|sample| sample.unsigned_abs() > 100)
        );
    }

    #[test]
    fn reexports_mpegts_layer2_program_audio_through_the_timeline_mix() {
        let source_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/mpegts/valid/single-program-mpeg2-mp2.ts");
        let settings = ProjectSettings {
            width: 96,
            height: 64,
            frame_rate: Rational::new(25, 1).unwrap(),
            ..ProjectSettings::default()
        };
        let time_base = settings.time_base().unwrap();
        let mut project = MediaProject::with_settings("MPEG-TS audio export", settings).unwrap();
        let media = project
            .create_media(
                "MPEG-TS clip",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                2,
                MediaOrigin::External { path: source_path },
            )
            .unwrap();
        project
            .link_media(
                project.root_id(),
                media,
                "Clip",
                time_range(0, 2, time_base).unwrap(),
                time_range(0, 2, time_base).unwrap(),
            )
            .unwrap();
        let session = EditorSession::new(project);
        let flattened = mmrecode_render::flatten_project_timeline(
            session.project(),
            session.project().root_id(),
        )
        .unwrap();
        let output_path = std::env::temp_dir().join(format!(
            "mmrecode-mpegts-layer2-program-output-{}.mp4",
            std::process::id()
        ));
        let report = export_root_timeline(
            &session,
            &flattened,
            Some(&output_path),
            TimelineDelivery::Youtube(YoutubeProfile {
                name: "youtube-test",
                width: 96,
                height: 64,
                standard_bitrate: 300_000,
                high_frame_rate_bitrate: 450_000,
            }),
        )
        .unwrap();
        let output = std::fs::read(&output_path).unwrap();
        let decoded = mmrecode_playback::decode_audio_source(&output)
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_file(output_path);
        assert!(report.contains("required source picture(s)"));
        assert_eq!(decoded.samples_per_channel, 3_840);
        assert!(
            decoded
                .samples
                .iter()
                .any(|sample| sample.unsigned_abs() > 100)
        );
    }

    #[test]
    fn youtube_export_plan_resolves_profile_and_upload_parameters() {
        let settings = ProjectSettings::from_preset("youtube-1080p30").unwrap();
        let mut project = MediaProject::with_settings("YouTube export", settings).unwrap();
        let link_id = project
            .add_generated(
                project.root_id(),
                MediaKind::new("fx").unwrap(),
                "Card",
                0,
                1,
            )
            .unwrap();
        let media_id = project.link(link_id).unwrap().media_id;
        project
            .set_mmfx_source(
                media_id,
                MmfxSource {
                    source: "@scene card { width: 1920px; height: 1080px; background: #336699; }"
                        .into(),
                    resource_base: None,
                    linked_path: None,
                    parameter_bindings: BTreeMap::default(),
                },
            )
            .unwrap();
        let session = EditorSession::new(project);

        let report = export_project(&session, None, Some("youtube-1080p")).unwrap();
        assert!(report.contains("Export preset: youtube-1080p"));
        assert!(report.contains("Fast Start MP4"));
        assert!(report.contains("H.264 High/CABAC"));
        assert!(report.contains("2 B-frames"));
        assert!(report.contains("closed 15-frame GOP"));
        assert!(report.contains("8 Mbps VBR"));
        assert!(report.contains("BT.709"));
        assert!(report.contains("native AAC-LC 48 kHz stereo/384 kbps timeline mix"));
        assert!(report.contains("MP4/MOV AAC and MPEG-TS Layer II"));
    }

    #[test]
    fn output_extension_selects_the_matching_delivery_preset() {
        let hd = ProjectSettings::from_preset("youtube-1080p30").unwrap();
        let uhd = ProjectSettings::from_preset("youtube-2160p30").unwrap();
        assert_eq!(
            infer_preset(Some(Path::new("delivery.mp4")), &hd).as_deref(),
            Some("youtube-1080p")
        );
        assert_eq!(
            infer_preset(Some(Path::new("delivery.MP4")), &uhd).as_deref(),
            Some("youtube-2160p")
        );
        assert_eq!(
            infer_preset(Some(Path::new("delivery.ts")), &hd).as_deref(),
            Some("mpeg2-ts")
        );
    }

    #[test]
    fn youtube_rate_control_uses_frame_rate_tier_and_half_second_gop() {
        let fps_30 = Rational::new(1, 30).unwrap();
        let fps_59_94 = Rational::new(1_001, 60_000).unwrap();
        assert_eq!(
            youtube_bitrate(YoutubeProfile::HD, fps_30).unwrap(),
            8_000_000
        );
        assert_eq!(
            youtube_bitrate(YoutubeProfile::HD, fps_59_94).unwrap(),
            12_000_000
        );
        assert_eq!(youtube_gop_size(fps_30).unwrap(), 15);
        assert_eq!(youtube_gop_size(fps_59_94).unwrap(), 30);
    }
}
