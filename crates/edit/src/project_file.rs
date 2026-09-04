//! Versioned, human-readable persistence for recursive media projects.

use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
};

use mmrecode_core::{Error, Rational, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    MediaId, MediaKind, MediaLinkId, MediaOrigin, MediaProject, MmfxSource, ProjectColorSpace,
    ProjectScanMode, ProjectSettings, TimeRange, VisualScaleMode,
};

const FORMAT: &str = "mmrecode-project";
const VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct ProjectDocument {
    format: String,
    version: u32,
    project: ProjectRecord,
    media: Vec<MediaRecord>,
    links: Vec<LinkRecord>,
}

#[derive(Deserialize, Serialize)]
struct ProjectRecord {
    name: String,
    settings: SettingsRecord,
}

#[derive(Deserialize, Serialize)]
struct SettingsRecord {
    width: u32,
    height: u32,
    frame_rate: RationalRecord,
    pixel_aspect: RationalRecord,
    scan_mode: String,
    color_space: String,
    audio_sample_rate: u32,
    audio_channels: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_preset: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct RationalRecord {
    numerator: i64,
    denominator: i64,
}

#[derive(Deserialize, Serialize)]
struct MediaRecord {
    id: String,
    name: String,
    kind: String,
    time_base: RationalRecord,
    duration: i64,
    origin: OriginRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mmfx: Option<MmfxRecord>,
}

#[derive(Deserialize, Serialize)]
struct MmfxRecord {
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_base: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OriginRecord {
    Generated,
    Managed { path: String },
    External { path: String },
}

#[derive(Deserialize, Serialize)]
struct LinkRecord {
    id: String,
    parent: String,
    media: String,
    alias: String,
    source: RangeRecord,
    timeline: RangeRecord,
    #[serde(default = "default_scale_mode")]
    scale_mode: String,
}

fn default_scale_mode() -> String {
    "fit".into()
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct RangeRecord {
    start: i64,
    end: i64,
}

/// Saves a project as pretty, versioned JSON.
///
/// External media below the project-file directory is written as a managed relative path. Other
/// external locations remain explicit absolute links.
///
/// # Errors
///
/// Returns an error for non-UTF-8 paths, serialization failure, or filesystem failure.
pub fn save_project_file(project: &MediaProject, path: &Path) -> Result<()> {
    save_project_file_from(project, path, Some(path))
}

/// Saves a project while rebasing managed media from `source_project_path` for Save As.
///
/// A managed path that no longer lies below the destination directory becomes an explicit
/// absolute external link. This preserves its target instead of reinterpreting it relative to the
/// new project location.
///
/// # Errors
///
/// Returns the same errors as [`save_project_file`].
pub fn save_project_file_from(
    project: &MediaProject,
    path: &Path,
    source_project_path: Option<&Path>,
) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let source_directory = source_project_path.and_then(Path::parent);
    let document = ProjectDocument::from_project(project, directory, source_directory)?;
    let mut json = serde_json::to_string_pretty(&document)
        .map_err(|error| Error::InvalidData(format!("cannot serialize project: {error}")))?;
    json.push('\n');
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| Error::InvalidData("project path must have a UTF-8 file name".into()))?;
    let temporary = directory.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let write_result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::Io(std::io::Error::new(
            error.kind(),
            format!("cannot write project '{}': {error}", path.display()),
        )));
    }
    Ok(())
}

/// Loads and validates a versioned `MMRecode` project file.
///
/// # Errors
///
/// Returns an error for unreadable JSON, an unsupported format/version, noncanonical identifiers,
/// invalid settings, missing media references, or invalid placement ranges.
pub fn load_project_file(path: &Path) -> Result<MediaProject> {
    let bytes = std::fs::read(path).map_err(|error| {
        Error::Io(std::io::Error::new(
            error.kind(),
            format!("cannot read project '{}': {error}", path.display()),
        ))
    })?;
    let document: ProjectDocument = serde_json::from_slice(&bytes)
        .map_err(|error| Error::InvalidData(format!("cannot parse project JSON: {error}")))?;
    document.into_project()
}

impl ProjectDocument {
    fn from_project(
        project: &MediaProject,
        directory: &Path,
        source_directory: Option<&Path>,
    ) -> Result<Self> {
        let media = project
            .media_nodes()
            .filter(|media| media.id != project.root_id())
            .map(|media| {
                let mmfx = if let Some(mmfx) = &media.mmfx {
                    Some(MmfxRecord {
                        source: mmfx.source.clone(),
                        resource_base: mmfx.resource_base.as_deref().map(path_text).transpose()?,
                    })
                } else {
                    None
                };
                Ok(MediaRecord {
                    id: media_id(media.id),
                    name: media.name.clone(),
                    kind: media.kind.as_str().into(),
                    time_base: media.time_base.into(),
                    duration: media.duration.value,
                    origin: OriginRecord::from_origin(&media.origin, directory, source_directory)?,
                    mmfx,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let links = project
            .placement_links()
            .map(|link| LinkRecord {
                id: link_id(link.id),
                parent: media_id(link.parent_id),
                media: media_id(link.media_id),
                alias: link.alias.clone(),
                source: link.source_range.into(),
                timeline: link.timeline_range.into(),
                scale_mode: link.scale_mode.as_str().into(),
            })
            .collect();
        Ok(Self {
            format: FORMAT.into(),
            version: VERSION,
            project: ProjectRecord {
                name: project.name.clone(),
                settings: project.settings().into(),
            },
            media,
            links,
        })
    }

    fn into_project(self) -> Result<MediaProject> {
        if self.format != FORMAT {
            return Err(Error::InvalidData(format!(
                "unsupported project format '{}'; expected '{FORMAT}'",
                self.format
            )));
        }
        if self.version != VERSION {
            return Err(Error::Unsupported(format!(
                "project version {} is not supported; this build supports version {VERSION}",
                self.version
            )));
        }
        let settings = self.project.settings.try_into()?;
        let mut project = MediaProject::with_settings(self.project.name, settings)?;
        let mut media_ids = BTreeMap::from([(1_u64, project.root_id())]);
        for (index, media) in self.media.into_iter().enumerate() {
            let serialized_id = parse_id(&media.id, 'm')?;
            let expected = u64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(2))
                .ok_or_else(|| Error::InvalidData("project media identifier overflows".into()))?;
            if serialized_id != expected {
                return Err(Error::InvalidData(format!(
                    "project media identifiers must be canonical; expected m{expected}, found {}",
                    media.id
                )));
            }
            let time_base = media.time_base.try_into()?;
            let mmfx = media.mmfx;
            let id = project.create_media(
                media.name,
                MediaKind::new(media.kind)?,
                time_base,
                media.duration,
                media.origin.into_origin()?,
            )?;
            if let Some(mmfx) = mmfx {
                project.set_mmfx_source(id, mmfx.into_source()?)?;
            }
            media_ids.insert(serialized_id, id);
        }
        for (index, link) in self.links.into_iter().enumerate() {
            let serialized_id = parse_id(&link.id, 'l')?;
            let expected = u64::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(1))
                .ok_or_else(|| Error::InvalidData("project link identifier overflows".into()))?;
            if serialized_id != expected {
                return Err(Error::InvalidData(format!(
                    "project link identifiers must be canonical; expected l{expected}, found {}",
                    link.id
                )));
            }
            let parent_serialized = parse_id(&link.parent, 'm')?;
            let media_serialized = parse_id(&link.media, 'm')?;
            let parent_id = *media_ids.get(&parent_serialized).ok_or_else(|| {
                Error::InvalidData(format!(
                    "link {} has missing parent {}",
                    link.id, link.parent
                ))
            })?;
            let media_id = *media_ids.get(&media_serialized).ok_or_else(|| {
                Error::InvalidData(format!("link {} has missing media {}", link.id, link.media))
            })?;
            let parent_time_base = project
                .media(parent_id)
                .ok_or_else(|| Error::InvalidState("loaded project parent disappeared".into()))?
                .time_base;
            let media_time_base = project
                .media(media_id)
                .ok_or_else(|| Error::InvalidState("loaded project media disappeared".into()))?
                .time_base;
            let id = project.link_media(
                parent_id,
                media_id,
                link.alias,
                link.source.into_range(media_time_base)?,
                link.timeline.into_range(parent_time_base)?,
            )?;
            let scale_mode = match link.scale_mode.as_str() {
                "fit" => VisualScaleMode::Fit,
                "fill" => VisualScaleMode::Fill,
                "stretch" => VisualScaleMode::Stretch,
                "native" => VisualScaleMode::Native,
                value => {
                    return Err(Error::InvalidData(format!(
                        "link {} has unknown visual scale mode '{value}'",
                        link.id
                    )));
                }
            };
            project.set_link_scale_mode(id, scale_mode)?;
            if id != MediaLinkId(serialized_id) {
                return Err(Error::InvalidState(
                    "project link allocation was not deterministic".into(),
                ));
            }
        }
        Ok(project)
    }
}

impl MmfxRecord {
    fn into_source(self) -> Result<MmfxSource> {
        let resource_base = self.resource_base.map(PathBuf::from);
        if resource_base
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(Error::InvalidData(
                "MMFX external resource base must be absolute".into(),
            ));
        }
        Ok(MmfxSource {
            source: self.source,
            resource_base,
        })
    }
}

impl OriginRecord {
    fn from_origin(
        origin: &MediaOrigin,
        directory: &Path,
        source_directory: Option<&Path>,
    ) -> Result<Self> {
        match origin {
            MediaOrigin::Generated => Ok(Self::Generated),
            MediaOrigin::Managed { path } => {
                let resolved =
                    source_directory.map_or_else(|| path.clone(), |base| base.join(path));
                if let Ok(relative) = resolved.strip_prefix(directory) {
                    Ok(Self::Managed {
                        path: path_text(relative)?,
                    })
                } else if resolved.is_absolute() {
                    Ok(Self::External {
                        path: path_text(&resolved)?,
                    })
                } else {
                    Ok(Self::Managed {
                        path: path_text(path)?,
                    })
                }
            }
            MediaOrigin::External { path } => {
                if let Ok(relative) = path.strip_prefix(directory) {
                    Ok(Self::Managed {
                        path: path_text(relative)?,
                    })
                } else if !path.is_absolute() {
                    Err(Error::InvalidData(
                        "external media path must be absolute".into(),
                    ))
                } else {
                    Ok(Self::External {
                        path: path_text(path)?,
                    })
                }
            }
        }
    }

    fn into_origin(self) -> Result<MediaOrigin> {
        match self {
            Self::Generated => Ok(MediaOrigin::Generated),
            Self::Managed { path } => {
                let path = std::path::PathBuf::from(path);
                if path.is_absolute()
                    || path
                        .components()
                        .any(|component| component == std::path::Component::ParentDir)
                {
                    return Err(Error::InvalidData(
                        "managed media path must remain inside the project directory".into(),
                    ));
                }
                Ok(MediaOrigin::Managed { path })
            }
            Self::External { path } => {
                let path = std::path::PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(Error::InvalidData(
                        "external media path must be absolute".into(),
                    ));
                }
                Ok(MediaOrigin::External { path })
            }
        }
    }
}

impl From<&ProjectSettings> for SettingsRecord {
    fn from(settings: &ProjectSettings) -> Self {
        Self {
            width: settings.width,
            height: settings.height,
            frame_rate: settings.frame_rate.into(),
            pixel_aspect: settings.pixel_aspect.into(),
            scan_mode: match settings.scan_mode {
                ProjectScanMode::Progressive => "progressive",
                ProjectScanMode::Interlaced => "interlaced",
            }
            .into(),
            color_space: match settings.color_space {
                ProjectColorSpace::Rec709 => "rec709",
                ProjectColorSpace::Srgb => "srgb",
                ProjectColorSpace::Rec2020 => "rec2020",
            }
            .into(),
            audio_sample_rate: settings.audio_sample_rate,
            audio_channels: settings.audio_channels,
            base_preset: settings.base_preset.clone(),
        }
    }
}

impl TryFrom<SettingsRecord> for ProjectSettings {
    type Error = Error;

    fn try_from(settings: SettingsRecord) -> Result<Self> {
        let scan_mode = match settings.scan_mode.as_str() {
            "progressive" => ProjectScanMode::Progressive,
            "interlaced" => ProjectScanMode::Interlaced,
            value => {
                return Err(Error::InvalidData(format!(
                    "unknown project scan mode '{value}'"
                )));
            }
        };
        let color_space = match settings.color_space.as_str() {
            "rec709" => ProjectColorSpace::Rec709,
            "srgb" => ProjectColorSpace::Srgb,
            "rec2020" => ProjectColorSpace::Rec2020,
            value => {
                return Err(Error::InvalidData(format!(
                    "unknown project color space '{value}'"
                )));
            }
        };
        let value = Self {
            width: settings.width,
            height: settings.height,
            frame_rate: settings.frame_rate.try_into()?,
            pixel_aspect: settings.pixel_aspect.try_into()?,
            scan_mode,
            color_space,
            audio_sample_rate: settings.audio_sample_rate,
            audio_channels: settings.audio_channels,
            base_preset: settings.base_preset,
        };
        value.validate()?;
        Ok(value)
    }
}

impl From<Rational> for RationalRecord {
    fn from(value: Rational) -> Self {
        Self {
            numerator: value.numerator(),
            denominator: value.denominator(),
        }
    }
}

impl TryFrom<RationalRecord> for Rational {
    type Error = Error;

    fn try_from(value: RationalRecord) -> Result<Self> {
        Rational::new(value.numerator, value.denominator)
    }
}

impl From<TimeRange> for RangeRecord {
    fn from(value: TimeRange) -> Self {
        Self {
            start: value.start.value,
            end: value.end.value,
        }
    }
}

impl RangeRecord {
    fn into_range(self, time_base: Rational) -> Result<TimeRange> {
        TimeRange::new(
            Timestamp {
                value: self.start,
                time_base,
            },
            Timestamp {
                value: self.end,
                time_base,
            },
        )
    }
}

fn media_id(id: MediaId) -> String {
    format!("m{}", id.0)
}

fn link_id(id: MediaLinkId) -> String {
    format!("l{}", id.0)
}

fn parse_id(value: &str, prefix: char) -> Result<u64> {
    value
        .strip_prefix(prefix)
        .ok_or_else(|| Error::InvalidData(format!("invalid project identifier '{value}'")))?
        .parse()
        .map_err(|_| Error::InvalidData(format!("invalid project identifier '{value}'")))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Error::Unsupported(format!("project path '{}' is not UTF-8", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_file_round_trips_settings_graph_and_relative_media() {
        let directory =
            std::env::temp_dir().join(format!("mmrecode-project-file-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("media")).unwrap();
        let path = directory.join("film.mmrecode");
        let mut project =
            MediaProject::from_preset("Film", "youtube-1080p30").expect("create project");
        let time_base = Rational::new(1, 30).unwrap();
        let media_id = project
            .create_media(
                "source.ts",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                90,
                MediaOrigin::External {
                    path: directory.join("media/source.ts"),
                },
            )
            .unwrap();
        let link_id = project
            .link_media(
                project.root_id(),
                media_id,
                "Clip0",
                RangeRecord { start: 3, end: 80 }
                    .into_range(time_base)
                    .unwrap(),
                RangeRecord { start: 0, end: 77 }
                    .into_range(time_base)
                    .unwrap(),
            )
            .unwrap();
        project
            .set_link_scale_mode(link_id, VisualScaleMode::Fill)
            .unwrap();
        let fx_id = project
            .create_media(
                "LowerThird",
                MediaKind::new("fx").unwrap(),
                time_base,
                60,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .set_mmfx_source(
                fx_id,
                MmfxSource {
                    source: "@scene LowerThird { width: 1920px; height: 1080px; }".into(),
                    resource_base: Some(directory.join("fx")),
                },
            )
            .unwrap();

        save_project_file(&project, &path).unwrap();
        let loaded = load_project_file(&path).unwrap();
        assert_eq!(loaded.settings(), project.settings());
        assert_eq!(loaded.name, "Film");
        let media = loaded.media(MediaId(2)).unwrap();
        assert_eq!(
            media.origin,
            MediaOrigin::Managed {
                path: "media/source.ts".into()
            }
        );
        assert_eq!(loaded.placement_links().count(), 1);
        assert_eq!(
            loaded.link(MediaLinkId(1)).unwrap().scale_mode,
            VisualScaleMode::Fill
        );
        assert_eq!(
            loaded.media(MediaId(3)).unwrap().mmfx.as_ref().unwrap(),
            &MmfxSource {
                source: "@scene LowerThird { width: 1920px; height: 1080px; }".into(),
                resource_base: Some(directory.join("fx")),
            }
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_as_preserves_the_target_of_managed_media() {
        let directory =
            std::env::temp_dir().join(format!("mmrecode-project-rebase-{}", std::process::id()));
        let original_directory = directory.join("original");
        let destination_directory = directory.join("destination");
        std::fs::create_dir_all(&original_directory).unwrap();
        std::fs::create_dir_all(&destination_directory).unwrap();
        let original_project = original_directory.join("film.mmrecode");
        let destination_project = destination_directory.join("film.mmrecode");
        let mut project = MediaProject::from_preset("Film", "web-1080p30").unwrap();
        project
            .create_media(
                "source.ts",
                MediaKind::new("video/mpeg2").unwrap(),
                Rational::new(1, 30).unwrap(),
                90,
                MediaOrigin::Managed {
                    path: "media/source.ts".into(),
                },
            )
            .unwrap();

        save_project_file_from(&project, &destination_project, Some(&original_project)).unwrap();
        let loaded = load_project_file(&destination_project).unwrap();
        assert_eq!(
            loaded.media(MediaId(2)).unwrap().origin,
            MediaOrigin::External {
                path: original_directory.join("media/source.ts")
            }
        );

        std::fs::remove_dir_all(directory).unwrap();
    }
}
