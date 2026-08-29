//! YUV4MPEG2 frame input and output.
//!
//! Y4M is treated as a deliberately simple development format rather than a general encoded
//! media container.

use std::io::{BufRead, Write};

use mmrecode_core::{Error, Result, VideoFrame};

/// A parsed Y4M stream header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Y4mHeader {
    /// Frame width.
    pub width: usize,
    /// Frame height.
    pub height: usize,
}

/// Streaming Y4M reader.
#[derive(Debug)]
pub struct Y4mReader<R> {
    input: R,
}

impl<R: BufRead> Y4mReader<R> {
    /// Wraps a buffered input source.
    #[must_use]
    pub const fn new(input: R) -> Self {
        Self { input }
    }

    /// Returns the wrapped source without consuming further data.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.input
    }

    /// Reads the next frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is malformed, truncated, or unsupported.
    pub fn read_frame(&mut self) -> Result<Option<VideoFrame>> {
        Err(Error::Unsupported(
            "Y4M frame reading is not implemented yet".into(),
        ))
    }
}

/// Streaming Y4M writer.
#[derive(Debug)]
pub struct Y4mWriter<W> {
    output: W,
}

impl<W: Write> Y4mWriter<W> {
    /// Wraps an output destination.
    #[must_use]
    pub const fn new(output: W) -> Self {
        Self { output }
    }

    /// Returns the wrapped destination.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.output
    }

    /// Writes one frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the frame format is unsupported or output fails.
    pub fn write_frame(&mut self, _frame: &VideoFrame) -> Result<()> {
        Err(Error::Unsupported(
            "Y4M frame writing is not implemented yet".into(),
        ))
    }
}
