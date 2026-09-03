use std::ops::Range;

use mmrecode_core::{Error, Packet, PacketFlags, Result, Timestamp};
use mmrecode_h264::{
    AvcDecoderConfigurationRecord, H264StreamIndexer, PictureTiming, PictureUnit,
    length_prefixed_nal_units,
};
use mmrecode_isobmff::{IsoBmffFile, mux_video_track};

/// An explainable packet-copy plan for one or more complete H.264 GOPs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H264CleanRemuxPlan {
    /// Source track identifier.
    pub track_id: u32,
    /// Requested half-open frame range in presentation order.
    pub presentation_frame_range: Range<usize>,
    /// Selected half-open sample range in decode order.
    pub sample_range: Range<usize>,
    /// Number of IDR-delimited GOPs copied.
    pub gop_count: usize,
    /// Encoded bytes copied without modification.
    pub copied_bytes: usize,
    /// Human-readable explanation of the decision.
    pub reason: String,
}

/// Plans a lossless H.264 MP4/MOV cut at closed IDR/sync boundaries.
///
/// `presentation_frame_range` uses zero-based, half-open display-frame indexes. This first remux
/// slice intentionally rejects cuts inside a GOP, samples without exactly one coded picture, and
/// ranges whose presentation selection is not contiguous in decode order.
///
/// # Errors
///
/// Returns an error for malformed H.264 syntax, an invalid range, or any boundary that cannot be
/// copied independently without re-encoding.
pub fn plan_h264_clean_remux(
    movie: &IsoBmffFile,
    presentation_frame_range: Range<usize>,
) -> Result<H264CleanRemuxPlan> {
    let track = movie
        .h264_track()
        .ok_or_else(|| Error::Unsupported("movie has no H.264 video track".into()))?;
    let pictures = index_pictures(movie)?;
    if pictures.len() != track.samples.len() {
        return Err(Error::Unsupported(
            "clean remux currently requires exactly one coded H.264 picture per MP4 sample".into(),
        ));
    }
    if presentation_frame_range.start >= presentation_frame_range.end
        || presentation_frame_range.end > pictures.len()
    {
        return Err(Error::InvalidData(format!(
            "presentation frame range {}..{} is outside 0..{}",
            presentation_frame_range.start,
            presentation_frame_range.end,
            pictures.len()
        )));
    }

    let start_picture = &pictures[presentation_frame_range.start];
    require_boundary(track, start_picture, "start")?;
    if presentation_frame_range.end < pictures.len() {
        require_boundary(track, &pictures[presentation_frame_range.end], "end")?;
    }

    let selected = &pictures[presentation_frame_range.clone()];
    let sample_start = selected
        .iter()
        .map(|picture| picture.sample_index)
        .min()
        .ok_or_else(|| Error::InvalidData("empty H.264 remux selection".into()))?;
    let sample_end = selected
        .iter()
        .map(|picture| picture.sample_index)
        .max()
        .ok_or_else(|| Error::InvalidData("empty H.264 remux selection".into()))?
        + 1;
    let mut selected_samples = selected
        .iter()
        .map(|picture| picture.sample_index)
        .collect::<Vec<_>>();
    selected_samples.sort_unstable();
    if selected_samples != (sample_start..sample_end).collect::<Vec<_>>() {
        return Err(Error::Unsupported(
            "selected presentation frames are not a contiguous decode-order sample range".into(),
        ));
    }
    if sample_start != start_picture.sample_index {
        return Err(Error::Unsupported(
            "start IDR is preceded in decode order by a selected reordered picture".into(),
        ));
    }
    if presentation_frame_range.end < pictures.len()
        && pictures[presentation_frame_range.end].sample_index != sample_end
    {
        return Err(Error::Unsupported(
            "end IDR does not immediately follow the selected decode-order samples".into(),
        ));
    }
    for picture in selected {
        if picture
            .dependencies
            .iter()
            .any(|dependency| !((sample_start..sample_end).contains(dependency)))
        {
            return Err(Error::Unsupported(format!(
                "sample {} references a picture outside the selected GOP range",
                picture.sample_index
            )));
        }
    }

    let gop_count = selected.iter().filter(|picture| picture.is_idr).count();
    let copied_bytes =
        track.samples[sample_start..sample_end]
            .iter()
            .try_fold(0_usize, |total, sample| {
                total
                    .checked_add(sample.source_range.len())
                    .ok_or_else(|| Error::InvalidData("copied-byte count overflows".into()))
            })?;
    Ok(H264CleanRemuxPlan {
        track_id: track.track_id,
        presentation_frame_range,
        sample_range: sample_start..sample_end,
        gop_count,
        copied_bytes,
        reason: format!(
            "copy {gop_count} complete IDR-delimited GOP(s); rewrite timestamps and MP4 sample tables; encode 0 frames"
        ),
    })
}

/// Executes a validated clean-GOP H.264 remux plan into a video-only MP4 image.
///
/// # Errors
///
/// Returns an error if the plan no longer matches the source or if packet timing/output tables
/// cannot be represented by the minimal MP4 writer.
pub fn execute_h264_clean_remux(movie: &IsoBmffFile, plan: &H264CleanRemuxPlan) -> Result<Vec<u8>> {
    let validated = plan_h264_clean_remux(movie, plan.presentation_frame_range.clone())?;
    if validated != *plan {
        return Err(Error::InvalidData(
            "H.264 remux plan does not match a fresh analysis of the source".into(),
        ));
    }
    let track = movie
        .h264_track()
        .ok_or_else(|| Error::Unsupported("movie has no H.264 video track".into()))?;
    if track.track_id != plan.track_id
        || plan.sample_range.start >= plan.sample_range.end
        || plan.sample_range.end > track.samples.len()
    {
        return Err(Error::InvalidData(
            "H.264 remux plan does not match the source track".into(),
        ));
    }
    let decode_origin = track.samples[plan.sample_range.start].dts;
    let presentation_origin = track.samples[plan.sample_range.clone()]
        .iter()
        .map(|sample| sample.pts)
        .min()
        .ok_or_else(|| Error::InvalidData("H.264 remux plan selects no samples".into()))?;
    let time_base = track.descriptor.time_base;
    let mut packets = Vec::with_capacity(plan.sample_range.len());
    for sample in &track.samples[plan.sample_range.clone()] {
        let dts = sample
            .dts
            .checked_sub(decode_origin)
            .ok_or_else(|| Error::InvalidData("rebased H.264 DTS underflows".into()))?;
        let pts = sample
            .pts
            .checked_sub(presentation_origin)
            .ok_or_else(|| Error::InvalidData("rebased H.264 PTS underflows".into()))?;
        if dts < 0 || pts < 0 {
            return Err(Error::Unsupported(
                "clean remux cannot normalize negative decode or presentation timestamps".into(),
            ));
        }
        let mut flags = PacketFlags::empty();
        if sample.is_sync {
            flags.insert(PacketFlags::KEY);
        }
        packets.push(Packet {
            stream_id: track.descriptor.id,
            data: movie.sample_data(sample)?.to_vec(),
            pts: Some(Timestamp {
                value: pts,
                time_base,
            }),
            dts: Some(Timestamp {
                value: dts,
                time_base,
            }),
            duration: Some(Timestamp {
                value: i64::from(sample.duration),
                time_base,
            }),
            flags,
            side_data: Vec::new(),
        });
    }
    mux_video_track(track, &packets)
}

fn index_pictures(movie: &IsoBmffFile) -> Result<Vec<PictureUnit>> {
    let track = movie
        .h264_track()
        .ok_or_else(|| Error::Unsupported("movie has no H.264 video track".into()))?;
    let configuration =
        AvcDecoderConfigurationRecord::parse(&track.descriptor.codec.configuration)?;
    let mut indexer = H264StreamIndexer::default();
    indexer.configure_avcc(&configuration)?;
    for (sample_index, sample) in track.samples.iter().enumerate() {
        let units =
            length_prefixed_nal_units(movie.sample_data(sample)?, configuration.length_size)?;
        indexer.push_access_unit(
            sample_index,
            PictureTiming {
                dts: sample.dts,
                pts: sample.pts,
                duration: sample.duration,
            },
            &units,
        )?;
    }
    let mut pictures = indexer
        .finish()
        .access_units
        .into_iter()
        .filter_map(|unit| unit.picture)
        .collect::<Vec<_>>();
    pictures.sort_by_key(|picture| (picture.timing.pts, picture.sample_index));
    Ok(pictures)
}

fn require_boundary(
    track: &mmrecode_isobmff::Track,
    picture: &PictureUnit,
    name: &str,
) -> Result<()> {
    if !picture.is_idr || !track.samples[picture.sample_index].is_sync {
        return Err(Error::Unsupported(format!(
            "{name} frame {} is not an IDR/sync GOP boundary; clean remux would require re-encoding",
            picture.sample_index
        )));
    }
    Ok(())
}
