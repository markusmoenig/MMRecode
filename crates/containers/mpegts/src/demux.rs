use std::collections::{HashMap, VecDeque};

use mmrecode_core::{
    CodecDescriptor, CodecId, Demuxer, Error, MediaType, Packet, PacketFlags, Rational, Result,
    SeekResult, StreamDescriptor, StreamId, Timestamp,
};

use crate::{
    SYSTEM_CLOCK_FREQUENCY, TS_PACKET_SIZE,
    crc::mpeg2_crc32,
    syntax::{
        ElementaryStreamInfo, PatProgram, ProgramAssociationTable, ProgramMapTable,
        TransportPacket, TransportPacketHeader, TransportStreamIssue,
    },
};

/// Fully indexed MPEG-2 Transport Stream.
#[derive(Clone, Debug)]
pub struct TransportStream {
    /// Parsed physical transport packets.
    pub packets: Vec<TransportPacket>,
    /// Current PAT sections found in the stream.
    pub program_association_tables: Vec<ProgramAssociationTable>,
    /// Current PMT sections found through the PAT.
    pub program_map_tables: Vec<ProgramMapTable>,
    /// Container stream descriptors, using the elementary PID as stream ID.
    pub streams: Vec<StreamDescriptor>,
    /// Reassembled PES payload packets in container order.
    pub elementary_packets: Vec<Packet>,
    /// Retained non-fatal inspection issues.
    pub issues: Vec<TransportStreamIssue>,
}

impl TransportStream {
    /// Concatenates PES payloads for the first stream matching `codec_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the codec is absent, its payload is empty, or its size overflows.
    pub fn elementary_stream_bytes(&self, codec_id: &str) -> Result<Vec<u8>> {
        let descriptor = self
            .streams
            .iter()
            .find(|stream| stream.codec.codec_id.as_str() == codec_id)
            .ok_or_else(|| {
                Error::Unsupported(format!("transport stream has no {codec_id} stream"))
            })?;
        let byte_count = self
            .elementary_packets
            .iter()
            .filter(|packet| packet.stream_id == descriptor.id)
            .try_fold(0_usize, |total, packet| {
                total.checked_add(packet.data.len())
            })
            .ok_or_else(|| Error::InvalidData("elementary stream size overflows".into()))?;
        let mut bytes = Vec::with_capacity(byte_count);
        for packet in &self.elementary_packets {
            if packet.stream_id == descriptor.id {
                bytes.extend_from_slice(&packet.data);
            }
        }
        if bytes.is_empty() {
            return Err(Error::InvalidData(format!(
                "{codec_id} PID contains no PES payload"
            )));
        }
        Ok(bytes)
    }

    /// Returns the first MPEG-2 Video elementary stream reconstructed from PES payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when no MPEG-2 Video stream exists or its size overflows.
    pub fn mpeg2_video_bytes(&self) -> Result<Vec<u8>> {
        self.elementary_stream_bytes("video/mpeg2")
    }

    /// Returns the first MPEG-1 Audio elementary stream reconstructed from PES payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when no MPEG-1 Audio stream exists or its size overflows.
    pub fn mpeg1_audio_bytes(&self) -> Result<Vec<u8>> {
        self.elementary_stream_bytes("audio/mpeg1")
    }
}

/// Parses and demultiplexes a complete 188-byte MPEG-2 Transport Stream.
///
/// # Errors
///
/// Returns an error for invalid packet alignment, continuity, PSI, CRC, or PES syntax.
pub fn demux_transport_stream(bytes: &[u8]) -> Result<TransportStream> {
    let packets = parse_transport_packets(bytes)?;
    validate_continuity(&packets)?;

    let pat_sections = collect_psi_sections(bytes, &packets, 0)?;
    if pat_sections.is_empty() {
        return Err(Error::InvalidData(
            "transport stream contains no Program Association Table".into(),
        ));
    }
    let mut association_tables = Vec::new();
    for section in pat_sections {
        let pat = parse_pat(&section)?;
        if pat.current_next && !association_tables.contains(&pat) {
            association_tables.push(pat);
        }
    }
    if association_tables.is_empty() {
        return Err(Error::InvalidData(
            "transport stream has no current Program Association Table".into(),
        ));
    }

    let mut map_tables = Vec::new();
    for pat in &association_tables {
        for program in &pat.programs {
            if program.program_number == 0 {
                continue;
            }
            for section in collect_psi_sections(bytes, &packets, program.pid)? {
                let pmt = parse_pmt(&section, program.pid)?;
                if pmt.program_number != program.program_number {
                    return Err(Error::InvalidData(format!(
                        "PMT PID 0x{:04x} declares program {}, PAT declares {}",
                        program.pid, pmt.program_number, program.program_number
                    )));
                }
                if pmt.current_next && !map_tables.contains(&pmt) {
                    map_tables.push(pmt);
                }
            }
        }
    }
    if map_tables.is_empty() {
        return Err(Error::InvalidData(
            "transport stream contains no current Program Map Table".into(),
        ));
    }

    let mut stream_types = HashMap::new();
    let mut streams = Vec::new();
    for pmt in &map_tables {
        for elementary in &pmt.streams {
            if let Some(previous) =
                stream_types.insert(elementary.elementary_pid, elementary.stream_type)
                && previous != elementary.stream_type
            {
                return Err(Error::InvalidData(format!(
                    "PID 0x{:04x} is declared with conflicting stream types",
                    elementary.elementary_pid
                )));
            }
            if !streams.iter().any(|stream: &StreamDescriptor| {
                stream.id == StreamId(u32::from(elementary.elementary_pid))
            }) {
                streams.push(stream_descriptor(elementary)?);
            }
        }
    }

    let elementary_packets = reassemble_pes(bytes, &packets, &stream_types)?;
    Ok(TransportStream {
        packets,
        program_association_tables: association_tables,
        program_map_tables: map_tables,
        streams,
        elementary_packets,
        issues: Vec::new(),
    })
}

fn parse_transport_packets(bytes: &[u8]) -> Result<Vec<TransportPacket>> {
    if bytes.is_empty() {
        return Err(Error::InvalidData("empty transport stream".into()));
    }
    if bytes.len() >= 192 && bytes[4] == 0x47 && bytes.len().is_multiple_of(192) {
        return Err(Error::Unsupported(
            "192-byte M2TS packets are not supported by this slice".into(),
        ));
    }
    if !bytes.len().is_multiple_of(TS_PACKET_SIZE) {
        return Err(Error::InvalidData(format!(
            "transport stream length {} is not a multiple of 188 bytes",
            bytes.len()
        )));
    }
    let (packet_bytes, remainder) = bytes.as_chunks::<TS_PACKET_SIZE>();
    debug_assert!(remainder.is_empty());
    let mut packets = Vec::with_capacity(packet_bytes.len());
    for (index, data) in packet_bytes.iter().enumerate() {
        packets.push(parse_transport_packet(data, index)?);
    }
    Ok(packets)
}

fn parse_transport_packet(data: &[u8; TS_PACKET_SIZE], index: usize) -> Result<TransportPacket> {
    let offset = index * TS_PACKET_SIZE;
    if data[0] != 0x47 {
        return Err(Error::InvalidData(format!(
            "transport packet {index} at byte {offset} has sync byte 0x{:02x}",
            data[0]
        )));
    }
    let adaptation_field_control = (data[3] >> 4) & 0x03;
    if adaptation_field_control == 0 {
        return Err(Error::InvalidData(format!(
            "transport packet {index} has reserved adaptation-field control"
        )));
    }
    let header = TransportPacketHeader {
        transport_error_indicator: data[1] & 0x80 != 0,
        payload_unit_start_indicator: data[1] & 0x40 != 0,
        transport_priority: data[1] & 0x20 != 0,
        pid: (u16::from(data[1] & 0x1f) << 8) | u16::from(data[2]),
        scrambling_control: data[3] >> 6,
        has_adaptation_field: adaptation_field_control & 0x02 != 0,
        has_payload: adaptation_field_control & 0x01 != 0,
        continuity_counter: data[3] & 0x0f,
    };
    if header.transport_error_indicator {
        return Err(Error::InvalidData(format!(
            "transport packet {index} has transport_error_indicator set"
        )));
    }
    if header.scrambling_control != 0 {
        return Err(Error::Unsupported(format!(
            "scrambled transport packet {index} on PID 0x{:04x}",
            header.pid
        )));
    }

    let mut cursor = 4_usize;
    let mut discontinuity_indicator = false;
    let mut random_access_indicator = false;
    let mut pcr = None;
    if header.has_adaptation_field {
        let length = usize::from(data[cursor]);
        cursor += 1;
        let end = cursor.checked_add(length).ok_or_else(|| {
            Error::InvalidData(format!("packet {index} adaptation length overflows"))
        })?;
        if end > TS_PACKET_SIZE {
            return Err(Error::InvalidData(format!(
                "transport packet {index} adaptation field exceeds packet"
            )));
        }
        if length > 0 {
            let flags = data[cursor];
            discontinuity_indicator = flags & 0x80 != 0;
            random_access_indicator = flags & 0x40 != 0;
            if flags & 0x10 != 0 {
                if length < 7 {
                    return Err(Error::InvalidData(format!(
                        "transport packet {index} PCR flag has no six-byte PCR"
                    )));
                }
                let value = &data[cursor + 1..cursor + 7];
                let base = (u64::from(value[0]) << 25)
                    | (u64::from(value[1]) << 17)
                    | (u64::from(value[2]) << 9)
                    | (u64::from(value[3]) << 1)
                    | (u64::from(value[4]) >> 7);
                let extension = (u16::from(value[4] & 0x01) << 8) | u16::from(value[5]);
                if value[4] & 0x7e != 0x7e || extension >= 300 {
                    return Err(Error::InvalidData(format!(
                        "transport packet {index} has invalid PCR reserved bits or extension"
                    )));
                }
                pcr = Some(base);
            }
        }
        cursor = end;
    }
    let payload_range = if header.has_payload {
        if cursor >= TS_PACKET_SIZE {
            return Err(Error::InvalidData(format!(
                "transport packet {index} signals an empty payload"
            )));
        }
        Some(offset + cursor..offset + TS_PACKET_SIZE)
    } else {
        None
    };
    Ok(TransportPacket {
        index,
        source_range: offset..offset + TS_PACKET_SIZE,
        header,
        discontinuity_indicator,
        random_access_indicator,
        pcr,
        payload_range,
    })
}

fn validate_continuity(packets: &[TransportPacket]) -> Result<()> {
    let mut counters = HashMap::<u16, u8>::new();
    for packet in packets {
        if packet.header.pid == 0x1fff || !packet.header.has_payload {
            continue;
        }
        if packet.discontinuity_indicator {
            counters.insert(packet.header.pid, packet.header.continuity_counter);
            continue;
        }
        if let Some(previous) = counters.insert(packet.header.pid, packet.header.continuity_counter)
        {
            let expected = (previous + 1) & 0x0f;
            if packet.header.continuity_counter != expected {
                return Err(Error::InvalidData(format!(
                    "PID 0x{:04x} continuity jumps from {previous} to {} at packet {} (expected {expected})",
                    packet.header.pid, packet.header.continuity_counter, packet.index
                )));
            }
        }
    }
    Ok(())
}

fn collect_psi_sections(
    bytes: &[u8],
    packets: &[TransportPacket],
    pid: u16,
) -> Result<Vec<Vec<u8>>> {
    let mut sections = Vec::new();
    let mut pending = Vec::new();
    for packet in packets.iter().filter(|packet| packet.header.pid == pid) {
        let Some(range) = &packet.payload_range else {
            continue;
        };
        let payload = &bytes[range.clone()];
        if packet.header.payload_unit_start_indicator {
            if payload.is_empty() {
                return Err(Error::InvalidData(format!(
                    "PSI PID 0x{pid:04x} packet {} has no pointer field",
                    packet.index
                )));
            }
            let pointer = usize::from(payload[0]);
            if 1 + pointer > payload.len() {
                return Err(Error::InvalidData(format!(
                    "PSI PID 0x{pid:04x} pointer exceeds packet {}",
                    packet.index
                )));
            }
            if !pending.is_empty() {
                pending.extend_from_slice(&payload[1..=pointer]);
                drain_sections(&mut pending, &mut sections, pid)?;
                if !pending.is_empty() {
                    return Err(Error::InvalidData(format!(
                        "PSI PID 0x{pid:04x} starts a new section before completing the previous one"
                    )));
                }
            }
            pending.extend_from_slice(&payload[1 + pointer..]);
        } else if !pending.is_empty() {
            pending.extend_from_slice(payload);
        }
        drain_sections(&mut pending, &mut sections, pid)?;
    }
    if !pending.is_empty() && pending.iter().any(|&byte| byte != 0xff) {
        return Err(Error::InvalidData(format!(
            "truncated PSI section on PID 0x{pid:04x}"
        )));
    }
    Ok(sections)
}

fn drain_sections(pending: &mut Vec<u8>, sections: &mut Vec<Vec<u8>>, pid: u16) -> Result<()> {
    loop {
        while pending.first() == Some(&0xff) {
            pending.remove(0);
        }
        if pending.len() < 3 {
            return Ok(());
        }
        let section_length = (usize::from(pending[1] & 0x0f) << 8) | usize::from(pending[2]);
        if section_length > 1021 {
            return Err(Error::InvalidData(format!(
                "PSI PID 0x{pid:04x} section_length {section_length} exceeds 1021"
            )));
        }
        let total = 3 + section_length;
        if pending.len() < total {
            return Ok(());
        }
        sections.push(pending.drain(..total).collect());
    }
}

fn validate_long_section(section: &[u8], expected_table_id: u8, name: &str) -> Result<()> {
    if section.len() < 12 || section[0] != expected_table_id {
        return Err(Error::InvalidData(format!(
            "invalid {name} table identifier or length"
        )));
    }
    if section[1] & 0xc0 != 0x80 || section[1] & 0x30 != 0x30 {
        return Err(Error::InvalidData(format!("invalid {name} section flags")));
    }
    if section[6] > section[7] {
        return Err(Error::InvalidData(format!(
            "invalid {name} section numbering"
        )));
    }
    if mpeg2_crc32(section) != 0 {
        return Err(Error::InvalidData(format!("{name} CRC-32 mismatch")));
    }
    Ok(())
}

fn parse_pat(section: &[u8]) -> Result<ProgramAssociationTable> {
    validate_long_section(section, 0x00, "PAT")?;
    if !(section.len() - 12).is_multiple_of(4) {
        return Err(Error::InvalidData(
            "PAT program loop is not four-byte aligned".into(),
        ));
    }
    let mut programs = Vec::new();
    let (entries, remainder) = section[8..section.len() - 4].as_chunks::<4>();
    debug_assert!(remainder.is_empty());
    for entry in entries {
        programs.push(PatProgram {
            program_number: u16::from_be_bytes([entry[0], entry[1]]),
            pid: (u16::from(entry[2] & 0x1f) << 8) | u16::from(entry[3]),
        });
    }
    Ok(ProgramAssociationTable {
        transport_stream_id: u16::from_be_bytes([section[3], section[4]]),
        version: (section[5] >> 1) & 0x1f,
        current_next: section[5] & 1 != 0,
        programs,
    })
}

fn parse_pmt(section: &[u8], pid: u16) -> Result<ProgramMapTable> {
    validate_long_section(section, 0x02, "PMT")?;
    if section.len() < 16 {
        return Err(Error::InvalidData(
            "PMT is shorter than its fixed fields".into(),
        ));
    }
    let descriptors_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut cursor = 12_usize;
    let descriptors_end = cursor
        .checked_add(descriptors_length)
        .ok_or_else(|| Error::InvalidData("PMT descriptor length overflows".into()))?;
    if descriptors_end > section.len() - 4 {
        return Err(Error::InvalidData(
            "PMT program descriptors exceed section".into(),
        ));
    }
    let descriptors = section[cursor..descriptors_end].to_vec();
    cursor = descriptors_end;
    let mut streams = Vec::new();
    while cursor < section.len() - 4 {
        if cursor + 5 > section.len() - 4 {
            return Err(Error::InvalidData(
                "truncated PMT elementary-stream entry".into(),
            ));
        }
        let info_length =
            (usize::from(section[cursor + 3] & 0x0f) << 8) | usize::from(section[cursor + 4]);
        let end = cursor
            .checked_add(5 + info_length)
            .ok_or_else(|| Error::InvalidData("PMT ES descriptor length overflows".into()))?;
        if end > section.len() - 4 {
            return Err(Error::InvalidData(
                "PMT ES descriptors exceed section".into(),
            ));
        }
        streams.push(ElementaryStreamInfo {
            stream_type: section[cursor],
            elementary_pid: (u16::from(section[cursor + 1] & 0x1f) << 8)
                | u16::from(section[cursor + 2]),
            descriptors: section[cursor + 5..end].to_vec(),
        });
        cursor = end;
    }
    Ok(ProgramMapTable {
        program_number: u16::from_be_bytes([section[3], section[4]]),
        pid,
        version: (section[5] >> 1) & 0x1f,
        current_next: section[5] & 1 != 0,
        pcr_pid: (u16::from(section[8] & 0x1f) << 8) | u16::from(section[9]),
        descriptors,
        streams,
    })
}

fn stream_descriptor(elementary: &ElementaryStreamInfo) -> Result<StreamDescriptor> {
    let (codec_id, media_type) = match elementary.stream_type {
        0x01 => ("video/mpeg1", MediaType::Video),
        0x02 => ("video/mpeg2", MediaType::Video),
        0x03 => ("audio/mpeg1", MediaType::Audio),
        0x04 => ("audio/mpeg2", MediaType::Audio),
        0x0f => ("audio/aac-adts", MediaType::Audio),
        0x1b => ("video/h264", MediaType::Video),
        0x24 => ("video/hevc", MediaType::Video),
        value => {
            return Ok(StreamDescriptor {
                id: StreamId(u32::from(elementary.elementary_pid)),
                codec: CodecDescriptor {
                    codec_id: CodecId::new(format!("mpegts/stream-type-{value:02x}")),
                    codec_tag: None,
                    media_type: MediaType::Data,
                    configuration: elementary.descriptors.clone(),
                },
                time_base: Rational::new(1, SYSTEM_CLOCK_FREQUENCY)?,
            });
        }
    };
    Ok(StreamDescriptor {
        id: StreamId(u32::from(elementary.elementary_pid)),
        codec: CodecDescriptor {
            codec_id: CodecId::new(codec_id),
            codec_tag: None,
            media_type,
            configuration: elementary.descriptors.clone(),
        },
        time_base: Rational::new(1, SYSTEM_CLOCK_FREQUENCY)?,
    })
}

#[derive(Debug)]
struct PesAssembly {
    first_packet_index: usize,
    random_access: bool,
    data: Vec<u8>,
}

fn reassemble_pes(
    bytes: &[u8],
    packets: &[TransportPacket],
    stream_types: &HashMap<u16, u8>,
) -> Result<Vec<Packet>> {
    let mut active = HashMap::<u16, PesAssembly>::new();
    let mut completed = Vec::<(usize, u16, PesAssembly)>::new();
    for packet in packets {
        if !stream_types.contains_key(&packet.header.pid) {
            continue;
        }
        let Some(range) = &packet.payload_range else {
            continue;
        };
        if packet.header.payload_unit_start_indicator {
            if let Some(previous) = active.remove(&packet.header.pid) {
                completed.push((previous.first_packet_index, packet.header.pid, previous));
            }
            active.insert(
                packet.header.pid,
                PesAssembly {
                    first_packet_index: packet.index,
                    random_access: packet.random_access_indicator,
                    data: bytes[range.clone()].to_vec(),
                },
            );
        } else if let Some(assembly) = active.get_mut(&packet.header.pid) {
            assembly.data.extend_from_slice(&bytes[range.clone()]);
        }
    }
    for (pid, assembly) in active {
        completed.push((assembly.first_packet_index, pid, assembly));
    }
    completed.sort_by_key(|(index, _, _)| *index);
    completed
        .into_iter()
        .map(|(_, pid, assembly)| parse_pes(pid, assembly))
        .collect()
}

fn parse_pes(pid: u16, assembly: PesAssembly) -> Result<Packet> {
    let data = assembly.data;
    if data.len() < 9 || data[..3] != [0, 0, 1] {
        return Err(Error::InvalidData(format!(
            "PID 0x{pid:04x} payload does not begin with a PES header"
        )));
    }
    let stream_id = data[3];
    let declared_length = usize::from(u16::from_be_bytes([data[4], data[5]]));
    let pes_end = if declared_length == 0 {
        data.len()
    } else {
        6_usize
            .checked_add(declared_length)
            .ok_or_else(|| Error::InvalidData("PES packet length overflows".into()))?
    };
    if pes_end > data.len() {
        return Err(Error::InvalidData(format!(
            "truncated PES packet on PID 0x{pid:04x}"
        )));
    }
    if data[6] & 0xc0 != 0x80 {
        return Err(Error::Unsupported(format!(
            "PID 0x{pid:04x} uses MPEG-1 or invalid PES optional-header syntax"
        )));
    }
    let pts_dts_flags = data[7] >> 6;
    if pts_dts_flags == 1 {
        return Err(Error::InvalidData(
            "PES has forbidden PTS_DTS_flags value 01".into(),
        ));
    }
    let header_length = usize::from(data[8]);
    let payload_start = 9_usize
        .checked_add(header_length)
        .ok_or_else(|| Error::InvalidData("PES header length overflows".into()))?;
    if payload_start > pes_end {
        return Err(Error::InvalidData(format!(
            "PES optional header exceeds packet on PID 0x{pid:04x}"
        )));
    }
    let mut cursor = 9_usize;
    let pts = if pts_dts_flags & 0x02 != 0 {
        let prefix = if pts_dts_flags == 3 { 0x03 } else { 0x02 };
        let value = parse_timestamp(&data, cursor, prefix, "PTS")?;
        cursor += 5;
        Some(system_timestamp(value)?)
    } else {
        None
    };
    let dts = if pts_dts_flags == 3 {
        Some(system_timestamp(parse_timestamp(
            &data, cursor, 0x01, "DTS",
        )?)?)
    } else {
        None
    };
    let mut flags = PacketFlags::empty();
    if assembly.random_access {
        flags.insert(PacketFlags::KEY);
    }
    let _ = stream_id;
    Ok(Packet {
        stream_id: StreamId(u32::from(pid)),
        data: data[payload_start..pes_end].to_vec(),
        pts,
        dts,
        duration: None,
        flags,
        side_data: Vec::new(),
    })
}

fn parse_timestamp(data: &[u8], offset: usize, prefix: u8, name: &str) -> Result<u64> {
    let end = offset
        .checked_add(5)
        .ok_or_else(|| Error::InvalidData(format!("{name} offset overflows")))?;
    if end > data.len() {
        return Err(Error::InvalidData(format!("truncated PES {name}")));
    }
    let value = &data[offset..end];
    if value[0] >> 4 != prefix || value[0] & 1 == 0 || value[2] & 1 == 0 || value[4] & 1 == 0 {
        return Err(Error::InvalidData(format!(
            "invalid PES {name} marker bits"
        )));
    }
    Ok((u64::from((value[0] >> 1) & 0x07) << 30)
        | (u64::from(value[1]) << 22)
        | (u64::from(value[2] >> 1) << 15)
        | (u64::from(value[3]) << 7)
        | u64::from(value[4] >> 1))
}

fn system_timestamp(value: u64) -> Result<Timestamp> {
    Ok(Timestamp {
        value: i64::try_from(value)
            .map_err(|_| Error::InvalidData("timestamp does not fit signed storage".into()))?,
        time_base: Rational::new(1, SYSTEM_CLOCK_FREQUENCY)?,
    })
}

/// Queue-based demuxer for a complete in-memory MPEG-2 Transport Stream.
#[derive(Debug)]
pub struct MpegTsDemuxer {
    streams: Vec<StreamDescriptor>,
    packets: VecDeque<Packet>,
}

impl MpegTsDemuxer {
    /// Parses an in-memory stream and queues its PES payloads.
    ///
    /// # Errors
    ///
    /// Returns an error when transport syntax or tables are invalid.
    pub fn new(bytes: &[u8]) -> Result<Self> {
        let stream = demux_transport_stream(bytes)?;
        Ok(Self {
            streams: stream.streams,
            packets: stream.elementary_packets.into(),
        })
    }
}

impl Demuxer for MpegTsDemuxer {
    fn streams(&self) -> &[StreamDescriptor] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        Ok(self.packets.pop_front())
    }

    fn seek(&mut self, _target: Timestamp) -> Result<SeekResult> {
        Err(Error::Unsupported(
            "MPEG-TS seeking requires an index and is not implemented".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_timestamp;

    #[test]
    fn parses_pes_timestamp_markers() {
        let encoded = [0x21, 0x00, 0x01, 0x00, 0x01];
        assert_eq!(parse_timestamp(&encoded, 0, 2, "PTS").unwrap(), 0);
    }
}
