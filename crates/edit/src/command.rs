//! Typed editor commands shared by scripts and interactive frontends.

use std::{fmt::Write as _, path::PathBuf};

use mmrecode_core::{Error, Rational, Result, Timestamp, TimestampRounding};

use crate::{
    MediaKind, MediaListing, MediaOrigin, MediaPath, MediaProject, ProjectColorSpace,
    ProjectRateConformPolicy, ProjectScanMode, ProjectSettings, TimeRange, VisualScaleMode,
};

/// Primary editor commands accepted by scripts and the interactive prompt.
pub const EDITOR_COMMAND_NAMES: &[&str] = &[
    "add", "cd", "export", "help", "import", "in", "info", "ls", "man", "new", "open", "out",
    "project", "pwd", "quit", "redo", "save", "scale", "undo",
];

/// Every topic accepted by `man`, including interactive context commands.
pub const EDITOR_MANUAL_TOPICS: &[&str] = &[
    "add", "cd", "export", "help", "import", "in", "info", "left", "ls", "man", "move", "new",
    "open", "out", "project", "pwd", "quit", "redo", "right", "save", "scale", "undo",
];

/// Stable field names accepted by `project set`.
pub const PROJECT_SETTING_NAMES: &[&str] = &[
    "size",
    "rate",
    "pixel-aspect",
    "scan",
    "color",
    "audio-rate",
    "audio-channels",
];

/// Stable delivery preset names exposed by the editor language.
pub const EXPORT_PRESET_NAMES: &[&str] = &["mpeg2-ts", "youtube-1080p", "youtube-2160p"];

/// Absolute or relative editor position.
///
/// Compact timecode is counted from the right: `1:15` is one second and fifteen frames,
/// `2:01:15` is two minutes, one second, and fifteen frames. Raw frame counts remain available for
/// compatibility with early scripts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameValue {
    /// A raw frame count in the current media's native time base.
    Frames {
        /// Signed frame count.
        frames: i64,
        /// Whether the count is an offset from the current value.
        relative: bool,
    },
    /// A compact, non-drop timecode value.
    Timecode {
        /// Whole seconds represented by every field except the rightmost one.
        seconds: u64,
        /// Rightmost frame field.
        frames: u64,
        /// Whether the complete value is negative.
        negative: bool,
        /// Whether the value is an offset from the current value.
        relative: bool,
    },
}

impl FrameValue {
    fn is_relative(self) -> bool {
        match self {
            Self::Frames { relative, .. } | Self::Timecode { relative, .. } => relative,
        }
    }

    fn resolve(self, time_base: Rational) -> Result<i64> {
        match self {
            Self::Frames { frames, .. } => Ok(frames),
            Self::Timecode {
                seconds,
                frames,
                negative,
                ..
            } => {
                let frames_per_second = u64::try_from(nominal_frame_rate(time_base)?)
                    .map_err(|_| Error::InvalidData("frame rate exceeds editor limits".into()))?;
                if frames >= frames_per_second {
                    return Err(Error::InvalidData(format!(
                        "timecode frame field must be below {frames_per_second} at this media rate"
                    )));
                }
                let magnitude = seconds
                    .checked_mul(frames_per_second)
                    .and_then(|value| value.checked_add(frames))
                    .ok_or_else(|| Error::InvalidData("timecode exceeds editor limits".into()))?;
                let magnitude = i64::try_from(magnitude)
                    .map_err(|_| Error::InvalidData("timecode exceeds editor limits".into()))?;
                if negative {
                    magnitude
                        .checked_neg()
                        .ok_or_else(|| Error::InvalidData("timecode exceeds editor limits".into()))
                } else {
                    Ok(magnitude)
                }
            }
        }
    }
}

/// Host-resolved description of media ready to enter the authoring graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedMedia {
    /// Human-facing reusable media name.
    pub name: String,
    /// Placement alias in the current local timeline.
    pub alias: String,
    /// Extensible media kind discovered by the importer.
    pub kind: MediaKind,
    /// Native frame/sample time base.
    pub time_base: Rational,
    /// Positive native duration.
    pub duration: i64,
    /// Managed or external source location.
    pub origin: MediaOrigin,
}

/// Versionable typed command understood by every editor frontend.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EditCommand {
    /// Start a new empty project.
    NewProject {
        /// Human-facing project name.
        name: String,
        /// Built-in authoring preset.
        preset: String,
        /// Permit replacing a project with unsaved changes.
        discard: bool,
    },
    /// Ask the host to load a project document.
    OpenProject {
        /// User-entered project locator.
        locator: String,
        /// Permit replacing a project with unsaved changes.
        discard: bool,
    },
    /// Ask the host to persist the current project.
    SaveProject {
        /// New project locator for `save as`, or `None` for the current project file.
        locator: Option<String>,
    },
    /// Ask the host to resolve real media and import it into the current local timeline.
    Import {
        /// User-entered media locator, normally a filesystem path.
        locator: String,
        /// Optional placement alias; the host derives one when omitted.
        alias: Option<String>,
    },
    /// Show all built-in authoring presets.
    ProjectPresets,
    /// Replace project settings from a built-in authoring preset.
    ProjectPreset {
        /// Stable preset name.
        preset: String,
    },
    /// Change one resolved project authoring setting.
    ProjectSet {
        /// Stable field name.
        field: String,
        /// User-entered field value.
        value: String,
        /// Root-timeline policy when `field` is `rate`.
        rate_conform: Option<ProjectRateConformPolicy>,
    },
    /// Ask the host to probe the focused media and adopt its technical format as project settings.
    ProjectMatch,
    /// Ask the host for a dry-run export plan.
    ExportPlan {
        /// Delivery preset; defaults to the first supported project export.
        preset: Option<String>,
    },
    /// Ask the host to execute an export.
    Export {
        /// Output file locator.
        locator: String,
        /// Delivery preset; may be inferred from the output extension.
        preset: Option<String>,
    },
    /// Show the current placement path.
    Pwd,
    /// List child media in the current local timeline.
    List,
    /// Inspect the current media and placement.
    Info,
    /// Request a kind-specific information view from the active frontend.
    InfoTopic {
        /// Information category such as `project`, `video`, `audio`, or `source`.
        topic: String,
    },
    /// Traverse placement aliases, `..`, or an absolute path.
    Cd {
        /// Placement path, alias, index, `.`, or `..` expression.
        path: String,
    },
    /// Create generated media in the current local timeline.
    Add {
        /// Extensible media kind.
        kind: MediaKind,
        /// New placement alias.
        alias: String,
        /// Positive duration in the current local time base.
        duration: FrameValue,
        /// Non-negative start in the current local time base.
        start: FrameValue,
    },
    /// Change the current placement's in-point.
    TrimIn {
        /// Absolute or relative child-source frame value.
        value: FrameValue,
    },
    /// Change the current placement's out-point.
    TrimOut {
        /// Absolute or relative child-source frame value.
        value: FrameValue,
    },
    /// Change how the current visual placement maps into its parent canvas.
    SetScaleMode {
        /// Aspect, crop, and padding policy.
        mode: VisualScaleMode,
    },
    /// Restore the previous project state.
    Undo,
    /// Reapply an undone project state.
    Redo,
    /// Show the initial editor command vocabulary.
    Help,
    /// Show detailed help for one editor command.
    Man {
        /// Command whose manual page should be displayed.
        command: String,
    },
    /// Ask the active frontend to end the session.
    Quit {
        /// Permit discarding unsaved changes.
        discard: bool,
    },
}

/// Structured result of applying one editor command.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandOutput {
    /// Command completed without printable output.
    None,
    /// Human-readable inspection text.
    Text(String),
    /// Child media in current composition order.
    Listing(Vec<MediaListing>),
    /// The application host must probe and resolve this media request.
    ImportRequested {
        /// User-entered media locator.
        locator: String,
        /// Optional requested placement alias.
        alias: Option<String>,
    },
    /// The application host must probe the currently focused media's technical format.
    ProjectMatchRequested,
    /// The application host must replace the session with a new project.
    NewProjectRequested {
        /// Human-facing name.
        name: String,
        /// Built-in authoring preset.
        preset: String,
        /// Whether unsaved work may be discarded.
        discard: bool,
    },
    /// The application host must read and validate a project document.
    OpenProjectRequested {
        /// User-entered project locator.
        locator: String,
        /// Whether unsaved work may be discarded.
        discard: bool,
    },
    /// The application host must save the project document.
    SaveProjectRequested {
        /// New locator, or `None` to reuse the current project path.
        locator: Option<String>,
    },
    /// The application host must plan or execute delivery.
    ExportRequested {
        /// Output locator; `None` means dry-run planning only.
        locator: Option<String>,
        /// Requested delivery preset.
        preset: Option<String>,
    },
    /// Mutation completed and should trigger an interactive preview refresh.
    Changed {
        /// Concise canonical change description.
        description: String,
        /// Current contextual placement path.
        path: String,
    },
    /// Frontend should terminate its input loop after checking dirty state.
    QuitRequested {
        /// Whether unsaved work may be discarded.
        discard: bool,
    },
}

/// In-memory editor session with navigation and undo/redo state.
#[derive(Clone, Debug)]
pub struct EditorSession {
    project: MediaProject,
    path: MediaPath,
    undo: Vec<MediaProject>,
    redo: Vec<MediaProject>,
    saved_project: Option<MediaProject>,
    project_file: Option<PathBuf>,
}

impl EditorSession {
    /// Starts a session at the project root.
    #[must_use]
    pub fn new(project: MediaProject) -> Self {
        Self {
            saved_project: Some(project.clone()),
            project,
            path: MediaPath::root(),
            undo: Vec::new(),
            redo: Vec::new(),
            project_file: None,
        }
    }

    /// Returns the current project state.
    #[must_use]
    pub fn project(&self) -> &MediaProject {
        &self.project
    }

    /// Returns the current contextual media path.
    #[must_use]
    pub fn path(&self) -> &MediaPath {
        &self.path
    }

    /// Returns whether the in-memory graph differs from its last saved or loaded state.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.saved_project.as_ref() != Some(&self.project)
    }

    /// Returns the project document currently associated with the session.
    #[must_use]
    pub fn project_file(&self) -> Option<&std::path::Path> {
        self.project_file.as_deref()
    }

    /// Replaces the complete session with a loaded project and marks it clean.
    pub fn replace_loaded_project(&mut self, project: MediaProject, path: PathBuf) {
        self.saved_project = Some(project.clone());
        self.project = project;
        self.project_file = Some(path);
        self.path.clear();
        self.undo.clear();
        self.redo.clear();
    }

    /// Replaces the complete session with a new unsaved project.
    pub fn replace_new_project(&mut self, project: MediaProject) {
        self.project = project;
        self.saved_project = None;
        self.project_file = None;
        self.path.clear();
        self.undo.clear();
        self.redo.clear();
    }

    /// Records that the current graph was saved to `path`.
    pub fn mark_saved(&mut self, path: PathBuf) {
        self.saved_project = Some(self.project.clone());
        self.project_file = Some(path);
    }

    /// Adopts the exact snapshot successfully written by a host and marks it clean.
    ///
    /// If saving canonicalized the project name, the same name is propagated through undo and
    /// redo snapshots so an ordinary content undo does not unexpectedly restore `Untitled`.
    ///
    /// # Errors
    ///
    /// Returns an error if an internal undo/redo snapshot has lost its project root.
    pub fn mark_saved_snapshot(&mut self, project: MediaProject, path: PathBuf) -> Result<()> {
        if self.project.name != project.name {
            for snapshot in self.undo.iter_mut().chain(&mut self.redo) {
                snapshot.set_name(project.name.clone())?;
            }
        }
        self.project = project;
        self.saved_project = Some(self.project.clone());
        self.project_file = Some(path);
        Ok(())
    }

    /// Returns a prompt breadcrumb such as `Film > Clip0 > Title`.
    ///
    /// # Errors
    ///
    /// Returns an error if session navigation points at a missing link.
    pub fn prompt(&self) -> Result<String> {
        let path = self.project.display_path(&self.path)?;
        if path == "/" {
            return Ok(self.project.name.clone());
        }
        Ok(format!(
            "{} > {}",
            self.project.name,
            path.trim_start_matches('/').replace('/', " > ")
        ))
    }

    /// Applies one typed command.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid navigation, media construction, trimming, or undo state.
    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, command: EditCommand) -> Result<CommandOutput> {
        match command {
            EditCommand::NewProject {
                name,
                preset,
                discard,
            } => Ok(CommandOutput::NewProjectRequested {
                name,
                preset,
                discard,
            }),
            EditCommand::OpenProject { locator, discard } => {
                Ok(CommandOutput::OpenProjectRequested { locator, discard })
            }
            EditCommand::SaveProject { locator } => {
                Ok(CommandOutput::SaveProjectRequested { locator })
            }
            EditCommand::Import { locator, alias } => {
                Ok(CommandOutput::ImportRequested { locator, alias })
            }
            EditCommand::ProjectPresets => Ok(CommandOutput::Text(format!(
                "Project presets:\n{}",
                ProjectSettings::preset_names().join("\n")
            ))),
            EditCommand::ProjectPreset { preset } => self.mutate(|project, _| {
                let settings = ProjectSettings::from_preset(&preset)?;
                let report = project.set_settings_with_rate_conform(
                    settings,
                    ProjectRateConformPolicy::PreserveTime,
                )?;
                Ok(format_rate_conform_description(
                    &format!("project preset {preset}"),
                    ProjectRateConformPolicy::PreserveTime,
                    report,
                ))
            }),
            EditCommand::ProjectSet {
                field,
                value,
                rate_conform,
            } => self.mutate(|project, _| {
                let mut settings = project.settings().clone();
                set_project_field(&mut settings, &field, &value)?;
                settings.base_preset = None;
                let policy = rate_conform.unwrap_or(ProjectRateConformPolicy::PreserveTime);
                let report = project.set_settings_with_rate_conform(settings, policy)?;
                let description = format!("project set {field} {value}");
                if field == "rate" {
                    Ok(format_rate_conform_description(
                        &description,
                        policy,
                        report,
                    ))
                } else {
                    Ok(description)
                }
            }),
            EditCommand::ProjectMatch => Ok(CommandOutput::ProjectMatchRequested),
            EditCommand::ExportPlan { preset } => Ok(CommandOutput::ExportRequested {
                locator: None,
                preset,
            }),
            EditCommand::Export { locator, preset } => Ok(CommandOutput::ExportRequested {
                locator: Some(locator),
                preset,
            }),
            EditCommand::Pwd => Ok(CommandOutput::Text(self.project.display_path(&self.path)?)),
            EditCommand::List => Ok(CommandOutput::Listing(self.project.list(&self.path)?)),
            EditCommand::Info => self.info(),
            EditCommand::InfoTopic { topic } if topic == "project" => Ok(self.project_info()),
            EditCommand::InfoTopic { topic } => Ok(CommandOutput::Text(format!(
                "{topic} information requested for {}",
                self.project.display_path(&self.path)?
            ))),
            EditCommand::Cd { path } => self.cd(&path),
            EditCommand::Add {
                kind,
                alias,
                duration,
                start,
            } => self.mutate(|project, path| {
                let parent = project.resolve_path(path)?;
                let time_base = project
                    .media(parent)
                    .ok_or_else(|| Error::InvalidState("current media disappeared".into()))?
                    .time_base;
                let duration_frames = duration.resolve(time_base)?;
                let start_frame = start.resolve(time_base)?;
                if duration.is_relative() || duration_frames <= 0 {
                    return Err(Error::InvalidData(
                        "add duration must be a positive absolute timecode".into(),
                    ));
                }
                if start.is_relative() || start_frame < 0 {
                    return Err(Error::InvalidData(
                        "add start must be a non-negative absolute timecode".into(),
                    ));
                }
                project.add_generated(parent, kind, alias.clone(), start_frame, duration_frames)?;
                Ok(format!(
                    "add {} {alias} {} at {}",
                    project
                        .list(path)?
                        .last()
                        .ok_or_else(|| { Error::InvalidState("added media is missing".into()) })?
                        .kind
                        .as_str(),
                    format_compact_timecode(duration_frames, time_base)?,
                    format_compact_timecode(start_frame, time_base)?,
                ))
            }),
            EditCommand::TrimIn { value } => self.mutate(|project, path| {
                let time_base = current_source_time_base(project, path)?;
                let frames = value.resolve(time_base)?;
                project.trim_in(path, frames, value.is_relative())?;
                Ok(format!("in {}", display_frame_value(value, time_base)?))
            }),
            EditCommand::TrimOut { value } => self.mutate(|project, path| {
                let time_base = current_source_time_base(project, path)?;
                let frames = value.resolve(time_base)?;
                project.trim_out(path, frames, value.is_relative())?;
                Ok(format!("out {}", display_frame_value(value, time_base)?))
            }),
            EditCommand::SetScaleMode { mode } => self.mutate(|project, path| {
                project.set_scale_mode(path, mode)?;
                Ok(format!("scale {}", mode.as_str()))
            }),
            EditCommand::Undo => self.undo(),
            EditCommand::Redo => self.redo(),
            EditCommand::Help => Ok(CommandOutput::Text(help_text().into())),
            EditCommand::Man { command } => Ok(CommandOutput::Text(man_text(&command)?)),
            EditCommand::Quit { discard } => Ok(CommandOutput::QuitRequested { discard }),
        }
    }

    /// Adds host-resolved media to the current local timeline as one undoable mutation.
    ///
    /// The project time base remains authoritative. Imported durations are conformed to the
    /// nearest parent frame and new media is appended after the current local timeline.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, duplicate aliases, or an invalid time-base mapping.
    pub fn add_imported_media(&mut self, media: &ImportedMedia) -> Result<CommandOutput> {
        if media.duration <= 0 {
            return Err(Error::InvalidData(
                "imported media duration must be positive".into(),
            ));
        }
        self.mutate(|project, path| {
            let parent_id = project.resolve_path(path)?;
            let parent = project
                .media(parent_id)
                .ok_or_else(|| Error::InvalidState("current media disappeared".into()))?;
            let parent_time_base = parent.time_base;
            let timeline_start = parent.duration.value;
            let timeline_duration = Timestamp {
                value: media.duration,
                time_base: media.time_base,
            }
            .rescale(parent_time_base, TimestampRounding::NearestTiesAway)?;
            if timeline_duration.value <= 0 {
                return Err(Error::InvalidData(
                    "imported media is shorter than one project frame".into(),
                ));
            }
            let timeline_end = timeline_start
                .checked_add(timeline_duration.value)
                .ok_or_else(|| Error::InvalidData("imported placement overflows".into()))?;
            let media_id = project.create_media(
                media.name.clone(),
                media.kind.clone(),
                media.time_base,
                media.duration,
                media.origin.clone(),
            )?;
            project.link_media(
                parent_id,
                media_id,
                media.alias.clone(),
                TimeRange::new(
                    Timestamp {
                        value: 0,
                        time_base: media.time_base,
                    },
                    Timestamp {
                        value: media.duration,
                        time_base: media.time_base,
                    },
                )?,
                TimeRange::new(
                    Timestamp {
                        value: timeline_start,
                        time_base: parent_time_base,
                    },
                    Timestamp {
                        value: timeline_end,
                        time_base: parent_time_base,
                    },
                )?,
            )?;
            Ok(format!("import {} as {}", media.name, media.alias))
        })
    }

    /// Replaces project authoring settings with a host-probed focused-media format as one
    /// undoable operation.
    ///
    /// Root timeline positions preserve presentation time when the matched frame rate differs.
    /// Source ranges and nested media time bases remain unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings are invalid or timeline conformance fails.
    pub fn match_project_settings(
        &mut self,
        mut settings: ProjectSettings,
        matched_audio: bool,
    ) -> Result<CommandOutput> {
        settings.base_preset = None;
        self.mutate(|project, _| {
            let report = project
                .set_settings_with_rate_conform(settings, ProjectRateConformPolicy::PreserveTime)?;
            let audio = if matched_audio {
                "video and audio"
            } else {
                "video; audio unchanged"
            };
            Ok(format_rate_conform_description(
                &format!("project match focused media ({audio})"),
                ProjectRateConformPolicy::PreserveTime,
                report,
            ))
        })
    }

    fn info(&self) -> Result<CommandOutput> {
        let media_id = self.project.resolve_path(&self.path)?;
        let media = self
            .project
            .media(media_id)
            .ok_or_else(|| Error::InvalidState("current media disappeared".into()))?;
        let mut text = format!(
            "{} [{}]\nduration: {}\nchildren: {}",
            media.name,
            media.kind.as_str(),
            format_compact_timecode(media.duration.value, media.duration.time_base)?,
            media.children().len()
        );
        if let Some(link_id) = self.path.current_link() {
            let link = self
                .project
                .link(link_id)
                .ok_or_else(|| Error::InvalidState("current placement disappeared".into()))?;
            let _ = write!(
                text,
                "\nsource: {}..{}\nparent timeline: {}..{}",
                format_compact_timecode(
                    link.source_range.start.value,
                    link.source_range.start.time_base
                )?,
                format_compact_timecode(
                    link.source_range.end.value,
                    link.source_range.end.time_base
                )?,
                format_compact_timecode(
                    link.timeline_range.start.value,
                    link.timeline_range.start.time_base
                )?,
                format_compact_timecode(
                    link.timeline_range.end.value,
                    link.timeline_range.end.time_base
                )?,
            );
            let _ = write!(text, "\nscale: {}", link.scale_mode.as_str());
        }
        Ok(CommandOutput::Text(text))
    }

    fn project_info(&self) -> CommandOutput {
        let settings = self.project.settings();
        let scan = match settings.scan_mode {
            ProjectScanMode::Progressive => "progressive",
            ProjectScanMode::Interlaced => "interlaced",
        };
        let color = match settings.color_space {
            ProjectColorSpace::Rec709 => "rec709",
            ProjectColorSpace::Srgb => "srgb",
            ProjectColorSpace::Rec2020 => "rec2020",
        };
        let file = self
            .project_file()
            .map_or_else(|| "unsaved".into(), |path| path.display().to_string());
        CommandOutput::Text(format!(
            "{}\ncanvas: {}x{}\nrate: {}/{} fps\npixel aspect: {}/{}\nscan: {scan}\ncolor: {color}\naudio: {} Hz, {} channels\npreset: {}\nfile: {file}\nstate: {}",
            self.project.name,
            settings.width,
            settings.height,
            settings.frame_rate.numerator(),
            settings.frame_rate.denominator(),
            settings.pixel_aspect.numerator(),
            settings.pixel_aspect.denominator(),
            settings.audio_sample_rate,
            settings.audio_channels,
            settings.base_preset.as_deref().unwrap_or("custom"),
            if self.is_dirty() { "modified" } else { "saved" },
        ))
    }

    fn cd(&mut self, target: &str) -> Result<CommandOutput> {
        if target.is_empty() {
            return Err(Error::InvalidData("cd requires a media path".into()));
        }
        let original = self.path.clone();
        if target.starts_with('/') {
            self.path.clear();
        }
        for segment in target.split('/').filter(|segment| !segment.is_empty()) {
            match segment {
                "." => {}
                ".." => self.path.pop(),
                alias => {
                    let link_id = self.project.child(&self.path, alias).inspect_err(|_| {
                        self.path = original.clone();
                    })?;
                    self.path.push(link_id);
                }
            }
        }
        let path = self.project.display_path(&self.path)?;
        Ok(CommandOutput::Text(path))
    }

    fn mutate(
        &mut self,
        mutation: impl FnOnce(&mut MediaProject, &MediaPath) -> Result<String>,
    ) -> Result<CommandOutput> {
        let before = self.project.clone();
        let description = match mutation(&mut self.project, &self.path) {
            Ok(description) => description,
            Err(error) => {
                self.project = before;
                return Err(error);
            }
        };
        self.undo.push(before);
        self.redo.clear();
        Ok(CommandOutput::Changed {
            description,
            path: self.project.display_path(&self.path)?,
        })
    }

    fn undo(&mut self) -> Result<CommandOutput> {
        let previous = self
            .undo
            .pop()
            .ok_or_else(|| Error::InvalidState("nothing to undo".into()))?;
        self.redo
            .push(std::mem::replace(&mut self.project, previous));
        self.repair_path();
        Ok(CommandOutput::Changed {
            description: "undo".into(),
            path: self.project.display_path(&self.path)?,
        })
    }

    fn redo(&mut self) -> Result<CommandOutput> {
        let next = self
            .redo
            .pop()
            .ok_or_else(|| Error::InvalidState("nothing to redo".into()))?;
        self.undo.push(std::mem::replace(&mut self.project, next));
        self.repair_path();
        Ok(CommandOutput::Changed {
            description: "redo".into(),
            path: self.project.display_path(&self.path)?,
        })
    }

    fn repair_path(&mut self) {
        while self.project.resolve_path(&self.path).is_err() {
            self.path.pop();
        }
    }
}

/// Parses one script/interactive line into the shared typed command representation.
///
/// Blank lines and lines beginning with `#` return `None`.
///
/// # Errors
///
/// Returns an error for malformed quoting, unsupported commands, or invalid arguments.
pub fn parse_command(line: &str) -> Result<Option<EditCommand>> {
    let tokens = tokenize(line)?;
    let Some(command) = tokens.first().map(String::as_str) else {
        return Ok(None);
    };
    if command.starts_with('#') {
        return Ok(None);
    }
    let parsed = match command {
        "new" => parse_new(&tokens)?,
        "open" => parse_open_project(&tokens)?,
        "save" => parse_save(&tokens)?,
        "import" => parse_import(&tokens)?,
        "project" => parse_project(&tokens)?,
        "export" => parse_export(&tokens)?,
        "pwd" => no_arguments(&tokens, EditCommand::Pwd)?,
        "ls" => no_arguments(&tokens, EditCommand::List)?,
        "info" => {
            if tokens.len() == 1 {
                EditCommand::Info
            } else if tokens.len() == 2 {
                parse_info_topic(&tokens[1])?
            } else {
                return Err(Error::InvalidData(
                    "usage: info [project|video|audio|source]".into(),
                ));
            }
        }
        "undo" => no_arguments(&tokens, EditCommand::Undo)?,
        "redo" => no_arguments(&tokens, EditCommand::Redo)?,
        "help" => no_arguments(&tokens, EditCommand::Help)?,
        "man" => {
            if tokens.len() != 2 {
                return Err(Error::InvalidData("usage: man <command>".into()));
            }
            man_text(&tokens[1])?;
            EditCommand::Man {
                command: tokens[1].clone(),
            }
        }
        "quit" | "exit" => {
            if tokens.len() == 1 {
                EditCommand::Quit { discard: false }
            } else if tokens.len() == 2 && tokens[1] == "--discard" {
                EditCommand::Quit { discard: true }
            } else {
                return Err(Error::InvalidData("usage: quit [--discard]".into()));
            }
        }
        "cd" => {
            if tokens.len() != 2 {
                return Err(Error::InvalidData("usage: cd <media-path>".into()));
            }
            EditCommand::Cd {
                path: tokens[1].clone(),
            }
        }
        "in" | "out" => {
            if tokens.len() != 2 {
                return Err(Error::InvalidData(format!(
                    "usage: {command} <S:FF|M:SS:FF|H:MM:SS:FF>"
                )));
            }
            let value = parse_frame_value(&tokens[1])?;
            if command == "in" {
                EditCommand::TrimIn { value }
            } else {
                EditCommand::TrimOut { value }
            }
        }
        "add" => parse_add(&tokens)?,
        "scale" => parse_scale(&tokens)?,
        _ => {
            return Err(Error::Unsupported(format!(
                "editor command '{command}' is not implemented"
            )));
        }
    };
    Ok(Some(parsed))
}

fn parse_scale(tokens: &[String]) -> Result<EditCommand> {
    let mode = match tokens {
        [_, mode] => match mode.as_str() {
            "fit" => VisualScaleMode::Fit,
            "fill" => VisualScaleMode::Fill,
            "stretch" => VisualScaleMode::Stretch,
            "native" => VisualScaleMode::Native,
            _ => {
                return Err(Error::InvalidData(
                    "scale mode must be fit, fill, stretch, or native".into(),
                ));
            }
        },
        _ => {
            return Err(Error::InvalidData(
                "usage: scale <fit|fill|stretch|native>".into(),
            ));
        }
    };
    Ok(EditCommand::SetScaleMode { mode })
}

fn parse_new(tokens: &[String]) -> Result<EditCommand> {
    let mut arguments = tokens[1..].to_vec();
    let discard = if let Some(index) = arguments.iter().position(|value| value == "--discard") {
        arguments.remove(index);
        true
    } else {
        false
    };
    if arguments.len() != 1 && arguments.len() != 3 {
        return Err(Error::InvalidData(
            "usage: new <name> [using <project-preset>] [--discard]".into(),
        ));
    }
    if arguments.len() == 3 && arguments[1] != "using" {
        return Err(Error::InvalidData(
            "usage: new <name> [using <project-preset>] [--discard]".into(),
        ));
    }
    Ok(EditCommand::NewProject {
        name: arguments[0].clone(),
        preset: arguments
            .get(2)
            .cloned()
            .unwrap_or_else(|| "web-1080p30".into()),
        discard,
    })
}

fn parse_open_project(tokens: &[String]) -> Result<EditCommand> {
    if tokens.len() != 2 && !(tokens.len() == 3 && tokens[2] == "--discard") {
        return Err(Error::InvalidData(
            "usage: open <project.mmrecode> [--discard]".into(),
        ));
    }
    Ok(EditCommand::OpenProject {
        locator: tokens[1].clone(),
        discard: tokens.len() == 3,
    })
}

fn parse_save(tokens: &[String]) -> Result<EditCommand> {
    match tokens {
        [_] => Ok(EditCommand::SaveProject { locator: None }),
        [_, keyword, locator] if keyword == "as" => Ok(EditCommand::SaveProject {
            locator: Some(locator.clone()),
        }),
        _ => Err(Error::InvalidData(
            "usage: save [as <project[.mmrecode]>]".into(),
        )),
    }
}

fn parse_import(tokens: &[String]) -> Result<EditCommand> {
    if tokens.len() != 2 && tokens.len() != 4 {
        return Err(Error::InvalidData(
            "usage: import <media-file> [as <alias>]".into(),
        ));
    }
    if tokens.len() == 4 && tokens[2] != "as" {
        return Err(Error::InvalidData(
            "usage: import <media-file> [as <alias>]".into(),
        ));
    }
    Ok(EditCommand::Import {
        locator: tokens[1].clone(),
        alias: tokens.get(3).cloned(),
    })
}

fn parse_project(tokens: &[String]) -> Result<EditCommand> {
    match tokens {
        [_, command] if command == "info" => Ok(EditCommand::InfoTopic {
            topic: "project".into(),
        }),
        [_, command] if command == "presets" => Ok(EditCommand::ProjectPresets),
        [_, command] if command == "match" => Ok(EditCommand::ProjectMatch),
        [_, command, preset] if command == "preset" => Ok(EditCommand::ProjectPreset {
            preset: preset.clone(),
        }),
        [_, command, field, value] if command == "set" => Ok(EditCommand::ProjectSet {
            field: field.clone(),
            value: value.clone(),
            rate_conform: (field == "rate").then_some(ProjectRateConformPolicy::PreserveTime),
        }),
        [_, command, field, value, conform, policy]
            if command == "set" && field == "rate" && conform == "conform" =>
        {
            let rate_conform = match policy.as_str() {
                "time" => ProjectRateConformPolicy::PreserveTime,
                "frames" => ProjectRateConformPolicy::PreserveFrames,
                _ => {
                    return Err(Error::InvalidData(
                        "project rate conform policy must be time or frames".into(),
                    ));
                }
            };
            Ok(EditCommand::ProjectSet {
                field: field.clone(),
                value: value.clone(),
                rate_conform: Some(rate_conform),
            })
        }
        _ => Err(Error::InvalidData(
            "usage: project <info|match|presets|preset <name>|set <field> <value> [conform <time|frames>]>".into(),
        )),
    }
}

fn parse_export(tokens: &[String]) -> Result<EditCommand> {
    if tokens.get(1).is_some_and(|value| value == "plan") {
        let preset = parse_optional_preset(&tokens[2..], "usage: export plan [using <preset>]")?;
        return Ok(EditCommand::ExportPlan { preset });
    }
    if tokens.len() < 2 {
        return Err(Error::InvalidData(
            "usage: export <output-file> [using <preset>]".into(),
        ));
    }
    let preset =
        parse_optional_preset(&tokens[2..], "usage: export <output-file> [using <preset>]")?;
    Ok(EditCommand::Export {
        locator: tokens[1].clone(),
        preset,
    })
}

fn parse_optional_preset(tokens: &[String], usage: &str) -> Result<Option<String>> {
    match tokens {
        [] => Ok(None),
        [using, preset] if using == "using" => Ok(Some(preset.clone())),
        _ => Err(Error::InvalidData(usage.into())),
    }
}

fn set_project_field(settings: &mut ProjectSettings, field: &str, value: &str) -> Result<()> {
    match field {
        "size" => {
            let (width, height) = value.split_once('x').ok_or_else(|| {
                Error::InvalidData("project size must look like 1920x1080".into())
            })?;
            settings.width = width
                .parse()
                .map_err(|_| Error::InvalidData("project width must be an integer".into()))?;
            settings.height = height
                .parse()
                .map_err(|_| Error::InvalidData("project height must be an integer".into()))?;
        }
        "rate" => settings.frame_rate = parse_positive_rational(value, "project rate")?,
        "pixel-aspect" => {
            settings.pixel_aspect = parse_positive_rational(value, "project pixel aspect")?;
        }
        "scan" => {
            settings.scan_mode = match value {
                "progressive" => ProjectScanMode::Progressive,
                "interlaced" => ProjectScanMode::Interlaced,
                _ => {
                    return Err(Error::InvalidData(
                        "project scan must be progressive or interlaced".into(),
                    ));
                }
            };
        }
        "color" => {
            settings.color_space = match value {
                "rec709" => ProjectColorSpace::Rec709,
                "srgb" => ProjectColorSpace::Srgb,
                "rec2020" => ProjectColorSpace::Rec2020,
                _ => {
                    return Err(Error::InvalidData(
                        "project color must be rec709, srgb, or rec2020".into(),
                    ));
                }
            };
        }
        "audio-rate" => {
            settings.audio_sample_rate = value
                .parse()
                .map_err(|_| Error::InvalidData("project audio rate must be an integer".into()))?;
        }
        "audio-channels" => {
            settings.audio_channels = value.parse().map_err(|_| {
                Error::InvalidData("project audio channels must be an integer".into())
            })?;
        }
        _ => {
            return Err(Error::InvalidData(format!(
                "unknown project field '{field}'; expected {}",
                PROJECT_SETTING_NAMES.join(", ")
            )));
        }
    }
    settings.validate()
}

fn format_rate_conform_description(
    description: &str,
    policy: ProjectRateConformPolicy,
    report: crate::ProjectRateConformReport,
) -> String {
    let policy = match policy {
        ProjectRateConformPolicy::PreserveTime => "time",
        ProjectRateConformPolicy::PreserveFrames => "frames",
    };
    format!(
        "{description} conform {policy} ({} placement(s), {} rounded boundary/boundaries)",
        report.conformed_placements, report.rounded_boundaries
    )
}

fn parse_positive_rational(value: &str, label: &str) -> Result<Rational> {
    let (numerator, denominator) = value.split_once('/').unwrap_or((value, "1"));
    let numerator = numerator
        .parse::<i64>()
        .map_err(|_| Error::InvalidData(format!("{label} must be N or N/D")))?;
    let denominator = denominator
        .parse::<i64>()
        .map_err(|_| Error::InvalidData(format!("{label} must be N or N/D")))?;
    let value = Rational::new(numerator, denominator)?;
    if value.numerator() <= 0 {
        return Err(Error::InvalidData(format!("{label} must be positive")));
    }
    Ok(value)
}

fn parse_info_topic(topic: &str) -> Result<EditCommand> {
    match topic {
        "project" | "video" | "audio" | "source" => Ok(EditCommand::InfoTopic {
            topic: topic.into(),
        }),
        _ => Err(Error::InvalidData(
            "usage: info [project|video|audio|source]".into(),
        )),
    }
}

fn parse_add(tokens: &[String]) -> Result<EditCommand> {
    if tokens.len() != 4 && tokens.len() != 6 {
        return Err(Error::InvalidData(
            "usage: add <kind> <alias> <duration> [at <start>]".into(),
        ));
    }
    if tokens.len() == 6 && tokens[4] != "at" {
        return Err(Error::InvalidData(
            "usage: add <kind> <alias> <duration> [at <start>]".into(),
        ));
    }
    let duration = parse_frame_value(&tokens[3])?;
    let start = if tokens.len() == 6 {
        parse_frame_value(&tokens[5])?
    } else {
        FrameValue::Frames {
            frames: 0,
            relative: false,
        }
    };
    Ok(EditCommand::Add {
        kind: MediaKind::new(tokens[1].clone())?,
        alias: tokens[2].clone(),
        duration,
        start,
    })
}

fn parse_frame_value(value: &str) -> Result<FrameValue> {
    if value.is_empty() {
        return Err(invalid_timecode());
    }
    let (relative, negative, unsigned) = match value.as_bytes()[0] {
        b'+' => (true, false, &value[1..]),
        b'-' => (true, true, &value[1..]),
        _ => (false, false, value),
    };
    if unsigned.is_empty() {
        return Err(invalid_timecode());
    }
    if unsigned.contains(':') {
        let fields = unsigned
            .split(':')
            .map(|field| {
                if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(invalid_timecode());
                }
                field
                    .parse::<u64>()
                    .map_err(|_| Error::InvalidData("timecode exceeds editor limits".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        if !(2..=4).contains(&fields.len()) {
            return Err(invalid_timecode());
        }
        let frames = *fields.last().expect("timecode has at least two fields");
        let clock = &fields[..fields.len() - 1];
        if clock.len() >= 2 && clock[clock.len() - 1] >= 60 {
            return Err(Error::InvalidData(
                "timecode seconds field must be below 60".into(),
            ));
        }
        if clock.len() == 3 && clock[1] >= 60 {
            return Err(Error::InvalidData(
                "timecode minutes field must be below 60".into(),
            ));
        }
        let seconds = clock.iter().try_fold(0_u64, |total, field| {
            total
                .checked_mul(60)
                .and_then(|value| value.checked_add(*field))
                .ok_or_else(|| Error::InvalidData("timecode exceeds editor limits".into()))
        })?;
        return Ok(FrameValue::Timecode {
            seconds,
            frames,
            negative,
            relative,
        });
    }

    let raw = if value.ends_with('f') {
        value.strip_suffix('f').expect("suffix was checked")
    } else {
        value
    };
    let frames = raw.parse::<i64>().map_err(|_| invalid_timecode())?;
    Ok(FrameValue::Frames { frames, relative })
}

fn invalid_timecode() -> Error {
    Error::InvalidData(
        "time must look like 1:15, 2:01:15, +0:10, or -0:05 (legacy frame counts also work)".into(),
    )
}

fn no_arguments(tokens: &[String], command: EditCommand) -> Result<EditCommand> {
    if tokens.len() != 1 {
        return Err(Error::InvalidData(format!(
            "command '{}' takes no arguments",
            tokens[0]
        )));
    }
    Ok(command)
}

fn tokenize(line: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '#' if current.is_empty() => break,
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if escaped || quote.is_some() {
        return Err(Error::InvalidData(
            "editor command contains an unfinished escape or quote".into(),
        ));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn display_frame_value(value: FrameValue, time_base: Rational) -> Result<String> {
    let frames = value.resolve(time_base)?;
    let formatted = format_compact_timecode(frames, time_base)?;
    if value.is_relative() && frames >= 0 {
        Ok(format!("+{formatted}"))
    } else {
        Ok(formatted)
    }
}

fn help_text() -> &'static str {
    "QUICK HELP\n\nnew <name> [using <preset>] [--discard]\nopen <project.mmrecode> [--discard]\nsave [as <project>]         save the project\nimport <file> [as <alias>]  import media\nproject info|match|presets|preset|set\nproject set rate <rate> [conform time|frames]\nscale fit|fill|stretch|native\nexport plan [using <preset>]\nexport <file> [using <preset>]\npwd / ls / cd <path>        navigate media\ninfo [project|video|audio|source]\nadd <kind> <alias> <duration> [at <start>]\nin <time> / out <time>      trim selected placement\nundo / redo                 edit history\nhelp / man <command>        contextual help\nquit [--discard]            leave MMRecode\n\nInteractive prompt: Tab completes commands, paths, settings, presets, topics, scale modes, and hierarchy aliases.\nAfter in/out: left <time>, right <time>, or move <direction> <time> adjusts the focused boundary.\nTime: S:FF, M:SS:FF, or H:MM:SS:FF. Prefix + or - for relative trims."
}

fn man_text(command: &str) -> Result<String> {
    let text = match command {
        "new" => {
            "NEW — create project\n\nnew <name> [using <project-preset>] [--discard]\n\nCreates an empty project with resolved authoring settings. Use `project presets` to list built-ins. Unsaved work is protected unless --discard is explicit.".into()
        }
        "open" => {
            "OPEN — load project\n\nopen <project.mmrecode> [--discard]\n\nLoads and validates a versioned project document. Relative managed-media paths resolve from the project-file directory. Unsaved work is protected unless --discard is explicit.".into()
        }
        "save" => {
            "SAVE — persist project\n\nsave\nsave as <project[.mmrecode]>\n\nWrites readable, versioned JSON. `.mmrecode` is appended when omitted. The first save requires `save as`; later saves reuse that path. Saving the initial Untitled project with `save as` adopts the file stem as its project name. `save as` rebases managed media paths so they continue to identify the same files from the new project directory.".into()
        }
        "scale" => {
            "SCALE — placement sizing\n\nscale fit\nscale fill\nscale stretch\nscale native\n\nChanges how the selected visual placement maps into the project canvas. `fit` is the default: preserve coded-pixel aspect ratio and show the whole image with black bars as needed. `fill` preserves coded-pixel aspect ratio while cropping centered excess pixels. `stretch` fills the canvas without preserving aspect ratio. `native` keeps source pixel size and centers it, padding or cropping at the edges. A size mismatch makes every affected frame a full-render operation; exact-size MPEG-2 material can still be packet copied because all four modes produce identical pixels there. The initial CPU renderer uses high-quality Lanczos resizing for progressive Yuv420p8 MPEG-2.".into()
        }
        "import" => {
            "IMPORT — add media\n\nimport <media-file> [as <alias>]\n\nProbes a path relative to the session directory, adds it at the end of the current local timeline, and enters its placement. This slice accepts MPEG-2 ES/TS and non-fragmented H.264 MP4/MOV video. H.264 preview tries MMRecode's native Rust decoder first and uses an optional installed FFmpeg fallback for reconstruction tools not implemented yet; MMRecode owns demuxing, timing, indexing, and seeking.".into()
        }
        "project" => format!(
            "PROJECT — authoring settings\n\nproject info\nproject match\nproject presets\nproject preset <name>\nproject set <field> <value>\nproject set rate <N|N/D> [conform <time|frames>]\n\n`project match` probes the currently focused media and adopts its display canvas (including MP4/MOV rotation), exact average frame rate, derived pixel aspect, scan organization, and working color. Supported MPEG audio or ISO-BMFF audio sample-entry rate/channel metadata is adopted too; otherwise existing project audio settings remain. The change is atomic and undoable, and root placement times preserve presentation time. Run it after `import`, while the imported placement remains focused.\n\nFields and values:\n  size WIDTHxHEIGHT       1..32768 pixels per dimension\n  rate N or N/D           positive exact frames per second\n  pixel-aspect N or N/D   positive exact ratio\n  scan                    progressive | interlaced\n  color                   rec709 | srgb | rec2020\n  audio-rate              8000..384000 Hz\n  audio-channels          1..64\n\nProject presets:\n{}\n\nA rate change conforms only direct project-root placement times and is undoable. `conform time` is the default: presentation time is preserved, with non-exact boundaries rounded to the nearest new frame (ties away from zero). `conform frames` preserves integer root frame numbers and therefore changes presentation time. Source in/out ranges and nested media time bases are never rewritten. Delivery codecs belong to export presets.",
            ProjectSettings::preset_names().join("\n")
        ),
        "export" => format!(
            "EXPORT — render the project timeline\n\nexport plan [using <preset>]\nexport <output-file> [using <preset>]\n\nDelivery presets:\n{}\n\nExport always starts at the project root and renders the complete root timeline; the current `cd` context does not select a source for export. The output filename is only the delivery destination. The executable mpeg2-ts slice supports any number of root video/mpeg2 placements, their trims and timeline positions, project-rate conformance, per-placement `scale`, and black frames for gaps. Later overlapping opaque video placements win in project composition order. A single placement covering the timeline with matching rate, canvas, and scan can use packet-preserving smart rendering as an internal optimization; all other supported timelines are fully rendered and re-encoded. Current full rendering requires progressive Yuv420p8 MPEG-2 placements, supports standard MPEG rates through 60 fps and even project canvases through 1920x1152 subject to the Main Profile/High Level limit of 62,668,800 luma samples per second, and does not yet render nested generated/effect media, alpha composition, audio, or interlaced scaling. Use `export plan` to inspect the complete timeline plan and why each path was selected. The YouTube/H.264 presets remain named future targets.",
            EXPORT_PRESET_NAMES.join("\n")
        ),
        "pwd" => {
            "PWD — current context\n\npwd\n\nShows the linked-media path used by the prompt and contextual inspector.".into()
        }
        "ls" => {
            "LS — local timeline\n\nls\n\nLists child media placed directly in the current media's local timeline.".into()
        }
        "cd" => {
            "CD — hierarchy navigation\n\ncd <alias|index|path|..>\n\nMoves through media-placement links. Paths may be relative or start at the project root with /.".into()
        }
        "info" => {
            "INFO — contextual inspection\n\ninfo\ninfo project\ninfo video\ninfo audio\ninfo source\n\nBare info follows the current hierarchy. A topic asks the frontend for a focused metadata view.".into()
        }
        "add" => {
            "ADD — generated media\n\nadd <kind> <alias> <duration> [at <start>]\n\nCreates generated media in the current local timeline. Time uses compact frame timecode.".into()
        }
        "in" => {
            "IN — source in-point\n\nin <time>\n\nSets an absolute in-point. Prefix the time with + or - for a relative trim. Afterward the interactive shortcuts `left <time>` and `right <time>` move this boundary.".into()
        }
        "out" => {
            "OUT — source out-point\n\nout <time>\n\nSets an absolute out-point. Prefix the time with + or - for a relative trim. Afterward the interactive shortcuts `left <time>` and `right <time>` move this boundary.".into()
        }
        "left" | "right" | "move" => {
            "LEFT / RIGHT / MOVE — adjust focused boundary\n\nleft <time>\nright <time>\nmove left <time>\nmove right <time>\n\nAvailable interactively immediately after `in` or `out`. The command moves the focused boundary by an unsigned compact time without making the user repeat which boundary is selected. Scripts should use canonical relative `in` or `out` commands.".into()
        }
        "undo" => {
            "UNDO — revert edit\n\nundo\n\nRestores the previous project state. Ctrl-Z is the full-screen shortcut.".into()
        }
        "redo" => {
            "REDO — restore reverted edit\n\nredo\n\nReapplies the most recently undone state. Ctrl-Y is the full-screen shortcut.".into()
        }
        "help" => {
            "HELP — command overview\n\nhelp\n\nShows concise commands appropriate for discovery. Use `man <command>` for details.".into()
        }
        "man" => {
            "MAN — detailed command help\n\nman <command>\n\nShows syntax, behavior, and contextual follow-up commands for one command.".into()
        }
        "quit" | "exit" => {
            "QUIT — leave editor\n\nquit [--discard]\n\nEnds the current MMRecode editor session. Unsaved work is protected unless --discard is explicit. Ctrl-Q only exits a clean session.".into()
        }
        _ => {
            return Err(Error::InvalidData(format!(
                "no manual entry for editor command '{command}'"
            )));
        }
    };
    Ok(text)
}

fn current_source_time_base(project: &MediaProject, path: &MediaPath) -> Result<Rational> {
    let link_id = path
        .current_link()
        .ok_or_else(|| Error::InvalidState("project root has no in/out points".into()))?;
    let link = project
        .link(link_id)
        .ok_or_else(|| Error::InvalidState("current placement disappeared".into()))?;
    Ok(link.source_range.start.time_base)
}

fn nominal_frame_rate(time_base: Rational) -> Result<i64> {
    let numerator = i128::from(time_base.numerator());
    let denominator = i128::from(time_base.denominator());
    if numerator <= 0 || denominator <= 0 {
        return Err(Error::InvalidData(
            "timecode requires a positive media time base".into(),
        ));
    }
    let rate = (denominator + numerator / 2) / numerator;
    if rate <= 0 {
        return Err(Error::InvalidData(
            "timecode requires at least one frame per second".into(),
        ));
    }
    i64::try_from(rate).map_err(|_| Error::InvalidData("frame rate exceeds editor limits".into()))
}

/// Formats a native frame position as compact, non-drop timecode.
///
/// Leading zero fields are omitted: at 30 fps, frame 15 is `0:15`, frame 150 is `5:00`, and
/// frame `109_845` is `1:01:01:15`.
///
/// # Errors
///
/// Returns an error when the time base cannot represent a positive nominal frame rate.
pub fn format_compact_timecode(frame: i64, time_base: Rational) -> Result<String> {
    let frames_per_second = u64::try_from(nominal_frame_rate(time_base)?)
        .map_err(|_| Error::InvalidData("frame rate exceeds editor limits".into()))?;
    let magnitude = frame.unsigned_abs();
    let frame_field = magnitude % frames_per_second;
    let total_seconds = magnitude / frames_per_second;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;
    let sign = if frame < 0 { "-" } else { "" };
    let frame_width = (frames_per_second - 1).to_string().len().max(2);
    if hours > 0 {
        Ok(format!(
            "{sign}{hours}:{minutes:02}:{seconds:02}:{frame_field:0frame_width$}"
        ))
    } else if total_minutes > 0 {
        Ok(format!(
            "{sign}{total_minutes}:{seconds:02}:{frame_field:0frame_width$}"
        ))
    } else {
        Ok(format!("{sign}{total_seconds}:{frame_field:0frame_width$}"))
    }
}

#[cfg(test)]
mod tests {
    use mmrecode_core::Rational;

    use super::*;

    fn session() -> EditorSession {
        EditorSession::new(MediaProject::new("Film", Rational::new(1, 25).unwrap()).unwrap())
    }

    fn apply(session: &mut EditorSession, line: &str) -> CommandOutput {
        let command = parse_command(line).unwrap().unwrap();
        session.apply(command).unwrap()
    }

    #[test]
    fn script_and_interactive_commands_build_the_media_hierarchy() {
        let mut session = session();
        apply(&mut session, "add video Clip0 4:00");
        apply(&mut session, "cd Clip0");
        apply(&mut session, "add text 'Opening Title' 0:20 at 0:10");
        apply(&mut session, "cd 'Opening Title'");

        assert_eq!(
            session.project().display_path(session.path()).unwrap(),
            "/Clip0/Opening Title"
        );
        let CommandOutput::Text(info) = apply(&mut session, "info") else {
            panic!("info should return text");
        };
        assert!(info.contains("Opening Title [text]"));
    }

    #[test]
    fn relative_trim_is_undoable_and_redoable() {
        let mut session = session();
        apply(&mut session, "add video Clip0 4:00 at 0:05");
        apply(&mut session, "cd Clip0");
        apply(&mut session, "in +0:10");
        apply(&mut session, "out -0:05");
        let link_id = session.path().current_link().unwrap();
        assert_eq!(
            session
                .project()
                .link(link_id)
                .unwrap()
                .source_range
                .start
                .value,
            10
        );
        apply(&mut session, "undo");
        assert_eq!(
            session
                .project()
                .link(link_id)
                .unwrap()
                .source_range
                .end
                .value,
            100
        );
        apply(&mut session, "redo");
        assert_eq!(
            session
                .project()
                .link(link_id)
                .unwrap()
                .source_range
                .end
                .value,
            95
        );
        assert_eq!(
            session
                .project()
                .media(session.project().root_id())
                .unwrap()
                .duration
                .value,
            100
        );
    }

    #[test]
    fn absolute_paths_and_indices_traverse_links() {
        let mut session = session();
        apply(&mut session, "add video Clip0 4:00");
        apply(&mut session, "cd 0");
        apply(&mut session, "add fx Grade 4:00");
        apply(&mut session, "cd /Clip0/Grade");
        assert_eq!(
            session.project().display_path(session.path()).unwrap(),
            "/Clip0/Grade"
        );
        apply(&mut session, "cd ../..");
        assert_eq!(session.project().display_path(session.path()).unwrap(), "/");
    }

    #[test]
    fn import_request_is_typed_and_resolved_media_is_undoable() {
        assert_eq!(
            parse_command("import 'media/source.ts' as Camera").unwrap(),
            Some(EditCommand::Import {
                locator: "media/source.ts".into(),
                alias: Some("Camera".into()),
            })
        );

        let mut session = session();
        let output = session
            .add_imported_media(&ImportedMedia {
                name: "source.ts".into(),
                alias: "Camera".into(),
                kind: MediaKind::new("video/mpeg2").unwrap(),
                time_base: Rational::new(1, 30).unwrap(),
                duration: 769,
                origin: MediaOrigin::External {
                    path: std::path::PathBuf::from("media/source.ts"),
                },
            })
            .unwrap();
        assert!(matches!(output, CommandOutput::Changed { .. }));
        assert_eq!(
            session
                .project()
                .media(session.project().root_id())
                .unwrap()
                .time_base,
            Rational::new(1, 25).unwrap()
        );
        assert_eq!(session.project().list(session.path()).unwrap().len(), 1);
        apply(&mut session, "undo");
        assert!(session.project().list(session.path()).unwrap().is_empty());
        apply(&mut session, "redo");
        assert_eq!(session.project().list(session.path()).unwrap().len(), 1);
    }

    #[test]
    fn compact_timecode_omits_only_unused_leading_fields() {
        let time_base = Rational::new(1, 30).unwrap();
        assert_eq!(format_compact_timecode(0, time_base).unwrap(), "0:00");
        assert_eq!(format_compact_timecode(15, time_base).unwrap(), "0:15");
        assert_eq!(format_compact_timecode(150, time_base).unwrap(), "5:00");
        assert_eq!(
            format_compact_timecode(109_845, time_base).unwrap(),
            "1:01:01:15"
        );
        assert_eq!(format_compact_timecode(-45, time_base).unwrap(), "-1:15");
    }

    #[test]
    fn trim_commands_resolve_compact_timecode_in_the_media_rate() {
        let mut session = session();
        apply(&mut session, "add video Clip0 5:00");
        apply(&mut session, "cd Clip0");
        let output = apply(&mut session, "out 1:15");
        assert!(matches!(
            output,
            CommandOutput::Changed { description, .. } if description == "out 1:15"
        ));
        apply(&mut session, "in +0:10");
        let link = session
            .project()
            .link(session.path().current_link().unwrap())
            .unwrap();
        assert_eq!(link.source_range.start.value, 10);
        assert_eq!(link.source_range.end.value, 40);
        let CommandOutput::Text(info) = apply(&mut session, "info") else {
            panic!("info should return text");
        };
        assert!(info.contains("duration: 5:00"));
        assert!(info.contains("source: 0:10..1:15"));
    }

    #[test]
    fn add_accepts_compact_duration_and_start() {
        let mut session = session();
        apply(&mut session, "add text Title 1:15 at 0:10");
        let entry = session.project().list(session.path()).unwrap().remove(0);
        assert_eq!(entry.timeline_range.start.value, 10);
        assert_eq!(entry.timeline_range.end.value, 50);
    }

    #[test]
    fn fractional_rates_use_the_nominal_frame_field_without_rounding_positions() {
        let time_base = Rational::new(1_001, 30_000).unwrap();
        let value = parse_frame_value("1:15").unwrap();
        assert_eq!(value.resolve(time_base).unwrap(), 45);
        assert_eq!(format_compact_timecode(45, time_base).unwrap(), "1:15");
        assert!(
            parse_frame_value("0:30")
                .unwrap()
                .resolve(time_base)
                .is_err()
        );
    }

    #[test]
    fn compact_timecode_validates_clock_fields() {
        assert!(parse_frame_value("1:60:00").is_err());
        assert!(parse_frame_value("1:00:60:00").is_err());
        assert!(parse_frame_value("1:2:3:4:5").is_err());
        assert!(parse_frame_value("1::05").is_err());
    }

    #[test]
    fn legacy_raw_frame_values_remain_readable_but_format_as_timecode() {
        let time_base = Rational::new(1, 30).unwrap();
        let value = parse_frame_value("150f").unwrap();
        assert_eq!(value.resolve(time_base).unwrap(), 150);
        assert_eq!(display_frame_value(value, time_base).unwrap(), "5:00");
    }

    #[test]
    fn help_manual_and_information_topics_are_typed_commands() {
        assert_eq!(
            parse_command("info video").unwrap(),
            Some(EditCommand::InfoTopic {
                topic: "video".into(),
            })
        );
        assert_eq!(
            parse_command("man out").unwrap(),
            Some(EditCommand::Man {
                command: "out".into(),
            })
        );
        assert!(parse_command("info pixels").is_err());
        assert!(parse_command("man imaginary").is_err());

        let mut session = session();
        let CommandOutput::Text(help) = apply(&mut session, "help") else {
            panic!("help should return text");
        };
        assert!(help.contains("open <project.mmrecode>"));
        assert!(help.contains("import <file>"));
        assert!(help.contains("man <command>"));
        let CommandOutput::Text(manual) = apply(&mut session, "man in") else {
            panic!("man should return text");
        };
        assert!(manual.contains("IN — source in-point"));
        assert!(manual.contains("left <time>"));
    }

    #[test]
    fn project_lifecycle_settings_and_export_are_typed() {
        assert_eq!(
            parse_command("project match").unwrap(),
            Some(EditCommand::ProjectMatch)
        );
        assert_eq!(
            parse_command("new Demo using pal-576i25 --discard").unwrap(),
            Some(EditCommand::NewProject {
                name: "Demo".into(),
                preset: "pal-576i25".into(),
                discard: true,
            })
        );
        assert_eq!(
            parse_command("open work.mmrecode").unwrap(),
            Some(EditCommand::OpenProject {
                locator: "work.mmrecode".into(),
                discard: false,
            })
        );
        assert_eq!(
            parse_command("save as work.mmrecode").unwrap(),
            Some(EditCommand::SaveProject {
                locator: Some("work.mmrecode".into()),
            })
        );
        assert_eq!(
            parse_command("export output.ts using mpeg2-ts").unwrap(),
            Some(EditCommand::Export {
                locator: "output.ts".into(),
                preset: Some("mpeg2-ts".into()),
            })
        );

        let mut session = session();
        assert!(!session.is_dirty());
        apply(&mut session, "project set size 1280x720");
        assert!(session.is_dirty());
        session.mark_saved(PathBuf::from("work.mmrecode"));
        assert!(!session.is_dirty());
        assert_eq!(session.project().settings().width, 1_280);
        assert_eq!(
            session.project_file(),
            Some(std::path::Path::new("work.mmrecode"))
        );
    }

    #[test]
    fn project_rate_conformance_is_explicit_and_undoable() {
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 30).unwrap()).unwrap());
        apply(&mut session, "add video Clip0 1:00 at 1:00");
        let CommandOutput::Changed { description, .. } = apply(&mut session, "project set rate 25")
        else {
            panic!("project rate should be an edit");
        };
        assert!(description.contains("conform time"));
        let link = session.project().link(crate::MediaLinkId(1)).unwrap();
        assert_eq!(link.timeline_range.start.value, 25);
        assert_eq!(link.timeline_range.end.value, 50);
        assert_eq!(link.source_range.end.value, 30);

        apply(&mut session, "undo");
        let link = session.project().link(crate::MediaLinkId(1)).unwrap();
        assert_eq!(link.timeline_range.start.value, 30);
        assert_eq!(link.timeline_range.end.value, 60);

        let command = parse_command("project set rate 25 conform frames")
            .unwrap()
            .unwrap();
        assert!(matches!(
            command,
            EditCommand::ProjectSet {
                rate_conform: Some(ProjectRateConformPolicy::PreserveFrames),
                ..
            }
        ));
        assert!(parse_command("project set rate 25 conform mystery").is_err());
    }

    #[test]
    fn matched_project_settings_are_one_undoable_change() {
        let mut session = session();
        let original = session.project().settings().clone();
        let mut matched = original.clone();
        matched.width = 720;
        matched.height = 576;
        matched.frame_rate = Rational::new(25, 1).unwrap();
        matched.audio_sample_rate = 44_100;
        matched.audio_channels = 1;
        let CommandOutput::Changed { description, .. } = session
            .match_project_settings(matched.clone(), true)
            .unwrap()
        else {
            panic!("project match should be an edit");
        };
        assert!(description.contains("video and audio"));
        assert_eq!(session.project().settings(), &matched);

        apply(&mut session, "undo");
        assert_eq!(session.project().settings(), &original);
    }

    #[test]
    fn saved_snapshot_name_survives_content_undo() {
        let mut session = session();
        apply(&mut session, "add text Title 1:00");
        let mut saved = session.project().clone();
        saved.set_name("MyFilm").unwrap();
        session
            .mark_saved_snapshot(saved, PathBuf::from("MyFilm.mmrecode"))
            .unwrap();
        assert_eq!(session.project().name, "MyFilm");
        assert!(!session.is_dirty());

        apply(&mut session, "undo");
        assert_eq!(session.project().name, "MyFilm");
        assert!(session.is_dirty());
    }

    #[test]
    fn canonical_vocabulary_is_covered_by_interactive_help() {
        let quick = help_text();
        for command in EDITOR_COMMAND_NAMES {
            assert!(
                quick.contains(command),
                "quick help lost the '{command}' command"
            );
        }
        for topic in EDITOR_MANUAL_TOPICS {
            assert!(man_text(topic).is_ok(), "manual lost the '{topic}' topic");
        }
        let project = man_text("project").unwrap();
        for field in PROJECT_SETTING_NAMES {
            assert!(
                project.contains(field),
                "project manual lost the '{field}' setting"
            );
        }
        for preset in ProjectSettings::preset_names() {
            assert!(
                project.contains(preset),
                "project manual lost the '{preset}' preset"
            );
        }
        let export = man_text("export").unwrap();
        for preset in EXPORT_PRESET_NAMES {
            assert!(
                export.contains(preset),
                "export manual lost the '{preset}' preset"
            );
        }
    }
}
