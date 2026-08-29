//! Reusable support for codec conformance, regression, and interoperability tests.

use mmrecode_core::{Error, Result, VideoFrame};

/// Confirms that two frames have identical layout, metadata, and samples.
///
/// # Errors
///
/// Returns an error describing a frame mismatch.
pub fn assert_frames_equal(reference: &VideoFrame, candidate: &VideoFrame) -> Result<()> {
    if reference == candidate {
        Ok(())
    } else {
        Err(Error::InvalidData("decoded frames differ".into()))
    }
}
