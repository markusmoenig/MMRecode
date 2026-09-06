//! Container and codec adaptation into the timeline's common PCM representation.

use mmrecode_core::{AudioFrame, FrameTiming, MediaType, Rational, Timestamp, TimestampRounding};
use mmrecode_isobmff::IsoBmffFile;

use crate::aac::decode_aac_track_native;

/// Demultiplexes and decodes an attached audio stream into timed, interleaved signed-16 PCM.
///
/// Container probing and codec dispatch are deliberately confined to this source boundary. A
/// recognized source with no audio returns `None`; an unrecognized byte stream also returns
/// `None`, allowing video-only elementary streams to use the same entry point.
///
/// The frame PTS is relative to the first video presentation timestamp when video is present, so
/// consumers can place the resulting PCM without knowing its original container or codec.
///
/// # Errors
///
/// Returns an error when a recognized container is malformed, carries unsupported audio, or its
/// audio cannot be reconstructed to the common PCM representation.
pub fn decode_audio_source(bytes: &[u8]) -> Result<Option<AudioFrame>, String> {
    if is_mpeg_ts(bytes) {
        return decode_mpegts_audio(bytes);
    }
    if is_iso_bmff(bytes) {
        return decode_isobmff_audio(bytes);
    }
    Ok(None)
}

fn is_mpeg_ts(bytes: &[u8]) -> bool {
    let (packets, _) = bytes.as_chunks::<{ mmrecode_mpegts::TS_PACKET_SIZE }>();
    !packets.is_empty()
        && packets
            .iter()
            .take(3)
            .all(|packet| packet.first() == Some(&0x47))
}

fn is_iso_bmff(bytes: &[u8]) -> bool {
    bytes.get(4..8).is_some_and(|kind| {
        matches!(
            kind,
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip"
        )
    })
}

fn decode_mpegts_audio(bytes: &[u8]) -> Result<Option<AudioFrame>, String> {
    let transport =
        mmrecode_mpegts::demux_transport_stream(bytes).map_err(|error| error.to_string())?;
    let Some(audio_stream) = transport
        .streams
        .iter()
        .find(|stream| stream.codec.codec_id.as_str() == "audio/mpeg1")
    else {
        if transport
            .streams
            .iter()
            .any(|stream| stream.codec.media_type == MediaType::Audio)
        {
            return Err("MPEG-TS audio is present but its codec is unsupported".into());
        }
        return Ok(None);
    };
    let audio_bytes = transport
        .mpeg1_audio_bytes()
        .map_err(|error| error.to_string())?;
    let mut audio = mmrecode_mpegaudio::decode_layer2_stream(&audio_bytes)
        .map_err(|error| error.to_string())?;
    let destination =
        Rational::new(1, i64::from(audio.sample_rate)).map_err(|error| error.to_string())?;
    let first_pts = |stream_id| {
        transport
            .elementary_packets
            .iter()
            .filter(|packet| packet.stream_id == stream_id)
            .filter_map(|packet| packet.pts)
            .min_by_key(|pts| pts.value)
    };
    let audio_start = first_pts(audio_stream.id)
        .map(|pts| pts.rescale(destination, TimestampRounding::NearestTiesAway))
        .transpose()
        .map_err(|error| error.to_string())?
        .map_or(0, |pts| pts.value);
    let video_start = transport
        .streams
        .iter()
        .find(|stream| stream.codec.media_type == MediaType::Video)
        .and_then(|stream| first_pts(stream.id))
        .map(|pts| pts.rescale(destination, TimestampRounding::NearestTiesAway))
        .transpose()
        .map_err(|error| error.to_string())?
        .map_or(0, |pts| pts.value);
    audio.timing.pts = Some(Timestamp {
        value: audio_start
            .checked_sub(video_start)
            .ok_or_else(|| "MPEG-TS A/V start offset overflows".to_owned())?,
        time_base: destination,
    });
    Ok(Some(audio))
}

fn decode_isobmff_audio(bytes: &[u8]) -> Result<Option<AudioFrame>, String> {
    let movie = IsoBmffFile::parse(bytes.to_vec()).map_err(|error| error.to_string())?;
    let Some((track_index, track)) = movie
        .tracks()
        .iter()
        .enumerate()
        .find(|(_, track)| track.descriptor.codec.codec_id.as_str() == "audio/aac")
    else {
        if movie
            .tracks()
            .iter()
            .any(|track| track.descriptor.codec.media_type == MediaType::Audio)
        {
            return Err("ISO-BMFF audio is present but is not supported AAC-LC".into());
        }
        return Ok(None);
    };
    let audio_time_base = track.descriptor.time_base;
    let audio_start = track
        .samples
        .iter()
        .map(|sample| sample.pts)
        .min()
        .ok_or_else(|| "AAC track has no samples".to_owned())?
        .max(0);
    let video_start = movie
        .tracks()
        .iter()
        .find(|track| track.descriptor.codec.media_type == MediaType::Video)
        .and_then(|track| {
            track
                .samples
                .iter()
                .map(|sample| sample.pts)
                .min()
                .map(|value| Timestamp {
                    value,
                    time_base: track.descriptor.time_base,
                })
        });
    let mut audio = decode_aac_track_native(movie, track_index)?;
    if !matches!(audio.channels, 1 | 2) {
        return Err(format!(
            "AAC audio has {} channels; timeline audio supports mono or stereo",
            audio.channels
        ));
    }
    let sample_time_base =
        Rational::new(1, i64::from(audio.sample_rate)).map_err(|error| error.to_string())?;
    let audio_start = Timestamp {
        value: audio_start,
        time_base: audio_time_base,
    }
    .rescale(sample_time_base, TimestampRounding::NearestTiesAway)
    .map_err(|error| error.to_string())?
    .value;
    let video_start = video_start
        .map(|timestamp| timestamp.rescale(sample_time_base, TimestampRounding::NearestTiesAway))
        .transpose()
        .map_err(|error| error.to_string())?
        .map_or(0, |timestamp| timestamp.value);
    let pts = audio_start
        .checked_sub(video_start)
        .ok_or_else(|| "ISO-BMFF A/V start offset overflows".to_owned())?;
    audio.timing = FrameTiming {
        pts: Some(Timestamp {
            value: pts,
            time_base: sample_time_base,
        }),
        duration: Some(Timestamp {
            value: i64::try_from(audio.samples_per_channel)
                .map_err(|_| "decoded audio duration exceeds i64".to_owned())?,
            time_base: sample_time_base,
        }),
    };
    audio.validate().map_err(|error| error.to_string())?;
    Ok(Some(audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPEG_TS: &[u8] =
        include_bytes!("../../../testdata/mpegts/valid/single-program-mpeg2-mp2.ts");

    #[test]
    fn ignores_unrecognized_and_elementary_video_sources() {
        assert!(
            decode_audio_source(b"not a media container")
                .unwrap()
                .is_none()
        );
        assert!(
            decode_audio_source(b"\0\0\x01\xb3elementary video")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_malformed_recognized_container() {
        let mut bytes = vec![0_u8; 16];
        bytes[4..8].copy_from_slice(b"ftyp");
        assert!(decode_audio_source(&bytes).is_err());
    }

    #[test]
    fn decodes_transport_audio_to_timed_pcm() {
        let audio = decode_audio_source(MPEG_TS).unwrap().unwrap();
        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.channels, 2);
        assert_eq!(
            audio.timing.pts.unwrap().time_base,
            Rational::new(1, 48_000).unwrap()
        );
        assert!(
            audio
                .samples
                .iter()
                .any(|sample| sample.unsigned_abs() > 100)
        );
        audio.validate().unwrap();
    }
}
