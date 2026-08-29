use std::collections::HashMap;

use mmrecode_core::{
    CodecDescriptor, CodecId, Error, MediaType, Muxer, Packet, PacketFlags, Rational, Result,
    StreamDescriptor, StreamId, Timestamp,
};

use crate::{SYSTEM_CLOCK_FREQUENCY, TS_PACKET_SIZE, crc::mpeg2_crc32};

/// Program and PID choices for deterministic single-program transport muxing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MpegTsMuxConfig {
    /// Transport-stream identifier written in the PAT.
    pub transport_stream_id: u16,
    /// Program number written in PAT and PMT.
    pub program_number: u16,
    /// PID carrying the PMT.
    pub pmt_pid: u16,
    /// PID carrying MPEG-2 Video PES and PCR.
    pub video_pid: u16,
    /// Maximum number of emitted TS packets between repeated PAT/PMT pairs.
    pub psi_interval_packets: usize,
}

impl Default for MpegTsMuxConfig {
    fn default() -> Self {
        Self {
            transport_stream_id: 1,
            program_number: 1,
            pmt_pid: 0x1000,
            video_pid: 0x0100,
            psi_interval_packets: 40,
        }
    }
}

/// In-memory deterministic MPEG-2 Transport Stream muxer.
#[derive(Debug)]
pub struct MpegTsMuxer {
    config: MpegTsMuxConfig,
    output: Vec<u8>,
    continuity: HashMap<u16, u8>,
    stream: Option<StreamDescriptor>,
    finalized: bool,
    wrote_payload: bool,
    packets_since_psi: usize,
    last_pcr: u64,
}

impl MpegTsMuxer {
    /// Creates a muxer with conventional deterministic PIDs.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(MpegTsMuxConfig::default())
    }

    /// Creates a muxer with explicit program and PID configuration.
    #[must_use]
    pub fn with_config(config: MpegTsMuxConfig) -> Self {
        Self {
            config,
            output: Vec::new(),
            continuity: HashMap::new(),
            stream: None,
            finalized: false,
            wrote_payload: false,
            packets_since_psi: 0,
            last_pcr: 0,
        }
    }

    /// Returns emitted bytes without consuming the muxer.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.output
    }

    /// Consumes the muxer and returns the complete transport stream.
    ///
    /// # Errors
    ///
    /// Returns an error if [`Muxer::finalize`] has not been called.
    pub fn into_bytes(self) -> Result<Vec<u8>> {
        if !self.finalized {
            return Err(Error::InvalidState(
                "transport muxer must be finalized before taking its bytes".into(),
            ));
        }
        Ok(self.output)
    }

    fn validate_config(&self) -> Result<()> {
        let config = self.config;
        if config.program_number == 0 {
            return Err(Error::InvalidData(
                "MPEG-TS program number must be non-zero".into(),
            ));
        }
        for (name, pid) in [("PMT", config.pmt_pid), ("video", config.video_pid)] {
            if !(0x0010..0x1fff).contains(&pid) {
                return Err(Error::InvalidData(format!(
                    "{name} PID 0x{pid:04x} is reserved"
                )));
            }
        }
        if config.pmt_pid == config.video_pid {
            return Err(Error::InvalidData("PMT and video PID must differ".into()));
        }
        if config.psi_interval_packets == 0 {
            return Err(Error::InvalidData(
                "PSI repetition interval must be non-zero".into(),
            ));
        }
        Ok(())
    }

    fn emit_psi(&mut self) -> Result<()> {
        let pat = make_pat(self.config);
        let pmt = make_pmt(self.config);
        self.emit_section(0, &pat)?;
        self.emit_section(self.config.pmt_pid, &pmt)?;
        self.packets_since_psi = 0;
        Ok(())
    }

    fn emit_section(&mut self, pid: u16, section: &[u8]) -> Result<()> {
        if section.len() + 1 > 184 {
            return Err(Error::Unsupported(
                "multi-packet PSI sections are not emitted by this muxer".into(),
            ));
        }
        let mut payload = [0xff_u8; 184];
        payload[0] = 0;
        payload[1..=section.len()].copy_from_slice(section);
        self.emit_transport_packet(pid, true, &payload, None, false)
    }

    fn emit_pes(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Err(Error::InvalidData(
                "cannot mux an empty MPEG-2 Video packet".into(),
            ));
        }
        if packet.dts.is_some() && packet.pts.is_none() {
            return Err(Error::InvalidData("PES DTS requires a PTS".into()));
        }
        let pts = packet.pts.map(timestamp_to_90k).transpose()?;
        let dts = packet.dts.map(timestamp_to_90k).transpose()?;
        let mut pes = Vec::with_capacity(packet.data.len() + 19);
        pes.extend_from_slice(&[0, 0, 1, 0xe0, 0, 0]);
        pes.push(0x80);
        match (pts, dts) {
            (Some(pts), Some(dts)) => {
                pes.push(0xc0);
                pes.push(10);
                write_timestamp(&mut pes, 3, pts);
                write_timestamp(&mut pes, 1, dts);
            }
            (Some(pts), None) => {
                pes.push(0x80);
                pes.push(5);
                write_timestamp(&mut pes, 2, pts);
            }
            (None, None) => {
                pes.extend_from_slice(&[0, 0]);
            }
            (None, Some(_)) => unreachable!("DTS without PTS rejected above"),
        }
        pes.extend_from_slice(&packet.data);

        let requested_pcr = dts.or(pts).unwrap_or(self.last_pcr);
        let pcr = requested_pcr.max(self.last_pcr) & ((1_u64 << 33) - 1);
        self.last_pcr = pcr;
        let random_access = packet.flags.contains(PacketFlags::KEY);
        let mut remaining = pes.as_slice();
        let mut first = true;
        while !remaining.is_empty() {
            let capacity = if first { 176 } else { 184 };
            let take = remaining.len().min(capacity);
            self.emit_transport_packet(
                self.config.video_pid,
                first,
                &remaining[..take],
                first.then_some(pcr),
                first && random_access,
            )?;
            remaining = &remaining[take..];
            first = false;
        }
        Ok(())
    }

    fn emit_transport_packet(
        &mut self,
        pid: u16,
        payload_start: bool,
        payload: &[u8],
        pcr: Option<u64>,
        random_access: bool,
    ) -> Result<()> {
        if payload.is_empty() || payload.len() > 184 {
            return Err(Error::InvalidData(format!(
                "transport payload length {} is outside 1..=184",
                payload.len()
            )));
        }
        let minimum_adaptation_length = usize::from(pcr.is_some()) * 6 + 1;
        let needs_adaptation = pcr.is_some() || random_access || payload.len() < 184;
        let adaptation_length = needs_adaptation.then(|| 183 - payload.len());
        if adaptation_length.is_some_and(|length| length < minimum_adaptation_length) {
            return Err(Error::InvalidData(
                "transport payload leaves insufficient adaptation-field space".into(),
            ));
        }

        let counter = self.continuity.entry(pid).or_insert(0);
        let current_counter = *counter;
        *counter = (*counter + 1) & 0x0f;
        let mut data = [0xff_u8; TS_PACKET_SIZE];
        data[0] = 0x47;
        data[1] = u8::try_from((pid >> 8) & 0x1f).expect("thirteen-bit PID high byte fits u8")
            | if payload_start { 0x40 } else { 0 };
        data[2] = u8::try_from(pid & 0xff).expect("PID low byte fits u8");
        data[3] = (if needs_adaptation { 0x30 } else { 0x10 }) | current_counter;
        let mut cursor = 4_usize;
        if let Some(length) = adaptation_length {
            data[cursor] = u8::try_from(length).expect("adaptation length fits u8");
            cursor += 1;
            if length > 0 {
                data[cursor] =
                    (if random_access { 0x40 } else { 0 }) | (if pcr.is_some() { 0x10 } else { 0 });
                cursor += 1;
                if let Some(base) = pcr {
                    write_pcr(&mut data[cursor..cursor + 6], base);
                    cursor += 6;
                }
                cursor += length - minimum_adaptation_length;
            }
        }
        if cursor + payload.len() != TS_PACKET_SIZE {
            return Err(Error::InvalidState(
                "internal TS packet payload placement mismatch".into(),
            ));
        }
        data[cursor..].copy_from_slice(payload);
        self.output.extend_from_slice(&data);
        self.packets_since_psi += 1;
        Ok(())
    }
}

impl Default for MpegTsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Muxer for MpegTsMuxer {
    fn add_stream(&mut self, mut descriptor: StreamDescriptor) -> Result<StreamId> {
        if self.finalized || self.wrote_payload {
            return Err(Error::InvalidState(
                "streams must be registered before writing transport packets".into(),
            ));
        }
        self.validate_config()?;
        if self.stream.is_some() {
            return Err(Error::Unsupported(
                "initial MPEG-TS muxer supports one MPEG-2 Video stream".into(),
            ));
        }
        if descriptor.codec.codec_id.as_str() != "video/mpeg2"
            || descriptor.codec.media_type != MediaType::Video
        {
            return Err(Error::Unsupported(
                "initial MPEG-TS muxer accepts only video/mpeg2".into(),
            ));
        }
        let id = StreamId(u32::from(self.config.video_pid));
        descriptor.id = id;
        self.stream = Some(descriptor);
        Ok(id)
    }

    fn write_packet(&mut self, packet: Packet) -> Result<()> {
        if self.finalized {
            return Err(Error::InvalidState("transport muxer is finalized".into()));
        }
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| Error::InvalidState("add a stream before writing packets".into()))?;
        if packet.stream_id != stream.id {
            return Err(Error::InvalidData(format!(
                "packet stream {:?} does not match registered stream {:?}",
                packet.stream_id, stream.id
            )));
        }
        if !self.wrote_payload || self.packets_since_psi >= self.config.psi_interval_packets {
            self.emit_psi()?;
        }
        self.emit_pes(&packet)?;
        self.wrote_payload = true;
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Err(Error::InvalidState(
                "transport muxer is already finalized".into(),
            ));
        }
        if self.stream.is_none() || !self.wrote_payload {
            return Err(Error::InvalidState(
                "transport muxer has no registered stream or payload".into(),
            ));
        }
        self.finalized = true;
        Ok(())
    }
}

/// Wraps one complete MPEG-2 Video elementary stream in a deterministic transport stream.
///
/// # Errors
///
/// Returns an error when the input is empty or muxer state is invalid.
pub fn mux_mpeg2_video(elementary_stream: &[u8]) -> Result<Vec<u8>> {
    let mut muxer = MpegTsMuxer::new();
    let descriptor = StreamDescriptor {
        id: StreamId(0),
        codec: CodecDescriptor {
            codec_id: CodecId::new("video/mpeg2"),
            codec_tag: None,
            media_type: MediaType::Video,
            configuration: Vec::new(),
        },
        time_base: Rational::new(1, SYSTEM_CLOCK_FREQUENCY)?,
    };
    let stream_id = muxer.add_stream(descriptor)?;
    muxer.write_packet(Packet {
        stream_id,
        data: elementary_stream.to_vec(),
        pts: Some(Timestamp {
            value: 0,
            time_base: Rational::new(1, SYSTEM_CLOCK_FREQUENCY)?,
        }),
        dts: None,
        duration: None,
        flags: PacketFlags::KEY,
        side_data: Vec::new(),
    })?;
    muxer.finalize()?;
    muxer.into_bytes()
}

#[allow(clippy::cast_possible_truncation)]
fn make_pat(config: MpegTsMuxConfig) -> Vec<u8> {
    let mut section = vec![
        0x00,
        0xb0,
        13,
        (config.transport_stream_id >> 8) as u8,
        config.transport_stream_id as u8,
        0xc1,
        0,
        0,
        (config.program_number >> 8) as u8,
        config.program_number as u8,
        0xe0 | ((config.pmt_pid >> 8) as u8 & 0x1f),
        config.pmt_pid as u8,
    ];
    append_crc(&mut section);
    section
}

#[allow(clippy::cast_possible_truncation)]
fn make_pmt(config: MpegTsMuxConfig) -> Vec<u8> {
    let mut section = vec![
        0x02,
        0xb0,
        18,
        (config.program_number >> 8) as u8,
        config.program_number as u8,
        0xc1,
        0,
        0,
        0xe0 | ((config.video_pid >> 8) as u8 & 0x1f),
        config.video_pid as u8,
        0xf0,
        0,
        0x02,
        0xe0 | ((config.video_pid >> 8) as u8 & 0x1f),
        config.video_pid as u8,
        0xf0,
        0,
    ];
    append_crc(&mut section);
    section
}

fn append_crc(section: &mut Vec<u8>) {
    section.extend_from_slice(&mpeg2_crc32(section).to_be_bytes());
}

#[allow(clippy::cast_possible_truncation)]
fn write_timestamp(output: &mut Vec<u8>, prefix: u8, value: u64) {
    let value = value & ((1_u64 << 33) - 1);
    output.push((prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 1);
    output.push((value >> 22) as u8);
    output.push((((value >> 15) as u8 & 0x7f) << 1) | 1);
    output.push((value >> 7) as u8);
    output.push(((value as u8 & 0x7f) << 1) | 1);
}

#[allow(clippy::cast_possible_truncation)]
fn write_pcr(output: &mut [u8], base: u64) {
    let base = base & ((1_u64 << 33) - 1);
    output[0] = (base >> 25) as u8;
    output[1] = (base >> 17) as u8;
    output[2] = (base >> 9) as u8;
    output[3] = (base >> 1) as u8;
    output[4] = ((base as u8 & 1) << 7) | 0x7e;
    output[5] = 0;
}

fn timestamp_to_90k(timestamp: Timestamp) -> Result<u64> {
    let numerator = i128::from(timestamp.value)
        .checked_mul(i128::from(timestamp.time_base.numerator()))
        .and_then(|value| value.checked_mul(i128::from(SYSTEM_CLOCK_FREQUENCY)))
        .ok_or_else(|| Error::InvalidData("timestamp rescaling overflows".into()))?;
    let denominator = i128::from(timestamp.time_base.denominator());
    if denominator == 0 {
        return Err(Error::InvalidData("timestamp has zero denominator".into()));
    }
    let value = numerator / denominator;
    u64::try_from(value).map_err(|_| Error::InvalidData("negative transport timestamp".into()))
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{Demuxer, Muxer};

    use super::*;
    use crate::{TS_PACKET_SIZE, demux_transport_stream};

    const MPEG2: &[u8] =
        include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");

    #[test]
    fn deterministic_mux_round_trips_elementary_bytes() {
        let first = mux_mpeg2_video(MPEG2).unwrap();
        let second = mux_mpeg2_video(MPEG2).unwrap();
        assert_eq!(first, second);
        assert!(first.len().is_multiple_of(TS_PACKET_SIZE));
        let parsed = demux_transport_stream(&first).unwrap();
        assert_eq!(parsed.mpeg2_video_bytes().unwrap(), MPEG2);
        assert_eq!(parsed.program_map_tables[0].streams[0].stream_type, 2);
        assert!(parsed.packets.iter().any(|packet| packet.pcr == Some(0)));
    }

    #[test]
    fn trait_demuxer_exposes_mpeg2_packet_and_timestamps() {
        let bytes = mux_mpeg2_video(MPEG2).unwrap();
        let mut demuxer = crate::MpegTsDemuxer::new(&bytes).unwrap();
        assert_eq!(demuxer.streams()[0].codec.codec_id.as_str(), "video/mpeg2");
        let packet = demuxer.read_packet().unwrap().unwrap();
        assert_eq!(packet.pts.unwrap().value, 0);
        assert_eq!(packet.data, MPEG2);
        assert!(demuxer.read_packet().unwrap().is_none());
    }

    #[test]
    fn muxer_rejects_non_mpeg2_streams() {
        let mut muxer = MpegTsMuxer::new();
        let error = muxer
            .add_stream(StreamDescriptor {
                id: StreamId(7),
                codec: CodecDescriptor {
                    codec_id: CodecId::new("video/h264"),
                    codec_tag: None,
                    media_type: MediaType::Video,
                    configuration: Vec::new(),
                },
                time_base: Rational::new(1, 25).unwrap(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("video/mpeg2"));
    }
}
