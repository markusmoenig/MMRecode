//! Persistent command history shared by the line and full-screen editor prompts.

use std::{io::ErrorKind, path::Path};

const MAX_ENTRIES: usize = 1_000;

/// Shell-style command history with draft restoration after the newest entry.
#[derive(Debug, Default)]
pub(crate) struct CommandHistory {
    entries: Vec<String>,
    cursor: Option<usize>,
    draft: String,
}

impl CommandHistory {
    /// Loads the application history from the platform's conventional state directory.
    pub(crate) fn load_default() -> Result<Self, String> {
        let path = history_path()?;
        Self::load(&path)
    }

    /// Persists the application history in the platform's conventional state directory.
    pub(crate) fn save_default(&self) -> Result<(), String> {
        let path = history_path()?;
        self.save(&path)
    }

    fn load(path: &Path) -> Result<Self, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(format!(
                    "cannot read command history '{}': {error}",
                    path.display()
                ));
            }
        };
        let mut entries = contents
            .lines()
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
        }
        Ok(Self {
            entries,
            ..Self::default()
        })
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path
            .parent()
            .ok_or_else(|| format!("command history path '{}' has no parent", path.display()))?;
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "cannot create command history directory '{}': {error}",
                parent.display()
            )
        })?;
        let first = self.entries.len().saturating_sub(MAX_ENTRIES);
        let mut contents = self.entries[first..].join("\n");
        if !contents.is_empty() {
            contents.push('\n');
        }
        std::fs::write(path, contents)
            .map_err(|error| format!("cannot write command history '{}': {error}", path.display()))
    }

    /// Adds one non-empty command, suppressing immediately repeated entries.
    pub(crate) fn record(&mut self, command: &str) {
        let command = command.trim();
        if !command.is_empty() && self.entries.last().is_none_or(|entry| entry != command) {
            self.entries.push(command.to_owned());
        }
        self.reset_navigation();
    }

    /// Selects the previous history entry, saving the current draft on first navigation.
    pub(crate) fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let index = if let Some(index) = self.cursor {
            index.saturating_sub(1)
        } else {
            current.clone_into(&mut self.draft);
            self.entries.len() - 1
        };
        self.cursor = Some(index);
        self.entries.get(index).cloned()
    }

    /// Selects the next history entry or restores the saved draft past the newest entry.
    pub(crate) fn next(&mut self) -> Option<String> {
        let index = self.cursor?;
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            return self.entries.get(index + 1).cloned();
        }
        self.cursor = None;
        Some(self.draft.clone())
    }

    /// Detaches an edited recalled command from history navigation.
    pub(crate) fn detach(&mut self) {
        self.reset_navigation();
    }

    fn reset_navigation(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}

fn history_path() -> Result<std::path::PathBuf, String> {
    let project = directories::ProjectDirs::from("com", "mmrecode", "MMRecode")
        .ok_or_else(|| "cannot determine the MMRecode application state directory".to_owned())?;
    let directory = project
        .state_dir()
        .unwrap_or_else(|| project.data_local_dir());
    Ok(directory.join("history"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigates_commands_and_restores_the_unsubmitted_draft() {
        let mut history = CommandHistory::default();
        history.record("open source.ts");
        history.record("out 1:15");

        assert_eq!(history.previous("in +0:10").as_deref(), Some("out 1:15"));
        assert_eq!(
            history.previous("ignored").as_deref(),
            Some("open source.ts")
        );
        assert_eq!(
            history.previous("ignored").as_deref(),
            Some("open source.ts")
        );
        assert_eq!(history.next().as_deref(), Some("out 1:15"));
        assert_eq!(history.next().as_deref(), Some("in +0:10"));
        assert_eq!(history.next(), None);
    }

    #[test]
    fn ignores_empty_and_immediately_repeated_commands() {
        let mut history = CommandHistory::default();
        history.record("  ");
        history.record("info");
        history.record("info");
        assert_eq!(history.previous("").as_deref(), Some("info"));
        assert_eq!(history.previous("").as_deref(), Some("info"));
    }

    #[test]
    fn persists_history_across_instances() {
        let path =
            std::env::temp_dir().join(format!("mmrecode-command-history-{}", std::process::id()));
        let mut history = CommandHistory::default();
        history.record("open source.ts");
        history.record("out -0:10");
        history.save(&path).unwrap();

        let mut loaded = CommandHistory::load(&path).unwrap();
        assert_eq!(loaded.previous("").as_deref(), Some("out -0:10"));
        assert_eq!(loaded.previous("").as_deref(), Some("open source.ts"));
        std::fs::remove_file(path).unwrap();
    }
}
