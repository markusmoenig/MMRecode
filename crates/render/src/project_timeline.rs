//! Recursive projection of linked media into one hierarchy context.
//!
//! The editor keeps timelines local to media definitions. Renderers instead need a flat list whose
//! ranges use the selected context's time base. This module performs that projection once while
//! retaining each placement path, so frame mapping remains exact even when nested media use
//! different time bases.

use std::ops::Range;

use mmrecode_core::{Error, Result, Timestamp, TimestampRounding};
use mmrecode_edit::{MediaId, MediaLinkId, MediaProject, VisualScaleMode};

/// One recursively flattened project placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenedProjectPlacement {
    /// Media definition reached by `link_path`.
    pub media_id: MediaId,
    /// Placement links from the flattened context to this media definition.
    pub link_path: Vec<MediaLinkId>,
    /// Slash-prefixed aliases suitable for reports and diagnostics.
    pub display_path: String,
    /// Active half-open interval in the flattened context's time base.
    pub timeline_range: Range<i64>,
    /// Source frames of the media definition reachable through `timeline_range`.
    pub source_range: Range<i64>,
    /// Visual scaling requested by the final placement link.
    pub scale_mode: VisualScaleMode,
    /// Stable depth-first composition order shared by video and generated objects.
    pub composition_order: usize,
    context_id: MediaId,
}

impl FlattenedProjectPlacement {
    /// Maps one context frame to this placement's exact media-local source frame.
    ///
    /// Returns `None` outside the active range. Each link uses floor rescaling, matching ordinary
    /// playback sampling while avoiding a lossy precomputed floating-point transform.
    ///
    /// # Errors
    ///
    /// Returns an error if the project graph changed or timestamp arithmetic overflows.
    pub fn source_frame_at(
        &self,
        project: &MediaProject,
        context_frame: i64,
    ) -> Result<Option<i64>> {
        if !self.timeline_range.contains(&context_frame) {
            return Ok(None);
        }
        map_context_frame(project, self.context_id, &self.link_path, context_frame)
    }
}

/// Flattens every placement below `context` into deterministic depth-first composition order.
///
/// A non-root context contributes its own source before its children. The project root is only a
/// timeline container and is not emitted. Reused media definitions are emitted once per placement
/// path, while the reusable source itself remains identifiable by `media_id` for caching.
///
/// # Errors
///
/// Returns an error for a corrupted graph, disconnected link, or overflowing timestamp mapping.
pub fn flatten_project_timeline(
    project: &MediaProject,
    context: MediaId,
) -> Result<Vec<FlattenedProjectPlacement>> {
    let media = project.media(context).ok_or_else(|| {
        Error::InvalidData(format!("cannot flatten missing project media {context:?}"))
    })?;
    let mut placements = Vec::new();
    let mut link_path = Vec::new();
    let mut aliases = Vec::new();
    flatten_media(
        project,
        context,
        context,
        0..media.duration.value,
        context != project.root_id(),
        &mut link_path,
        &mut aliases,
        &mut placements,
    )?;
    Ok(placements)
}

#[allow(clippy::too_many_arguments)]
fn flatten_media(
    project: &MediaProject,
    context: MediaId,
    media_id: MediaId,
    active_context: Range<i64>,
    include_self: bool,
    link_path: &mut Vec<MediaLinkId>,
    aliases: &mut Vec<String>,
    placements: &mut Vec<FlattenedProjectPlacement>,
) -> Result<()> {
    if active_context.start >= active_context.end {
        return Ok(());
    }
    let media = project
        .media(media_id)
        .ok_or_else(|| Error::InvalidState(format!("flattened media {media_id:?} disappeared")))?;
    if include_self {
        let source_start = map_context_frame(project, context, link_path, active_context.start)?
            .ok_or_else(|| {
                Error::InvalidState("active placement has no first source frame".into())
            })?;
        let source_last = map_context_frame(
            project,
            context,
            link_path,
            active_context.end.saturating_sub(1),
        )?
        .ok_or_else(|| Error::InvalidState("active placement has no last source frame".into()))?;
        let source_end = source_last
            .checked_add(1)
            .ok_or_else(|| Error::InvalidData("flattened source range overflows".into()))?;
        placements.push(FlattenedProjectPlacement {
            media_id,
            link_path: link_path.clone(),
            display_path: format!("/{}", aliases.join("/")),
            timeline_range: active_context.clone(),
            source_range: source_start..source_end,
            scale_mode: link_path
                .last()
                .and_then(|link_id| project.link(*link_id))
                .map_or(VisualScaleMode::Stretch, |link| link.scale_mode),
            composition_order: placements.len(),
            context_id: context,
        });
    }

    for link_id in media.children() {
        let link = project.link(*link_id).ok_or_else(|| {
            Error::InvalidState(format!(
                "media {media_id:?} references missing link {link_id:?}"
            ))
        })?;
        if link.parent_id != media_id {
            return Err(Error::InvalidState(format!(
                "media {media_id:?} contains disconnected link {link_id:?}"
            )));
        }
        let child_start = first_context_frame_at_or_after(
            project,
            context,
            link_path,
            active_context.clone(),
            link.timeline_range.start.value,
        )?;
        let child_end = first_context_frame_at_or_after(
            project,
            context,
            link_path,
            active_context.clone(),
            link.timeline_range.end.value,
        )?;
        if child_start >= child_end {
            continue;
        }
        link_path.push(*link_id);
        aliases.push(link.alias.clone());
        flatten_media(
            project,
            context,
            link.media_id,
            child_start..child_end,
            true,
            link_path,
            aliases,
            placements,
        )?;
        aliases.pop();
        link_path.pop();
    }
    Ok(())
}

fn first_context_frame_at_or_after(
    project: &MediaProject,
    context: MediaId,
    link_path: &[MediaLinkId],
    range: Range<i64>,
    target_local_frame: i64,
) -> Result<i64> {
    let mut low = range.start;
    let mut high = range.end;
    while low < high {
        let midpoint = low + (high - low) / 2;
        let local = map_context_frame(project, context, link_path, midpoint)?.ok_or_else(|| {
            Error::InvalidState("active parent range no longer maps to its media".into())
        })?;
        if local < target_local_frame {
            low = midpoint
                .checked_add(1)
                .ok_or_else(|| Error::InvalidData("timeline search overflows".into()))?;
        } else {
            high = midpoint;
        }
    }
    Ok(low)
}

fn map_context_frame(
    project: &MediaProject,
    context: MediaId,
    link_path: &[MediaLinkId],
    context_frame: i64,
) -> Result<Option<i64>> {
    let mut media_id = context;
    let mut local_frame = context_frame;
    for link_id in link_path {
        let parent = project.media(media_id).ok_or_else(|| {
            Error::InvalidState(format!("timeline mapping lost parent media {media_id:?}"))
        })?;
        let link = project.link(*link_id).ok_or_else(|| {
            Error::InvalidState(format!("timeline mapping lost link {link_id:?}"))
        })?;
        if link.parent_id != media_id {
            return Err(Error::InvalidState(format!(
                "timeline mapping encountered disconnected link {link_id:?}"
            )));
        }
        if local_frame < link.timeline_range.start.value
            || local_frame >= link.timeline_range.end.value
        {
            return Ok(None);
        }
        let child = project.media(link.media_id).ok_or_else(|| {
            Error::InvalidState(format!(
                "timeline mapping lost child media {:?}",
                link.media_id
            ))
        })?;
        let offset = local_frame
            .checked_sub(link.timeline_range.start.value)
            .ok_or_else(|| Error::InvalidData("timeline offset overflows".into()))?;
        let source_offset = Timestamp {
            value: offset,
            time_base: parent.time_base,
        }
        .rescale(child.time_base, TimestampRounding::Floor)?
        .value;
        local_frame = link
            .source_range
            .start
            .value
            .checked_add(source_offset)
            .ok_or_else(|| Error::InvalidData("source frame mapping overflows".into()))?;
        if local_frame < link.source_range.start.value || local_frame >= link.source_range.end.value
        {
            return Ok(None);
        }
        media_id = link.media_id;
    }
    Ok(Some(local_frame))
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{Rational, Timestamp};
    use mmrecode_edit::{MediaKind, MediaOrigin, MediaProject, TimeRange};

    use super::*;

    fn range(start: i64, end: i64, time_base: Rational) -> TimeRange {
        TimeRange::new(
            Timestamp {
                value: start,
                time_base,
            },
            Timestamp {
                value: end,
                time_base,
            },
        )
        .unwrap()
    }

    #[test]
    fn flattens_nested_media_in_depth_first_composition_order() {
        let mut project = MediaProject::new("nested", Rational::new(1, 30).unwrap()).unwrap();
        let root = project.root_id();
        let time_base = project.media(root).unwrap().time_base;
        let video = project
            .create_media(
                "clip",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                10,
                MediaOrigin::Generated,
            )
            .unwrap();
        let video_link = project
            .link_media(
                root,
                video,
                "Clip",
                range(0, 10, time_base),
                range(5, 15, time_base),
            )
            .unwrap();
        let fx = project
            .create_media(
                "lower third",
                MediaKind::new("fx").unwrap(),
                time_base,
                4,
                MediaOrigin::Generated,
            )
            .unwrap();
        let fx_link = project
            .link_media(
                video,
                fx,
                "Lower",
                range(0, 4, time_base),
                range(2, 6, time_base),
            )
            .unwrap();

        let flattened = flatten_project_timeline(&project, root).unwrap();
        assert_eq!(flattened.len(), 2);
        assert_eq!(flattened[0].media_id, video);
        assert_eq!(flattened[0].link_path, vec![video_link]);
        assert_eq!(flattened[0].timeline_range, 5..15);
        assert_eq!(flattened[0].source_range, 0..10);
        assert_eq!(flattened[1].media_id, fx);
        assert_eq!(flattened[1].link_path, vec![video_link, fx_link]);
        assert_eq!(flattened[1].display_path, "/Clip/Lower");
        assert_eq!(flattened[1].timeline_range, 7..11);
        assert_eq!(flattened[1].source_range, 0..4);
        assert_eq!(flattened[1].composition_order, 1);
        assert_eq!(flattened[1].source_frame_at(&project, 9).unwrap(), Some(2));
        assert_eq!(flattened[1].source_frame_at(&project, 11).unwrap(), None);
    }

    #[test]
    fn clips_descendants_to_an_ancestor_source_trim() {
        let mut project = MediaProject::new("trimmed", Rational::new(1, 30).unwrap()).unwrap();
        let root = project.root_id();
        let time_base = project.media(root).unwrap().time_base;
        let video = project
            .create_media(
                "clip",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                10,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .link_media(
                root,
                video,
                "Clip",
                range(3, 9, time_base),
                range(5, 11, time_base),
            )
            .unwrap();
        let fx = project
            .create_media(
                "fx",
                MediaKind::new("fx").unwrap(),
                time_base,
                4,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .link_media(
                video,
                fx,
                "FX",
                range(0, 4, time_base),
                range(2, 6, time_base),
            )
            .unwrap();

        let flattened = flatten_project_timeline(&project, root).unwrap();
        assert_eq!(flattened[1].timeline_range, 5..8);
        assert_eq!(flattened[1].source_range, 1..4);
    }

    #[test]
    fn preserves_exact_sampling_across_different_time_bases() {
        let mut project = MediaProject::new("rates", Rational::new(1, 30).unwrap()).unwrap();
        let root = project.root_id();
        let root_time_base = project.media(root).unwrap().time_base;
        let sixty_fps = Rational::new(1, 60).unwrap();
        let video = project
            .create_media(
                "60 fps",
                MediaKind::new("video/mpeg2").unwrap(),
                sixty_fps,
                20,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .link_media(
                root,
                video,
                "Fast",
                range(0, 20, sixty_fps),
                range(0, 10, root_time_base),
            )
            .unwrap();
        let fx = project
            .create_media(
                "fx",
                MediaKind::new("fx").unwrap(),
                sixty_fps,
                4,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .link_media(
                video,
                fx,
                "FX",
                range(0, 4, sixty_fps),
                range(4, 8, sixty_fps),
            )
            .unwrap();

        let flattened = flatten_project_timeline(&project, root).unwrap();
        assert_eq!(flattened[0].source_frame_at(&project, 3).unwrap(), Some(6));
        assert_eq!(flattened[1].timeline_range, 2..4);
        assert_eq!(flattened[1].source_frame_at(&project, 3).unwrap(), Some(2));
    }
}
