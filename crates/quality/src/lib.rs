//! Objective frame comparison and quality measurements.

use mmrecode_core::{Error, Result};

/// Calculates peak signal-to-noise ratio for equally sized 8-bit sample planes.
///
/// # Errors
///
/// Returns an error when the planes differ in length, are empty, or exceed the supported sample
/// count.
pub fn psnr_u8(reference: &[u8], candidate: &[u8]) -> Result<f64> {
    if reference.len() != candidate.len() {
        return Err(Error::InvalidData(
            "PSNR inputs must have equal lengths".into(),
        ));
    }
    if reference.is_empty() {
        return Err(Error::InvalidData("PSNR inputs cannot be empty".into()));
    }

    let squared_error: f64 = reference
        .iter()
        .zip(candidate)
        .map(|(&left, &right)| {
            let difference = f64::from(left) - f64::from(right);
            difference * difference
        })
        .sum();

    if squared_error == 0.0 {
        return Ok(f64::INFINITY);
    }

    let sample_count = u32::try_from(reference.len())
        .map_err(|_| Error::Unsupported("PSNR planes exceed 2^32 samples".into()))?;
    let mean_squared_error = squared_error / f64::from(sample_count);
    Ok(10.0 * ((255.0 * 255.0) / mean_squared_error).log10())
}

#[cfg(test)]
mod tests {
    use super::psnr_u8;

    #[test]
    fn identical_planes_have_infinite_psnr() {
        assert!(psnr_u8(&[0, 64, 255], &[0, 64, 255]).unwrap().is_infinite());
    }
}
