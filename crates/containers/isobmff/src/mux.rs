use mmrecode_core::{Error, MediaType, Packet, PacketFlags, Result, Timestamp};

use crate::{ColourInformation, PixelAspectRatio, Track};

/// Muxes one already-timed video track into a non-fragmented MP4 file.
///
/// Encoded packet payloads and the opaque sample-entry configuration are copied verbatim. Packets
/// must be in decode order, use the track time base, and have non-negative monotonically increasing
/// DTS values. Audio, edit lists, multiple sample descriptions, and files larger than 4 GiB are
/// intentionally outside this first packet-copy writer.
///
/// # Errors
///
/// Returns an error for missing video metadata, inconsistent timing, unsupported codec tags, empty
/// input, or output that cannot be represented by this minimal writer.
pub fn mux_video_track(track: &Track, packets: &[Packet]) -> Result<Vec<u8>> {
    validate(track, packets)?;
    let timescale = u32::try_from(track.descriptor.time_base.denominator())
        .map_err(|_| Error::Unsupported("MP4 timescale does not fit in u32".into()))?;
    if track.descriptor.time_base.numerator() != 1 || timescale == 0 {
        return Err(Error::Unsupported(
            "MP4 muxing currently requires a 1/timescale packet time base".into(),
        ));
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

    let mut media_payload = Vec::new();
    let mut sample_sizes = Vec::with_capacity(packets.len());
    for packet in packets {
        sample_sizes.push(u32::try_from(packet.data.len()).map_err(|_| {
            Error::Unsupported("one encoded sample is larger than the MP4 writer supports".into())
        })?);
        media_payload.extend_from_slice(&packet.data);
    }
    let mdat = atom(*b"mdat", media_payload)?;
    let chunk_offset = u32::try_from(ftyp.len() + 8)
        .map_err(|_| Error::Unsupported("MP4 chunk offset does not fit in u32".into()))?;

    let durations = packets
        .iter()
        .map(packet_duration)
        .collect::<Result<Vec<_>>>()?;
    let composition_offsets = packets
        .iter()
        .map(|packet| {
            packet_pts(packet)?
                .value
                .checked_sub(packet_dts(packet)?.value)
                .ok_or_else(|| Error::InvalidData("packet composition offset overflows".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let duration = packets
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

    let stbl = sample_table(
        track,
        &durations,
        &composition_offsets,
        &sample_sizes,
        chunk_offset,
        packets,
    )?;
    let minf = atom(
        *b"minf",
        [
            full_box(*b"vmhd", 0, 1, vec![0; 8])?,
            data_information()?,
            atom(*b"stbl", stbl)?,
        ]
        .concat(),
    )?;
    let media_box = atom(
        *b"mdia",
        [media_header(timescale, duration)?, handler()?, minf].concat(),
    )?;
    let track_box = atom(
        *b"trak",
        [track_header(track, duration)?, media_box].concat(),
    )?;
    let moov = atom(
        *b"moov",
        [movie_header(timescale, duration)?, track_box].concat(),
    )?;
    Ok([ftyp, mdat, moov].concat())
}

fn validate(track: &Track, packets: &[Packet]) -> Result<()> {
    if track.descriptor.codec.media_type != MediaType::Video {
        return Err(Error::InvalidData(
            "MP4 video muxer requires a video track".into(),
        ));
    }
    if packets.is_empty() {
        return Err(Error::InvalidData("cannot mux an empty video track".into()));
    }
    if track.width.is_none() || track.height.is_none() {
        return Err(Error::InvalidData(
            "video track has no coded dimensions".into(),
        ));
    }
    let tag =
        track.descriptor.codec.codec_tag.ok_or_else(|| {
            Error::InvalidData("video track has no sample-entry codec tag".into())
        })?;
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
    chunk_offset: u32,
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
    tables.push(full_table3(*b"stsc", &[(1, sample_count, 1)])?);

    let mut sample_size_payload = vec![0; 4];
    sample_size_payload.extend(0_u32.to_be_bytes());
    sample_size_payload.extend(sample_count.to_be_bytes());
    for &size in sizes {
        sample_size_payload.extend(size.to_be_bytes());
    }
    tables.push(atom(*b"stsz", sample_size_payload)?);
    tables.push(full_values(*b"stco", &[chunk_offset])?);
    let sync_samples = packets
        .iter()
        .enumerate()
        .filter(|(_, packet)| packet.flags.contains(PacketFlags::KEY))
        .map(|(index, _)| {
            u32::try_from(index + 1).map_err(|_| Error::Unsupported("too many MP4 samples".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    if !sync_samples.is_empty() {
        tables.push(full_values(*b"stss", &sync_samples)?);
    }
    Ok(tables.concat())
}

fn sample_entry(track: &Track) -> Result<Vec<u8>> {
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

fn movie_header(timescale: u32, duration: u64) -> Result<Vec<u8>> {
    let mut payload = vec![0; 16];
    payload.extend(timescale.to_be_bytes());
    payload.extend(duration.to_be_bytes());
    payload.extend(0x0001_0000_u32.to_be_bytes());
    payload.extend(0x0100_u16.to_be_bytes());
    payload.extend([0; 10]);
    payload.extend(identity_matrix());
    payload.extend([0; 24]);
    payload.extend(2_u32.to_be_bytes());
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
    payload.extend(0_u16.to_be_bytes());
    payload.extend(0_u16.to_be_bytes());
    payload.extend(rotation_matrix(track.rotation_degrees));
    payload.extend(
        track
            .width
            .expect("validated width")
            .checked_shl(16)
            .ok_or_else(|| Error::Unsupported("track width overflows 16.16 fixed point".into()))?
            .to_be_bytes(),
    );
    payload.extend(
        track
            .height
            .expect("validated height")
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

fn handler() -> Result<Vec<u8>> {
    let mut payload = vec![0; 4];
    payload.extend(*b"vide");
    payload.extend([0; 12]);
    payload.extend(b"MMRecode Video\0");
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
        CodecDescriptor, CodecId, FourCc, MediaType, Packet, PacketFlags, Rational,
        StreamDescriptor, StreamId, Timestamp,
    };

    use super::mux_video_track;
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
        let parsed = IsoBmffFile::parse(output).unwrap();
        let parsed_track = parsed.h264_track().unwrap();
        assert_eq!(parsed_track.rotation_degrees, 90);
        assert_eq!(parsed_track.samples.len(), 3);
        assert_eq!(parsed_track.samples[0].pts, 40);
        assert_eq!(parsed_track.samples[1].pts, 80);
        assert_eq!(parsed_track.samples[2].pts, 40);
        assert!(parsed_track.samples[0].is_sync);
    }
}
