//! Interactive terminal graphics preview.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{IsTerminal as _, Write as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime},
};

use image::{DynamicImage, Rgb, RgbImage};
use mmrecode_core::{ColorRange, PixelFormat, Plane, Rational, VideoFrame};
use mmrecode_edit::{
    CommandOutput, EditCommand, EditorSession, MediaOrigin, MediaPath, format_compact_timecode,
};
use mmrecode_mpeg2::DecodedMpeg2Picture;
use mmrecode_playback::{
    Mpeg2PlaybackEvent, Mpeg2PlaybackSource, PlaybackController, PlaybackTimeline,
};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::{
        ExecutableCommand as _,
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        },
    },
    layout::{Alignment, Constraint, Layout, Rect, Size},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::{
    FilterType, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    thread::{ResizeRequest, ResizeResponse, ThreadProtocol},
};

use crate::command_history::CommandHistory;
use crate::prompt_completion;

const LOOK_AHEAD: usize = 23;
const BUFFER_FRAMES: usize = 8;
const REFILL_THRESHOLD: usize = 12;
const CACHE_FRAMES: usize = 36;
const EVENT_WAIT: Duration = Duration::from_millis(8);

/// Runs the interactive terminal preview for an MPEG-2 elementary or transport stream.
pub(crate) fn run(path: &Path) -> Result<(), String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("preview requires an interactive terminal on stdin and stdout".into());
    }

    let source = open_source(path)?;
    let mut terminal = ratatui::try_init()
        .map_err(|error| format!("cannot initialize terminal preview: {error}"))?;
    let result = run_initialized(&mut terminal, source, path);
    let restore_result = ratatui::try_restore()
        .map_err(|error| format!("cannot restore terminal after preview: {error}"));
    result.and(restore_result)
}

pub(crate) fn open_source(path: &Path) -> Result<Mpeg2PlaybackSource, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    let elementary = if bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE && bytes[0] == 0x47 {
        mmrecode_mpegts::demux_transport_stream(&bytes)
            .map_err(|error| error.to_string())?
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string())?
    } else {
        bytes
    };
    Mpeg2PlaybackSource::new(elementary)
}

/// Runs the full-screen editor shell, initially with an empty project.
pub(crate) fn run_editor(
    session: &mut EditorSession,
    history: &mut CommandHistory,
    base_directory: &Path,
) -> Result<(), String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("interactive editor preview requires a terminal on stdin and stdout".into());
    }
    let mut terminal = ratatui::try_init()
        .map_err(|error| format!("cannot initialize terminal editor: {error}"))?;
    let result = run_editor_initialized(&mut terminal, session, history, base_directory);
    let restore_result = ratatui::try_restore()
        .map_err(|error| format!("cannot restore terminal after editor: {error}"));
    result.and(restore_result)
}

type ResizeWorker = (
    mpsc::Sender<ResizeRequest>,
    Receiver<Result<ResizeResponse, String>>,
    thread::JoinHandle<()>,
);

fn spawn_resize_worker() -> Result<ResizeWorker, String> {
    let (resize_tx, resize_rx) = mpsc::channel::<ResizeRequest>();
    let (complete_tx, complete_rx) = mpsc::channel::<Result<ResizeResponse, String>>();
    let worker = thread::Builder::new()
        .name("mmrecode-terminal-image".into())
        .spawn(move || {
            while let Ok(request) = resize_rx.recv() {
                let result = request.resize_encode().map_err(|error| error.to_string());
                if complete_tx.send(result).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("cannot start terminal image worker: {error}"))?;
    Ok((resize_tx, complete_rx, worker))
}

fn run_initialized(
    terminal: &mut DefaultTerminal,
    source: Mpeg2PlaybackSource,
    path: &Path,
) -> Result<(), String> {
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let protocol = picker.protocol_type();
    let (resize_tx, complete_rx, resize_worker) = spawn_resize_worker()?;

    let size = terminal
        .size()
        .map_err(|error| format!("cannot read terminal size: {error}"))?;
    let mut app = PreviewApp::new(source, picker, resize_tx, path, size)?;
    app.request_frame(0)?;
    let result = event_loop(terminal, &mut app, &complete_rx);
    let clear_result = app.clear_kitty();
    drop(app);
    resize_worker
        .join()
        .map_err(|_| "terminal image worker panicked".to_owned())?;
    result
        .and(clear_result)
        .map_err(|error| format!("{error} (graphics protocol: {})", protocol_name(protocol)))
}

fn run_editor_initialized(
    terminal: &mut DefaultTerminal,
    session: &mut EditorSession,
    history: &mut CommandHistory,
    base_directory: &Path,
) -> Result<(), String> {
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let protocol = picker.protocol_type();
    let (resize_tx, complete_rx, resize_worker) = spawn_resize_worker()?;
    let mut app = None;
    std::io::stdout()
        .execute(EnableMouseCapture)
        .map_err(|error| format!("cannot enable terminal timeline mouse input: {error}"))?;
    let mut editor = EditorUi {
        message: "Ready. Type import <media-file>, or use help / man <command>.".into(),
        inspector_focus: InspectorFocus::Help,
        ..EditorUi::default()
    };
    let result = {
        let mut host = EditorHost {
            resize_tx: &resize_tx,
            picker: &picker,
            base_directory,
            terminal_size: terminal
                .size()
                .map_err(|error| format!("cannot read terminal size: {error}"))?,
        };
        editor_event_loop(
            terminal,
            &mut app,
            session,
            history,
            &mut editor,
            &complete_rx,
            &mut host,
        )
    };
    let mouse_result = std::io::stdout()
        .execute(DisableMouseCapture)
        .map(|_| ())
        .map_err(|error| format!("cannot disable terminal timeline mouse input: {error}"));
    let clear_result = app.as_mut().map_or(Ok(()), PreviewApp::clear_kitty);
    drop(app);
    drop(resize_tx);
    resize_worker
        .join()
        .map_err(|_| "terminal image worker panicked".to_owned())?;
    result
        .and(mouse_result)
        .and(clear_result)
        .map_err(|error| format!("{error} (graphics protocol: {})", protocol_name(protocol)))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum InspectorFocus {
    #[default]
    Context,
    InPoint,
    OutPoint,
    Help,
    Manual,
    ProjectInfo,
    ProjectPresets,
    VideoInfo,
    AudioInfo,
    SourceInfo,
    ExportReport,
}

#[derive(Default)]
struct EditorUi {
    input: String,
    message: String,
    timeline_area: Rect,
    inspector_focus: InspectorFocus,
    last_command: Option<String>,
    panel_text: Option<String>,
}

struct EditorHost<'a> {
    resize_tx: &'a mpsc::Sender<ResizeRequest>,
    picker: &'a Picker,
    base_directory: &'a Path,
    terminal_size: Size,
}

fn editor_event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut Option<PreviewApp>,
    session: &mut EditorSession,
    history: &mut CommandHistory,
    editor: &mut EditorUi,
    completed: &Receiver<Result<ResizeResponse, String>>,
    host: &mut EditorHost<'_>,
) -> Result<(), String> {
    let mut redraw = true;
    let mut last_frame = None;
    let mut last_status = String::new();
    let mut last_range = None;
    loop {
        if let Some(app) = app.as_mut() {
            app.tick(Instant::now())?;
        }
        let resized = app
            .as_mut()
            .is_some_and(|app| receive_resized_images(app, completed));
        let current = app.as_ref().map(|app| app.playback.frame_index());
        let status = app.as_ref().map_or("empty", PreviewApp::status);
        let range = app.as_ref().map(|app| app.playback_range.clone());
        if redraw
            || resized
            || current != last_frame
            || status != last_status
            || range != last_range
        {
            terminal
                .draw(|frame| draw_editor(frame, app.as_mut(), session, editor))
                .map_err(|error| format!("cannot draw terminal editor: {error}"))?;
            last_frame = current;
            status.clone_into(&mut last_status);
            last_range = range;
            redraw = false;
        }
        if let Some(app) = app.as_mut() {
            app.flush_kitty_frame()?;
        }

        if event::poll(EVENT_WAIT).map_err(|error| format!("cannot poll terminal: {error}"))? {
            redraw = true;
            match event::read().map_err(|error| format!("cannot read terminal input: {error}"))? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    host.terminal_size = terminal.size().map_err(|error| {
                        format!("cannot read terminal size while opening media: {error}")
                    })?;
                    if handle_editor_key(app, session, history, editor, key, Instant::now(), host)?
                    {
                        return Ok(());
                    }
                }
                Event::Resize(width, height) => {
                    host.terminal_size = Size::new(width, height);
                    if let Some(app) = app.as_mut() {
                        app.set_terminal_size(Size::new(width, height));
                    }
                }
                Event::Mouse(mouse) => {
                    if let Some(app) = app.as_mut() {
                        handle_editor_mouse(app, editor, mouse, Instant::now())?;
                    }
                }
                _ => {}
            }
        }
    }
}

fn receive_resized_images(
    app: &mut PreviewApp,
    completed: &Receiver<Result<ResizeResponse, String>>,
) -> bool {
    let mut received = false;
    while let Ok(response) = completed.try_recv() {
        received = true;
        match response {
            Ok(response) => {
                if let Some(image_state) = &mut app.image_state {
                    image_state.update_resized_protocol(response);
                }
            }
            Err(error) => app.error = Some(error),
        }
    }
    received
}

#[allow(clippy::too_many_lines)]
fn handle_editor_key(
    app: &mut Option<PreviewApp>,
    session: &mut EditorSession,
    history: &mut CommandHistory,
    editor: &mut EditorUi,
    key: KeyEvent,
    now: Instant,
    host: &EditorHost<'_>,
) -> Result<bool, String> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q' | 'c') => {
                if session.is_dirty() {
                    editor.message =
                        "error: project has unsaved changes; use save or quit --discard".into();
                    return Ok(false);
                }
                return Ok(true);
            }
            KeyCode::Char(' ') => {
                if let Some(app) = app.as_mut() {
                    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), now)?;
                } else {
                    editor.message = "No media loaded. Use import <media-file>.".into();
                }
            }
            KeyCode::Char('z') => {
                editor.input = "undo".into();
                return execute_editor_input(app, session, history, editor, now, host);
            }
            KeyCode::Char('y') => {
                editor.input = "redo".into();
                return execute_editor_input(app, session, history, editor, now, host);
            }
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Enter => execute_editor_input(app, session, history, editor, now, host),
        KeyCode::Tab => {
            let completion =
                prompt_completion::complete(&editor.input, session, host.base_directory);
            let changed = completion.replacement != editor.input;
            editor.input = completion.replacement;
            history.detach();
            editor.message = match completion.candidates.as_slice() {
                [] => "No completion matches this context.".into(),
                [candidate] if changed => format!("Completed: {candidate}"),
                candidates => completion_candidates_message(candidates),
            };
            Ok(false)
        }
        KeyCode::Backspace => {
            editor.input.pop();
            history.detach();
            Ok(false)
        }
        KeyCode::Esc => {
            editor.input.clear();
            history.detach();
            Ok(false)
        }
        KeyCode::Left if editor.input.is_empty() => {
            let amount = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            if let Some(app) = app.as_mut() {
                app.step(-amount, now)?;
                editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
            }
            Ok(false)
        }
        KeyCode::Right if editor.input.is_empty() => {
            let amount = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            if let Some(app) = app.as_mut() {
                app.step(amount, now)?;
                editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
            }
            Ok(false)
        }
        KeyCode::PageUp if editor.input.is_empty() => {
            if let Some(app) = app.as_mut() {
                let amount = isize::try_from(app.nominal_frames_per_second()).unwrap_or(isize::MAX);
                app.step(-amount, now)?;
                editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
            }
            Ok(false)
        }
        KeyCode::PageDown if editor.input.is_empty() => {
            if let Some(app) = app.as_mut() {
                let amount = isize::try_from(app.nominal_frames_per_second()).unwrap_or(isize::MAX);
                app.step(amount, now)?;
                editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
            }
            Ok(false)
        }
        KeyCode::Up => {
            if let Some(previous) = history.previous(&editor.input) {
                editor.input = previous;
            }
            Ok(false)
        }
        KeyCode::Down => {
            if let Some(next) = history.next() {
                editor.input = next;
            }
            Ok(false)
        }
        KeyCode::Home | KeyCode::End if editor.input.is_empty() => {
            if let Some(app) = app.as_mut() {
                app.handle_key(key, now)?;
                editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
            }
            Ok(false)
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            editor.input.push(character);
            history.detach();
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn completion_candidates_message(candidates: &[String]) -> String {
    const VISIBLE_CANDIDATES: usize = 8;
    let mut message = format!(
        "Matches: {}",
        candidates
            .iter()
            .take(VISIBLE_CANDIDATES)
            .cloned()
            .collect::<Vec<_>>()
            .join("   ")
    );
    if candidates.len() > VISIBLE_CANDIDATES {
        let _ = write!(
            message,
            "   … and {} more",
            candidates.len() - VISIBLE_CANDIDATES
        );
    }
    message
}

fn handle_editor_mouse(
    app: &mut PreviewApp,
    editor: &mut EditorUi,
    mouse: MouseEvent,
    now: Instant,
) -> Result<(), String> {
    if !editor
        .timeline_area
        .contains((mouse.column, mouse.row).into())
    {
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
            let relative = usize::from(mouse.column.saturating_sub(editor.timeline_area.x));
            let width = usize::from(editor.timeline_area.width);
            let source_frame = frame_at_timeline_column(relative, width, app.frame_count());
            app.seek_frame(source_frame, now)?;
            editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
        }
        MouseEventKind::ScrollUp => {
            let amount = isize::try_from(app.nominal_frames_per_second()).unwrap_or(isize::MAX);
            app.step(-amount, now)?;
            editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
        }
        MouseEventKind::ScrollDown => {
            let amount = isize::try_from(app.nominal_frames_per_second()).unwrap_or(isize::MAX);
            app.step(amount, now)?;
            editor.message = format!("scrub: {}", app.timecode(app.playback.frame_index()));
        }
        _ => {}
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_editor_input(
    app: &mut Option<PreviewApp>,
    session: &mut EditorSession,
    history: &mut CommandHistory,
    editor: &mut EditorUi,
    now: Instant,
    host: &EditorHost<'_>,
) -> Result<bool, String> {
    let line = std::mem::take(&mut editor.input);
    history.record(&line);
    let expanded = match expand_context_command(&line, editor.inspector_focus) {
        Ok(expanded) => expanded,
        Err(error) => {
            editor.message = format!("error: {error}");
            return Ok(false);
        }
    };
    let command = match mmrecode_edit::parse_command(&expanded) {
        Ok(Some(command)) => command,
        Ok(None) => return Ok(false),
        Err(error) => {
            editor.message = format!("error: {error}");
            return Ok(false);
        }
    };
    let inspector_focus = match &command {
        EditCommand::TrimIn { .. } => InspectorFocus::InPoint,
        EditCommand::TrimOut { .. } => InspectorFocus::OutPoint,
        EditCommand::Help => InspectorFocus::Help,
        EditCommand::Man { .. } => InspectorFocus::Manual,
        EditCommand::InfoTopic { topic } => match topic.as_str() {
            "project" => InspectorFocus::ProjectInfo,
            "video" => InspectorFocus::VideoInfo,
            "audio" => InspectorFocus::AudioInfo,
            "source" => InspectorFocus::SourceInfo,
            _ => InspectorFocus::Context,
        },
        EditCommand::ProjectPresets => InspectorFocus::ProjectPresets,
        EditCommand::ProjectMatch
        | EditCommand::ProjectPreset { .. }
        | EditCommand::ProjectSet { .. } => InspectorFocus::ProjectInfo,
        _ => InspectorFocus::Context,
    };
    let output = match session.apply(command) {
        Ok(output) => output,
        Err(error) => {
            editor.message = format!("error: {error}");
            return Ok(false);
        }
    };
    editor.inspector_focus = inspector_focus;
    editor.last_command = Some(line.trim().to_owned());
    let showing_help = matches!(
        inspector_focus,
        InspectorFocus::Help | InspectorFocus::Manual
    );
    editor.panel_text = matches!(
        inspector_focus,
        InspectorFocus::Manual | InspectorFocus::ProjectPresets
    )
    .then(|| editor_output_text(&output));
    let output = match output {
        CommandOutput::QuitRequested { discard } => {
            match crate::protect_unsaved(session, discard) {
                Ok(()) => return Ok(true),
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        CommandOutput::ImportRequested { locator, alias } => {
            match load_editor_media(
                session,
                host.base_directory,
                &locator,
                alias,
                host.picker,
                host.resize_tx,
                host.terminal_size,
                now,
            ) {
                Ok((loaded, message)) => {
                    if let Some(previous) = app.as_mut() {
                        previous.clear_kitty()?;
                    }
                    *app = Some(loaded);
                    editor.inspector_focus = InspectorFocus::Context;
                    editor.message = message;
                }
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        CommandOutput::ProjectMatchRequested => {
            match crate::match_project_to_focused_media(session) {
                Ok(output) => output,
                Err(error) => {
                    editor.message = format!("error: {error}");
                    return Ok(false);
                }
            }
        }
        CommandOutput::NewProjectRequested {
            name,
            preset,
            discard,
        } => {
            let result = crate::protect_unsaved(session, discard).and_then(|()| {
                mmrecode_edit::MediaProject::from_preset(name, &preset)
                    .map_err(|error| error.to_string())
            });
            match result {
                Ok(project) => {
                    if let Some(previous) = app.as_mut() {
                        previous.clear_kitty()?;
                    }
                    *app = None;
                    session.replace_new_project(project);
                    editor.inspector_focus = InspectorFocus::ProjectInfo;
                    editor.message = format!("ok: new project using {preset}");
                }
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        CommandOutput::OpenProjectRequested { locator, discard } => {
            let result = crate::protect_unsaved(session, discard).and_then(|()| {
                let path = crate::resolve_existing_path(host.base_directory, &locator, "project")?;
                let project =
                    mmrecode_edit::load_project_file(&path).map_err(|error| error.to_string())?;
                Ok((project, path))
            });
            match result {
                Ok((project, path)) => {
                    if let Some(previous) = app.as_mut() {
                        previous.clear_kitty()?;
                    }
                    session.replace_loaded_project(project, path.clone());
                    let preview = load_project_preview(
                        session,
                        &path,
                        host.picker,
                        host.resize_tx,
                        host.terminal_size,
                        now,
                    );
                    match preview {
                        Ok(loaded) => {
                            *app = loaded;
                            editor.message = format!("ok: opened {}", path.display());
                        }
                        Err(error) => {
                            *app = None;
                            editor.message = format!(
                                "ok: opened {} (preview unavailable: {error})",
                                path.display()
                            );
                        }
                    }
                    editor.inspector_focus = InspectorFocus::ProjectInfo;
                }
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        CommandOutput::SaveProjectRequested { locator } => {
            let save_as = locator.is_some();
            let result = locator.map_or_else(
                || {
                    session
                        .project_file()
                        .map(Path::to_path_buf)
                        .ok_or_else(|| "project has no file yet; use save as <project>".to_owned())
                },
                |locator| Ok(crate::resolve_output_path(host.base_directory, &locator)),
            );
            match result.and_then(|path| crate::save_editor_project(session, &path, save_as)) {
                Ok(path) => {
                    editor.message = format!("ok: saved {}", path.display());
                }
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        CommandOutput::ExportRequested { locator, preset } => {
            let output_path = locator
                .as_deref()
                .map(|locator| crate::resolve_output_path(host.base_directory, locator));
            match crate::editor_export::export_project(
                session,
                output_path.as_deref(),
                preset.as_deref(),
            ) {
                Ok(report) => {
                    editor.panel_text = Some(report.clone());
                    editor.inspector_focus = InspectorFocus::ExportReport;
                    editor.message = report.lines().next().unwrap_or("export complete").into();
                }
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        output => output,
    };
    let changed = matches!(output, CommandOutput::Changed { .. });
    editor.message = if showing_help {
        format!("Showing help for '{}'.", line.trim())
    } else {
        editor_output_text(&output)
    };
    if changed && let Some(app) = app.as_mut() {
        if let Ok(range) = editor_source_range(session) {
            let current = app.playback.frame_index();
            let target = current.clamp(range.start, range.end - 1);
            app.set_playback_range(range, target, now)?;
        } else {
            app.playback.pause(now);
            editor
                .message
                .push_str("  (no previewable source; redo restores it)");
        }
    }
    Ok(false)
}

fn expand_context_command(line: &str, focus: InspectorFocus) -> Result<String, String> {
    let tokens = line.split_whitespace().collect::<Vec<_>>();
    let (direction, value) = match tokens.as_slice() {
        [direction @ ("left" | "right"), value]
        | ["move", direction @ ("left" | "right"), value] => (*direction, *value),
        _ => return Ok(line.to_owned()),
    };
    let boundary = match focus {
        InspectorFocus::InPoint => "in",
        InspectorFocus::OutPoint => "out",
        _ => {
            return Err(
                "left/right is available after an in or out command selects a boundary".into(),
            );
        }
    };
    if value.starts_with(['+', '-']) {
        return Err("left/right takes an unsigned time such as 0:13".into());
    }
    let sign = if direction == "left" { '-' } else { '+' };
    Ok(format!("{boundary} {sign}{value}"))
}

#[allow(clippy::too_many_arguments)]
fn load_editor_media(
    session: &mut EditorSession,
    base_directory: &Path,
    locator: &str,
    requested_alias: Option<String>,
    picker: &Picker,
    resize_tx: &mpsc::Sender<ResizeRequest>,
    terminal_size: Size,
    now: Instant,
) -> Result<(PreviewApp, String), String> {
    let requested = Path::new(locator);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base_directory.join(requested)
    };
    let path = std::fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "cannot open editor media '{}': {error}",
            candidate.display()
        )
    })?;
    let source = open_source(&path)?;
    let rate = source.index().frame_rate();
    let time_base =
        Rational::new(rate.denominator(), rate.numerator()).map_err(|error| error.to_string())?;
    let duration = i64::try_from(source.index().frame_count())
        .map_err(|_| "media frame count exceeds editor limits".to_owned())?;
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Media")
        .to_owned();
    let alias = requested_alias.unwrap_or_else(|| {
        path.file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("Media")
            .to_owned()
    });
    let mut loaded = PreviewApp::new(
        source,
        picker.clone(),
        resize_tx.clone(),
        &path,
        terminal_size,
    )?;
    let changed = session
        .add_imported_media(&mmrecode_edit::ImportedMedia {
            name: name.clone(),
            alias: alias.clone(),
            kind: mmrecode_edit::MediaKind::new("video/mpeg2")
                .map_err(|error| error.to_string())?,
            time_base,
            duration,
            origin: MediaOrigin::External { path: path.clone() },
        })
        .map_err(|error| error.to_string())?;
    session
        .apply(EditCommand::Cd {
            path: alias.clone(),
        })
        .map_err(|error| error.to_string())?;
    let range = editor_source_range(session)?;
    loaded.set_playback_range(range.clone(), range.start, now)?;
    let description = editor_output_text(&changed);
    Ok((loaded, format!("{description}; entered /{alias}")))
}

fn load_project_preview(
    session: &EditorSession,
    project_path: &Path,
    picker: &Picker,
    resize_tx: &mpsc::Sender<ResizeRequest>,
    terminal_size: Size,
    now: Instant,
) -> Result<Option<PreviewApp>, String> {
    let Some(entry) = session
        .project()
        .list(&MediaPath::root())
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|entry| entry.kind.as_str() == "video/mpeg2")
    else {
        return Ok(None);
    };
    let link = session
        .project()
        .link(entry.link_id)
        .ok_or_else(|| "project preview placement disappeared".to_owned())?;
    let media = session
        .project()
        .media(link.media_id)
        .ok_or_else(|| "project preview media disappeared".to_owned())?;
    let path = match &media.origin {
        MediaOrigin::Managed { path } => project_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path),
        MediaOrigin::External { path } => path.clone(),
        _ => return Ok(None),
    };
    let source = open_source(&path)?;
    let mut loaded = PreviewApp::new(
        source,
        picker.clone(),
        resize_tx.clone(),
        &path,
        terminal_size,
    )?;
    let start = usize::try_from(link.source_range.start.value)
        .map_err(|_| "project preview in-point is invalid".to_owned())?;
    let end = usize::try_from(link.source_range.end.value)
        .map_err(|_| "project preview out-point is invalid".to_owned())?;
    loaded.set_playback_range(start..end, start, now)?;
    Ok(Some(loaded))
}

fn editor_source_range(session: &EditorSession) -> Result<Range<usize>, String> {
    let link_id = session
        .path()
        .current_link()
        .or_else(|| {
            session
                .project()
                .list(&MediaPath::root())
                .ok()?
                .first()
                .map(|entry| entry.link_id)
        })
        .ok_or_else(|| "the project has no previewable media placement".to_owned())?;
    let link = session
        .project()
        .link(link_id)
        .ok_or_else(|| "the preview media placement is missing".to_owned())?;
    let start = usize::try_from(link.source_range.start.value)
        .map_err(|_| "the preview in-point is negative or too large".to_owned())?;
    let end = usize::try_from(link.source_range.end.value)
        .map_err(|_| "the preview out-point is negative or too large".to_owned())?;
    if start >= end {
        return Err("the preview source range is empty".into());
    }
    Ok(start..end)
}

fn editor_output_text(output: &CommandOutput) -> String {
    match output {
        CommandOutput::Text(text) => text.clone(),
        CommandOutput::Listing(entries) if entries.is_empty() => "(empty local timeline)".into(),
        CommandOutput::Listing(entries) => entries
            .iter()
            .map(|entry| {
                let start = format_compact_timecode(
                    entry.timeline_range.start.value,
                    entry.timeline_range.start.time_base,
                )
                .unwrap_or_else(|_| "?:??".into());
                let end = format_compact_timecode(
                    entry.timeline_range.end.value,
                    entry.timeline_range.end.time_base,
                )
                .unwrap_or_else(|_| "?:??".into());
                format!(
                    "{} [{}] |{}-----{}|",
                    entry.alias,
                    entry.kind.as_str(),
                    start,
                    end,
                )
            })
            .collect::<Vec<_>>()
            .join("   "),
        CommandOutput::Changed { description, path } => {
            format!("ok: {description}  [{path}]")
        }
        CommandOutput::ImportRequested { .. } => "import request".into(),
        CommandOutput::ProjectMatchRequested => "project match request".into(),
        CommandOutput::NewProjectRequested { .. } => "new project request".into(),
        CommandOutput::OpenProjectRequested { .. } => "open project request".into(),
        CommandOutput::SaveProjectRequested { .. } => "save project request".into(),
        CommandOutput::ExportRequested { .. } => "export request".into(),
        CommandOutput::QuitRequested { .. } => "quit request".into(),
        _ => "ok".into(),
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut PreviewApp,
    completed: &Receiver<Result<ResizeResponse, String>>,
) -> Result<(), String> {
    let mut redraw = true;
    let mut last_frame = usize::MAX;
    let mut last_status = "";
    loop {
        app.tick(Instant::now())?;
        let resized = receive_resized_images(app, completed);
        let current = app.playback.frame_index();
        let status = app.status();
        if redraw || resized || current != last_frame || status != last_status {
            terminal
                .draw(|frame| draw(frame, app))
                .map_err(|error| format!("cannot draw terminal preview: {error}"))?;
            last_frame = current;
            last_status = status;
            redraw = false;
        }
        app.flush_kitty_frame()?;

        if event::poll(EVENT_WAIT).map_err(|error| format!("cannot poll terminal: {error}"))? {
            redraw = true;
            match event::read().map_err(|error| format!("cannot read terminal input: {error}"))? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.handle_key(key, Instant::now())? {
                        return Ok(());
                    }
                }
                Event::Resize(width, height) => {
                    app.set_terminal_size(Size::new(width, height));
                }
                _ => {}
            }
        }
    }
}

struct PreviewApp {
    source: Mpeg2PlaybackSource,
    frames: BTreeMap<usize, Box<DecodedMpeg2Picture>>,
    generation: u64,
    requested_range: Range<usize>,
    playback: PlaybackController,
    playback_range: Range<usize>,
    resume_when_buffered: bool,
    picker: Picker,
    image_state: Option<ThreadProtocol>,
    kitty: Option<KittyStreamer>,
    image_frame: Option<usize>,
    terminal_size: Size,
    preview_area: Rect,
    path: String,
    error: Option<String>,
}

impl PreviewApp {
    fn new(
        source: Mpeg2PlaybackSource,
        picker: Picker,
        resize_tx: mpsc::Sender<ResizeRequest>,
        path: &Path,
        terminal_size: Size,
    ) -> Result<Self, String> {
        let timeline =
            PlaybackTimeline::new(source.index().frame_rate(), source.index().frame_count())
                .map_err(|error| error.to_string())?;
        let playback_range = 0..source.index().frame_count();
        let direct_kitty = picker.protocol_type() == ProtocolType::Kitty && !inside_tmux();
        Ok(Self {
            source,
            frames: BTreeMap::new(),
            generation: 0,
            requested_range: 0..0,
            playback: PlaybackController::new(timeline),
            playback_range,
            resume_when_buffered: false,
            image_state: (!direct_kitty).then(|| ThreadProtocol::new(resize_tx, None)),
            kitty: direct_kitty.then(KittyStreamer::new).transpose()?,
            picker,
            image_frame: None,
            terminal_size,
            preview_area: Rect::new(
                1,
                2,
                terminal_size.width.saturating_sub(2),
                terminal_size.height.saturating_sub(4),
            ),
            path: path.display().to_string(),
            error: None,
        })
    }

    fn tick(&mut self, now: Instant) -> Result<(), String> {
        self.poll_decoder()?;
        self.playback.advance(now);
        let mut current = self.playback.frame_index();
        if current < self.playback_range.start || current >= self.playback_range.end {
            let was_playing = self.playback.is_playing();
            let target = if was_playing && self.playback.is_looping() {
                self.playback_range.start
            } else {
                self.playback_range.end - 1
            };
            self.playback.pause(now);
            self.playback
                .seek(self.playback.timeline().position_of_frame(target), now);
            if was_playing && self.playback.is_looping() {
                self.playback.play(now);
            }
            current = target;
        }
        if self.playback.is_playing() && !self.frames.contains_key(&current) {
            self.playback.pause(now);
            self.resume_when_buffered = true;
        }
        self.request_frame(current)?;
        if self.resume_when_buffered && self.has_buffer(current) {
            self.resume_when_buffered = false;
            self.playback.play(now);
        }
        self.update_image(current)
    }

    fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Result<bool, String> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char(' ') => {
                if self.playback.is_playing() || self.resume_when_buffered {
                    self.playback.pause(now);
                    self.resume_when_buffered = false;
                } else {
                    let mut current = self.playback.frame_index();
                    if current == self.playback_range.end - 1 && self.playback_range.start < current
                    {
                        current = self.playback_range.start;
                        self.seek_frame(current, now)?;
                    }
                    if self.has_buffer(current) {
                        self.playback.play(now);
                    } else {
                        self.resume_when_buffered = true;
                        self.request_frame(current)?;
                    }
                }
            }
            KeyCode::Right => self.step(1, now)?,
            KeyCode::Left => self.step(-1, now)?,
            KeyCode::Home => self.seek_frame(0, now)?,
            KeyCode::End => self.seek_frame(self.frame_count() - 1, now)?,
            KeyCode::Char('l') => {
                let looping = !self.playback.is_looping();
                self.playback.set_looping(looping);
            }
            _ => {}
        }
        Ok(false)
    }

    fn step(&mut self, delta: isize, now: Instant) -> Result<(), String> {
        let current = self.playback.frame_index();
        let target = if delta.is_negative() {
            current
                .saturating_sub(delta.unsigned_abs())
                .max(self.playback_range.start)
        } else {
            current
                .saturating_add(delta.unsigned_abs())
                .min(self.playback_range.end - 1)
        };
        self.seek_frame(target, now)
    }

    fn seek_frame(&mut self, frame: usize, now: Instant) -> Result<(), String> {
        let frame = frame.clamp(self.playback_range.start, self.playback_range.end - 1);
        self.playback.pause(now);
        self.resume_when_buffered = false;
        self.playback
            .seek(self.playback.timeline().position_of_frame(frame), now);
        self.request_frame(frame)
    }

    fn request_frame(&mut self, frame: usize) -> Result<(), String> {
        let cached = self.frames.contains_key(&frame);
        let request_pending = self
            .requested_range
            .clone()
            .any(|index| !self.frames.contains_key(&index));
        let remaining = self.requested_range.end.saturating_sub(frame);
        let target = if cached {
            if self.requested_range.start > frame {
                return Ok(());
            }
            if self.requested_range.contains(&frame)
                && (remaining > REFILL_THRESHOLD || request_pending)
            {
                return Ok(());
            }
            if self.requested_range.contains(&frame) {
                self.requested_range.end
            } else {
                frame.saturating_add(1)
            }
        } else if self.requested_range.contains(&frame) {
            return Ok(());
        } else {
            frame
        };
        if target >= self.frame_count() {
            return Ok(());
        }
        self.generation = self.source.request(target, LOOK_AHEAD)?;
        self.requested_range = target
            ..target
                .saturating_add(LOOK_AHEAD + 1)
                .min(self.frame_count());
        Ok(())
    }

    fn poll_decoder(&mut self) -> Result<(), String> {
        while let Some(event) = self.source.try_event()? {
            match event {
                Mpeg2PlaybackEvent::Frame {
                    generation,
                    frame_index,
                    picture,
                } if generation == self.generation => {
                    self.frames.insert(frame_index, picture);
                }
                Mpeg2PlaybackEvent::Error {
                    generation,
                    message,
                } if generation == 0 || generation == self.generation => return Err(message),
                Mpeg2PlaybackEvent::Frame { .. } | Mpeg2PlaybackEvent::Error { .. } => {}
            }
        }
        let focus = self.playback.frame_index();
        let mut retained = self.frames.keys().copied().collect::<Vec<_>>();
        retained.sort_by_key(|&index| {
            if index >= focus {
                (0_u8, index - focus)
            } else {
                (1_u8, focus - index)
            }
        });
        for index in retained.into_iter().skip(CACHE_FRAMES) {
            self.frames.remove(&index);
        }
        Ok(())
    }

    fn update_image(&mut self, frame_index: usize) -> Result<(), String> {
        if self.image_frame == Some(frame_index)
            || self
                .kitty
                .as_ref()
                .is_some_and(|kitty| kitty.queued_frame() == Some(frame_index))
        {
            return Ok(());
        }
        let Some(picture) = self.frames.get(&frame_index) else {
            return Ok(());
        };
        let bounds = if self.kitty.is_some() {
            native_pixel_bounds(&picture.frame)?
        } else {
            fallback_pixel_bounds(self.terminal_size, self.picker.font_size())
        };
        let image = video_frame_image(&picture.frame, bounds)?;
        if let Some(kitty) = &mut self.kitty {
            kitty.queue(frame_index, image.into_rgb8());
        } else if let Some(image_state) = &mut self.image_state {
            image_state.replace_protocol(self.picker.new_resize_protocol(image));
            self.image_frame = Some(frame_index);
        }
        Ok(())
    }

    fn set_terminal_size(&mut self, size: Size) {
        if self.terminal_size != size {
            self.terminal_size = size;
            if self.kitty.is_none() {
                self.image_frame = None;
            }
        }
    }

    fn has_buffer(&self, start: usize) -> bool {
        let end = start
            .saturating_add(BUFFER_FRAMES)
            .min(self.playback_range.end);
        (start..end).all(|index| self.frames.contains_key(&index))
    }

    fn set_playback_range(
        &mut self,
        range: Range<usize>,
        target: usize,
        now: Instant,
    ) -> Result<(), String> {
        if range.start >= range.end || range.end > self.frame_count() {
            return Err(format!(
                "editor preview range {}..{} is outside 0..{}",
                range.start,
                range.end,
                self.frame_count()
            ));
        }
        self.playback_range = range;
        self.image_frame = None;
        self.seek_frame(target, now)
    }

    const fn frame_count(&self) -> usize {
        self.playback.timeline().frame_count()
    }

    fn nominal_frames_per_second(&self) -> usize {
        let rate = self.playback.timeline().frame_rate();
        let numerator = usize::try_from(rate.numerator()).unwrap_or(1);
        let denominator = usize::try_from(rate.denominator()).unwrap_or(1).max(1);
        numerator.div_ceil(denominator).max(1)
    }

    fn time_base(&self) -> Option<Rational> {
        let rate = self.playback.timeline().frame_rate();
        Rational::new(rate.denominator(), rate.numerator()).ok()
    }

    fn timecode(&self, frame: usize) -> String {
        let Ok(frame) = i64::try_from(frame) else {
            return "?:??".into();
        };
        self.time_base()
            .and_then(|time_base| format_compact_timecode(frame, time_base).ok())
            .unwrap_or_else(|| "?:??".into())
    }

    fn status(&self) -> &'static str {
        if self.error.is_some() {
            "error"
        } else if self.resume_when_buffered {
            "buffering"
        } else if self.playback.is_playing() {
            "playing"
        } else if !self.frames.contains_key(&self.playback.frame_index()) {
            "decoding"
        } else {
            "paused"
        }
    }

    fn protocol_label(&self) -> &'static str {
        if self.kitty.is_some() {
            "Kitty direct"
        } else {
            protocol_name(self.picker.protocol_type())
        }
    }

    fn flush_kitty_frame(&mut self) -> Result<(), String> {
        let Some(kitty) = &mut self.kitty else {
            return Ok(());
        };
        let displayed = kitty.flush(self.preview_area, self.picker.font_size())?;
        if displayed.is_some() {
            self.image_frame = displayed;
        }
        Ok(())
    }

    fn clear_kitty(&mut self) -> Result<(), String> {
        self.kitty.as_mut().map_or(Ok(()), KittyStreamer::clear)
    }
}

struct QueuedKittyFrame {
    frame_index: usize,
    image: RgbImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KittyPlacement {
    column: u16,
    row: u16,
    columns: u16,
    rows: u16,
}

struct KittyStreamer {
    image_ids: [u32; 2],
    placement_id: u32,
    temp_directory: PathBuf,
    created_files: Vec<PathBuf>,
    next_file: u64,
    active_slot: Option<usize>,
    next_slot: usize,
    active_z_index: i32,
    image_size: Option<(u32, u32)>,
    placement: Option<KittyPlacement>,
    queued: Option<QueuedKittyFrame>,
}

impl KittyStreamer {
    fn new() -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let process = std::process::id();
        let stamp_bytes = stamp.to_le_bytes();
        let stamp_fold = u32::from_le_bytes([
            stamp_bytes[0],
            stamp_bytes[4],
            stamp_bytes[8],
            stamp_bytes[12],
        ]);
        let image_id = (stamp_fold.rotate_left(7) ^ process ^ 0x4d4d_0000).max(1);
        let next_image_id = image_id.checked_add(1).unwrap_or(1);
        let base = std::env::temp_dir();
        let mut temp_directory = None;
        for suffix in 0..100_u32 {
            let candidate = base.join(format!(
                "mmrecode-tty-graphics-protocol-{process}-{stamp}-{suffix}"
            ));
            match std::fs::create_dir(&candidate) {
                Ok(()) => {
                    temp_directory = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "cannot create Kitty transfer directory '{}': {error}",
                        candidate.display()
                    ));
                }
            }
        }
        let temp_directory = temp_directory
            .ok_or_else(|| "cannot allocate a unique Kitty transfer directory".to_owned())?;
        Ok(Self {
            image_ids: [image_id, next_image_id],
            placement_id: 1,
            temp_directory,
            created_files: Vec::new(),
            next_file: 0,
            active_slot: None,
            next_slot: 0,
            active_z_index: -1,
            image_size: None,
            placement: None,
            queued: None,
        })
    }

    fn queue(&mut self, frame_index: usize, image: RgbImage) {
        self.queued = Some(QueuedKittyFrame { frame_index, image });
    }

    fn queued_frame(&self) -> Option<usize> {
        self.queued.as_ref().map(|frame| frame.frame_index)
    }

    fn flush(
        &mut self,
        preview_area: Rect,
        font_size: ratatui_image::FontSize,
    ) -> Result<Option<usize>, String> {
        let Some(frame) = self.queued.take() else {
            self.update_placement(preview_area, font_size)?;
            return Ok(None);
        };
        let width = frame.image.width();
        let height = frame.image.height();
        let placement = kitty_placement(width, height, preview_area, font_size);
        let path = self.write_transfer_file(frame.image.as_raw())?;
        let encoded_path = encode_kitty_path(&path)?;
        let mut command = String::new();
        let slot = self.next_slot;
        let image_id = self.image_ids[slot];
        let z_index = self.active_z_index.saturating_add(1);
        write!(
            command,
            "\x1b_Ga=t,q=2,f=24,t=t,s={width},v={height},i={image_id};{encoded_path}\x1b\\"
        )
        .expect("writing to a String cannot fail");
        command.push_str(&self.placement_command(image_id, placement, z_index));
        if let Some(active_slot) = self.active_slot {
            write!(
                command,
                "\x1b_Ga=d,d=I,q=2,i={}\x1b\\",
                self.image_ids[active_slot]
            )
            .expect("writing to a String cannot fail");
        }

        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(command.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("cannot write Kitty video frame: {error}"))?;
        self.image_size = Some((width, height));
        self.placement = Some(placement);
        self.active_slot = Some(slot);
        self.next_slot = 1 - slot;
        self.active_z_index = z_index;
        Ok(Some(frame.frame_index))
    }

    fn update_placement(
        &mut self,
        preview_area: Rect,
        font_size: ratatui_image::FontSize,
    ) -> Result<(), String> {
        let Some((width, height)) = self.image_size else {
            return Ok(());
        };
        let placement = kitty_placement(width, height, preview_area, font_size);
        if self.placement == Some(placement) {
            return Ok(());
        }
        let Some(active_slot) = self.active_slot else {
            return Ok(());
        };
        let command =
            self.placement_command(self.image_ids[active_slot], placement, self.active_z_index);
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(command.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("cannot resize Kitty video placement: {error}"))?;
        self.placement = Some(placement);
        Ok(())
    }

    fn placement_command(&self, image_id: u32, placement: KittyPlacement, z_index: i32) -> String {
        format!(
            "\x1b[{};{}H\x1b_Ga=p,q=2,i={},p={},c={},r={},z={},C=1\x1b\\",
            placement.row,
            placement.column,
            image_id,
            self.placement_id,
            placement.columns,
            placement.rows,
            z_index
        )
    }

    fn write_transfer_file(&mut self, bytes: &[u8]) -> Result<PathBuf, String> {
        self.next_file = self.next_file.wrapping_add(1);
        let path = self
            .temp_directory
            .join(format!("frame-{}.rgb", self.next_file));
        std::fs::write(&path, bytes).map_err(|error| {
            format!(
                "cannot write Kitty transfer file '{}': {error}",
                path.display()
            )
        })?;
        self.created_files.push(path.clone());
        Ok(path)
    }

    fn clear(&mut self) -> Result<(), String> {
        if self.active_slot.is_some() {
            let command = format!(
                "\x1b_Ga=d,d=I,q=2,i={}\x1b\\\x1b_Ga=d,d=I,q=2,i={}\x1b\\",
                self.image_ids[0], self.image_ids[1]
            );
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(command.as_bytes())
                .and_then(|()| stdout.flush())
                .map_err(|error| format!("cannot clear Kitty preview image: {error}"))?;
            self.active_slot = None;
        }
        self.clean_transfer_files();
        Ok(())
    }

    fn clean_transfer_files(&mut self) {
        for path in self.created_files.drain(..) {
            // Best effort: the terminal normally deletes `t=t` transfers itself.
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir(&self.temp_directory);
    }
}

impl Drop for KittyStreamer {
    fn drop(&mut self) {
        self.clean_transfer_files();
    }
}

fn encode_kitty_path(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| "Kitty transfer path is not valid UTF-8".to_owned())?;
    let mut encoded = String::with_capacity(path.len().div_ceil(3) * 4);
    base64_simd::STANDARD.encode_append(path.as_bytes(), &mut encoded);
    Ok(encoded)
}

fn kitty_placement(
    image_width: u32,
    image_height: u32,
    available: Rect,
    font: ratatui_image::FontSize,
) -> KittyPlacement {
    if available.width == 0 || available.height == 0 || image_width == 0 || image_height == 0 {
        return KittyPlacement {
            column: available.x.saturating_add(1),
            row: available.y.saturating_add(1),
            columns: 1,
            rows: 1,
        };
    }

    let width_for_full_height = u128::from(image_width)
        .saturating_mul(u128::from(available.height))
        .saturating_mul(u128::from(font.height))
        .div_ceil(
            u128::from(image_height)
                .saturating_mul(u128::from(font.width))
                .max(1),
        );
    let (columns, rows) = if width_for_full_height <= u128::from(available.width) {
        (
            u16::try_from(width_for_full_height).unwrap_or(available.width),
            available.height,
        )
    } else {
        let height_for_full_width = u128::from(image_height)
            .saturating_mul(u128::from(available.width))
            .saturating_mul(u128::from(font.width))
            .div_ceil(
                u128::from(image_width)
                    .saturating_mul(u128::from(font.height))
                    .max(1),
            );
        (
            available.width,
            u16::try_from(height_for_full_width).unwrap_or(available.height),
        )
    };
    let columns = columns.clamp(1, available.width);
    let rows = rows.clamp(1, available.height);
    KittyPlacement {
        column: available.x + (available.width - columns) / 2 + 1,
        row: available.y + (available.height - rows) / 2 + 1,
        columns,
        rows,
    }
}

fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
}

fn draw(frame: &mut Frame<'_>, app: &mut PreviewApp) {
    let [header, preview, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let current = app.playback.frame_index();
    let rate = app.playback.timeline().frame_rate();
    let protocol = app.protocol_label();
    let title = format!(
        " {} | {} | {} / {} | {}/{} fps | {} ",
        app.path,
        protocol,
        app.timecode(current),
        app.timecode(app.frame_count()),
        rate.numerator(),
        rate.denominator(),
        app.status()
    );
    frame.render_widget(Paragraph::new(title), header);

    let image_block = Block::default().borders(Borders::ALL).title("Preview");
    let image_area = image_block.inner(preview);
    app.preview_area = image_area;
    frame.render_widget(image_block, preview);
    if let Some(image_state) = &mut app.image_state {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(Some(FilterType::Triangle))),
            image_area,
            image_state,
        );
    } else if app.image_frame.is_none() {
        frame.render_widget(
            Paragraph::new("Decoding preview frame…")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Yellow)),
            image_area,
        );
    }

    let footer_text = app.error.as_deref().map_or_else(
        || "Space play/pause   ←/→ step   Home/End seek   l loop   q quit".to_owned(),
        |error| format!("{error}   (q to quit)"),
    );
    let footer_style = if app.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    frame.render_widget(Paragraph::new(footer_text).style(footer_style), footer);
}

#[allow(clippy::too_many_lines)]
fn draw_editor(
    frame: &mut Frame<'_>,
    app: Option<&mut PreviewApp>,
    session: &EditorSession,
    editor: &mut EditorUi,
) {
    let mut app = app;
    let [header, workspace, timeline, result, prompt] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(55),
        Constraint::Min(8),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [preview, context] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .areas(workspace);
    let breadcrumb = session.prompt().unwrap_or_else(|_| "Project".into());
    let dirty = if session.is_dirty() { "*" } else { "" };
    let title = app.as_deref().map_or_else(
        || format!(" MMRecode | {breadcrumb}{dirty} | no media | editing "),
        |app| {
            let current = app.playback.frame_index();
            let rate = app.playback.timeline().frame_rate();
            format!(
                " MMRecode | {} | {} / {} | {}/{} fps | {} | {} ",
                format_args!("{breadcrumb}{dirty}"),
                app.timecode(current),
                app.timecode(app.frame_count()),
                rate.numerator(),
                rate.denominator(),
                app.status(),
                app.protocol_label(),
            )
        },
    );
    frame.render_widget(Paragraph::new(title), header);

    let image_block = Block::default().borders(Borders::ALL).title("Monitor");
    let image_area = image_block.inner(preview);
    if let Some(app) = app.as_deref_mut() {
        app.preview_area = image_area;
    }
    frame.render_widget(image_block, preview);
    if let Some(image_state) = app.as_deref_mut().and_then(|app| app.image_state.as_mut()) {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(Some(FilterType::Triangle))),
            image_area,
            image_state,
        );
    } else if app.as_deref().is_some_and(|app| app.image_frame.is_none()) {
        frame.render_widget(
            Paragraph::new("Decoding edited frame…")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Yellow)),
            image_area,
        );
    } else if app.is_none() {
        frame.render_widget(
            Paragraph::new(
                "No media loaded\n\nimport <media-file> [as <alias>]\n\nType help for commands",
            )
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
            image_area,
        );
    }

    frame.render_widget(
        Paragraph::new(editor_context_text(app.as_deref(), session, editor))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(editor_context_title(session, editor)),
            ),
        context,
    );

    let timeline_title = if app.is_some() {
        " Timeline • click/drag to scrub "
    } else {
        " Timeline • empty project "
    };
    let timeline_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(timeline_title);
    let timeline_inner = timeline_block.inner(timeline);
    editor.timeline_area = timeline_inner;
    frame.render_widget(timeline_block, timeline);
    frame.render_widget(
        Paragraph::new(editor_timeline_text(app.as_deref(), timeline_inner.width)),
        timeline_inner,
    );

    let result_style = if editor.message.starts_with("error:")
        || app.as_deref().is_some_and(|app| app.error.is_some())
    {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    let message = app
        .as_deref()
        .and_then(|app| app.error.as_deref())
        .unwrap_or(&editor.message);
    frame.render_widget(
        Paragraph::new(message)
            .style(result_style)
            .block(Block::default().borders(Borders::ALL).title("Result")),
        result,
    );
    frame.render_widget(
        Paragraph::new(format!("{breadcrumb} > {}", editor.input)),
        prompt,
    );
    let cursor_x = prompt
        .x
        .saturating_add(u16::try_from(breadcrumb.chars().count()).unwrap_or(u16::MAX))
        .saturating_add(3)
        .saturating_add(u16::try_from(editor.input.chars().count()).unwrap_or(u16::MAX))
        .min(prompt.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, prompt.y));
}

fn editor_context_title(session: &EditorSession, editor: &EditorUi) -> String {
    match editor.inspector_focus {
        InspectorFocus::InPoint => "Inspector — In point".into(),
        InspectorFocus::OutPoint => "Inspector — Out point".into(),
        InspectorFocus::Help => "Help — Commands".into(),
        InspectorFocus::Manual => editor.last_command.as_deref().map_or_else(
            || "Manual".into(),
            |command| {
                format!(
                    "Manual — {}",
                    command.strip_prefix("man ").unwrap_or(command)
                )
            },
        ),
        InspectorFocus::ProjectInfo => "Info — Project".into(),
        InspectorFocus::ProjectPresets => "Project — Presets".into(),
        InspectorFocus::VideoInfo => "Info — Video".into(),
        InspectorFocus::AudioInfo => "Info — Audio".into(),
        InspectorFocus::SourceInfo => "Info — Source".into(),
        InspectorFocus::ExportReport => "Export — Plan".into(),
        InspectorFocus::Context => session
            .project()
            .resolve_path(session.path())
            .ok()
            .and_then(|media_id| session.project().media(media_id))
            .map_or_else(
                || "Inspector".into(),
                |media| {
                    if media.kind.as_str() == "project" {
                        "Inspector — Project".into()
                    } else if media.kind.as_str().starts_with("video") {
                        "Inspector — Video".into()
                    } else {
                        format!("Inspector — {}", media.kind.as_str())
                    }
                },
            ),
    }
}

#[allow(clippy::too_many_lines)]
fn editor_context_text(
    app: Option<&PreviewApp>,
    session: &EditorSession,
    editor: &EditorUi,
) -> String {
    if matches!(
        editor.inspector_focus,
        InspectorFocus::Help
            | InspectorFocus::Manual
            | InspectorFocus::ProjectPresets
            | InspectorFocus::ExportReport
    ) {
        return editor
            .panel_text
            .clone()
            .unwrap_or_else(|| quick_help_text(session));
    }
    if matches!(editor.inspector_focus, InspectorFocus::AudioInfo) {
        return "AUDIO INFO\n\nNo audio context is available in the first MPEG-2 editor preview slice.\n\nUse `man info` for the information commands.".into();
    }
    if matches!(editor.inspector_focus, InspectorFocus::VideoInfo) {
        let mut text = "VIDEO INFO".to_owned();
        if let Some(app) = app {
            append_video_inspector(&mut text, app);
        } else {
            text.push_str("\n\nNo video source is currently attached to the monitor.\n\nUse `import <media-file>` first.");
        }
        return text;
    }
    if matches!(
        editor.inspector_focus,
        InspectorFocus::InPoint | InspectorFocus::OutPoint
    ) && let Some(text) = trim_inspector_text(app, session, editor)
    {
        return text;
    }

    let project_focus = matches!(editor.inspector_focus, InspectorFocus::ProjectInfo);
    let path = if project_focus {
        "/".into()
    } else {
        session
            .project()
            .display_path(session.path())
            .unwrap_or_else(|_| "/".into())
    };
    let media_id = if project_focus {
        session.project().root_id()
    } else if let Ok(media_id) = session.project().resolve_path(session.path()) {
        media_id
    } else {
        return format!("Path       {path}\n\nContext is unavailable");
    };
    let Some(media) = session.project().media(media_id) else {
        return format!("Path       {path}\n\nContext is unavailable");
    };
    let duration = format_compact_timecode(media.duration.value, media.duration.time_base)
        .unwrap_or_else(|_| "?:??".into());
    let mut text = format!(
        "Name       {}\nPath       {path}\nKind       {}\nDuration   {duration}\nChildren   {}\nTime base  {}/{} s",
        media.name,
        media.kind.as_str(),
        media.children().len(),
        media.time_base.numerator(),
        media.time_base.denominator(),
    );

    if project_focus || media.kind.as_str() == "project" {
        let settings = session.project().settings();
        let scan = match settings.scan_mode {
            mmrecode_edit::ProjectScanMode::Progressive => "progressive",
            mmrecode_edit::ProjectScanMode::Interlaced => "interlaced",
            _ => "unknown",
        };
        let color = match settings.color_space {
            mmrecode_edit::ProjectColorSpace::Rec709 => "rec709",
            mmrecode_edit::ProjectColorSpace::Srgb => "srgb",
            mmrecode_edit::ProjectColorSpace::Rec2020 => "rec2020",
            _ => "unknown",
        };
        let project_file = session
            .project_file()
            .map_or_else(|| "unsaved".into(), |path| path.display().to_string());
        let _ = write!(
            text,
            "\n\nProject settings\nCanvas     {}x{}\nRate       {}/{} fps\nPixels     {}/{}\nScan       {scan}\nColor      {color}\nAudio      {} Hz, {} ch\nFile       {project_file}\nState      {}",
            settings.width,
            settings.height,
            settings.frame_rate.numerator(),
            settings.frame_rate.denominator(),
            settings.pixel_aspect.numerator(),
            settings.pixel_aspect.denominator(),
            settings.audio_sample_rate,
            settings.audio_channels,
            if session.is_dirty() {
                "modified"
            } else {
                "saved"
            },
        );
    }

    if !project_focus
        && let Some(link_id) = session.path().current_link()
        && let Some(link) = session.project().link(link_id)
    {
        let source_start = timecode_or_unknown(
            link.source_range.start.value,
            link.source_range.start.time_base,
        );
        let source_end =
            timecode_or_unknown(link.source_range.end.value, link.source_range.end.time_base);
        let timeline_start = timecode_or_unknown(
            link.timeline_range.start.value,
            link.timeline_range.start.time_base,
        );
        let timeline_end = timecode_or_unknown(
            link.timeline_range.end.value,
            link.timeline_range.end.time_base,
        );
        let _ = write!(
            text,
            "\nAlias      {}\nSource     {source_start}..{source_end}\nTimeline   {timeline_start}..{timeline_end}\nScale      {}",
            link.alias,
            link.scale_mode.as_str(),
        );
    }

    match &media.origin {
        MediaOrigin::Generated => text.push_str("\nOrigin     generated"),
        MediaOrigin::Managed { path } => {
            let _ = write!(text, "\nOrigin     managed\nFile       {}", path.display());
        }
        MediaOrigin::External { path } => {
            let _ = write!(text, "\nOrigin     external\nFile       {}", path.display());
        }
        _ => text.push_str("\nOrigin     unknown"),
    }

    if matches!(editor.inspector_focus, InspectorFocus::SourceInfo) {
        text = source_inspector_text(media);
    } else if media.kind.as_str().starts_with("video") {
        if let Some(app) = app {
            append_video_inspector(&mut text, app);
        } else {
            text.push_str("\n\nVideo\nNo decoded source is currently attached to the monitor.");
        }
    } else if let Some(app) = app {
        let _ = write!(
            text,
            "\n\nPlayhead\nPosition   {}",
            app.timecode(app.playback.frame_index())
        );
    }
    if project_focus || media.kind.as_str() == "project" {
        text.push_str("\n\nAvailable here\nimport <file>   save   export plan\nproject info|match|preset|set   ls   cd <alias>");
    } else {
        text.push_str("\n\nAvailable here\nin <time>   out <time>   scale <mode>   project match\nls   cd ..   info video   info source   help");
    }
    text
}

fn trim_inspector_text(
    app: Option<&PreviewApp>,
    session: &EditorSession,
    editor: &EditorUi,
) -> Option<String> {
    let path = session.project().display_path(session.path()).ok()?;
    let link = session
        .path()
        .current_link()
        .and_then(|link_id| session.project().link(link_id))?;
    let source_start = timecode_or_unknown(
        link.source_range.start.value,
        link.source_range.start.time_base,
    );
    let source_end =
        timecode_or_unknown(link.source_range.end.value, link.source_range.end.time_base);
    let duration = link.source_range.duration().ok()?;
    let duration = timecode_or_unknown(duration.value, duration.time_base);
    let (label, boundary, opposite) = match editor.inspector_focus {
        InspectorFocus::InPoint => ("IN", &source_start, &source_end),
        InspectorFocus::OutPoint => ("OUT", &source_end, &source_start),
        _ => return None,
    };
    Some(format!(
        "Context    {path}\nCommand    {}\n\n{label} point\nBoundary   {boundary}\nOpposite   {opposite}\nDuration   {duration}\nPlayhead   {}\n\nFollow-up commands\nleft 0:13       move earlier\nright 0:13      move later\nmove left 0:13  verbose form\n\nUndo Ctrl-Z     Redo Ctrl-Y",
        editor.last_command.as_deref().unwrap_or("—"),
        app.map_or_else(
            || "—".into(),
            |app| app.timecode(app.playback.frame_index())
        ),
    ))
}

fn quick_help_text(session: &EditorSession) -> String {
    let context = session
        .project()
        .display_path(session.path())
        .unwrap_or_else(|_| "/".into());
    let local = if session.path().current_link().is_some() {
        "After trim: left/right <time>"
    } else {
        "Project root: import adds media"
    };
    format!(
        "QUICK HELP — {context}\nnew <name> [using <preset>]\nopen <project>   save [as <project>]\nimport <file> [as <name>]\nproject info|match|presets|preset|set\nscale fit|fill|stretch|native\nexport plan | export <file>\npwd  ls  cd <path>\nin <time>  out <time>\nundo  redo  help  man <cmd>  quit\n{local}\nTab completes • time S:FF/M:SS:FF"
    )
}

fn source_inspector_text(media: &mmrecode_edit::MediaNode) -> String {
    match &media.origin {
        MediaOrigin::Generated => format!(
            "SOURCE INFO\n\nName       {}\nKind       {}\nOrigin     generated\n\nThis media has no external source file.",
            media.name,
            media.kind.as_str()
        ),
        MediaOrigin::Managed { path } => format!(
            "SOURCE INFO\n\nName       {}\nKind       {}\nOrigin     managed\nFile       {}",
            media.name,
            media.kind.as_str(),
            path.display()
        ),
        MediaOrigin::External { path } => format!(
            "SOURCE INFO\n\nName       {}\nKind       {}\nOrigin     external\nFile       {}",
            media.name,
            media.kind.as_str(),
            path.display()
        ),
        _ => "SOURCE INFO\n\nUnknown media origin.".into(),
    }
}

fn append_video_inspector(text: &mut String, app: &PreviewApp) {
    let Some(frame) = app
        .source
        .index()
        .frames()
        .get(app.playback.frame_index())
        .or_else(|| app.source.index().frames().first())
    else {
        return;
    };
    let sequence = &frame.sequence;
    let chroma = match sequence.chroma_format {
        mmrecode_mpeg2::ChromaFormat::Yuv420 => "4:2:0",
        mmrecode_mpeg2::ChromaFormat::Yuv422 => "4:2:2",
        mmrecode_mpeg2::ChromaFormat::Yuv444 => "4:4:4",
        mmrecode_mpeg2::ChromaFormat::Reserved => "reserved",
    };
    let scan = if sequence.progressive_sequence {
        "progressive"
    } else {
        "interlaced/mixed"
    };
    let display = sequence.display.map_or_else(
        || format!("{}×{}", sequence.width, sequence.height),
        |display| {
            format!(
                "{}×{}",
                display.display_horizontal_size, display.display_vertical_size
            )
        },
    );
    let bit_rate = sequence.bit_rate.map_or_else(
        || "unspecified".into(),
        |bits| format!("{}.{:03} Mb/s", bits / 1_000_000, bits % 1_000_000 / 1_000),
    );
    let _ = write!(
        text,
        "\n\nVideo\nCodec      MPEG-2 Video\nCoded      {}×{}\nDisplay    {display}\nChroma     {chroma}\nScan       {scan}\nRate       {}/{} fps\nBit rate   {bit_rate}\nProfile    0x{:02x}\nPicture    {:?} / {:?}",
        sequence.width,
        sequence.height,
        sequence.frame_rate.numerator(),
        sequence.frame_rate.denominator(),
        sequence.profile_and_level_indication,
        frame.picture_type,
        frame.picture_structure,
    );
}

fn timecode_or_unknown(frame: i64, time_base: Rational) -> String {
    format_compact_timecode(frame, time_base).unwrap_or_else(|_| "?:??".into())
}

fn editor_timeline_text(app: Option<&PreviewApp>, width: u16) -> String {
    let Some(app) = app else {
        let width = usize::from(width);
        return format!(
            "{}\n{}\n{}\n\nType import <media-file> to create the first timeline placement.",
            timeline_label_row(None, width),
            timeline_ruler(width),
            "░".repeat(width),
        );
    };
    let current = app.playback.frame_index();
    format!(
        "{}\n{}\n{}\n{}\nIN {}   PLAY {}   OUT {}   SOURCE {}\n←/→ frame  Shift-←/→ 10 frames  PgUp/PgDn second  Ctrl-Space play  Up/Down history",
        timeline_label_row(Some(app), usize::from(width)),
        timeline_ruler(usize::from(width)),
        timeline_bar(app, usize::from(width)),
        timeline_picture_row(app, usize::from(width)),
        app.timecode(app.playback_range.start),
        app.timecode(current),
        app.timecode(app.playback_range.end),
        app.timecode(app.frame_count()),
    )
}

fn timeline_label_row(app: Option<&PreviewApp>, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec![' '; width];
    let labels = app.map_or_else(
        || vec![(0, "PROJECT".into())],
        |app| {
            vec![
                (0, app.timecode(0)),
                (width / 2, app.timecode(app.frame_count() / 2)),
                (width.saturating_sub(1), app.timecode(app.frame_count())),
            ]
        },
    );
    for (anchor, label) in labels {
        let start = if anchor == 0 {
            0
        } else if anchor + 1 >= width {
            width.saturating_sub(label.chars().count())
        } else {
            anchor.saturating_sub(label.chars().count() / 2)
        };
        for (index, character) in label.chars().enumerate() {
            if let Some(cell) = cells.get_mut(start + index) {
                *cell = character;
            }
        }
    }
    cells.into_iter().collect()
}

fn timeline_ruler(width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec!['─'; width];
    for column in [
        0,
        width / 4,
        width / 2,
        width.saturating_mul(3) / 4,
        width - 1,
    ] {
        cells[column] = if column == 0 {
            '┌'
        } else if column + 1 == width {
            '┐'
        } else {
            '┬'
        };
    }
    cells.into_iter().collect()
}

fn timeline_picture_row(app: &PreviewApp, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec![' '; width];
    for (index, frame) in app.source.index().frames().iter().enumerate() {
        if frame.picture_type == mmrecode_mpeg2::PictureType::I {
            let column = timeline_column_for_frame(index, width, app.frame_count());
            cells[column] = 'I';
        }
    }
    let current = timeline_column_for_frame(app.playback.frame_index(), width, app.frame_count());
    cells[current] = '▲';
    cells.into_iter().collect()
}

fn timeline_bar(app: &PreviewApp, width: usize) -> String {
    if width == 0 || app.frame_count() == 0 {
        return String::new();
    }
    let mut cells = (0..width)
        .map(|column| {
            let frame = frame_at_timeline_column(column, width, app.frame_count());
            if app.playback_range.contains(&frame) {
                '━'
            } else {
                '·'
            }
        })
        .collect::<Vec<_>>();
    let start = timeline_column_for_frame(app.playback_range.start, width, app.frame_count());
    let end = timeline_column_for_frame(app.playback_range.end - 1, width, app.frame_count());
    let current = timeline_column_for_frame(app.playback.frame_index(), width, app.frame_count());
    cells[start] = '┣';
    cells[end] = '┫';
    cells[current] = '◆';
    cells.into_iter().collect()
}

fn frame_at_timeline_column(column: usize, width: usize, frame_count: usize) -> usize {
    if width <= 1 || frame_count <= 1 {
        return 0;
    }
    let numerator = (column.min(width - 1) as u128) * ((frame_count - 1) as u128);
    usize::try_from(numerator / ((width - 1) as u128)).unwrap_or(frame_count - 1)
}

fn timeline_column_for_frame(frame: usize, width: usize, frame_count: usize) -> usize {
    if width <= 1 || frame_count <= 1 {
        return 0;
    }
    let numerator = (frame.min(frame_count - 1) as u128) * ((width - 1) as u128);
    usize::try_from(numerator / ((frame_count - 1) as u128)).unwrap_or(width - 1)
}

fn fallback_pixel_bounds(terminal: Size, font: ratatui_image::FontSize) -> (u32, u32) {
    let width = u32::from(terminal.width.saturating_sub(2)) * u32::from(font.width);
    let height = u32::from(terminal.height.saturating_sub(5)) * u32::from(font.height);
    (width.clamp(1, 1_280), height.clamp(1, 720))
}

fn native_pixel_bounds(frame: &VideoFrame) -> Result<(u32, u32), String> {
    Ok((
        u32::try_from(frame.width)
            .map_err(|_| "video width exceeds Kitty image limits".to_owned())?,
        u32::try_from(frame.height)
            .map_err(|_| "video height exceeds Kitty image limits".to_owned())?,
    ))
}

fn video_frame_image(frame: &VideoFrame, bounds: (u32, u32)) -> Result<DynamicImage, String> {
    validate_frame(frame)?;
    let (width, height) = fitted_dimensions(frame.width, frame.height, bounds)?;
    let mut image = RgbImage::new(width, height);
    for target_y in 0..height {
        let source_y = usize::try_from(target_y)
            .expect("u32 fits usize")
            .saturating_mul(frame.height)
            / usize::try_from(height).expect("u32 fits usize");
        for target_x in 0..width {
            let source_x = usize::try_from(target_x)
                .expect("u32 fits usize")
                .saturating_mul(frame.width)
                / usize::try_from(width).expect("u32 fits usize");
            let rgb = sample_rgb(frame, source_x, source_y);
            image.put_pixel(target_x, target_y, Rgb(rgb));
        }
    }
    Ok(DynamicImage::ImageRgb8(image))
}

fn fitted_dimensions(
    source_width: usize,
    source_height: usize,
    bounds: (u32, u32),
) -> Result<(u32, u32), String> {
    let source_width = u32::try_from(source_width)
        .map_err(|_| "video width exceeds terminal image limits".to_owned())?;
    let source_height = u32::try_from(source_height)
        .map_err(|_| "video height exceeds terminal image limits".to_owned())?;
    let width_scale = f64::from(bounds.0) / f64::from(source_width);
    let height_scale = f64::from(bounds.1) / f64::from(source_height);
    let scale = width_scale.min(height_scale).min(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let dimensions = (
        (f64::from(source_width) * scale).round().max(1.0) as u32,
        (f64::from(source_height) * scale).round().max(1.0) as u32,
    );
    Ok(dimensions)
}

fn validate_frame(frame: &VideoFrame) -> Result<(), String> {
    if frame.width == 0 || frame.height == 0 {
        return Err("decoded video frame has zero dimensions".into());
    }
    match frame.format {
        PixelFormat::Gray8 => validate_plane(&frame.planes, 0, "Y"),
        PixelFormat::Yuv420p8
        | PixelFormat::Yuv411p8
        | PixelFormat::Yuv422p8
        | PixelFormat::Yuv444p8 => {
            validate_plane(&frame.planes, 0, "Y")?;
            validate_plane(&frame.planes, 1, "Cb")?;
            validate_plane(&frame.planes, 2, "Cr")
        }
        PixelFormat::Rgb24 => {
            let plane = frame
                .planes
                .first()
                .ok_or_else(|| "RGB frame has no pixel plane".to_owned())?;
            if plane.width < frame.width.saturating_mul(3)
                || plane.stride < frame.width.saturating_mul(3)
                || plane.height < frame.height
            {
                return Err("RGB frame has an invalid packed layout".into());
            }
            Ok(())
        }
        _ => Err("decoded pixel format is not supported by terminal preview".into()),
    }
}

fn validate_plane(planes: &[Plane], index: usize, name: &str) -> Result<(), String> {
    let plane = planes
        .get(index)
        .ok_or_else(|| format!("decoded frame has no {name} plane"))?;
    if plane.width == 0 || plane.height == 0 || plane.stride < plane.width {
        return Err(format!("decoded {name} plane has an invalid layout"));
    }
    let required = (plane.height - 1)
        .checked_mul(plane.stride)
        .and_then(|offset| offset.checked_add(plane.width))
        .ok_or_else(|| format!("decoded {name} plane layout overflows"))?;
    if plane.data.len() < required {
        return Err(format!("decoded {name} plane is truncated"));
    }
    Ok(())
}

fn sample_rgb(frame: &VideoFrame, x: usize, y: usize) -> [u8; 3] {
    match frame.format {
        PixelFormat::Gray8 => {
            let value = sample(&frame.planes[0], x, y);
            [value, value, value]
        }
        PixelFormat::Yuv420p8
        | PixelFormat::Yuv411p8
        | PixelFormat::Yuv422p8
        | PixelFormat::Yuv444p8 => {
            let luma = sample_scaled(&frame.planes[0], x, y, frame.width, frame.height);
            let cb = sample_scaled(&frame.planes[1], x, y, frame.width, frame.height);
            let cr = sample_scaled(&frame.planes[2], x, y, frame.width, frame.height);
            ycbcr_to_rgb(luma, cb, cr, frame.color.range)
        }
        PixelFormat::Rgb24 => {
            let plane = &frame.planes[0];
            let offset = y * plane.stride + x * 3;
            [
                plane.data[offset],
                plane.data[offset + 1],
                plane.data[offset + 2],
            ]
        }
        _ => [0, 0, 0],
    }
}

fn sample(plane: &Plane, x: usize, y: usize) -> u8 {
    plane.data[y * plane.stride + x]
}

fn sample_scaled(
    plane: &Plane,
    x: usize,
    y: usize,
    source_width: usize,
    source_height: usize,
) -> u8 {
    let plane_x = (x * plane.width / source_width).min(plane.width - 1);
    let plane_y = (y * plane.height / source_height).min(plane.height - 1);
    sample(plane, plane_x, plane_y)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8, range: ColorRange) -> [u8; 3] {
    let (luma, blue_difference, red_difference) = if range == ColorRange::Limited {
        (
            (f32::from(y) - 16.0) * (255.0 / 219.0),
            (f32::from(cb) - 128.0) * (255.0 / 224.0),
            (f32::from(cr) - 128.0) * (255.0 / 224.0),
        )
    } else {
        (f32::from(y), f32::from(cb) - 128.0, f32::from(cr) - 128.0)
    };
    [
        (luma + 1.402 * red_difference).round().clamp(0.0, 255.0) as u8,
        (luma - 0.344_136 * blue_difference - 0.714_136 * red_difference)
            .round()
            .clamp(0.0, 255.0) as u8,
        (luma + 1.772 * blue_difference).round().clamp(0.0, 255.0) as u8,
    ]
}

const fn protocol_name(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::Kitty => "Kitty",
        ProtocolType::Sixel => "Sixel",
        ProtocolType::Iterm2 => "iTerm2",
        ProtocolType::Halfblocks => "ANSI half-blocks",
    }
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{ColorDescription, FieldOrder, FrameTiming, Rational};
    use mmrecode_edit::{EditCommand, ImportedMedia, MediaKind, MediaOrigin, MediaProject};
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    #[test]
    fn converts_limited_range_yuv_to_rgb_preview() {
        let frame = VideoFrame {
            format: PixelFormat::Yuv420p8,
            width: 2,
            height: 2,
            planes: vec![
                Plane {
                    data: vec![16, 235, 81, 145],
                    stride: 2,
                    width: 2,
                    height: 2,
                },
                Plane {
                    data: vec![128],
                    stride: 1,
                    width: 1,
                    height: 1,
                },
                Plane {
                    data: vec![128],
                    stride: 1,
                    width: 1,
                    height: 1,
                },
            ],
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Limited,
                ..ColorDescription::default()
            },
            field_order: FieldOrder::Progressive,
        };
        let image = video_frame_image(&frame, (2, 2)).expect("convert frame");
        let rgb = image.into_rgb8();
        assert_eq!(rgb.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn preserves_aspect_ratio_inside_terminal_bounds() {
        assert_eq!(
            fitted_dimensions(1_920, 1_080, (800, 600)).unwrap(),
            (800, 450)
        );
        assert_eq!(
            fitted_dimensions(640, 480, (1_280, 720)).unwrap(),
            (640, 480)
        );
    }

    #[test]
    fn kitty_placement_fills_preview_without_distortion() {
        let font = ratatui_image::FontSize::new(10, 20);
        assert_eq!(
            kitty_placement(1_920, 1_080, Rect::new(1, 2, 98, 36), font),
            KittyPlacement {
                column: 2,
                row: 7,
                columns: 98,
                rows: 28,
            }
        );
        assert_eq!(
            kitty_placement(1_080, 1_920, Rect::new(1, 2, 98, 36), font),
            KittyPlacement {
                column: 30,
                row: 3,
                columns: 41,
                rows: 36,
            }
        );
    }

    #[test]
    fn buffered_preview_advances_on_the_shared_playback_clock() {
        let source = Mpeg2PlaybackSource::new(
            include_bytes!("../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v").to_vec(),
        )
        .expect("index MPEG-2 test stream");
        let (resize_tx, _resize_rx) = mpsc::channel();
        let mut app = PreviewApp::new(
            source,
            Picker::halfblocks(),
            resize_tx,
            Path::new("test.m2v"),
            Size::new(80, 24),
        )
        .expect("create preview");
        app.request_frame(0).expect("request initial window");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !app.has_buffer(0) {
            app.tick(Instant::now()).expect("poll decoder");
            assert!(Instant::now() < deadline, "preview preroll timed out");
            thread::sleep(Duration::from_millis(1));
        }

        let start = Instant::now();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), start)
            .expect("start playback");
        assert!(app.playback.is_playing());
        app.tick(start + Duration::from_millis(200))
            .expect("advance preview");
        assert!(app.playback.frame_index() >= 4);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn editor_commands_update_and_bound_the_live_preview_range() {
        let source = Mpeg2PlaybackSource::new(
            include_bytes!("../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v").to_vec(),
        )
        .expect("index MPEG-2 test stream");
        let (resize_tx, _resize_rx) = mpsc::channel();
        let picker = Picker::halfblocks();
        let mut app = Some(
            PreviewApp::new(
                source,
                picker.clone(),
                resize_tx.clone(),
                Path::new("test.m2v"),
                Size::new(80, 24),
            )
            .expect("create preview"),
        );
        let host = EditorHost {
            resize_tx: &resize_tx,
            picker: &picker,
            base_directory: Path::new("."),
            terminal_size: Size::new(80, 24),
        };
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 25).unwrap()).unwrap());
        session
            .add_imported_media(&ImportedMedia {
                name: "test.m2v".into(),
                alias: "Clip0".into(),
                kind: MediaKind::new("video/mpeg2").unwrap(),
                time_base: Rational::new(1, 30).unwrap(),
                duration: 12,
                origin: MediaOrigin::External {
                    path: PathBuf::from("test.m2v"),
                },
            })
            .unwrap();
        session
            .apply(EditCommand::Cd {
                path: "Clip0".into(),
            })
            .unwrap();
        let now = Instant::now();
        let mut editor = EditorUi {
            input: "in +0:02".into(),
            ..EditorUi::default()
        };
        let mut history = CommandHistory::default();

        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert_eq!(app.as_ref().unwrap().playback_range, 2..12);
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 2);

        editor.input = "out -0:03".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert_eq!(app.as_ref().unwrap().playback_range, 2..9);
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 2);
        let timeline = editor_timeline_text(app.as_ref(), 80);
        assert!(timeline.contains("IN 0:02"));
        assert!(timeline.contains("OUT 0:09"));
        assert!(!timeline.contains("2f"));
        assert_eq!(editor.inspector_focus, InspectorFocus::OutPoint);
        let trim_context = editor_context_text(app.as_ref(), &session, &editor);
        assert!(trim_context.contains("Command    out -0:03"));
        assert!(trim_context.contains("Boundary   0:09"));
        editor.inspector_focus = InspectorFocus::Context;
        let context = editor_context_text(app.as_ref(), &session, &editor);
        assert!(context.contains("Source     0:02..0:09"));
        assert!(context.contains("Codec      MPEG-2 Video"));
        session
            .apply(EditCommand::Cd { path: "..".into() })
            .unwrap();
        let project_context = editor_context_text(app.as_ref(), &session, &editor);
        assert!(project_context.contains("Name       Film"));
        assert!(project_context.contains("Kind       project"));
        assert_eq!(
            editor_context_title(&session, &editor),
            "Inspector — Project"
        );
        editor.input = "info video".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.inspector_focus, InspectorFocus::VideoInfo);
        assert!(editor_context_text(app.as_ref(), &session, &editor).contains("Codec      MPEG-2"));
        session
            .apply(EditCommand::Cd {
                path: "Clip0".into(),
            })
            .unwrap();

        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.input, "info video");
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.input, "out -0:03");
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.input, "info video");
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert!(editor.input.is_empty());

        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 8);

        editor.timeline_area = Rect::new(10, 5, 101, 4);
        handle_editor_mouse(
            app.as_mut().unwrap(),
            &mut editor,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 60,
                row: 7,
                modifiers: KeyModifiers::NONE,
            },
            now,
        )
        .unwrap();
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 5);

        app.as_mut().unwrap().seek_frame(0, now).unwrap();
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 2);
        app.as_mut().unwrap().seek_frame(11, now).unwrap();
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 8);

        app.as_mut()
            .unwrap()
            .handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), now)
            .unwrap();
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 2);
        assert!(
            app.as_ref().unwrap().playback.is_playing()
                || app.as_ref().unwrap().resume_when_buffered
        );
    }

    #[test]
    fn timeline_coordinates_cover_the_complete_source() {
        assert_eq!(frame_at_timeline_column(0, 101, 769), 0);
        assert_eq!(frame_at_timeline_column(100, 101, 769), 768);
        assert_eq!(timeline_column_for_frame(0, 101, 769), 0);
        assert_eq!(timeline_column_for_frame(768, 101, 769), 100);
    }

    #[test]
    fn empty_editor_still_has_help_inspector_and_timeline() {
        let session = EditorSession::new(
            MediaProject::new("Untitled", Rational::new(1, 25).unwrap()).unwrap(),
        );
        let editor = EditorUi {
            inspector_focus: InspectorFocus::Help,
            ..EditorUi::default()
        };

        let help = editor_context_text(None, &session, &editor);
        assert!(help.contains("QUICK HELP — /"));
        assert!(help.contains("open <project>"));
        assert!(help.contains("import <file>"));
        assert!(help.contains("man <cmd>"));
        let timeline = editor_timeline_text(None, 40);
        assert!(timeline.contains("PROJECT"));
        assert!(timeline.contains("import <media-file>"));
    }

    #[test]
    fn focused_trim_shortcuts_expand_to_canonical_commands() {
        assert_eq!(
            expand_context_command("left 0:13", InspectorFocus::InPoint).unwrap(),
            "in -0:13"
        );
        assert_eq!(
            expand_context_command("move right 1:00", InspectorFocus::OutPoint).unwrap(),
            "out +1:00"
        );
        assert!(expand_context_command("left 0:01", InspectorFocus::Context).is_err());
        assert!(expand_context_command("right -0:01", InspectorFocus::InPoint).is_err());
    }

    #[test]
    fn malformed_editor_command_stays_in_the_preview() {
        let source = Mpeg2PlaybackSource::new(
            include_bytes!("../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v").to_vec(),
        )
        .expect("index MPEG-2 test stream");
        let (resize_tx, _resize_rx) = mpsc::channel();
        let picker = Picker::halfblocks();
        let mut app = Some(
            PreviewApp::new(
                source,
                picker.clone(),
                resize_tx.clone(),
                Path::new("test.m2v"),
                Size::new(80, 24),
            )
            .expect("create preview"),
        );
        let host = EditorHost {
            resize_tx: &resize_tx,
            picker: &picker,
            base_directory: Path::new("."),
            terminal_size: Size::new(80, 24),
        };
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 30).unwrap()).unwrap());
        let mut editor = EditorUi {
            input: "in definitely-not-a-frame".into(),
            ..EditorUi::default()
        };
        let mut history = CommandHistory::default();

        assert!(
            !execute_editor_input(
                &mut app,
                &mut session,
                &mut history,
                &mut editor,
                Instant::now(),
                &host,
            )
            .unwrap()
        );
        assert!(editor.message.starts_with("error:"));
    }
}
