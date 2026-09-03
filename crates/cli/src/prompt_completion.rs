//! Context-aware completion for the full-screen editor prompt.

use std::path::{Path, PathBuf};

use mmrecode_edit::{
    EDITOR_COMMAND_NAMES, EDITOR_MANUAL_TOPICS, EXPORT_PRESET_NAMES, EditorSession,
    PROJECT_SETTING_NAMES, ProjectSettings,
};

const INFO_TOPICS: &[&str] = &["audio", "project", "source", "video"];
const PROJECT_COMMANDS: &[&str] = &["info", "match", "preset", "presets", "set"];
const RATE_CONFORM_POLICIES: &[&str] = &["frames", "time"];
const SCALE_MODES: &[&str] = &["fill", "fit", "native", "stretch"];

/// A prompt replacement and the candidates that produced it.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Completion {
    pub(crate) replacement: String,
    pub(crate) candidates: Vec<String>,
}

/// Completes commands, manual/info topics, hierarchy aliases, and `open` filesystem paths.
pub(crate) fn complete(input: &str, session: &EditorSession, base_directory: &Path) -> Completion {
    if !input.contains(char::is_whitespace) {
        return complete_words(input, "", EDITOR_COMMAND_NAMES);
    }
    if let Some(partial) = input.strip_prefix("man ") {
        return complete_words(partial, "man ", EDITOR_MANUAL_TOPICS);
    }
    if let Some(partial) = input.strip_prefix("info ") {
        return complete_words(partial, "info ", INFO_TOPICS);
    }
    if let Some(partial) = input.strip_prefix("scale ") {
        return complete_words(partial, "scale ", SCALE_MODES);
    }
    if let Some(partial) = input.strip_prefix("project preset ") {
        return complete_words(partial, "project preset ", ProjectSettings::preset_names());
    }
    if let Some(partial) = input.strip_prefix("project set ") {
        if let Some((setting, policy)) = partial.rsplit_once(" conform ") {
            return complete_words(
                policy,
                &format!("project set {setting} conform "),
                RATE_CONFORM_POLICIES,
            );
        }
        if !partial.contains(char::is_whitespace) {
            return complete_words(partial, "project set ", PROJECT_SETTING_NAMES);
        }
    }
    if let Some(partial) = input.strip_prefix("project ") {
        return complete_words(partial, "project ", PROJECT_COMMANDS);
    }
    if let Some(partial) = input.strip_prefix("export plan using ") {
        return complete_words(partial, "export plan using ", EXPORT_PRESET_NAMES);
    }
    if let Some(partial) = input.strip_prefix("new ")
        && let Some((project, preset)) = partial.rsplit_once(" using ")
    {
        return complete_words(
            preset,
            &format!("new {project} using "),
            ProjectSettings::preset_names(),
        );
    }
    if let Some(partial) = input.strip_prefix("cd ") {
        let mut aliases = vec![".".to_owned(), "..".to_owned()];
        if let Ok(entries) = session.project().list(session.path()) {
            aliases.extend(entries.into_iter().map(|entry| entry.alias));
        }
        aliases.sort();
        return complete_aliases(partial, &aliases);
    }
    if let Some(partial) = input.strip_prefix("import ") {
        return complete_path(partial, "import ", Some(" as "), base_directory);
    }
    if let Some(partial) = input.strip_prefix("open ") {
        return complete_path(partial, "open ", None, base_directory);
    }
    if let Some(partial) = input.strip_prefix("save as ") {
        return complete_path(partial, "save as ", None, base_directory);
    }
    if let Some(partial) = input.strip_prefix("export ")
        && !partial.starts_with("plan")
    {
        if let Some((locator, preset)) = partial.rsplit_once(" using ") {
            return complete_words(
                preset,
                &format!("export {locator} using "),
                EXPORT_PRESET_NAMES,
            );
        }
        return complete_path(partial, "export ", Some(" using "), base_directory);
    }
    Completion {
        replacement: input.to_owned(),
        candidates: Vec::new(),
    }
}

fn complete_words(partial: &str, command_prefix: &str, words: &[&str]) -> Completion {
    let candidates = words
        .iter()
        .filter(|word| word.starts_with(partial))
        .map(|word| (*word).to_owned())
        .collect::<Vec<_>>();
    finish_word_completion(partial, command_prefix, candidates)
}

fn complete_aliases(partial: &str, aliases: &[String]) -> Completion {
    let decoded = decode_partial_path(partial);
    let candidates = aliases
        .iter()
        .filter(|word| word.starts_with(&decoded))
        .cloned()
        .collect::<Vec<_>>();
    let common = common_prefix(&candidates);
    let completed = if candidates.len() == 1 {
        format!("cd {} ", quote_argument(&candidates[0], false))
    } else if common.len() > decoded.len() {
        format!("cd {}", quote_argument(&common, true))
    } else {
        format!("cd {partial}")
    };
    Completion {
        replacement: completed,
        candidates,
    }
}

fn finish_word_completion(
    partial: &str,
    command_prefix: &str,
    candidates: Vec<String>,
) -> Completion {
    let common = common_prefix(&candidates);
    let completed = if candidates.len() == 1 {
        format!("{}{} ", command_prefix, candidates[0])
    } else if common.len() > partial.len() {
        format!("{command_prefix}{common}")
    } else {
        format!("{command_prefix}{partial}")
    };
    Completion {
        replacement: completed,
        candidates,
    }
}

fn complete_path(
    partial: &str,
    command_prefix: &str,
    stop_marker: Option<&str>,
    base_directory: &Path,
) -> Completion {
    if stop_marker.is_some_and(|marker| partial.contains(marker)) {
        return Completion {
            replacement: format!("{command_prefix}{partial}"),
            candidates: Vec::new(),
        };
    }
    let decoded = decode_partial_path(partial);
    let requested = Path::new(&decoded);
    let ends_with_separator = decoded.ends_with(std::path::MAIN_SEPARATOR);
    let directory_fragment = if ends_with_separator {
        requested
    } else {
        requested.parent().unwrap_or_else(|| Path::new(""))
    };
    let name_prefix = if ends_with_separator {
        ""
    } else {
        requested
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("")
    };
    let search_directory = if directory_fragment.as_os_str().is_empty() {
        base_directory.to_owned()
    } else if directory_fragment.is_absolute() {
        directory_fragment.to_owned()
    } else {
        base_directory.join(directory_fragment)
    };

    let mut matches = std::fs::read_dir(&search_directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(name_prefix)
                || (name.starts_with('.') && !name_prefix.starts_with('.'))
            {
                return None;
            }
            let mut candidate = join_display_path(directory_fragment, &name);
            let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_directory {
                candidate.push(std::path::MAIN_SEPARATOR);
            }
            Some((candidate, is_directory))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0));
    let candidates = matches
        .iter()
        .map(|(candidate, _)| candidate.clone())
        .collect::<Vec<_>>();
    let common = common_prefix(&candidates);
    let replacement_path = if matches.len() == 1 {
        let (candidate, is_directory) = &matches[0];
        quote_path(candidate, *is_directory)
    } else if common.len() > decoded.len() {
        quote_path(&common, true)
    } else {
        partial.to_owned()
    };
    Completion {
        replacement: format!("{command_prefix}{replacement_path}"),
        candidates,
    }
}

fn decode_partial_path(partial: &str) -> String {
    let partial = partial.strip_prefix('"').unwrap_or(partial);
    partial
        .strip_suffix('"')
        .unwrap_or(partial)
        .replace("\\ ", " ")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn quote_path(path: &str, keep_open: bool) -> String {
    quote_argument(path, keep_open)
}

fn quote_argument(value: &str, keep_open: bool) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    if keep_open || value.contains(char::is_whitespace) {
        if keep_open {
            return format!("\"{escaped}");
        }
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn join_display_path(directory: &Path, name: &str) -> String {
    let path = if directory.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        directory.join(name)
    };
    path.to_string_lossy().into_owned()
}

fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for candidate in &candidates[1..] {
        while !candidate.starts_with(&prefix) {
            if prefix.pop().is_none() {
                break;
            }
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use mmrecode_core::Rational;
    use mmrecode_edit::MediaProject;

    use super::*;

    fn session() -> EditorSession {
        EditorSession::new(MediaProject::new("Film", Rational::new(1, 25).unwrap()).unwrap())
    }

    #[test]
    fn completes_commands_topics_and_hierarchy_aliases() {
        let mut session = session();
        let command = complete("op", &session, Path::new("."));
        assert_eq!(command.replacement, "open ");
        let topic = complete("info vi", &session, Path::new("."));
        assert_eq!(topic.replacement, "info video ");
        let setting = complete("project set ra", &session, Path::new("."));
        assert_eq!(setting.replacement, "project set rate ");
        let policy = complete("project set rate 25 conform t", &session, Path::new("."));
        assert_eq!(policy.replacement, "project set rate 25 conform time ");
        let project_match = complete("project ma", &session, Path::new("."));
        assert_eq!(project_match.replacement, "project match ");
        let project_preset = complete("new Demo using pal", &session, Path::new("."));
        assert_eq!(project_preset.replacement, "new Demo using pal-576i25 ");
        let export_preset = complete("export output.ts using mpeg", &session, Path::new("."));
        assert_eq!(
            export_preset.replacement,
            "export output.ts using mpeg2-ts "
        );
        let manual = complete("man mo", &session, Path::new("."));
        assert_eq!(manual.replacement, "man move ");
        let scale = complete("scale fi", &session, Path::new("."));
        assert_eq!(scale.replacement, "scale fi");
        assert_eq!(scale.candidates, vec!["fill", "fit"]);

        let add = mmrecode_edit::parse_command("add video Clip0 1:00")
            .unwrap()
            .unwrap();
        session.apply(add).unwrap();
        let alias = complete("cd Cl", &session, Path::new("."));
        assert_eq!(alias.replacement, "cd Clip0 ");

        let add = mmrecode_edit::parse_command("add text 'Opening Title' 0:10")
            .unwrap()
            .unwrap();
        session.apply(add).unwrap();
        let spaced = complete("cd Op", &session, Path::new("."));
        assert_eq!(spaced.replacement, "cd \"Opening Title\" ");
    }

    #[test]
    fn completes_files_and_quotes_spaces() {
        let directory =
            std::env::temp_dir().join(format!("mmrecode-completion-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("Media Folder")).unwrap();
        std::fs::write(directory.join("Media Folder").join("clip one.ts"), []).unwrap();

        let folder = complete("open Med", &session(), &directory);
        assert_eq!(folder.replacement, "open \"Media Folder/");
        let file = complete("open \"Media Folder/clip", &session(), &directory);
        assert_eq!(file.replacement, "open \"Media Folder/clip one.ts\"");

        std::fs::remove_dir_all(directory).unwrap();
    }
}
