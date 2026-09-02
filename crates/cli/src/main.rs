//! `MMRecode` command-line entry point.

fn main() {
    if let Err(error) = run() {
        eprintln!("mmrecode: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let command = arguments.next();
    match command.as_deref().and_then(std::ffi::OsStr::to_str) {
        None | Some("help" | "--help" | "-h") => {
            print_help();
            Ok(())
        }
        Some("version" | "--version" | "-V") => {
            println!("mmrecode {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("inspect") => {
            let path = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode inspect <media-file>".to_owned())?;
            if arguments.next().is_some() {
                return Err("usage: mmrecode inspect <media-file>".to_owned());
            }
            inspect(std::path::Path::new(&path))
        }
        Some("extract-dv-audio") => extract_dv_audio_command(&mut arguments),
        Some("encode-dv") => encode_dv_command(&mut arguments),
        Some("encode-mpeg2") => encode_mpeg2_command(&mut arguments),
        Some("mux-mpegts") => mux_mpegts_command(&mut arguments),
        Some("demux-mpegts") => demux_mpegts_command(&mut arguments),
        Some("extract-mpegts-audio") => extract_mpegts_audio_command(&mut arguments),
        Some("edit") => editor_command(&mut arguments),
        Some("plan-mpeg2") => plan_mpeg2_command(&mut arguments),
        Some("render-plan") => render_plan_command(&mut arguments),
        Some("render") => render_command(&mut arguments),
        Some("decode") => {
            let input = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode decode <media-file> <output.y4m>".to_owned())?;
            let output = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode decode <media-file> <output.y4m>".to_owned())?;
            if arguments.next().is_some() {
                return Err("usage: mmrecode decode <media-file> <output.y4m>".to_owned());
            }
            decode(std::path::Path::new(&input), std::path::Path::new(&output))
        }
        Some("encode") => {
            let input = arguments.next().ok_or_else(|| {
                "usage: mmrecode encode <input.y4m> <output.mjpg> [quality]".to_owned()
            })?;
            let output = arguments.next().ok_or_else(|| {
                "usage: mmrecode encode <input.y4m> <output.mjpg> [quality]".to_owned()
            })?;
            let quality = arguments
                .next()
                .map(|value| {
                    value
                        .to_str()
                        .ok_or_else(|| "quality must be valid UTF-8".to_owned())?
                        .parse::<u8>()
                        .map_err(|_| "quality must be an integer from 1 through 100".to_owned())
                })
                .transpose()?
                .unwrap_or(75);
            if arguments.next().is_some() {
                return Err("usage: mmrecode encode <input.y4m> <output.mjpg> [quality]".to_owned());
            }
            encode_y4m(
                std::path::Path::new(&input),
                std::path::Path::new(&output),
                quality,
            )
        }
        Some("verify") => {
            let input = arguments
                .next()
                .ok_or_else(|| "usage: mmrecode verify <media-file> [reference.y4m]".to_owned())?;
            let reference = arguments.next();
            if arguments.next().is_some() {
                return Err("usage: mmrecode verify <media-file> [reference.y4m]".to_owned());
            }
            verify(
                std::path::Path::new(&input),
                reference.as_deref().map(std::path::Path::new),
            )
        }
        Some("compare") => {
            let reference = arguments.next().ok_or_else(|| {
                "usage: mmrecode compare <reference.y4m> <candidate.y4m>".to_owned()
            })?;
            let candidate = arguments.next().ok_or_else(|| {
                "usage: mmrecode compare <reference.y4m> <candidate.y4m>".to_owned()
            })?;
            if arguments.next().is_some() {
                return Err("usage: mmrecode compare <reference.y4m> <candidate.y4m>".to_owned());
            }
            compare_y4m(
                std::path::Path::new(&reference),
                std::path::Path::new(&candidate),
            )
        }
        Some(other) => Err(format!(
            "command '{other}' is not implemented; run 'mmrecode help' for available commands"
        )),
    }
}

fn editor_command(arguments: &mut impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let script = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: mmrecode edit [command-script]".into());
    }
    let project_name = script
        .as_deref()
        .map(std::path::Path::new)
        .and_then(std::path::Path::file_stem)
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Untitled");
    let time_base = mmrecode_core::Rational::new(1, 25).map_err(|error| error.to_string())?;
    let project = mmrecode_edit::MediaProject::new(project_name, time_base)
        .map_err(|error| error.to_string())?;
    let mut session = mmrecode_edit::EditorSession::new(project);
    if let Some(script) = script {
        run_editor_script(std::path::Path::new(&script), &mut session)
    } else {
        run_editor_interactive(&mut session)
    }
}

fn run_editor_script(
    path: &std::path::Path,
    session: &mut mmrecode_edit::EditorSession,
) -> Result<(), String> {
    use std::io::BufRead as _;

    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open editor script '{}': {error}", path.display()))?;
    for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| {
            format!(
                "cannot read editor script '{}' line {}: {error}",
                path.display(),
                index + 1
            )
        })?;
        if execute_editor_line(session, &line)
            .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))?
        {
            break;
        }
    }
    Ok(())
}

fn run_editor_interactive(session: &mut mmrecode_edit::EditorSession) -> Result<(), String> {
    use std::io::{BufRead as _, Write as _};

    println!("MMRecode linked-media editor prototype. Type 'help' for commands.");
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        let prompt = session.prompt().map_err(|error| error.to_string())?;
        print!("{prompt} > ");
        std::io::stdout()
            .flush()
            .map_err(|error| format!("cannot flush editor prompt: {error}"))?;
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .map_err(|error| format!("cannot read editor command: {error}"))?
            == 0
        {
            break;
        }
        match execute_editor_line(session, &line) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => eprintln!("mmrecode edit: {error}"),
        }
    }
    Ok(())
}

fn execute_editor_line(
    session: &mut mmrecode_edit::EditorSession,
    line: &str,
) -> Result<bool, String> {
    let Some(command) = mmrecode_edit::parse_command(line).map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    let output = session.apply(command).map_err(|error| error.to_string())?;
    if output == mmrecode_edit::CommandOutput::Quit {
        return Ok(true);
    }
    print_editor_output(output);
    Ok(false)
}

fn print_editor_output(output: mmrecode_edit::CommandOutput) {
    match output {
        mmrecode_edit::CommandOutput::Text(text) => println!("{text}"),
        mmrecode_edit::CommandOutput::Listing(entries) => {
            if entries.is_empty() {
                println!("(empty local timeline)");
            }
            for entry in entries {
                println!(
                    "{:<16} [{:<10}] |{}f-----{}f|",
                    entry.alias,
                    entry.kind.as_str(),
                    entry.timeline_range.start.value,
                    entry.timeline_range.end.value
                );
            }
        }
        mmrecode_edit::CommandOutput::Changed { description, path } => {
            println!("ok: {description}  [{path}]");
        }
        _ => {}
    }
}

fn encode_mpeg2_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = "usage: mmrecode encode-mpeg2 <input.y4m> <output.m2v> [quantiser-scale-code]";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    let quantiser_scale_code = arguments
        .next()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| "quantiser scale must be valid UTF-8".to_owned())?
                .parse::<u8>()
                .map_err(|_| "quantiser scale must be an integer from 1 through 31".to_owned())
        })
        .transpose()?
        .unwrap_or(8);
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    encode_mpeg2(
        std::path::Path::new(&input),
        std::path::Path::new(&output),
        quantiser_scale_code,
    )
}

#[allow(clippy::too_many_lines)]
fn mux_mpegts_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    use mmrecode_core::Muxer as _;

    let usage = "usage: mmrecode mux-mpegts <input.m2v> <output.ts> [input.mp2]";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    let audio = arguments.next();
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    let input = std::path::Path::new(&input);
    let output = std::path::Path::new(&output);
    let elementary = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let stream = mmrecode_mpeg2::parse_stream(&elementary).map_err(|error| error.to_string())?;
    let dependencies =
        mmrecode_mpeg2::analyze_dependencies(&stream).map_err(|error| error.to_string())?;
    let frame_rate = stream.pictures()[0].sequence.frame_rate;
    let frame_time = mmrecode_core::Rational::new(frame_rate.denominator(), frame_rate.numerator())
        .map_err(|error| error.to_string())?;
    let mut muxer = mmrecode_mpegts::MpegTsMuxer::new();
    let video_stream_id = muxer
        .add_stream(mmrecode_core::StreamDescriptor {
            id: mmrecode_core::StreamId(0),
            codec: mmrecode_core::CodecDescriptor {
                codec_id: mmrecode_core::CodecId::new("video/mpeg2"),
                codec_tag: None,
                media_type: mmrecode_core::MediaType::Video,
                configuration: Vec::new(),
            },
            time_base: mmrecode_core::Rational::new(1, 90_000)
                .map_err(|error| error.to_string())?,
        })
        .map_err(|error| error.to_string())?;
    let audio_data = audio
        .as_deref()
        .map(|path| {
            let path = std::path::Path::new(path);
            std::fs::read(path)
                .map_err(|error| format!("cannot read '{}': {error}", path.display()))
        })
        .transpose()?;
    let audio_frames = audio_data
        .as_deref()
        .map(mmrecode_mpegaudio::parse_layer2_stream)
        .transpose()
        .map_err(|error| error.to_string())?;
    let audio_stream_id = if audio_frames.is_some() {
        Some(
            muxer
                .add_stream(mmrecode_core::StreamDescriptor {
                    id: mmrecode_core::StreamId(0),
                    codec: mmrecode_core::CodecDescriptor {
                        codec_id: mmrecode_core::CodecId::new("audio/mpeg1"),
                        codec_tag: None,
                        media_type: mmrecode_core::MediaType::Audio,
                        configuration: Vec::new(),
                    },
                    time_base: mmrecode_core::Rational::new(1, 90_000)
                        .map_err(|error| error.to_string())?,
                })
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let mut packets = Vec::new();
    for (index, (picture, dependency)) in stream.pictures().iter().zip(&dependencies).enumerate() {
        let start = if index == 0 {
            0
        } else {
            stream.pictures()[index - 1].source_range.end
        };
        let end = if index + 1 == stream.pictures().len() {
            elementary.len()
        } else {
            picture.source_range.end
        };
        let mut flags = mmrecode_core::PacketFlags::empty();
        if dependency.random_access == mmrecode_core::RandomAccessKind::Clean {
            flags.insert(mmrecode_core::PacketFlags::KEY);
        }
        packets.push((
            transport_clock_value(dependency.decode_order, frame_time)?,
            mmrecode_core::Packet {
                stream_id: video_stream_id,
                data: elementary[start..end].to_vec(),
                pts: Some(mmrecode_core::Timestamp {
                    value: dependency.presentation_order,
                    time_base: frame_time,
                }),
                dts: Some(mmrecode_core::Timestamp {
                    value: dependency.decode_order,
                    time_base: frame_time,
                }),
                duration: Some(mmrecode_core::Timestamp {
                    value: 1,
                    time_base: frame_time,
                }),
                flags,
                side_data: Vec::new(),
            },
        ));
    }
    if let (Some(audio_data), Some(audio_frames), Some(audio_stream_id)) = (
        audio_data.as_deref(),
        audio_frames.as_ref(),
        audio_stream_id,
    ) {
        let sample_rate = i64::from(audio_frames[0].header.sample_rate);
        let audio_time =
            mmrecode_core::Rational::new(1, sample_rate).map_err(|error| error.to_string())?;
        for frame in audio_frames {
            let sample = i64::try_from(frame.index)
                .map_err(|_| "MPEG audio frame index exceeds i64".to_owned())?
                .checked_mul(i64::from(frame.header.samples_per_frame))
                .ok_or_else(|| "MPEG audio timestamp overflows".to_owned())?;
            packets.push((
                transport_clock_value(sample, audio_time)?,
                mmrecode_core::Packet {
                    stream_id: audio_stream_id,
                    data: frame.data(audio_data).to_vec(),
                    pts: Some(mmrecode_core::Timestamp {
                        value: sample,
                        time_base: audio_time,
                    }),
                    dts: None,
                    duration: Some(mmrecode_core::Timestamp {
                        value: i64::from(frame.header.samples_per_frame),
                        time_base: audio_time,
                    }),
                    flags: mmrecode_core::PacketFlags::empty(),
                    side_data: Vec::new(),
                },
            ));
        }
    }
    packets.sort_by_key(|(timestamp, _)| *timestamp);
    for (_, packet) in packets {
        muxer
            .write_packet(packet)
            .map_err(|error| error.to_string())?;
    }
    muxer.finalize().map_err(|error| error.to_string())?;
    let transport = muxer.into_bytes().map_err(|error| error.to_string())?;
    std::fs::write(output, &transport)
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Muxed {} MPEG-2 Video bytes{} into {} transport packets at {}",
        elementary.len(),
        audio_data
            .as_ref()
            .map_or_else(String::new, |audio| format!(
                " and {} MPEG Layer II audio bytes",
                audio.len()
            )),
        transport.len() / mmrecode_mpegts::TS_PACKET_SIZE,
        output.display()
    );
    Ok(())
}

fn transport_clock_value(value: i64, time_base: mmrecode_core::Rational) -> Result<i64, String> {
    let numerator = i128::from(value)
        .checked_mul(i128::from(time_base.numerator()))
        .and_then(|scaled| scaled.checked_mul(90_000))
        .ok_or_else(|| "transport timestamp overflows".to_owned())?;
    let result = numerator / i128::from(time_base.denominator());
    i64::try_from(result).map_err(|_| "transport timestamp exceeds i64".to_owned())
}

fn demux_mpegts_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = "usage: mmrecode demux-mpegts <input.ts> <output.m2v>";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    let input = std::path::Path::new(&input);
    let output = std::path::Path::new(&output);
    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let transport =
        mmrecode_mpegts::demux_transport_stream(&bytes).map_err(|error| error.to_string())?;
    let elementary = transport
        .mpeg2_video_bytes()
        .map_err(|error| error.to_string())?;
    std::fs::write(output, &elementary)
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Demuxed {} MPEG-2 Video bytes from {} transport packets to {}",
        elementary.len(),
        transport.packets.len(),
        output.display()
    );
    Ok(())
}

fn extract_mpegts_audio_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = "usage: mmrecode extract-mpegts-audio <input.ts> <output.mp2>";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    let input = std::path::Path::new(&input);
    let output = std::path::Path::new(&output);
    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let transport =
        mmrecode_mpegts::demux_transport_stream(&bytes).map_err(|error| error.to_string())?;
    let audio = transport
        .mpeg1_audio_bytes()
        .map_err(|error| error.to_string())?;
    let frames =
        mmrecode_mpegaudio::parse_layer2_stream(&audio).map_err(|error| error.to_string())?;
    std::fs::write(output, &audio)
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Extracted {} MPEG Layer II frame(s), {} Hz, {} channel(s), {} bytes to {}",
        frames.len(),
        frames[0].header.sample_rate,
        frames[0].header.channels,
        audio.len(),
        output.display()
    );
    Ok(())
}

fn plan_mpeg2_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = "usage: mmrecode plan-mpeg2 <input.m2v> <display-start> <display-end>";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let start = parse_i64_argument(arguments.next(), "display-start", usage)?;
    let end = parse_i64_argument(arguments.next(), "display-end", usage)?;
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    plan_mpeg2(std::path::Path::new(&input), start..end)
}

fn parse_i64_argument(
    argument: Option<std::ffi::OsString>,
    name: &str,
    usage: &str,
) -> Result<i64, String> {
    argument
        .ok_or_else(|| usage.to_owned())?
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))?
        .parse()
        .map_err(|_| format!("{name} must be an integer"))
}

fn plan_mpeg2(input: &std::path::Path, edited: std::ops::Range<i64>) -> Result<(), String> {
    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let stream = mmrecode_mpeg2::parse_stream(&bytes).map_err(|error| error.to_string())?;
    let plan =
        mmrecode_mpeg2::plan_smart_render(&stream, edited).map_err(|error| error.to_string())?;
    println!("MPEG-2 smart-render plan: {}", input.display());
    println!(
        "Edited display range: {}..{}",
        plan.edited_presentation_range.start, plan.edited_presentation_range.end
    );
    for picture in &plan.pictures {
        let action = match &picture.disposition {
            mmrecode_mpeg2::SmartRenderDisposition::Copy => "copy".to_owned(),
            mmrecode_mpeg2::SmartRenderDisposition::EncodeEdited => "encode (edited)".to_owned(),
            mmrecode_mpeg2::SmartRenderDisposition::BridgeEncode {
                affected_references,
            } => format!(
                "bridge-encode (affected refs {})",
                affected_references
                    .iter()
                    .map(|reference| reference.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        };
        println!(
            "Picture {}: decode {}, display {}, bytes 0x{:08x}..0x{:08x}: {action}",
            picture.picture_id.0,
            picture.decode_order,
            picture.presentation_order,
            picture.source_range.start,
            picture.source_range.end
        );
    }
    let encoded_count = plan
        .pictures
        .iter()
        .filter(|picture| picture.disposition != mmrecode_mpeg2::SmartRenderDisposition::Copy)
        .count();
    let ranges = plan
        .encode_presentation_ranges
        .iter()
        .map(|range| format!("{}..{}", range.start, range.end))
        .collect::<Vec<_>>()
        .join(", ");
    println!("Encode display range(s): {ranges}");
    println!(
        "Pictures copied: {}; encoded/bridged: {}",
        plan.pictures.len() - encoded_count,
        encoded_count
    );
    Ok(())
}

#[derive(Debug)]
struct RenderCliOptions {
    display_frame: i64,
    replacement: std::ffi::OsString,
    audio: Option<std::ffi::OsString>,
    audio_boundary: mmrecode_render::AudioBoundaryPolicy,
}

fn render_plan_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = render_plan_usage();
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let options = parse_render_options(arguments, usage)?;
    run_mpeg2_render(
        std::path::Path::new(&input),
        None,
        &options,
        RenderCommandMode::Plan,
    )
}

fn render_command(arguments: &mut impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
    let usage = render_usage();
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    let options = parse_render_options(arguments, usage)?;
    run_mpeg2_render(
        std::path::Path::new(&input),
        Some(std::path::Path::new(&output)),
        &options,
        RenderCommandMode::Execute,
    )
}

const fn render_plan_usage() -> &'static str {
    "usage: mmrecode render-plan <input.m2v> --replace <display-frame> <replacement.y4m> [--audio <input.mp2>] [--audio-end <exact|contained|cover>]"
}

const fn render_usage() -> &'static str {
    "usage: mmrecode render <input.m2v> <output.ts> --replace <display-frame> <replacement.y4m> [--audio <input.mp2>] [--audio-end <exact|contained|cover>]"
}

fn parse_render_options(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    usage: &str,
) -> Result<RenderCliOptions, String> {
    let mut display_frame = None;
    let mut replacement = None;
    let mut audio = None;
    let mut audio_boundary = mmrecode_render::AudioBoundaryPolicy::Exact;
    let mut has_audio_boundary = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--replace") => {
                if replacement.is_some() {
                    return Err("--replace may only be specified once".to_owned());
                }
                display_frame = Some(parse_i64_argument(
                    arguments.next(),
                    "display-frame",
                    usage,
                )?);
                replacement = Some(arguments.next().ok_or_else(|| usage.to_owned())?);
            }
            Some("--audio") => {
                if audio.is_some() {
                    return Err("--audio may only be specified once".to_owned());
                }
                audio = Some(arguments.next().ok_or_else(|| usage.to_owned())?);
            }
            Some("--audio-end") => {
                if has_audio_boundary {
                    return Err("--audio-end may only be specified once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| usage.to_owned())?
                    .into_string()
                    .map_err(|_| "audio-end policy must be valid UTF-8".to_owned())?;
                audio_boundary = match value.as_str() {
                    "exact" => mmrecode_render::AudioBoundaryPolicy::Exact,
                    "contained" => mmrecode_render::AudioBoundaryPolicy::Contained,
                    "cover" => mmrecode_render::AudioBoundaryPolicy::Cover,
                    _ => {
                        return Err(format!(
                            "unknown audio-end policy '{value}'; expected exact, contained, or cover"
                        ));
                    }
                };
                has_audio_boundary = true;
            }
            _ => return Err(usage.to_owned()),
        }
    }
    Ok(RenderCliOptions {
        display_frame: display_frame.ok_or_else(|| usage.to_owned())?,
        replacement: replacement.ok_or_else(|| usage.to_owned())?,
        audio,
        audio_boundary,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderCommandMode {
    Plan,
    Execute,
}

#[allow(clippy::too_many_lines)]
fn run_mpeg2_render(
    input: &std::path::Path,
    output: Option<&std::path::Path>,
    options: &RenderCliOptions,
    mode: RenderCommandMode,
) -> Result<(), String> {
    let source_id = mmrecode_edit::SourceId(0);
    let stream_id = mmrecode_core::StreamId(0);
    let clip_id = mmrecode_edit::ClipId(0);
    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let packet_source = mmrecode_render::analyze_mpeg2_source(&bytes, source_id, stream_id)
        .map_err(|error| error.to_string())?;
    let first_packet = packet_source
        .packets
        .first()
        .ok_or_else(|| "MPEG-2 input contains no pictures".to_owned())?;
    let time_base = first_packet
        .packet
        .pts
        .ok_or_else(|| "analyzed MPEG-2 picture has no PTS".to_owned())?
        .time_base;
    let picture_count = i64::try_from(packet_source.packets.len())
        .map_err(|_| "MPEG-2 picture count exceeds i64".to_owned())?;
    if !(0..picture_count).contains(&options.display_frame) {
        return Err(format!(
            "display frame {} is outside input range 0..{picture_count}",
            options.display_frame
        ));
    }
    let full_range = cli_time_range(0, picture_count, time_base)?;
    let sequence = mmrecode_edit::EditSequence {
        time_base,
        sources: vec![mmrecode_edit::MediaSource {
            id: source_id,
            locator: input.to_string_lossy().into_owned(),
            streams: vec![mmrecode_core::StreamDescriptor {
                id: stream_id,
                codec: mmrecode_core::CodecDescriptor {
                    codec_id: mmrecode_core::CodecId::new("video/mpeg2"),
                    codec_tag: None,
                    media_type: mmrecode_core::MediaType::Video,
                    configuration: Vec::new(),
                },
                time_base,
            }],
        }],
        tracks: vec![mmrecode_edit::Track {
            id: mmrecode_edit::TrackId(0),
            media_type: mmrecode_core::MediaType::Video,
            clips: vec![mmrecode_edit::Clip {
                id: clip_id,
                source_id,
                source_stream_id: stream_id,
                source_range: full_range,
                timeline_range: full_range,
                effects: Vec::new(),
            }],
            transitions: Vec::new(),
        }],
        output: mmrecode_edit::OutputIntent {
            time_base,
            container: Some("container/mpegts".into()),
            video_codec: Some(mmrecode_core::CodecId::new("video/mpeg2")),
            audio_codec: options
                .audio
                .as_ref()
                .map(|_| mmrecode_core::CodecId::new("audio/mpeg1")),
        },
    };
    let change_end = options
        .display_frame
        .checked_add(1)
        .ok_or_else(|| "changed frame range overflows".to_owned())?;
    let render_plan = mmrecode_render::plan_interframe_video(
        &sequence,
        std::slice::from_ref(&packet_source),
        &[mmrecode_render::VideoChange {
            clip_id,
            timeline_range: cli_time_range(options.display_frame, change_end, time_base)?,
        }],
    )
    .map_err(|error| error.to_string())?;
    let replacement = read_single_y4m(std::path::Path::new(&options.replacement))?;
    let mpeg2_output = mmrecode_render::execute_mpeg2_plan_with_report(
        &render_plan,
        std::slice::from_ref(&packet_source),
        &[mmrecode_render::Mpeg2FrameReplacement {
            timeline_pts: mmrecode_core::Timestamp {
                value: options.display_frame,
                time_base,
            },
            frame: replacement,
        }],
        mmrecode_render::Mpeg2BridgeOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    let audio_data = options
        .audio
        .as_deref()
        .map(|path| {
            let path = std::path::Path::new(path);
            std::fs::read(path)
                .map_err(|error| format!("cannot read '{}': {error}", path.display()))
        })
        .transpose()?;
    let audio = audio_data
        .as_deref()
        .map(|data| mmrecode_render::Layer2AudioInput {
            data,
            start: mmrecode_core::Timestamp {
                value: 0,
                time_base,
            },
        });
    let delivery = mmrecode_render::plan_mpeg2_mpegts(
        &render_plan,
        &mpeg2_output.packets,
        audio,
        mmrecode_render::MpegTsRenderOptions {
            audio_boundary: options.audio_boundary,
            ..mmrecode_render::MpegTsRenderOptions::default()
        },
    )
    .map_err(|error| error.to_string())?;
    println!("MMRecode render plan: {}", input.display());
    println!("Changed display frame: {}", options.display_frame);
    println!("Decision: {}", render_plan.decisions[0].reason);
    println!(
        "Work: {} decode, {} encode, {} copy packet(s)",
        render_plan.summary.decoded_frames,
        render_plan.summary.encoded_frames,
        render_plan.summary.copied_packets
    );
    println!("{}", mpeg2_output.splice.explanation());
    println!("{}", delivery.report().explanation());
    if mode == RenderCommandMode::Execute {
        let output = output.ok_or_else(|| "render output path is missing".to_owned())?;
        let executed =
            mmrecode_render::execute_mpeg2_mpegts(&delivery).map_err(|error| error.to_string())?;
        std::fs::write(output, &executed.data)
            .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
        println!("{}", executed.report.explanation());
        println!(
            "Wrote {} bytes to {}",
            executed.data.len(),
            output.display()
        );
    }
    Ok(())
}

fn cli_time_range(
    start: i64,
    end: i64,
    time_base: mmrecode_core::Rational,
) -> Result<mmrecode_edit::TimeRange, String> {
    mmrecode_edit::TimeRange::new(
        mmrecode_core::Timestamp {
            value: start,
            time_base,
        },
        mmrecode_core::Timestamp {
            value: end,
            time_base,
        },
    )
    .map_err(|error| error.to_string())
}

fn read_single_y4m(path: &std::path::Path) -> Result<mmrecode_core::VideoFrame, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
    let mut reader = mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file));
    let frame = reader
        .read_frame()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("replacement Y4M '{}' contains no frames", path.display()))?;
    if reader
        .read_frame()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err(format!(
            "replacement Y4M '{}' must contain exactly one frame",
            path.display()
        ));
    }
    Ok(frame)
}

fn encode_mpeg2(
    input: &std::path::Path,
    output: &std::path::Path,
    quantiser_scale_code: u8,
) -> Result<(), String> {
    use std::io::Write as _;

    let file = std::fs::File::open(input)
        .map_err(|error| format!("cannot open '{}': {error}", input.display()))?;
    let mut reader = mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_frame().map_err(|error| error.to_string())? {
        frames.push(frame);
    }
    if frames.is_empty() {
        return Err("Y4M input contains no frames".to_owned());
    }
    let encoded = mmrecode_mpeg2::encode_stream(
        &frames,
        mmrecode_mpeg2::Mpeg2EncodeOptions {
            quantiser_scale_code,
            ..mmrecode_mpeg2::Mpeg2EncodeOptions::default()
        },
    )
    .map_err(|error| error.to_string())?;
    let mut aggregate_mse = 0.0;
    for (index, (source, reconstruction)) in frames.iter().zip(&encoded.reconstructed).enumerate() {
        let report = mmrecode_quality::compare_video_frames(source, reconstruction)
            .map_err(|error| error.to_string())?;
        aggregate_mse += report.mean_squared_error;
        println!("Frame {}: {}", index + 1, quality_summary(&report));
    }
    std::fs::File::create(output)
        .and_then(|mut file| file.write_all(&encoded.data))
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    let frame_count_f64 =
        f64::from(u32::try_from(frames.len()).map_err(|_| "too many frames for quality summary")?);
    println!(
        "Encoded {} frame(s) as MPEG-2 Video to {} ({} bytes, mean frame MSE {:.4})",
        frames.len(),
        output.display(),
        encoded.data.len(),
        aggregate_mse / frame_count_f64
    );
    Ok(())
}

fn encode_dv_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = "usage: mmrecode encode-dv <input.y4m> <output.dv>";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    encode_dv(std::path::Path::new(&input), std::path::Path::new(&output))
}

fn encode_dv(input: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
    use std::io::Write as _;

    let file = std::fs::File::open(input)
        .map_err(|error| format!("cannot open '{}': {error}", input.display()))?;
    let mut reader = mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file));
    let mut encoded_stream = Vec::new();
    let mut frame_count = 0;
    while let Some(frame) = reader.read_frame().map_err(|error| error.to_string())? {
        let encoded = mmrecode_dv::encode_video(&frame).map_err(|error| error.to_string())?;
        let report = mmrecode_quality::compare_video_frames(&frame, &encoded.reconstructed)
            .map_err(|error| error.to_string())?;
        println!("Frame {}: {}", frame_count + 1, quality_summary(&report));
        encoded_stream.extend_from_slice(&encoded.data);
        frame_count += 1;
    }
    if frame_count == 0 {
        return Err("Y4M input contains no frames".to_owned());
    }
    std::fs::File::create(output)
        .and_then(|mut file| file.write_all(&encoded_stream))
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Encoded {frame_count} Y4M frame(s) as raw DV to {} ({} bytes)",
        output.display(),
        encoded_stream.len()
    );
    Ok(())
}

fn extract_dv_audio_command(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
) -> Result<(), String> {
    let usage = "usage: mmrecode extract-dv-audio <input.dv> <output.s16le>";
    let input = arguments.next().ok_or_else(|| usage.to_owned())?;
    let output = arguments.next().ok_or_else(|| usage.to_owned())?;
    if arguments.next().is_some() {
        return Err(usage.to_owned());
    }
    extract_dv_audio(std::path::Path::new(&input), std::path::Path::new(&output))
}

fn extract_dv_audio(input: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
    use std::io::Write as _;

    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    let profile = mmrecode_dv::detect_profile_prefix(&bytes).map_err(|error| error.to_string())?;
    if !bytes.len().is_multiple_of(profile.frame_size) {
        return Err("raw DV input ends with an incomplete frame".to_owned());
    }
    let mut pcm = Vec::new();
    let mut sample_rate = None;
    let mut samples_per_channel = 0_usize;
    for (index, data) in bytes.chunks_exact(profile.frame_size).enumerate() {
        let frame = mmrecode_dv::parse_frame(data).map_err(|error| error.to_string())?;
        let audio = mmrecode_dv::extract_audio(&frame).map_err(|error| error.to_string())?;
        if audio.len() != 1 {
            return Err(format!(
                "frame {} contains {} stereo pairs; raw extraction currently requires one",
                index + 1,
                audio.len()
            ));
        }
        let audio = &audio[0];
        if sample_rate.is_some_and(|rate| rate != audio.sample_rate) {
            return Err(format!("audio sample rate changes at frame {}", index + 1));
        }
        sample_rate = Some(audio.sample_rate);
        samples_per_channel += audio.samples_per_channel;
        pcm.reserve(audio.samples.len() * 2);
        for sample in &audio.samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
    }
    std::fs::File::create(output)
        .and_then(|mut file| file.write_all(&pcm))
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Extracted {} stereo samples/channel at {} Hz to {} (signed 16-bit little-endian)",
        samples_per_channel,
        sample_rate.unwrap_or(0),
        output.display()
    );
    Ok(())
}

fn decode(input: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
    use std::io::Write as _;

    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    if bytes.is_empty() {
        return Err("input contains no media frames".to_owned());
    }
    if is_mpegts(&bytes) {
        let transport =
            mmrecode_mpegts::demux_transport_stream(&bytes).map_err(|error| error.to_string())?;
        let elementary = transport
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string())?;
        return decode_mpeg2(output, &elementary);
    }
    if is_mpeg2_video(&bytes) {
        return decode_mpeg2(output, &bytes);
    }
    if mmrecode_dv::detect_profile_prefix(&bytes).is_ok() {
        return decode_dv(input, output, &bytes);
    }
    let file = std::fs::File::create(output)
        .map_err(|error| format!("cannot create '{}': {error}", output.display()))?;
    let mut y4m = mmrecode_y4m::Y4mWriter::new(std::io::BufWriter::new(file));
    let mut remaining = bytes.as_slice();
    let mut frame_count = 0_usize;
    while !remaining.is_empty() {
        let structure = mmrecode_mjpeg::parse_jpeg(remaining).map_err(|error| error.to_string())?;
        let consumed = remaining.len() - structure.trailing_data.len();
        if consumed == 0 {
            return Err("JPEG parser did not consume input".to_owned());
        }
        let frame = mmrecode_mjpeg::decode_jpeg(&remaining[..consumed])
            .and_then(mmrecode_mjpeg::DecodedJpeg::into_video_frame)
            .map_err(|error| error.to_string())?;
        y4m.write_frame(&frame).map_err(|error| error.to_string())?;
        frame_count += 1;
        remaining = &remaining[consumed..];
    }
    y4m.into_inner()
        .flush()
        .map_err(|error| format!("cannot finish '{}': {error}", output.display()))?;
    println!(
        "Decoded {frame_count} JPEG frame(s) to {}",
        output.display()
    );
    Ok(())
}

fn decode_mpeg2(output: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;

    let decoded = mmrecode_mpeg2::decode_stream(bytes).map_err(|error| error.to_string())?;
    let file = std::fs::File::create(output)
        .map_err(|error| format!("cannot create '{}': {error}", output.display()))?;
    let mut y4m = mmrecode_y4m::Y4mWriter::new(std::io::BufWriter::new(file));
    for picture in &decoded {
        y4m.write_frame(&picture.frame)
            .map_err(|error| error.to_string())?;
    }
    y4m.into_inner()
        .flush()
        .map_err(|error| format!("cannot finish '{}': {error}", output.display()))?;
    println!(
        "Decoded {} MPEG-2 picture(s) in presentation order to {}",
        decoded.len(),
        output.display()
    );
    Ok(())
}

fn decode_dv(
    input: &std::path::Path,
    output: &std::path::Path,
    bytes: &[u8],
) -> Result<(), String> {
    use std::io::Write as _;

    let profile = mmrecode_dv::detect_profile_prefix(bytes).map_err(|error| error.to_string())?;
    if !bytes.len().is_multiple_of(profile.frame_size) {
        return Err(format!(
            "'{}' ends with an incomplete raw DV frame",
            input.display()
        ));
    }
    let file = std::fs::File::create(output)
        .map_err(|error| format!("cannot create '{}': {error}", output.display()))?;
    let mut y4m = mmrecode_y4m::Y4mWriter::new(std::io::BufWriter::new(file));
    let mut frame_count = 0;
    for data in bytes.chunks_exact(profile.frame_size) {
        let parsed = mmrecode_dv::parse_frame(data).map_err(|error| error.to_string())?;
        let frame = mmrecode_dv::decode_video(&parsed).map_err(|error| error.to_string())?;
        y4m.write_frame(&frame).map_err(|error| error.to_string())?;
        frame_count += 1;
    }
    y4m.into_inner()
        .flush()
        .map_err(|error| format!("cannot finish '{}': {error}", output.display()))?;
    println!(
        "Decoded {frame_count} raw DV frame(s) to {}",
        output.display()
    );
    Ok(())
}

fn encode_y4m(
    input: &std::path::Path,
    output: &std::path::Path,
    quality: u8,
) -> Result<(), String> {
    use std::io::Write as _;

    let file = std::fs::File::open(input)
        .map_err(|error| format!("cannot open '{}': {error}", input.display()))?;
    let mut reader = mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file));
    let mut encoded_stream = Vec::new();
    let mut frame_count = 0_usize;
    while let Some(frame) = reader.read_frame().map_err(|error| error.to_string())? {
        let encoded =
            mmrecode_mjpeg::encode_jpeg(&frame, mmrecode_mjpeg::JpegEncodeOptions { quality })
                .map_err(|error| error.to_string())?;
        let report = mmrecode_quality::compare_video_frames(&frame, &encoded.reconstructed)
            .map_err(|error| error.to_string())?;
        println!("Frame {}: {}", frame_count + 1, quality_summary(&report));
        encoded_stream.extend_from_slice(&encoded.data);
        frame_count += 1;
    }
    if frame_count == 0 {
        return Err("Y4M input contains no frames".to_owned());
    }
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(output)
            .map_err(|error| format!("cannot create '{}': {error}", output.display()))?,
    );
    file.write_all(&encoded_stream)
        .and_then(|()| file.flush())
        .map_err(|error| format!("cannot write '{}': {error}", output.display()))?;
    println!(
        "Encoded {frame_count} Y4M frame(s) at quality {quality} to {} ({} bytes)",
        output.display(),
        encoded_stream.len()
    );
    Ok(())
}

fn verify(input: &std::path::Path, reference: Option<&std::path::Path>) -> Result<(), String> {
    let bytes = std::fs::read(input)
        .map_err(|error| format!("cannot read '{}': {error}", input.display()))?;
    if is_mpegts(&bytes) {
        let transport =
            mmrecode_mpegts::demux_transport_stream(&bytes).map_err(|error| error.to_string())?;
        let elementary = transport
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string())?;
        return verify_mpeg2(&elementary, reference);
    }
    if is_mpeg2_video(&bytes) {
        return verify_mpeg2(&bytes, reference);
    }
    let mut reference_reader = reference
        .map(|path| {
            let file = std::fs::File::open(path)
                .map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
            Ok::<_, String>(mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file)))
        })
        .transpose()?;
    let mut remaining = bytes.as_slice();
    let mut frame_count = 0_usize;
    while !remaining.is_empty() {
        let structure = mmrecode_mjpeg::parse_jpeg(remaining).map_err(|error| error.to_string())?;
        let consumed = remaining.len() - structure.trailing_data.len();
        if consumed == 0 {
            return Err("JPEG parser did not consume input".to_owned());
        }
        let frame = mmrecode_mjpeg::decode_jpeg(&remaining[..consumed])
            .and_then(mmrecode_mjpeg::DecodedJpeg::into_video_frame)
            .map_err(|error| error.to_string())?;
        frame_count += 1;
        println!(
            "Frame {frame_count}: {}x{} {:?}, {consumed} bytes, {} segment(s)",
            frame.width,
            frame.height,
            frame.format,
            structure.segments.len()
        );
        if let Some(reader) = &mut reference_reader {
            let expected = reader
                .read_frame()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("reference Y4M has fewer than {frame_count} frames"))?;
            let report = mmrecode_quality::compare_video_frames(&expected, &frame)
                .map_err(|error| error.to_string())?;
            print_quality_report(frame_count, &report);
        }
        remaining = &remaining[consumed..];
    }
    if frame_count == 0 {
        return Err("input contains no JPEG frames".to_owned());
    }
    if let Some(reader) = &mut reference_reader
        && reader
            .read_frame()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("reference Y4M has more frames than the JPEG input".to_owned());
    }
    println!("Verification passed for {frame_count} frame(s)");
    Ok(())
}

fn verify_mpeg2(bytes: &[u8], reference: Option<&std::path::Path>) -> Result<(), String> {
    let stream = mmrecode_mpeg2::parse_stream(bytes).map_err(|error| error.to_string())?;
    let dependencies =
        mmrecode_mpeg2::analyze_dependencies(&stream).map_err(|error| error.to_string())?;
    let decoded = mmrecode_mpeg2::decode_stream(bytes).map_err(|error| error.to_string())?;
    let mut reference_reader = reference
        .map(|path| {
            let file = std::fs::File::open(path)
                .map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
            Ok::<_, String>(mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(file)))
        })
        .transpose()?;
    for (index, picture) in decoded.iter().enumerate() {
        println!(
            "Picture {}: {:?}, decode {}, display {}, {}x{} {:?}",
            index + 1,
            picture.picture_type,
            picture.decode_order,
            picture.presentation_order,
            picture.frame.width,
            picture.frame.height,
            picture.frame.format
        );
        if let Some(reader) = &mut reference_reader {
            let expected = reader
                .read_frame()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("reference Y4M has fewer than {} frames", index + 1))?;
            let report = mmrecode_quality::compare_video_frames(&expected, &picture.frame)
                .map_err(|error| error.to_string())?;
            print_quality_report(index + 1, &report);
        }
    }
    if let Some(reader) = &mut reference_reader
        && reader
            .read_frame()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("reference Y4M has more frames than the MPEG-2 input".to_owned());
    }
    println!(
        "Verification passed for {} MPEG-2 picture(s), {} dependency record(s), {} start code(s)",
        decoded.len(),
        dependencies.len(),
        stream.units().len()
    );
    Ok(())
}

fn compare_y4m(reference: &std::path::Path, candidate: &std::path::Path) -> Result<(), String> {
    let reference_file = std::fs::File::open(reference)
        .map_err(|error| format!("cannot open '{}': {error}", reference.display()))?;
    let candidate_file = std::fs::File::open(candidate)
        .map_err(|error| format!("cannot open '{}': {error}", candidate.display()))?;
    let mut reference_reader =
        mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(reference_file));
    let mut candidate_reader =
        mmrecode_y4m::Y4mReader::new(std::io::BufReader::new(candidate_file));
    let mut frame_count = 0_usize;
    loop {
        let reference_frame = reference_reader
            .read_frame()
            .map_err(|error| error.to_string())?;
        let candidate_frame = candidate_reader
            .read_frame()
            .map_err(|error| error.to_string())?;
        match (reference_frame, candidate_frame) {
            (Some(reference_frame), Some(candidate_frame)) => {
                frame_count += 1;
                let report =
                    mmrecode_quality::compare_video_frames(&reference_frame, &candidate_frame)
                        .map_err(|error| error.to_string())?;
                print_quality_report(frame_count, &report);
            }
            (None, None) => break,
            (Some(_), None) => return Err("candidate Y4M has fewer frames".to_owned()),
            (None, Some(_)) => return Err("candidate Y4M has more frames".to_owned()),
        }
    }
    if frame_count == 0 {
        return Err("Y4M inputs contain no frames".to_owned());
    }
    println!("Compared {frame_count} frame(s)");
    Ok(())
}

fn print_quality_report(frame_number: usize, report: &mmrecode_quality::FrameQualityReport) {
    println!(
        "  Frame {frame_number} quality: {}",
        quality_summary(report)
    );
    for plane in &report.planes {
        let psnr = if plane.psnr.is_infinite() {
            "exact".to_owned()
        } else {
            format!("{:.3} dB", plane.psnr)
        };
        println!(
            "    Plane {}: PSNR {psnr}, MSE {:.4}, max error {}",
            plane.plane_index, plane.mean_squared_error, plane.maximum_absolute_error
        );
    }
}

fn quality_summary(report: &mmrecode_quality::FrameQualityReport) -> String {
    let psnr = if report.psnr.is_infinite() {
        "exact".to_owned()
    } else {
        format!("{:.3} dB", report.psnr)
    };
    format!(
        "PSNR {psnr}, MSE {:.4}, max error {}",
        report.mean_squared_error, report.maximum_absolute_error
    )
}

fn inspect(path: &std::path::Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if bytes.is_empty() {
        return Err("input is empty".to_owned());
    }
    if is_mpegts(&bytes) {
        return inspect_mpegts(path, &bytes);
    }
    if is_mpeg2_video(&bytes) {
        return inspect_mpeg2(path, &bytes);
    }
    if mmrecode_dv::detect_profile_prefix(&bytes).is_ok() {
        return inspect_dv(path, &bytes);
    }
    let multiple_frames = !mmrecode_mjpeg::parse_jpeg(&bytes)
        .map_err(|error| error.to_string())?
        .trailing_data
        .is_empty();
    let mut remaining = bytes.as_slice();
    let mut frame_count = 0_usize;
    let mut file_offset = 0_usize;
    while !remaining.is_empty() {
        let boundary = mmrecode_mjpeg::parse_jpeg(remaining).map_err(|error| error.to_string())?;
        let consumed = remaining.len() - boundary.trailing_data.len();
        let image = mmrecode_mjpeg::parse_jpeg(&remaining[..consumed])
            .map_err(|error| error.to_string())?;
        frame_count += 1;
        if multiple_frames {
            println!(
                "Motion JPEG frame {frame_count} at file offset 0x{file_offset:08x} (marker offsets below are frame-relative)"
            );
        }
        print!("{}", inspection_report(path, consumed, &image));
        remaining = &remaining[consumed..];
        file_offset += consumed;
    }
    if multiple_frames {
        println!("Motion JPEG frames: {frame_count}");
    }
    Ok(())
}

fn is_mpeg2_video(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .take(256)
        .any(|window| window == [0, 0, 1, 0xb3])
}

fn is_mpegts(bytes: &[u8]) -> bool {
    bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE
        && bytes
            .chunks(mmrecode_mpegts::TS_PACKET_SIZE)
            .take(3)
            .all(|packet| packet.first() == Some(&0x47))
}

fn inspect_mpegts(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let transport =
        mmrecode_mpegts::demux_transport_stream(bytes).map_err(|error| error.to_string())?;
    println!("MPEG-2 Transport Stream: {}", path.display());
    println!(
        "File size: {} bytes, {} packet(s) of 188 bytes",
        bytes.len(),
        transport.packets.len()
    );
    println!(
        "PAT section(s): {}, PMT section(s): {}, PES packet(s): {}, issue(s): {}",
        transport.program_association_tables.len(),
        transport.program_map_tables.len(),
        transport.elementary_packets.len(),
        transport.issues.len()
    );
    for table in &transport.program_map_tables {
        println!(
            "Program {}: PMT PID 0x{:04x}, PCR PID 0x{:04x}, version {}",
            table.program_number, table.pid, table.pcr_pid, table.version
        );
        for stream in &table.streams {
            let descriptor = transport
                .streams
                .iter()
                .find(|descriptor| descriptor.id.0 == u32::from(stream.elementary_pid));
            println!(
                "  PID 0x{:04x}: stream type 0x{:02x}, {}",
                stream.elementary_pid,
                stream.stream_type,
                descriptor.map_or("unknown", |value| value.codec.codec_id.as_str())
            );
        }
    }
    let pcr_count = transport
        .packets
        .iter()
        .filter(|packet| packet.pcr.is_some())
        .count();
    println!("PCR samples: {pcr_count}");
    if let Ok(elementary) = transport.mpeg2_video_bytes() {
        let video = mmrecode_mpeg2::parse_stream(&elementary).map_err(|error| error.to_string())?;
        let sequence = &video.pictures()[0].sequence;
        println!(
            "MPEG-2 Video: {}x{}, {}/{} fps, {} picture(s), {} elementary byte(s)",
            sequence.width,
            sequence.height,
            sequence.frame_rate.numerator(),
            sequence.frame_rate.denominator(),
            video.pictures().len(),
            elementary.len()
        );
    }
    if let Ok(audio) = transport.mpeg1_audio_bytes() {
        let frames =
            mmrecode_mpegaudio::parse_layer2_stream(&audio).map_err(|error| error.to_string())?;
        let header = frames[0].header;
        println!(
            "MPEG-1 Audio Layer II: {} Hz, {} channel(s), {} bit/s, {} frame(s), {} elementary byte(s)",
            header.sample_rate,
            header.channels,
            header.bit_rate,
            frames.len(),
            audio.len()
        );
    }
    Ok(())
}

fn inspect_mpeg2(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::fmt::Write as _;

    let stream = mmrecode_mpeg2::parse_stream(bytes).map_err(|error| error.to_string())?;
    let dependencies =
        mmrecode_mpeg2::analyze_dependencies(&stream).map_err(|error| error.to_string())?;
    let sequence = &stream.pictures()[0].sequence;
    let mut report = String::new();
    let _ = writeln!(report, "MPEG-2 Video: {}", path.display());
    let _ = writeln!(report, "File size: {} bytes", bytes.len());
    let _ = writeln!(
        report,
        "Sequence: {}x{}, {:?}, {}/{} fps, profile/level 0x{:02x}",
        sequence.width,
        sequence.height,
        sequence.chroma_format,
        sequence.frame_rate.numerator(),
        sequence.frame_rate.denominator(),
        sequence.profile_and_level_indication
    );
    let _ = writeln!(
        report,
        "Progressive sequence: {}, bit rate: {}, VBV: {} bits",
        sequence.progressive_sequence,
        sequence
            .bit_rate
            .map_or_else(|| "variable".to_owned(), |rate| format!("{rate} bit/s")),
        sequence.vbv_buffer_size_bits
    );
    if let Some(display) = sequence.display {
        let _ = writeln!(
            report,
            "Display: {}x{}, video format {}, colour {:?}",
            display.display_horizontal_size,
            display.display_vertical_size,
            display.video_format,
            display.colour_description
        );
    }
    let _ = writeln!(
        report,
        "Start codes: {}, GOPs: {}, pictures: {} (decode order below)",
        stream.units().len(),
        stream.groups().len(),
        stream.pictures().len()
    );
    for (index, (picture, dependency)) in stream.pictures().iter().zip(&dependencies).enumerate() {
        let references = dependency
            .references
            .iter()
            .map(|reference| reference.0.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            report,
            "Picture {index}: {:?}, temporal {}, decode {}, display {}, {:?}, refs [{}], {} slice(s), bytes 0x{:08x}..0x{:08x}",
            picture.header.picture_coding_type,
            picture.header.temporal_reference,
            dependency.decode_order,
            dependency.presentation_order,
            dependency.random_access,
            references,
            picture.slices.len(),
            picture.source_range.start,
            picture.source_range.end
        );
        let extension = picture.coding_extension;
        let _ = writeln!(
            report,
            "  {:?}, progressive {}, top-first {}, frame-pred {}, qscale {}, intra-VLC {}, alternate-scan {}",
            extension.picture_structure,
            extension.progressive_frame,
            extension.top_field_first,
            extension.frame_pred_frame_dct,
            if extension.q_scale_type {
                "non-linear"
            } else {
                "linear"
            },
            extension.intra_vlc_format,
            extension.alternate_scan
        );
    }
    print!("{report}");
    Ok(())
}

fn inspect_dv(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::fmt::Write as _;

    use mmrecode_dv::{DifSection, DvPackData};

    let profile = mmrecode_dv::detect_profile_prefix(bytes).map_err(|error| error.to_string())?;
    if !bytes.len().is_multiple_of(profile.frame_size) {
        return Err(format!(
            "raw DV stream has {} trailing byte(s) after complete {}-byte frames",
            bytes.len() % profile.frame_size,
            profile.frame_size
        ));
    }
    let mut report = String::new();
    let frame_count = bytes.len() / profile.frame_size;
    let rate = profile.frame_rate();
    let _ = writeln!(report, "DV: {}", path.display());
    let _ = writeln!(report, "File size: {} bytes", bytes.len());
    let _ = writeln!(
        report,
        "Profile: {:?}, {}x{} {:?}, {}/{} fps",
        profile.system,
        profile.width,
        profile.height,
        profile.pixel_format,
        rate.numerator(),
        rate.denominator()
    );
    let _ = writeln!(
        report,
        "Frames: {frame_count}, {} bytes/frame, {} DIF sequences/frame",
        profile.frame_size, profile.dif_sequences
    );
    for (frame_index, data) in bytes.chunks_exact(profile.frame_size).enumerate() {
        let frame = mmrecode_dv::parse_frame(data)
            .map_err(|error| format!("DV frame {} cannot be parsed: {error}", frame_index + 1))?;
        let counts = [
            DifSection::Header,
            DifSection::Subcode,
            DifSection::Vaux,
            DifSection::Audio,
            DifSection::Video,
        ]
        .map(|section| {
            frame
                .blocks()
                .iter()
                .filter(|block| block.id.section == section)
                .count()
        });
        let timecode = frame.packs().iter().find_map(|pack| match pack.data {
            DvPackData::Timecode(value) => Some(value),
            _ => None,
        });
        let audio = mmrecode_dv::extract_audio(&frame).ok();
        let _ = writeln!(
            report,
            "Frame {} @ 0x{:08x}: DIF H/S/V/A/Video = {}/{}/{}/{}/{}, {} issue(s), {} pack(s)",
            frame_index + 1,
            frame_index * profile.frame_size,
            counts[0],
            counts[1],
            counts[2],
            counts[3],
            counts[4],
            frame.issues().len(),
            frame.packs().len()
        );
        if let Some(timecode) = timecode {
            let separator = if timecode.drop_frame { ';' } else { ':' };
            let _ = writeln!(
                report,
                "  Timecode: {:02}:{:02}:{:02}{separator}{:02}",
                timecode.hours, timecode.minutes, timecode.seconds, timecode.frames
            );
        }
        if let Some(audio) = audio
            && let Some(first) = audio.first()
        {
            let _ = writeln!(
                report,
                "  Audio: {} stereo pair(s), {} Hz, {} samples/channel",
                audio.len(),
                first.sample_rate,
                first.samples_per_channel
            );
        }
        for issue in frame.issues().iter().take(8) {
            let _ = writeln!(
                report,
                "  Issue at frame byte 0x{:08x}: {:?}",
                issue.offset, issue.kind
            );
        }
        if frame.issues().len() > 8 {
            let _ = writeln!(report, "  … {} more issue(s)", frame.issues().len() - 8);
        }
    }
    print!("{report}");
    Ok(())
}

fn inspection_report(
    path: &std::path::Path,
    byte_length: usize,
    image: &mmrecode_mjpeg::JpegImage,
) -> String {
    use std::fmt::Write as _;

    let mut report = String::new();
    let _ = writeln!(report, "JPEG: {}", path.display());
    let _ = writeln!(report, "File size: {byte_length} bytes");
    if let Some(frame) = image.frame_header() {
        let _ = writeln!(
            report,
            "Frame: {}x{}, {}-bit baseline sequential DCT, {} component(s)",
            frame.width,
            frame.height,
            frame.sample_precision,
            frame.components.len()
        );
        for component in &frame.components {
            let _ = writeln!(
                report,
                "  Component {}: sampling {}x{}, quantization table {}",
                component.id,
                component.horizontal_sampling,
                component.vertical_sampling,
                component.quantization_table
            );
        }
    } else {
        let _ = writeln!(report, "Frame: no baseline SOF0 header");
    }
    if let Some(jfif) = image.jfif_header() {
        let _ = writeln!(
            report,
            "JFIF: {}.{:02}, density {}x{} (unit {})",
            jfif.version_major,
            jfif.version_minor,
            jfif.density_x,
            jfif.density_y,
            jfif.density_units
        );
    }

    let _ = writeln!(report, "Segments:");
    for segment in &image.segments {
        append_segment(&mut report, segment);
    }

    let _ = writeln!(report, "Entropy scans:");
    for (index, scan) in image.entropy_scans.iter().enumerate() {
        let _ = writeln!(
            report,
            "  {}: offset 0x{:08x}, {} source bytes, {} restart marker(s)",
            index + 1,
            scan.data_offset,
            scan.data_length,
            scan.restart_markers.len()
        );
    }
    if !image.trailing_data.is_empty() {
        let _ = writeln!(
            report,
            "Trailing data: {} byte(s) after EOI",
            image.trailing_data.len()
        );
    }
    report
}

fn append_segment(report: &mut String, segment: &mmrecode_mjpeg::JpegSegment) {
    use std::fmt::Write as _;

    use mmrecode_mjpeg::{HuffmanTableClass, Marker, QuantizationPrecision, SegmentData};

    let marker_label = match segment.marker {
        Marker::Application(number) => format!("APP{number}"),
        Marker::Restart(number) => format!("RST{number}"),
        Marker::Other(code) => format!("0x{code:02x}"),
        marker => marker.name().to_owned(),
    };
    let _ = write!(report, "  0x{:08x}  {marker_label}", segment.offset);
    if segment.payload_offset.is_some() {
        let _ = write!(report, "  payload {} bytes", segment.payload_length);
    }
    match &segment.data {
        SegmentData::QuantizationTables(tables) => {
            let details = tables
                .iter()
                .map(|table| {
                    let bits = match table.precision {
                        QuantizationPrecision::EightBit => 8,
                        QuantizationPrecision::SixteenBit => 16,
                    };
                    format!("Q{} ({bits}-bit)", table.id)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(report, "  [{details}]");
        }
        SegmentData::HuffmanTables(tables) => {
            let details = tables
                .iter()
                .map(|table| {
                    let class = match table.class {
                        HuffmanTableClass::Dc => "DC",
                        HuffmanTableClass::Ac => "AC",
                    };
                    format!("{class}{} ({} symbols)", table.id, table.symbols.len())
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(report, "  [{details}]");
        }
        SegmentData::RestartInterval(interval) => {
            let _ = write!(report, "  {interval} MCU(s)");
        }
        SegmentData::Scan(scan) => {
            let _ = write!(report, "  {} component(s)", scan.components.len());
        }
        SegmentData::Comment(comment) => {
            let _ = write!(report, "  {} comment byte(s)", comment.len());
        }
        SegmentData::Empty
        | SegmentData::Frame(_)
        | SegmentData::Jfif(_)
        | SegmentData::Application(_)
        | SegmentData::Unknown(_) => {}
    }
    report.push('\n');
}

fn print_help() {
    println!(
        "MMRecode media-codec tools\n\n\
         Usage: mmrecode <command> [arguments]\n\n\
         Available commands:\n  edit [script]         Start the linked-media editor or execute a command script\n  \
         inspect <media-file>  Inspect JPEG/MJPEG, raw DV, MPEG-2 Video, or MPEG-TS syntax\n  \
         extract-dv-audio <dv> <s16le>  Extract one DV stereo pair as raw PCM\n  \
         decode <media-file> <y4m>  Decode JPEG, raw DV, MPEG-2 Video, or MPEG-TS to YUV4MPEG2\n  \
         encode-dv <y4m> <dv>  Encode native-layout Y4M frame(s) as raw DV25\n  \
         encode-mpeg2 <y4m> <m2v> [qscale]  Encode Y4M as MPEG-2 Main Profile Video\n  \
         mux-mpegts <m2v> <ts> [mp2]  Mux MPEG-2 Video and optional Layer II audio\n  \
         demux-mpegts <ts> <m2v>  Extract the first MPEG-2 Video elementary stream\n  \
         extract-mpegts-audio <ts> <mp2>  Extract MPEG-1 Audio Layer II\n  \
         plan-mpeg2 <m2v> <start> <end>  Explain copy and bridge-encode picture ranges\n  \
         render-plan <m2v> --replace <frame> <y4m> [--audio <mp2>] [--audio-end <policy>]\n  \
             Validate and explain one MPEG-2 replacement render without writing a container\n  \
         render <m2v> <ts> --replace <frame> <y4m> [--audio <mp2>] [--audio-end <policy>]\n  \
             Smart-render one MPEG-2 replacement into MPEG-TS\n  \
         encode <y4m> <mjpg> [quality]  Encode Y4M frame(s) as baseline JPEG\n  \
         verify <media> [reference.y4m]  Verify JPEG/MJPEG or MPEG-2 ES/TS reconstruction and quality\n  \
         compare <reference.y4m> <candidate.y4m>  Compare decoded frame quality\n  \
         help                 Show this help\n  version              Show the version\n\n\
         Planned commands:\n  benchmark"
    );
}

#[cfg(test)]
mod tests {
    use mmrecode_mjpeg::parse_jpeg;

    use super::inspection_report;

    #[test]
    fn report_includes_frame_and_marker_offsets() {
        let bytes = include_bytes!("../../../testdata/jpeg/valid/baseline-420.jpg");
        let image = parse_jpeg(bytes).expect("valid checked-in JPEG");
        let report = inspection_report(std::path::Path::new("sample.jpg"), bytes.len(), &image);
        assert!(report.contains("Frame: 16x16, 8-bit baseline sequential DCT"));
        assert!(report.contains("SOF0"));
    }
}
