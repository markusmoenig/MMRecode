//! Codec interfaces.

use std::collections::BTreeMap;

use crate::{AudioFrame, CodecDescriptor, Packet, PixelFormat, Rational, Result, VideoFrame};

/// A stateful compressed-audio decoder with explicit input and output queues.
pub trait AudioDecoder {
    /// Configures the decoder from container or caller-provided codec metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor is invalid or unsupported.
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()>;

    /// Submits one encoded audio packet.
    ///
    /// # Errors
    ///
    /// Returns an error when the packet is invalid or cannot be accepted.
    fn send_packet(&mut self, packet: Packet) -> Result<()>;

    /// Receives one decoded PCM block, if available.
    ///
    /// # Errors
    ///
    /// Returns an error when audio reconstruction fails.
    fn receive_frame(&mut self) -> Result<Option<AudioFrame>>;

    /// Signals end of input and drains delayed samples.
    ///
    /// # Errors
    ///
    /// Returns an error when delayed input cannot be decoded.
    fn flush(&mut self) -> Result<()>;
}

/// Generic video encoder settings shared across codec implementations.
#[derive(Clone, Debug)]
pub struct VideoEncoderSettings {
    /// Coded frame width.
    pub width: usize,
    /// Coded frame height.
    pub height: usize,
    /// Input pixel format.
    pub pixel_format: PixelFormat,
    /// Timestamp unit used for input frames and output packets.
    pub time_base: Rational,
    /// Optional target bitrate in bits per second.
    pub bitrate: Option<u64>,
    /// Codec-specific options. Stable options should eventually become typed fields.
    pub options: BTreeMap<String, String>,
}

/// A stateful decoder with explicit input and output queues.
pub trait Decoder {
    /// Configures the decoder from container or caller-provided codec metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor is invalid or unsupported.
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()>;

    /// Submits one encoded packet.
    ///
    /// # Errors
    ///
    /// Returns an error when the packet is invalid or cannot be accepted in the current state.
    fn send_packet(&mut self, packet: Packet) -> Result<()>;

    /// Receives one decoded frame, if available.
    ///
    /// # Errors
    ///
    /// Returns an error when decoding fails.
    fn receive_frame(&mut self) -> Result<Option<VideoFrame>>;

    /// Signals end of input and drains delayed frames.
    ///
    /// # Errors
    ///
    /// Returns an error when delayed input cannot be decoded.
    fn flush(&mut self) -> Result<()>;
}

/// A stateful encoder with explicit input and output queues.
pub trait Encoder {
    /// Configures the encoder and returns the descriptor required by a muxer.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings are invalid or unsupported.
    fn configure(&mut self, settings: &VideoEncoderSettings) -> Result<CodecDescriptor>;

    /// Submits one uncompressed frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the frame is invalid or cannot be accepted in the current state.
    fn send_frame(&mut self, frame: VideoFrame) -> Result<()>;

    /// Receives one encoded packet, if available.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding fails.
    fn receive_packet(&mut self) -> Result<Option<Packet>>;

    /// Signals end of input and drains delayed packets.
    ///
    /// # Errors
    ///
    /// Returns an error when delayed frames cannot be encoded.
    fn flush(&mut self) -> Result<()>;
}
