use std::cmp::Ordering;

use mmrecode_core::{Error, MediaType, Packet, PacketFlags, Result, Timestamp};

use crate::{ColourInformation, PixelAspectRatio, Track};

/// One track and its decode-ordered packets supplied to [`mux_tracks`].
#[derive(Clone, Copy, Debug)]
pub struct TrackMuxInput<'a> {
    /// Track metadata and codec configuration.
    pub track: &'a Track,
    /// Complete packet sequence for this track.
    pub packets: &'a [Packet],
    /// Optional media trim represented by a single rate-1 edit-list entry.
    pub edit: Option<TrackMuxEdit>,
}

/// One rate-1 MP4 media edit, expressed in the track's media timescale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackMuxEdit {
    /// Leading decoded media ticks skipped before presentation starts.
    pub media_time: u64,
    /// Presented duration after the leading trim.
    pub presentation_duration: u64,
}

#[derive(Debug)]
struct PreparedTrack {
    timescale: u32,
    duration: u64,
    durations: Vec<u32>,
    composition_offsets: Vec<i64>,
    sample_sizes: Vec<u32>,
}

/// Muxes one already-timed video track into a non-fragmented Fast Start MP4 file.
///
/// Encoded packet payloads and the opaque sample-entry configuration are copied verbatim. Packets
/// must be in decode order, use the track time base, and have non-negative monotonically increasing
/// DTS values. The movie metadata precedes the media payload so HTTP playback can initialize
/// without reading the end of the file. Use [`mux_tracks`] for AAC audio and a leading media trim.
/// Multiple sample descriptions and files larger than 4 GiB remain outside this packet-copy writer.
///
/// # Errors
///
/// Returns an error for missing video metadata, inconsistent timing, unsupported codec tags, empty
/// input, or output that cannot be represented by this minimal writer.
pub fn mux_video_track(track: &Track, packets: &[Packet]) -> Result<Vec<u8>> {
    mux_tracks(&[TrackMuxInput {
        track,
        packets,
        edit: None,
    }])
}

/// Muxes complete H.264 video and/or AAC audio tracks into one Fast Start MP4 file.
///
/// Samples are physically interleaved by decode time and represented as one-sample chunks. Each
/// track retains its own exact media timescale, while movie and track-header durations use the
/// least common timescale when it fits in 32 bits. An optional single rate-1 edit can trim leading
/// codec priming while retaining the encoded preroll sample in the media timeline.
///
/// # Errors
///
/// Returns an error for empty or duplicate tracks, unsupported sample entries, inconsistent
/// packet timing, or output that exceeds the writer's 32-bit box/chunk limits.
pub fn mux_tracks(inputs: &[TrackMuxInput<'_>]) -> Result<Vec<u8>> {
    if inputs.is_empty() {
        return Err(Error::InvalidData(
            "cannot mux an MP4 without tracks".into(),
        ));
    }
    for (index, input) in inputs.iter().enumerate() {
        validate(input.track, input.packets)?;
        if inputs[..index].iter().any(|previous| {
            previous.track.track_id == input.track.track_id
                || previous.track.descriptor.id == input.track.descriptor.id
        }) {
            return Err(Error::InvalidData(
                "MP4 tracks require unique track and stream identifiers".into(),
            ));
        }
    }

    let ftyp = atom(*b"ftyp", {
        let mut payload = Vec::new();
        payload.extend(*b"isom");
        payload.extend(512_u32.to_be_bytes());
        for brand in [*b"isom", *b"iso2", *b"avc1", *b"mp41"] {
            payload.extend(brand);
        }
        payload
    })?;

    let prepared = inputs
        .iter()
        .map(prepare_track)
        .collect::<Result<Vec<_>>>()?;
    let movie_timescale = common_timescale(&prepared)?;
    let provisional_offsets = inputs
        .iter()
        .map(|input| vec![0; input.packets.len()])
        .collect::<Vec<_>>();
    let provisional_moov = movie_box(inputs, &prepared, &provisional_offsets, movie_timescale)?;
    let data_start = u32::try_from(ftyp.len() + provisional_moov.len() + 8)
        .map_err(|_| Error::Unsupported("MP4 chunk offset does not fit in u32".into()))?;
    let mut packet_order = inputs
        .iter()
        .enumerate()
        .flat_map(|(track_index, input)| {
            (0..input.packets.len()).map(move |sample_index| (track_index, sample_index))
        })
        .collect::<Vec<_>>();
    packet_order.sort_by(|&(left_track, left_sample), &(right_track, right_sample)| {
        compare_packet_time(
            &inputs[left_track].packets[left_sample],
            prepared[left_track].timescale,
            &inputs[right_track].packets[right_sample],
            prepared[right_track].timescale,
        )
        .then_with(|| left_track.cmp(&right_track))
        .then_with(|| left_sample.cmp(&right_sample))
    });
    let mut chunk_offsets = provisional_offsets;
    let mut media_payload = Vec::new();
    let mut offset = u64::from(data_start);
    for &(track_index, sample_index) in &packet_order {
        chunk_offsets[track_index][sample_index] = u32::try_from(offset)
            .map_err(|_| Error::Unsupported("MP4 chunk offset does not fit in u32".into()))?;
        let data = &inputs[track_index].packets[sample_index].data;
        media_payload.extend_from_slice(data);
        offset = offset
            .checked_add(
                u64::try_from(data.len())
                    .map_err(|_| Error::Unsupported("MP4 sample length exceeds u64".into()))?,
            )
            .ok_or_else(|| Error::Unsupported("MP4 media size overflows".into()))?;
    }
    let moov = movie_box(inputs, &prepared, &chunk_offsets, movie_timescale)?;
    debug_assert_eq!(moov.len(), provisional_moov.len());
    let mdat = atom(*b"mdat", media_payload)?;
    Ok([ftyp, moov, mdat].concat())
}

fn movie_box(
    inputs: &[TrackMuxInput<'_>],
    prepared: &[PreparedTrack],
    chunk_offsets: &[Vec<u32>],
    movie_timescale: u32,
) -> Result<Vec<u8>> {
    let movie_duration = inputs
        .iter()
        .zip(prepared)
        .map(|(input, track)| {
            rescale_duration(
                input
                    .edit
                    .map_or(track.duration, |edit| edit.presentation_duration),
                track.timescale,
                movie_timescale,
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let mut track_boxes = Vec::with_capacity(inputs.len());
    for (index, input) in inputs.iter().enumerate() {
        let track = &prepared[index];
        let stbl = sample_table(
            input.track,
            &track.durations,
            &track.composition_offsets,
            &track.sample_sizes,
            &chunk_offsets[index],
            input.packets,
        )?;
        let media_header_box = match input.track.descriptor.codec.media_type {
            MediaType::Video => full_box(*b"vmhd", 0, 1, vec![0; 8])?,
            MediaType::Audio => full_box(*b"smhd", 0, 0, vec![0; 4])?,
            _ => unreachable!("validated media type"),
        };
        let minf = atom(
            *b"minf",
            [media_header_box, data_information()?, atom(*b"stbl", stbl)?].concat(),
        )?;
        let media_box = atom(
            *b"mdia",
            [
                media_header(track.timescale, track.duration)?,
                handler(input.track.descriptor.codec.media_type)?,
                minf,
            ]
            .concat(),
        )?;
        if let Some(edit) = input.edit {
            let edit_end = edit
                .media_time
                .checked_add(edit.presentation_duration)
                .ok_or_else(|| Error::InvalidData("MP4 track edit overflows".into()))?;
            if edit.presentation_duration == 0 || edit_end > track.duration {
                return Err(Error::InvalidData(
                    "MP4 track edit lies outside the encoded media duration".into(),
                ));
            }
        }
        let presented_duration = input
            .edit
            .map_or(track.duration, |edit| edit.presentation_duration);
        let header_duration =
            rescale_duration(presented_duration, track.timescale, movie_timescale)?;
        let mut track_payload = track_header(input.track, header_duration)?;
        if let Some(edit) = input.edit {
            track_payload.extend(edit_box(edit, track.timescale, movie_timescale)?);
        }
        track_payload.extend(media_box);
        track_boxes.push(atom(*b"trak", track_payload)?);
    }
    let next_track_id = inputs
        .iter()
        .map(|input| input.track.track_id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| Error::Unsupported("MP4 next track identifier overflows".into()))?;
    let mut payload = movie_header(movie_timescale, movie_duration, next_track_id)?;
    payload.extend(track_boxes.concat());
    atom(*b"moov", payload)
}

fn prepare_track(input: &TrackMuxInput<'_>) -> Result<PreparedTrack> {
    let timescale = u32::try_from(input.track.descriptor.time_base.denominator())
        .map_err(|_| Error::Unsupported("MP4 timescale does not fit in u32".into()))?;
    let durations = input
        .packets
        .iter()
        .map(packet_duration)
        .collect::<Result<Vec<_>>>()?;
    let composition_offsets = input
        .packets
        .iter()
        .map(|packet| {
            packet_pts(packet)?
                .value
                .checked_sub(packet_dts(packet)?.value)
                .ok_or_else(|| Error::InvalidData("packet composition offset overflows".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let sample_sizes = input
        .packets
        .iter()
        .map(|packet| {
            u32::try_from(packet.data.len()).map_err(|_| {
                Error::Unsupported(
                    "one encoded sample is larger than the MP4 writer supports".into(),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let duration = input
        .packets
        .iter()
        .zip(&durations)
        .map(|(packet, &sample_duration)| {
            let decode_end = packet_dts(packet)?
                .value
                .checked_add(i64::from(sample_duration))
                .ok_or_else(|| Error::InvalidData("track duration overflows".into()))?;
            let presentation_end = packet_pts(packet)?
                .value
                .checked_add(i64::from(sample_duration))
                .ok_or_else(|| Error::InvalidData("track duration overflows".into()))?;
            u64::try_from(decode_end.max(presentation_end))
                .map_err(|_| Error::InvalidData("track duration may not be negative".into()))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    Ok(PreparedTrack {
        timescale,
        duration,
        durations,
        composition_offsets,
        sample_sizes,
    })
}

fn compare_packet_time(
    left: &Packet,
    left_timescale: u32,
    right: &Packet,
    right_timescale: u32,
) -> Ordering {
    let left = i128::from(packet_dts(left).expect("validated packet DTS").value)
        * i128::from(right_timescale);
    let right = i128::from(packet_dts(right).expect("validated packet DTS").value)
        * i128::from(left_timescale);
    left.cmp(&right)
}

fn common_timescale(tracks: &[PreparedTrack]) -> Result<u32> {
    tracks.iter().try_fold(1_u32, |common, track| {
        let divisor = gcd(common, track.timescale);
        common
            .checked_div(divisor)
            .and_then(|value| value.checked_mul(track.timescale))
            .ok_or_else(|| Error::Unsupported("MP4 common movie timescale overflows u32".into()))
    })
}

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn rescale_duration(value: u64, source: u32, destination: u32) -> Result<u64> {
    let numerator = u128::from(value)
        .checked_mul(u128::from(destination))
        .ok_or_else(|| Error::Unsupported("MP4 duration rescale overflows".into()))?;
    u64::try_from(numerator.div_ceil(u128::from(source)))
        .map_err(|_| Error::Unsupported("MP4 duration exceeds u64".into()))
}

fn edit_box(edit: TrackMuxEdit, track_timescale: u32, movie_timescale: u32) -> Result<Vec<u8>> {
    let segment_duration =
        rescale_duration(edit.presentation_duration, track_timescale, movie_timescale)?;
    let media_time = i64::try_from(edit.media_time)
        .map_err(|_| Error::Unsupported("MP4 edit media time exceeds i64".into()))?;
    let mut payload = Vec::new();
    payload.extend(1_u32.to_be_bytes());
    payload.extend(segment_duration.to_be_bytes());
    payload.extend(media_time.to_be_bytes());
    payload.extend(1_i16.to_be_bytes());
    payload.extend(0_i16.to_be_bytes());
    atom(*b"edts", full_box(*b"elst", 1, 0, payload)?)
}

#[allow(clippy::too_many_lines)]
fn validate(track: &Track, packets: &[Packet]) -> Result<()> {
    if packets.is_empty() {
        return Err(Error::InvalidData("cannot mux an empty track".into()));
    }
    if track.descriptor.time_base.numerator() != 1 || track.descriptor.time_base.denominator() <= 0
    {
        return Err(Error::Unsupported(
            "MP4 muxing requires a positive 1/timescale packet time base".into(),
        ));
    }
    let tag = track
        .descriptor
        .codec
        .codec_tag
        .ok_or_else(|| Error::InvalidData("MP4 track has no sample-entry codec tag".into()))?;
    match track.descriptor.codec.media_type {
        MediaType::Video => {
            if track.handler_type.0 != *b"vide" || track.width.is_none() || track.height.is_none() {
                return Err(Error::InvalidData(
                    "MP4 video track requires a vide handler and coded dimensions".into(),
                ));
            }
            if tag.0 != *b"avc1" && tag.0 != *b"avc3" {
                return Err(Error::Unsupported(format!(
                    "MP4 video muxer does not support sample entry {}",
                    String::from_utf8_lossy(&tag.0)
                )));
            }
            if track.descriptor.codec.configuration.is_empty() {
                return Err(Error::InvalidData(
                    "H.264 video track has no avcC configuration".into(),
                ));
            }
        }
        MediaType::Audio => {
            if track.handler_type.0 != *b"soun" {
                return Err(Error::InvalidData(
                    "MP4 audio track requires a soun handler".into(),
                ));
            }
            if tag.0 != *b"mp4a" || track.descriptor.codec.codec_id.as_str() != "audio/aac" {
                return Err(Error::Unsupported(format!(
                    "MP4 audio muxer does not support sample entry {}",
                    String::from_utf8_lossy(&tag.0)
                )));
            }
            let channels = track
                .channel_count
                .ok_or_else(|| Error::InvalidData("AAC track has no channel count".into()))?;
            let sample_rate = track
                .sample_rate
                .ok_or_else(|| Error::InvalidData("AAC track has no sample rate".into()))?;
            if channels == 0 || sample_rate == 0 {
                return Err(Error::InvalidData(
                    "AAC channel count and sample rate must be positive".into(),
                ));
            }
            if i64::from(sample_rate) != track.descriptor.time_base.denominator() {
                return Err(Error::Unsupported(
                    "AAC MP4 track time base must be 1/sample_rate".into(),
                ));
            }
            if track.descriptor.codec.configuration.is_empty() {
                return Err(Error::InvalidData(
                    "AAC audio track has no AudioSpecificConfig".into(),
                ));
            }
        }
        _ => {
            return Err(Error::Unsupported(
                "MP4 muxing currently supports only H.264 video and AAC audio".into(),
            ));
        }
    }
    let mut previous_dts = None;
    for packet in packets {
        if packet.stream_id != track.descriptor.id {
            return Err(Error::InvalidData(
                "packet belongs to a different stream".into(),
            ));
        }
        for timing in [packet.pts, packet.dts, packet.duration]
            .into_iter()
            .flatten()
        {
            if timing.time_base != track.descriptor.time_base {
                return Err(Error::InvalidData(
                    "packet time base differs from its track".into(),
                ));
            }
        }
        let dts = packet_dts(packet)?.value;
        if dts < 0 || previous_dts.is_some_and(|previous| dts < previous) {
            return Err(Error::InvalidData(
                "MP4 packet DTS values must be non-negative and monotonic".into(),
            ));
        }
        previous_dts = Some(dts);
        let _ = packet_pts(packet)?;
        let _ = packet_duration(packet)?;
    }
    Ok(())
}

fn sample_table(
    track: &Track,
    durations: &[u32],
    composition_offsets: &[i64],
    sizes: &[u32],
    chunk_offsets: &[u32],
    packets: &[Packet],
) -> Result<Vec<u8>> {
    let sample_count = u32::try_from(packets.len())
        .map_err(|_| Error::Unsupported("too many MP4 samples".into()))?;
    let mut stsd_payload = vec![0; 4];
    stsd_payload.extend(1_u32.to_be_bytes());
    stsd_payload.extend(sample_entry(track)?);

    let mut tables = vec![atom(*b"stsd", stsd_payload)?, stts(durations)?];
    if composition_offsets.iter().any(|&offset| offset != 0) {
        tables.push(ctts(composition_offsets)?);
    }
    tables.push(full_table3(*b"stsc", &[(1, 1, 1)])?);

    let mut sample_size_payload = vec![0; 4];
    sample_size_payload.extend(0_u32.to_be_bytes());
    sample_size_payload.extend(sample_count.to_be_bytes());
    for &size in sizes {
        sample_size_payload.extend(size.to_be_bytes());
    }
    tables.push(atom(*b"stsz", sample_size_payload)?);
    if chunk_offsets.len() != packets.len() {
        return Err(Error::InvalidData(
            "MP4 chunk offset count differs from sample count".into(),
        ));
    }
    tables.push(full_values(*b"stco", chunk_offsets)?);
    let sync_samples = packets
        .iter()
        .enumerate()
        .filter(|(_, packet)| packet.flags.contains(PacketFlags::KEY))
        .map(|(index, _)| {
            u32::try_from(index + 1).map_err(|_| Error::Unsupported("too many MP4 samples".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    if track.descriptor.codec.media_type == MediaType::Video && !sync_samples.is_empty() {
        tables.push(full_values(*b"stss", &sync_samples)?);
    }
    Ok(tables.concat())
}

fn sample_entry(track: &Track) -> Result<Vec<u8>> {
    match track.descriptor.codec.media_type {
        MediaType::Video => visual_sample_entry(track),
        MediaType::Audio => audio_sample_entry(track),
        _ => unreachable!("validated media type"),
    }
}

fn visual_sample_entry(track: &Track) -> Result<Vec<u8>> {
    let width = u16::try_from(track.width.expect("validated width"))
        .map_err(|_| Error::Unsupported("video width exceeds MP4 visual sample entry".into()))?;
    let height = u16::try_from(track.height.expect("validated height"))
        .map_err(|_| Error::Unsupported("video height exceeds MP4 visual sample entry".into()))?;
    let mut payload = vec![0; 6];
    payload.extend(1_u16.to_be_bytes());
    payload.extend([0; 16]);
    payload.extend(width.to_be_bytes());
    payload.extend(height.to_be_bytes());
    payload.extend(0x0048_0000_u32.to_be_bytes());
    payload.extend(0x0048_0000_u32.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.extend(1_u16.to_be_bytes());
    payload.extend([0; 32]);
    payload.extend(0x0018_u16.to_be_bytes());
    payload.extend(u16::MAX.to_be_bytes());
    payload.extend(atom(
        *b"avcC",
        track.descriptor.codec.configuration.clone(),
    )?);
    if let Some(pixel_aspect) = track.pixel_aspect {
        payload.extend(pixel_aspect_box(pixel_aspect)?);
    }
    if let Some(colour) = track.colour {
        payload.extend(colour_box(colour)?);
    }
    atom(
        track.descriptor.codec.codec_tag.expect("validated tag").0,
        payload,
    )
}

fn audio_sample_entry(track: &Track) -> Result<Vec<u8>> {
    let channels = track.channel_count.expect("validated channel count");
    let sample_rate = track.sample_rate.expect("validated sample rate");
    let fixed_rate = sample_rate
        .checked_shl(16)
        .ok_or_else(|| Error::Unsupported("audio sample rate exceeds 16.16 fixed point".into()))?;
    let mut payload = vec![0; 6];
    payload.extend(1_u16.to_be_bytes());
    payload.extend(0_u16.to_be_bytes());
    payload.extend(0_u16.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.extend(channels.to_be_bytes());
    payload.extend(16_u16.to_be_bytes());
    payload.extend(0_i16.to_be_bytes());
    payload.extend(0_u16.to_be_bytes());
    payload.extend(fixed_rate.to_be_bytes());
    payload.extend(esds(&track.descriptor.codec.configuration)?);
    atom(*b"mp4a", payload)
}

fn esds(configuration: &[u8]) -> Result<Vec<u8>> {
    let decoder_specific = descriptor(0x05, configuration.to_vec())?;
    let mut decoder_config = vec![0x40, 0x15, 0, 0, 0];
    decoder_config.extend(0_u32.to_be_bytes());
    decoder_config.extend(0_u32.to_be_bytes());
    decoder_config.extend(decoder_specific);
    let decoder_config = descriptor(0x04, decoder_config)?;
    let sl_config = descriptor(0x06, vec![0x02])?;
    let mut elementary_stream = 1_u16.to_be_bytes().to_vec();
    elementary_stream.push(0);
    elementary_stream.extend(decoder_config);
    elementary_stream.extend(sl_config);
    full_box(*b"esds", 0, 0, descriptor(0x03, elementary_stream)?)
}

fn descriptor(tag: u8, payload: Vec<u8>) -> Result<Vec<u8>> {
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Unsupported("MPEG-4 descriptor exceeds u32".into()))?;
    if length > 0x0fff_ffff {
        return Err(Error::Unsupported(
            "MPEG-4 descriptor exceeds 28-bit length".into(),
        ));
    }
    let mut output = vec![tag];
    let mut started = false;
    for shift in [21, 14, 7, 0] {
        let value = u8::try_from((length >> shift) & 0x7f).expect("seven-bit descriptor group");
        if value != 0 || started || shift == 0 {
            started = true;
            output.push(value | if shift == 0 { 0 } else { 0x80 });
        }
    }
    output.extend(payload);
    Ok(output)
}

fn pixel_aspect_box(value: PixelAspectRatio) -> Result<Vec<u8>> {
    atom(
        *b"pasp",
        [
            value.horizontal_spacing.to_be_bytes(),
            value.vertical_spacing.to_be_bytes(),
        ]
        .concat(),
    )
}

fn colour_box(value: ColourInformation) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend(if value.full_range.is_some() {
        *b"nclx"
    } else {
        *b"nclc"
    });
    payload.extend(value.primaries.to_be_bytes());
    payload.extend(value.transfer_characteristics.to_be_bytes());
    payload.extend(value.matrix_coefficients.to_be_bytes());
    if let Some(full_range) = value.full_range {
        payload.push(if full_range { 0x80 } else { 0 });
    }
    atom(*b"colr", payload)
}

fn stts(durations: &[u32]) -> Result<Vec<u8>> {
    full_table(*b"stts", &runs(durations))
}

fn ctts(offsets: &[i64]) -> Result<Vec<u8>> {
    let signed = offsets.iter().any(|&offset| offset < 0);
    let mut payload = vec![u8::from(signed), 0, 0, 0];
    let entries = runs(offsets);
    payload.extend(
        u32::try_from(entries.len())
            .map_err(|_| Error::Unsupported("too many MP4 timing runs".into()))?
            .to_be_bytes(),
    );
    for (count, offset) in entries {
        payload.extend(count.to_be_bytes());
        if signed {
            payload.extend(
                i32::try_from(offset)
                    .map_err(|_| {
                        Error::Unsupported("signed MP4 composition offset overflows".into())
                    })?
                    .to_be_bytes(),
            );
        } else {
            payload.extend(
                u32::try_from(offset)
                    .map_err(|_| Error::Unsupported("MP4 composition offset overflows".into()))?
                    .to_be_bytes(),
            );
        }
    }
    atom(*b"ctts", payload)
}

fn runs<T: Copy + Eq>(values: &[T]) -> Vec<(u32, T)> {
    let mut output: Vec<(u32, T)> = Vec::new();
    for &value in values {
        if let Some((count, previous)) = output.last_mut()
            && *previous == value
        {
            *count += 1;
        } else {
            output.push((1, value));
        }
    }
    output
}

fn movie_header(timescale: u32, duration: u64, next_track_id: u32) -> Result<Vec<u8>> {
    let mut payload = vec![0; 16];
    payload.extend(timescale.to_be_bytes());
    payload.extend(duration.to_be_bytes());
    payload.extend(0x0001_0000_u32.to_be_bytes());
    payload.extend(0x0100_u16.to_be_bytes());
    payload.extend([0; 10]);
    payload.extend(identity_matrix());
    payload.extend([0; 24]);
    payload.extend(next_track_id.to_be_bytes());
    full_box(*b"mvhd", 1, 0, payload)
}

fn track_header(track: &Track, duration: u64) -> Result<Vec<u8>> {
    let mut payload = vec![0; 16];
    payload.extend(track.track_id.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.extend(duration.to_be_bytes());
    payload.extend([0; 8]);
    payload.extend(0_i16.to_be_bytes());
    payload.extend(0_i16.to_be_bytes());
    payload.extend(
        if track.descriptor.codec.media_type == MediaType::Audio {
            0x0100_u16
        } else {
            0
        }
        .to_be_bytes(),
    );
    payload.extend(0_u16.to_be_bytes());
    payload.extend(rotation_matrix(track.rotation_degrees));
    let width = track.width.unwrap_or(0);
    let height = track.height.unwrap_or(0);
    payload.extend(
        width
            .checked_shl(16)
            .ok_or_else(|| Error::Unsupported("track width overflows 16.16 fixed point".into()))?
            .to_be_bytes(),
    );
    payload.extend(
        height
            .checked_shl(16)
            .ok_or_else(|| Error::Unsupported("track height overflows 16.16 fixed point".into()))?
            .to_be_bytes(),
    );
    full_box(*b"tkhd", 1, 7, payload)
}

fn media_header(timescale: u32, duration: u64) -> Result<Vec<u8>> {
    let mut payload = vec![0; 16];
    payload.extend(timescale.to_be_bytes());
    payload.extend(duration.to_be_bytes());
    payload.extend(0x55c4_u16.to_be_bytes());
    payload.extend(0_u16.to_be_bytes());
    full_box(*b"mdhd", 1, 0, payload)
}

fn handler(media_type: MediaType) -> Result<Vec<u8>> {
    let mut payload = vec![0; 4];
    payload.extend(match media_type {
        MediaType::Video => *b"vide",
        MediaType::Audio => *b"soun",
        _ => unreachable!("validated media type"),
    });
    payload.extend([0; 12]);
    payload.extend(match media_type {
        MediaType::Video => b"MMRecode Video\0".as_slice(),
        MediaType::Audio => b"MMRecode Audio\0".as_slice(),
        _ => unreachable!("validated media type"),
    });
    full_box(*b"hdlr", 0, 0, payload)
}

fn data_information() -> Result<Vec<u8>> {
    let url = full_box(*b"url ", 0, 1, Vec::new())?;
    let mut dref_payload = Vec::new();
    dref_payload.extend(1_u32.to_be_bytes());
    dref_payload.extend(url);
    let dref = full_box(*b"dref", 0, 0, dref_payload)?;
    atom(*b"dinf", dref)
}

fn identity_matrix() -> [u8; 36] {
    matrix_bytes(0x0001_0000, 0, 0, 0x0001_0000)
}

fn rotation_matrix(degrees: i16) -> [u8; 36] {
    match degrees.rem_euclid(360) {
        90 => matrix_bytes(0, 0x0001_0000, -0x0001_0000, 0),
        180 => matrix_bytes(-0x0001_0000, 0, 0, -0x0001_0000),
        270 => matrix_bytes(0, -0x0001_0000, 0x0001_0000, 0),
        _ => identity_matrix(),
    }
}

fn matrix_bytes(a: i32, b: i32, c: i32, d: i32) -> [u8; 36] {
    let values = [a, b, 0, c, d, 0, 0, 0, 0x4000_0000];
    let mut bytes = [0; 36];
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    bytes
}

fn packet_pts(packet: &Packet) -> Result<Timestamp> {
    packet
        .pts
        .ok_or_else(|| Error::InvalidData("MP4 packet has no PTS".into()))
}

fn packet_dts(packet: &Packet) -> Result<Timestamp> {
    packet
        .dts
        .ok_or_else(|| Error::InvalidData("MP4 packet has no DTS".into()))
}

fn packet_duration(packet: &Packet) -> Result<u32> {
    let duration = packet
        .duration
        .ok_or_else(|| Error::InvalidData("MP4 packet has no duration".into()))?
        .value;
    u32::try_from(duration)
        .map_err(|_| Error::InvalidData("MP4 packet duration must fit in positive u32".into()))
        .and_then(|duration| {
            if duration == 0 {
                Err(Error::InvalidData(
                    "MP4 packet duration may not be zero".into(),
                ))
            } else {
                Ok(duration)
            }
        })
}

fn full_table(kind: [u8; 4], entries: &[(u32, u32)]) -> Result<Vec<u8>> {
    let mut payload = vec![0; 4];
    payload.extend(
        u32::try_from(entries.len())
            .map_err(|_| Error::Unsupported("too many MP4 table entries".into()))?
            .to_be_bytes(),
    );
    for &(left, right) in entries {
        payload.extend(left.to_be_bytes());
        payload.extend(right.to_be_bytes());
    }
    atom(kind, payload)
}

fn full_table3(kind: [u8; 4], entries: &[(u32, u32, u32)]) -> Result<Vec<u8>> {
    let mut payload = vec![0; 4];
    payload.extend(
        u32::try_from(entries.len())
            .map_err(|_| Error::Unsupported("too many MP4 table entries".into()))?
            .to_be_bytes(),
    );
    for &(a, b, c) in entries {
        payload.extend(a.to_be_bytes());
        payload.extend(b.to_be_bytes());
        payload.extend(c.to_be_bytes());
    }
    atom(kind, payload)
}

fn full_values(kind: [u8; 4], entries: &[u32]) -> Result<Vec<u8>> {
    let mut payload = vec![0; 4];
    payload.extend(
        u32::try_from(entries.len())
            .map_err(|_| Error::Unsupported("too many MP4 table entries".into()))?
            .to_be_bytes(),
    );
    for &entry in entries {
        payload.extend(entry.to_be_bytes());
    }
    atom(kind, payload)
}

fn full_box(kind: [u8; 4], version: u8, flags: u32, mut payload: Vec<u8>) -> Result<Vec<u8>> {
    let mut header = vec![version];
    let flag_bytes = flags.to_be_bytes();
    header.extend_from_slice(&flag_bytes[1..]);
    header.append(&mut payload);
    atom(kind, header)
}

fn atom(kind: [u8; 4], payload: Vec<u8>) -> Result<Vec<u8>> {
    let size = u32::try_from(payload.len().saturating_add(8))
        .map_err(|_| Error::Unsupported("MP4 box exceeds 32-bit size".into()))?;
    let mut output = Vec::with_capacity(payload.len() + 8);
    output.extend(size.to_be_bytes());
    output.extend(kind);
    output.extend(payload);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{
        CodecDescriptor, CodecId, Demuxer, FourCc, MediaType, Packet, PacketFlags, Rational,
        StreamDescriptor, StreamId, Timestamp,
    };

    use super::{TrackMuxEdit, TrackMuxInput, mux_tracks, mux_video_track};
    use crate::{IsoBmffFile, Track};

    #[test]
    fn round_trips_one_video_track_and_composition_offsets() {
        let time_base = Rational::new(1, 1000).unwrap();
        let descriptor = StreamDescriptor {
            id: StreamId(0),
            codec: CodecDescriptor {
                codec_id: CodecId::new("video/h264"),
                codec_tag: Some(FourCc(*b"avc1")),
                media_type: MediaType::Video,
                configuration: vec![1, 66, 0, 30, 0xff, 0xe0, 0],
            },
            time_base,
        };
        let track = Track {
            descriptor,
            track_id: 1,
            handler_type: FourCc(*b"vide"),
            width: Some(320),
            height: Some(180),
            pixel_aspect: None,
            colour: None,
            rotation_degrees: 90,
            channel_count: None,
            sample_rate: None,
            presentation_duration: None,
            samples: Vec::new(),
        };
        let packets = [(0, 40, true), (40, 80, false), (80, 40, false)]
            .into_iter()
            .map(|(dts, pts, key)| Packet {
                stream_id: StreamId(0),
                data: vec![0, 0, 0, 1, 9],
                pts: Some(Timestamp {
                    value: pts,
                    time_base,
                }),
                dts: Some(Timestamp {
                    value: dts,
                    time_base,
                }),
                duration: Some(Timestamp {
                    value: 40,
                    time_base,
                }),
                flags: if key {
                    PacketFlags::KEY
                } else {
                    PacketFlags::empty()
                },
                side_data: Vec::new(),
            })
            .collect::<Vec<_>>();
        let output = mux_video_track(&track, &packets).unwrap();
        assert_eq!(&output[4..8], b"ftyp");
        let ftyp_size =
            usize::try_from(u32::from_be_bytes(output[..4].try_into().unwrap())).unwrap();
        assert_eq!(&output[ftyp_size + 4..ftyp_size + 8], b"moov");
        let moov_size = usize::try_from(u32::from_be_bytes(
            output[ftyp_size..ftyp_size + 4].try_into().unwrap(),
        ))
        .unwrap();
        assert_eq!(
            &output[ftyp_size + moov_size + 4..ftyp_size + moov_size + 8],
            b"mdat"
        );
        let parsed = IsoBmffFile::parse(output).unwrap();
        let parsed_track = parsed.h264_track().unwrap();
        assert_eq!(parsed_track.rotation_degrees, 90);
        assert_eq!(parsed_track.samples.len(), 3);
        assert_eq!(parsed_track.samples[0].pts, 40);
        assert_eq!(parsed_track.samples[1].pts, 80);
        assert_eq!(parsed_track.samples[2].pts, 40);
        assert!(parsed_track.samples[0].is_sync);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn interleaves_h264_and_aac_tracks_and_round_trips_esds() {
        let video_time_base = Rational::new(1, 1_000).unwrap();
        let audio_time_base = Rational::new(1, 48_000).unwrap();
        let video_track = Track {
            descriptor: StreamDescriptor {
                id: StreamId(0),
                codec: CodecDescriptor {
                    codec_id: CodecId::new("video/h264"),
                    codec_tag: Some(FourCc(*b"avc1")),
                    media_type: MediaType::Video,
                    configuration: vec![1, 66, 0, 30, 0xff, 0xe0, 0],
                },
                time_base: video_time_base,
            },
            track_id: 1,
            handler_type: FourCc(*b"vide"),
            width: Some(16),
            height: Some(16),
            pixel_aspect: None,
            colour: None,
            rotation_degrees: 0,
            channel_count: None,
            sample_rate: None,
            presentation_duration: None,
            samples: Vec::new(),
        };
        let audio_track = Track {
            descriptor: StreamDescriptor {
                id: StreamId(1),
                codec: CodecDescriptor {
                    codec_id: CodecId::new("audio/aac"),
                    codec_tag: Some(FourCc(*b"mp4a")),
                    media_type: MediaType::Audio,
                    configuration: vec![0x11, 0x90],
                },
                time_base: audio_time_base,
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
            presentation_duration: None,
            samples: Vec::new(),
        };
        let video_packets = [0, 40]
            .into_iter()
            .map(|timestamp| Packet {
                stream_id: StreamId(0),
                data: vec![0, 0, 0, 1, 9],
                pts: Some(Timestamp {
                    value: timestamp,
                    time_base: video_time_base,
                }),
                dts: Some(Timestamp {
                    value: timestamp,
                    time_base: video_time_base,
                }),
                duration: Some(Timestamp {
                    value: 40,
                    time_base: video_time_base,
                }),
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .collect::<Vec<_>>();
        let audio_packets = [0, 1_024, 2_048]
            .into_iter()
            .map(|timestamp| Packet {
                stream_id: StreamId(1),
                data: vec![0x21, 0x00, 0x03, 0x40, 0x68, 0x1c],
                pts: Some(Timestamp {
                    value: timestamp,
                    time_base: audio_time_base,
                }),
                dts: Some(Timestamp {
                    value: timestamp,
                    time_base: audio_time_base,
                }),
                duration: Some(Timestamp {
                    value: 1_024,
                    time_base: audio_time_base,
                }),
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .collect::<Vec<_>>();

        let output = mux_tracks(&[
            TrackMuxInput {
                track: &video_track,
                packets: &video_packets,
                edit: None,
            },
            TrackMuxInput {
                track: &audio_track,
                packets: &audio_packets,
                edit: Some(TrackMuxEdit {
                    media_time: 1_024,
                    presentation_duration: 2_048,
                }),
            },
        ])
        .unwrap();
        let mut parsed = IsoBmffFile::parse(output).unwrap();
        assert_eq!(parsed.tracks().len(), 2);
        let parsed_audio = parsed
            .tracks()
            .iter()
            .find(|track| track.descriptor.codec.media_type == MediaType::Audio)
            .unwrap();
        assert_eq!(parsed_audio.descriptor.codec.configuration, [0x11, 0x90]);
        assert_eq!(parsed_audio.channel_count, Some(2));
        assert_eq!(parsed_audio.sample_rate, Some(48_000));
        assert_eq!(parsed_audio.samples.len(), 3);
        assert_eq!(parsed_audio.samples[0].pts, -1_024);
        assert_eq!(parsed_audio.presentation_duration, Some(2_048));
        let mut order = Vec::new();
        while let Some(packet) = parsed.read_packet().unwrap() {
            order.push(packet.stream_id);
        }
        assert_eq!(
            order,
            [
                StreamId(1),
                StreamId(2),
                StreamId(2),
                StreamId(1),
                StreamId(2)
            ]
        );
    }
}
