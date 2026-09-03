use std::path::Path;

use mmrecode_core::Rational;
use mmrecode_edit::{MediaKind, ProjectColorSpace, ProjectScanMode, ProjectSettings};

#[derive(Clone, Debug)]
pub(crate) struct MediaProbe {
    pub(crate) kind: MediaKind,
    pub(crate) frame_rate: Rational,
    pub(crate) frame_count: usize,
    pub(crate) project_settings: ProjectSettings,
    pub(crate) has_audio_format: bool,
}

impl MediaProbe {
    pub(crate) fn frame_time_base(&self) -> Result<Rational, String> {
        Rational::new(self.frame_rate.denominator(), self.frame_rate.numerator())
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn probe_media(path: &Path, current: &ProjectSettings) -> Result<MediaProbe, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if looks_like_isobmff(&bytes) {
        return probe_isobmff(path, bytes, current);
    }
    probe_mpeg2(path, bytes, current)
}

pub(crate) fn looks_like_isobmff(bytes: &[u8]) -> bool {
    bytes
        .get(4..8)
        .is_some_and(|kind| matches!(kind, b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free"))
}

fn probe_isobmff(
    _path: &Path,
    bytes: Vec<u8>,
    current: &ProjectSettings,
) -> Result<MediaProbe, String> {
    let movie = mmrecode_isobmff::IsoBmffFile::parse(bytes).map_err(|error| error.to_string())?;
    let track = movie
        .h264_track()
        .ok_or_else(|| "ISO-BMFF file has no H.264/AVC video track".to_owned())?;
    let avcc =
        mmrecode_h264::AvcDecoderConfigurationRecord::parse(&track.descriptor.codec.configuration)
            .map_err(|error| error.to_string())?;
    let sps = avcc
        .sequence_parameter_sets
        .first()
        .ok_or_else(|| "H.264 avcC record has no sequence parameter set".to_owned())
        .and_then(|nal| mmrecode_h264::parse_sps(nal).map_err(|error| error.to_string()))?;
    let frame_count = track.samples.len();
    let total_duration = track.samples.iter().try_fold(0_u64, |total, sample| {
        total
            .checked_add(u64::from(sample.duration))
            .ok_or_else(|| "H.264 sample duration overflows".to_owned())
    })?;
    if total_duration == 0 {
        return Err("H.264 track duration is zero".into());
    }
    let timescale = u64::try_from(track.descriptor.time_base.denominator())
        .map_err(|_| "invalid H.264 track timescale".to_owned())?;
    let rate_numerator = timescale
        .checked_mul(u64::try_from(frame_count).map_err(|_| "frame count overflows".to_owned())?)
        .ok_or_else(|| "H.264 frame rate overflows".to_owned())?;
    let divisor = gcd_u64(rate_numerator, total_duration);
    let frame_rate = Rational::new(
        i64::try_from(rate_numerator / divisor)
            .map_err(|_| "H.264 frame-rate numerator exceeds i64".to_owned())?,
        i64::try_from(total_duration / divisor)
            .map_err(|_| "H.264 frame-rate denominator exceeds i64".to_owned())?,
    )
    .map_err(|error| error.to_string())?;

    let mut settings = current.clone();
    let mut width = sps.width;
    let mut height = sps.height;
    if matches!(track.rotation_degrees, 90 | 270) {
        std::mem::swap(&mut width, &mut height);
    }
    settings.width = width;
    settings.height = height;
    settings.frame_rate = frame_rate;
    settings.pixel_aspect = if let Some(aspect) = sps.vui.and_then(|vui| vui.aspect_ratio) {
        rotated_aspect(
            u32::from(aspect.width),
            u32::from(aspect.height),
            track.rotation_degrees,
        )
    } else if let Some(aspect) = track.pixel_aspect {
        rotated_aspect(
            aspect.horizontal_spacing,
            aspect.vertical_spacing,
            track.rotation_degrees,
        )
    } else {
        Rational::new(1, 1)
    }
    .map_err(|error| error.to_string())?;
    settings.scan_mode = if sps.frame_mbs_only {
        ProjectScanMode::Progressive
    } else {
        ProjectScanMode::Interlaced
    };
    let primaries = sps
        .vui
        .and_then(|vui| vui.colour_primaries)
        .map(u16::from)
        .or_else(|| track.colour.map(|colour| colour.primaries));
    settings.color_space = if primaries == Some(9) {
        ProjectColorSpace::Rec2020
    } else {
        ProjectColorSpace::Rec709
    };
    let audio = movie.tracks().iter().find(|candidate| {
        candidate.descriptor.codec.media_type == mmrecode_core::MediaType::Audio
            && candidate.channel_count.is_some()
            && candidate.sample_rate.is_some()
    });
    if let Some(audio) = audio {
        settings.audio_channels = audio.channel_count.expect("checked");
        settings.audio_sample_rate = audio.sample_rate.expect("checked");
    }
    Ok(MediaProbe {
        kind: MediaKind::new(mmrecode_h264::CODEC_NAME).map_err(|error| error.to_string())?,
        frame_rate,
        frame_count,
        project_settings: settings,
        has_audio_format: audio.is_some(),
    })
}

fn rotated_aspect(
    horizontal: u32,
    vertical: u32,
    rotation_degrees: i16,
) -> Result<Rational, mmrecode_core::Error> {
    let (numerator, denominator) = if matches!(rotation_degrees, 90 | 270) {
        (vertical, horizontal)
    } else {
        (horizontal, vertical)
    };
    Rational::new(i64::from(numerator), i64::from(denominator))
}

fn probe_mpeg2(
    _path: &Path,
    bytes: Vec<u8>,
    current: &ProjectSettings,
) -> Result<MediaProbe, String> {
    let (elementary, audio_format) =
        if bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE && bytes.first() == Some(&0x47) {
            let transport = mmrecode_mpegts::demux_transport_stream(&bytes)
                .map_err(|error| error.to_string())?;
            let audio_format = match transport.mpeg1_audio_bytes() {
                Ok(audio) => {
                    let frames = mmrecode_mpegaudio::parse_layer2_stream(&audio)
                        .map_err(|error| error.to_string())?;
                    frames
                        .first()
                        .map(|frame| (frame.header.sample_rate, u16::from(frame.header.channels)))
                }
                Err(_) => None,
            };
            (
                transport
                    .mpeg2_video_bytes()
                    .map_err(|error| error.to_string())?,
                audio_format,
            )
        } else {
            (bytes, None)
        };
    let stream = mmrecode_mpeg2::parse_stream(&elementary).map_err(|error| error.to_string())?;
    let sequence = &stream
        .pictures()
        .first()
        .ok_or_else(|| "focused MPEG-2 media contains no pictures".to_owned())?
        .sequence;
    let mut settings = current.clone();
    settings.width = u32::try_from(sequence.width)
        .map_err(|_| "focused video width exceeds project limits".to_owned())?;
    settings.height = u32::try_from(sequence.height)
        .map_err(|_| "focused video height exceeds project limits".to_owned())?;
    settings.frame_rate = sequence.frame_rate;
    let aspect_ratio_information = stream
        .sequence_headers()
        .first()
        .map_or(1, |header| header.aspect_ratio_information);
    settings.pixel_aspect = mpeg2_pixel_aspect(sequence, aspect_ratio_information)?;
    settings.scan_mode = if sequence.progressive_sequence {
        ProjectScanMode::Progressive
    } else {
        ProjectScanMode::Interlaced
    };
    settings.color_space = if sequence
        .display
        .and_then(|display| display.colour_description)
        .is_some_and(|colour| colour.colour_primaries == 9)
    {
        ProjectColorSpace::Rec2020
    } else {
        ProjectColorSpace::Rec709
    };
    if let Some((sample_rate, channels)) = audio_format {
        settings.audio_sample_rate = sample_rate;
        settings.audio_channels = channels;
    }
    Ok(MediaProbe {
        kind: MediaKind::new(mmrecode_mpeg2::CODEC_NAME).map_err(|error| error.to_string())?,
        frame_rate: sequence.frame_rate,
        frame_count: stream.pictures().len(),
        project_settings: settings,
        has_audio_format: audio_format.is_some(),
    })
}

fn mpeg2_pixel_aspect(
    sequence: &mmrecode_mpeg2::SequenceParameters,
    aspect_ratio_information: u8,
) -> Result<Rational, String> {
    let width = i64::try_from(sequence.width)
        .map_err(|_| "focused video width exceeds rational limits".to_owned())?;
    let height = i64::try_from(sequence.height)
        .map_err(|_| "focused video height exceeds rational limits".to_owned())?;
    let (display_width, display_height) = match aspect_ratio_information {
        1 => return Rational::new(1, 1).map_err(|error| error.to_string()),
        2 => (4_i64, 3_i64),
        3 => (16_i64, 9_i64),
        4 => (221_i64, 100_i64),
        _ => return Rational::new(1, 1).map_err(|error| error.to_string()),
    };
    let numerator = display_width
        .checked_mul(height)
        .ok_or_else(|| "focused video pixel aspect overflows".to_owned())?;
    let denominator = display_height
        .checked_mul(width)
        .ok_or_else(|| "focused video pixel aspect overflows".to_owned())?;
    let divisor = gcd_i64(numerator, denominator);
    Rational::new(numerator / divisor, denominator / divisor).map_err(|error| error.to_string())
}

fn gcd_u64(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

fn gcd_i64(mut left: i64, mut right: i64) -> i64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.abs().max(1)
}
