//! Objective frame comparison and quality measurements.

use mmrecode_core::{Error, Result};
use mmrecode_core::{Plane, VideoFrame};

/// Quality statistics for one visible frame plane.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaneQualityReport {
    /// Zero-based plane index.
    pub plane_index: usize,
    /// Number of visible samples compared.
    pub sample_count: usize,
    /// Mean squared error.
    pub mean_squared_error: f64,
    /// Peak signal-to-noise ratio in decibels, or infinity for exact equality.
    pub psnr: f64,
    /// Largest absolute sample difference.
    pub maximum_absolute_error: u8,
}

/// Aggregate and per-plane quality statistics for two video frames.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameQualityReport {
    /// Per-plane results in storage order.
    pub planes: Vec<PlaneQualityReport>,
    /// Mean squared error across every visible sample.
    pub mean_squared_error: f64,
    /// Aggregate peak signal-to-noise ratio in decibels.
    pub psnr: f64,
    /// Largest absolute error in any plane.
    pub maximum_absolute_error: u8,
}

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

/// Compares equally laid-out video frames over their visible plane samples.
///
/// Padding bytes beyond each plane's visible width are ignored.
///
/// # Errors
///
/// Returns an error when formats, dimensions, plane layouts, or sample storage
/// differ, or when the visible sample count exceeds the supported range.
pub fn compare_video_frames(
    reference: &VideoFrame,
    candidate: &VideoFrame,
) -> Result<FrameQualityReport> {
    if (reference.format, reference.width, reference.height)
        != (candidate.format, candidate.width, candidate.height)
        || reference.planes.len() != candidate.planes.len()
    {
        return Err(Error::InvalidData(
            "quality comparison requires matching frame formats and dimensions".into(),
        ));
    }
    let mut planes = Vec::with_capacity(reference.planes.len());
    let mut total_squared_error = 0.0_f64;
    let mut total_samples = 0_u32;
    let mut maximum_absolute_error = 0_u8;
    for (plane_index, (reference_plane, candidate_plane)) in
        reference.planes.iter().zip(&candidate.planes).enumerate()
    {
        let (squared_error, sample_count, maximum_error) =
            compare_planes(reference_plane, candidate_plane)?;
        let mean_squared_error = squared_error / f64::from(sample_count);
        let sample_count_usize = usize::try_from(sample_count).map_err(|_| {
            Error::Unsupported("quality sample count does not fit address space".into())
        })?;
        planes.push(PlaneQualityReport {
            plane_index,
            sample_count: sample_count_usize,
            mean_squared_error,
            psnr: psnr_from_mse(mean_squared_error),
            maximum_absolute_error: maximum_error,
        });
        total_squared_error += squared_error;
        total_samples = total_samples.checked_add(sample_count).ok_or_else(|| {
            Error::Unsupported("quality comparison exceeds 2^32 visible samples".into())
        })?;
        maximum_absolute_error = maximum_absolute_error.max(maximum_error);
    }
    if total_samples == 0 {
        return Err(Error::InvalidData(
            "quality comparison contains no visible samples".into(),
        ));
    }
    let mean_squared_error = total_squared_error / f64::from(total_samples);
    Ok(FrameQualityReport {
        planes,
        mean_squared_error,
        psnr: psnr_from_mse(mean_squared_error),
        maximum_absolute_error,
    })
}

fn compare_planes(reference: &Plane, candidate: &Plane) -> Result<(f64, u32, u8)> {
    let reference_storage = reference
        .stride
        .checked_mul(reference.height)
        .ok_or_else(|| Error::InvalidData("reference plane storage size overflows".into()))?;
    let candidate_storage = candidate
        .stride
        .checked_mul(candidate.height)
        .ok_or_else(|| Error::InvalidData("candidate plane storage size overflows".into()))?;
    if (reference.width, reference.height) != (candidate.width, candidate.height)
        || reference.stride < reference.width
        || candidate.stride < candidate.width
        || reference.data.len() < reference_storage
        || candidate.data.len() < candidate_storage
    {
        return Err(Error::InvalidData(
            "quality comparison requires matching valid plane layouts".into(),
        ));
    }
    let visible_samples = reference
        .width
        .checked_mul(reference.height)
        .ok_or_else(|| Error::Unsupported("quality plane sample count overflows".into()))?;
    let sample_count = u32::try_from(visible_samples)
        .map_err(|_| Error::Unsupported("quality plane exceeds 2^32 samples".into()))?;
    if sample_count == 0 {
        return Err(Error::InvalidData(
            "quality comparison plane is empty".into(),
        ));
    }
    let mut squared_error = 0.0_f64;
    let mut maximum_error = 0_u8;
    for row in 0..reference.height {
        let reference_start = row * reference.stride;
        let candidate_start = row * candidate.stride;
        for (&left, &right) in reference.data[reference_start..reference_start + reference.width]
            .iter()
            .zip(&candidate.data[candidate_start..candidate_start + candidate.width])
        {
            let difference = f64::from(left) - f64::from(right);
            squared_error += difference * difference;
            maximum_error = maximum_error.max(left.abs_diff(right));
        }
    }
    Ok((squared_error, sample_count, maximum_error))
}

fn psnr_from_mse(mean_squared_error: f64) -> f64 {
    if mean_squared_error == 0.0 {
        f64::INFINITY
    } else {
        10.0 * ((255.0 * 255.0) / mean_squared_error).log10()
    }
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{
        ColorDescription, FieldOrder, FrameTiming, PixelFormat, Plane, VideoFrame,
    };

    use super::{compare_video_frames, psnr_u8};

    #[test]
    fn identical_planes_have_infinite_psnr() {
        assert!(psnr_u8(&[0, 64, 255], &[0, 64, 255]).unwrap().is_infinite());
    }

    #[test]
    fn reports_visible_frame_error_and_ignores_padding() {
        let frame = |data| VideoFrame {
            format: PixelFormat::Gray8,
            width: 2,
            height: 1,
            planes: vec![Plane {
                data,
                stride: 3,
                width: 2,
                height: 1,
            }],
            timing: FrameTiming::default(),
            color: ColorDescription::default(),
            field_order: FieldOrder::Progressive,
        };
        let report = compare_video_frames(&frame(vec![10, 20, 99]), &frame(vec![10, 22, 0]))
            .expect("compare frames");
        assert_eq!(report.maximum_absolute_error, 2);
        assert!((report.mean_squared_error - 2.0).abs() < f64::EPSILON);
    }
}
