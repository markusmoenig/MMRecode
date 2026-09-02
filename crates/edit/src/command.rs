//! Typed editor commands shared by scripts and interactive frontends.

use std::fmt::Write as _;

use mmrecode_core::{Error, Result};

use crate::{MediaKind, MediaListing, MediaPath, MediaProject};

/// Absolute or relative frame value used by trim commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameValue {
    /// Frame count in the current media's native time base.
    pub frames: i64,
    /// Whether `frames` is an offset from the current value.
    pub relative: bool,
}

/// Versionable typed command understood by every editor frontend.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EditCommand {
    /// Show the current placement path.
    Pwd,
    /// List child media in the current local timeline.
    List,
    /// Inspect the current media and placement.
    Info,
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
        /// Positive duration in current local frames.
        duration_frames: i64,
        /// Non-negative start in current local frames.
        start_frame: i64,
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
    /// Restore the previous project state.
    Undo,
    /// Reapply an undone project state.
    Redo,
    /// Show the initial editor command vocabulary.
    Help,
    /// Ask the active frontend to end the session.
    Quit,
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
    /// Mutation completed and should trigger an interactive preview refresh.
    Changed {
        /// Concise canonical change description.
        description: String,
        /// Current contextual placement path.
        path: String,
    },
    /// Frontend should terminate its input loop.
    Quit,
}

/// In-memory editor session with navigation and undo/redo state.
#[derive(Clone, Debug)]
pub struct EditorSession {
    project: MediaProject,
    path: MediaPath,
    undo: Vec<MediaProject>,
    redo: Vec<MediaProject>,
}

impl EditorSession {
    /// Starts a session at the project root.
    #[must_use]
    pub fn new(project: MediaProject) -> Self {
        Self {
            project,
            path: MediaPath::root(),
            undo: Vec::new(),
            redo: Vec::new(),
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
    pub fn apply(&mut self, command: EditCommand) -> Result<CommandOutput> {
        match command {
            EditCommand::Pwd => Ok(CommandOutput::Text(self.project.display_path(&self.path)?)),
            EditCommand::List => Ok(CommandOutput::Listing(self.project.list(&self.path)?)),
            EditCommand::Info => self.info(),
            EditCommand::Cd { path } => self.cd(&path),
            EditCommand::Add {
                kind,
                alias,
                duration_frames,
                start_frame,
            } => self.mutate(|project, path| {
                let parent = project.resolve_path(path)?;
                project.add_generated(parent, kind, alias.clone(), start_frame, duration_frames)?;
                Ok(format!(
                    "add {} {alias} {duration_frames}f at {start_frame}f",
                    project
                        .list(path)?
                        .last()
                        .ok_or_else(|| { Error::InvalidState("added media is missing".into()) })?
                        .kind
                        .as_str()
                ))
            }),
            EditCommand::TrimIn { value } => self.mutate(|project, path| {
                project.trim_in(path, value.frames, value.relative)?;
                Ok(format!("in {}", display_frame_value(value)))
            }),
            EditCommand::TrimOut { value } => self.mutate(|project, path| {
                project.trim_out(path, value.frames, value.relative)?;
                Ok(format!("out {}", display_frame_value(value)))
            }),
            EditCommand::Undo => self.undo(),
            EditCommand::Redo => self.redo(),
            EditCommand::Help => Ok(CommandOutput::Text(help_text().into())),
            EditCommand::Quit => Ok(CommandOutput::Quit),
        }
    }

    fn info(&self) -> Result<CommandOutput> {
        let media_id = self.project.resolve_path(&self.path)?;
        let media = self
            .project
            .media(media_id)
            .ok_or_else(|| Error::InvalidState("current media disappeared".into()))?;
        let mut text = format!(
            "{} [{}]\nduration: {}f\nchildren: {}",
            media.name,
            media.kind.as_str(),
            media.duration.value,
            media.children().len()
        );
        if let Some(link_id) = self.path.current_link() {
            let link = self
                .project
                .link(link_id)
                .ok_or_else(|| Error::InvalidState("current placement disappeared".into()))?;
            let _ = write!(
                text,
                "\nsource: {}f..{}f\nparent timeline: {}f..{}f",
                link.source_range.start.value,
                link.source_range.end.value,
                link.timeline_range.start.value,
                link.timeline_range.end.value
            );
        }
        Ok(CommandOutput::Text(text))
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
        "pwd" => no_arguments(&tokens, EditCommand::Pwd)?,
        "ls" => no_arguments(&tokens, EditCommand::List)?,
        "info" => no_arguments(&tokens, EditCommand::Info)?,
        "undo" => no_arguments(&tokens, EditCommand::Undo)?,
        "redo" => no_arguments(&tokens, EditCommand::Redo)?,
        "help" => no_arguments(&tokens, EditCommand::Help)?,
        "quit" | "exit" => no_arguments(&tokens, EditCommand::Quit)?,
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
                    "usage: {command} <frame|+offset|-offset>"
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
        _ => {
            return Err(Error::Unsupported(format!(
                "editor command '{command}' is not implemented"
            )));
        }
    };
    Ok(Some(parsed))
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
    if duration.relative || duration.frames <= 0 {
        return Err(Error::InvalidData(
            "add duration must be a positive absolute frame count".into(),
        ));
    }
    let start_frame = if tokens.len() == 6 {
        let start = parse_frame_value(&tokens[5])?;
        if start.relative || start.frames < 0 {
            return Err(Error::InvalidData(
                "add start must be a non-negative absolute frame position".into(),
            ));
        }
        start.frames
    } else {
        0
    };
    Ok(EditCommand::Add {
        kind: MediaKind::new(tokens[1].clone())?,
        alias: tokens[2].clone(),
        duration_frames: duration.frames,
        start_frame,
    })
}

fn parse_frame_value(value: &str) -> Result<FrameValue> {
    let value = value.strip_suffix('f').unwrap_or(value);
    let relative = value.starts_with(['+', '-']);
    let frames = value
        .parse::<i64>()
        .map_err(|_| Error::InvalidData("frame value must look like 10f, +10f, or -5f".into()))?;
    Ok(FrameValue { frames, relative })
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

fn display_frame_value(value: FrameValue) -> String {
    if value.relative && value.frames >= 0 {
        format!("+{}f", value.frames)
    } else {
        format!("{}f", value.frames)
    }
}

fn help_text() -> &'static str {
    "pwd | ls | info | cd <path> | add <kind> <alias> <duration> [at <start>] | in <frame> | out <frame> | undo | redo | quit"
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
        apply(&mut session, "add video Clip0 100f");
        apply(&mut session, "cd Clip0");
        apply(&mut session, "add text 'Opening Title' 20f at 10f");
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
        apply(&mut session, "add video Clip0 100f at 5f");
        apply(&mut session, "cd Clip0");
        apply(&mut session, "in +10f");
        apply(&mut session, "out -5f");
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
    }

    #[test]
    fn absolute_paths_and_indices_traverse_links() {
        let mut session = session();
        apply(&mut session, "add video Clip0 100f");
        apply(&mut session, "cd 0");
        apply(&mut session, "add fx Grade 100f");
        apply(&mut session, "cd /Clip0/Grade");
        assert_eq!(
            session.project().display_path(session.path()).unwrap(),
            "/Clip0/Grade"
        );
        apply(&mut session, "cd ../..");
        assert_eq!(session.project().display_path(session.path()).unwrap(), "/");
    }
}
