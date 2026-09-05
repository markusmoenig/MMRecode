//! Interactive terminal graphics preview.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::{IsTerminal as _, Write as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use image::{DynamicImage, RgbImage};
use mmrecode_core::{ColorRange, PixelFormat, Plane, Rational, VideoFrame};
use mmrecode_edit::{
    CommandOutput, EditCommand, EditorSession, MediaId, MediaOrigin, MediaPath, MmfxSource,
    MonitorTarget, format_compact_timecode,
};
use mmrecode_playback::{
    AacDecodeBackend, AacPlaybackEvent, AacPlaybackSource, H264PlaybackEvent, H264PlaybackSource,
    Mpeg2PlaybackEvent, Mpeg2PlaybackSource, PlaybackController, PlaybackEvent, PlaybackTimeline,
};
use mmrecode_render::ProjectCompositor;
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
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_image::{
    FilterType, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    thread::{ResizeRequest, ResizeResponse, ThreadProtocol},
};

use crate::{
    audio::AudioOutput,
    command_history::CommandHistory,
    media_probe, prompt_completion,
    timeline_raster::{
        SmartRenderSpan, SmartRenderState, TimelineObjectLane, TimelinePicture,
        TimelinePictureKind, TimelineRasterInput, render_timeline,
    },
    timeline_view::{TimelineViewport, TimelineZoom},
};

const LOOK_AHEAD: usize = 23;
const BUFFER_FRAMES: usize = 8;
const REFILL_THRESHOLD: usize = 12;
const CACHE_FRAMES: usize = 36;
const EVENT_WAIT: Duration = Duration::from_millis(8);
const THUMBNAIL_WIDTH: u32 = 96;
const THUMBNAIL_HEIGHT: u32 = 54;
const MAX_TIMELINE_THUMBNAILS: usize = 128;
const MMFX_COMPILE_DELAY: Duration = Duration::from_millis(140);

/// Runs the interactive terminal preview for supported MPEG-2 or H.264 media.
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

fn open_source(path: &Path) -> Result<PreviewSource, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if media_probe::looks_like_isobmff(&bytes) {
        return H264PlaybackSource::new(bytes).map(PreviewSource::H264);
    }
    let elementary = if bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE && bytes[0] == 0x47 {
        mmrecode_mpegts::demux_transport_stream(&bytes)
            .map_err(|error| error.to_string())?
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string())?
    } else {
        bytes
    };
    Mpeg2PlaybackSource::new(elementary).map(PreviewSource::Mpeg2)
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
    source: PreviewSource,
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
    terminal
        .hide_cursor()
        .map_err(|error| format!("cannot hide terminal cursor in editor: {error}"))?;
    std::io::stdout()
        .execute(EnableMouseCapture)
        .map_err(|error| format!("cannot enable terminal timeline mouse input: {error}"))?;
    let mut editor = EditorUi {
        message: "Ready. Type import <media-file>, or use help / man <command>.".into(),
        inspector_focus: InspectorFocus::Help,
        timeline_image: Some(TimelineImageBuffer::new()?),
        timeline_monitor_image: Some(TimelineImageBuffer::new()?),
        mmfx_image: Some(TimelineImageBuffer::new()?),
        mmfx_worker: Some(MmfxCompileWorker::spawn()?),
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
    drop(editor);
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
    Mmfx,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EditorPaneFocus {
    Timeline,
    Inspector,
    Code,
    #[default]
    Command,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MonitorScope {
    #[default]
    Project,
    Local,
}

impl EditorPaneFocus {
    const fn next(self, code_available: bool) -> Self {
        match (self, code_available) {
            (Self::Command, _) => Self::Timeline,
            (Self::Timeline, _) => Self::Inspector,
            (Self::Inspector, true) => Self::Code,
            (Self::Inspector, false) | (Self::Code, _) => Self::Command,
        }
    }

    const fn previous(self, code_available: bool) -> Self {
        match (self, code_available) {
            (Self::Command, true) => Self::Code,
            (Self::Command, false) | (Self::Code, _) => Self::Inspector,
            (Self::Inspector, _) => Self::Timeline,
            (Self::Timeline, _) => Self::Command,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Timeline => "Timeline",
            Self::Inspector => "Inspector",
            Self::Code => "MMFX source",
            Self::Command => "Command",
        }
    }
}

#[derive(Default)]
struct EditorUi {
    input: String,
    message: String,
    timeline_area: Rect,
    inspector_area: Rect,
    prompt_area: Rect,
    timeline: TimelineViewport,
    /// Playhead in the project root timeline. The monitor always renders this time.
    project_playhead: usize,
    /// Playhead in the hierarchy level currently shown by the timeline pane.
    timeline_playhead: usize,
    timeline_image: Option<TimelineImageBuffer>,
    timeline_raster_key: Option<TimelineRasterKey>,
    timeline_monitor_image: Option<TimelineImageBuffer>,
    timeline_monitor_key: Option<TimelineMonitorKey>,
    project_compositor: ProjectCompositor,
    project_compositor_state: Option<(u64, MediaId)>,
    monitor_scope: MonitorScope,
    timeline_context: Option<MediaId>,
    cursor_position: (u16, u16),
    pane_focus: EditorPaneFocus,
    inspector_scroll: u16,
    inspector_max_scroll: u16,
    inspector_focus: InspectorFocus,
    last_command: Option<String>,
    panel_text: Option<String>,
    code_area: Rect,
    mmfx: Option<MmfxDocument>,
    mmfx_image: Option<TimelineImageBuffer>,
    mmfx_worker: Option<MmfxCompileWorker>,
    mmfx_generation: u64,
}

#[derive(Clone, Debug)]
struct MmfxDocument {
    generation: u64,
    media_id: MediaId,
    name: String,
    source: String,
    resource_base: Option<PathBuf>,
    cursor: usize,
    scroll: usize,
    column_scroll: usize,
    revision: u64,
    compile_due: Option<Instant>,
    compile_status: String,
    last_good_revision: Option<u64>,
}

impl MmfxDocument {
    fn new(
        media_id: MediaId,
        name: String,
        source: String,
        resource_base: Option<PathBuf>,
        generation: u64,
    ) -> Self {
        Self {
            generation,
            media_id,
            name,
            cursor: 0,
            scroll: 0,
            column_scroll: 0,
            revision: 1,
            compile_due: Some(Instant::now()),
            compile_status: "compiling preview…".into(),
            last_good_revision: None,
            source,
            resource_base,
        }
    }

    fn display_name(&self) -> String {
        self.name.clone()
    }

    fn replace_source(&mut self, source: String, cursor: usize, now: Instant) {
        if source == self.source {
            return;
        }
        self.source = source;
        self.cursor = cursor.min(self.source.len());
        self.changed(now);
    }

    fn changed(&mut self, now: Instant) {
        self.revision = self.revision.wrapping_add(1);
        self.compile_due = Some(now + MMFX_COMPILE_DELAY);
        self.compile_status = "source changed; preview pending…".into();
    }
}

struct MmfxCompileRequest {
    generation: u64,
    revision: u64,
    source: String,
    base_directory: PathBuf,
}

struct MmfxCompileResult {
    generation: u64,
    revision: u64,
    result: Result<DynamicImage, String>,
}

struct MmfxCompileWorker {
    requests: Option<mpsc::Sender<MmfxCompileRequest>>,
    results: Receiver<MmfxCompileResult>,
    worker: Option<JoinHandle<()>>,
}

impl MmfxCompileWorker {
    fn spawn() -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::channel::<MmfxCompileRequest>();
        let (result_tx, result_rx) = mpsc::channel::<MmfxCompileResult>();
        let worker = thread::Builder::new()
            .name("mmrecode-mmfx-preview".into())
            .spawn(move || {
                while let Ok(mut request) = request_rx.recv() {
                    while let Ok(newer) = request_rx.try_recv() {
                        request = newer;
                    }
                    let result = compile_mmfx_preview(&request.source, &request.base_directory);
                    if result_tx
                        .send(MmfxCompileResult {
                            generation: request.generation,
                            revision: request.revision,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|error| format!("cannot start MMFX preview worker: {error}"))?;
        Ok(Self {
            requests: Some(request_tx),
            results: result_rx,
            worker: Some(worker),
        })
    }
}

impl Drop for MmfxCompileWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn compile_mmfx_preview(source: &str, base_directory: &Path) -> Result<DynamicImage, String> {
    let scene = mmrecode_mmfx::parse_scene(source).map_err(|diagnostics| {
        diagnostics
            .into_iter()
            .map(|diagnostic| {
                let (line, column) = diagnostic.span.line_column(source);
                diagnostic.help.map_or_else(
                    || format!("{line}:{column}: {}", diagnostic.message),
                    |help| format!("{line}:{column}: {} — {help}", diagnostic.message),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let resources = crate::load_mmfx_resources(&scene, base_directory)?;
    let surface = mmrecode_mmfx::render_with_resources(&scene, &resources)
        .map_err(|error| error.to_string())?;
    let image = image::RgbaImage::from_raw(surface.width(), surface.height(), surface.to_rgba8())
        .ok_or_else(|| "MMFX renderer returned an invalid image buffer".to_owned())?;
    Ok(DynamicImage::ImageRgba8(image))
}

struct TimelineImageSlot {
    protocol: Option<ThreadProtocol>,
    completed: Receiver<Result<ResizeResponse, String>>,
    worker: Option<JoinHandle<()>>,
}

impl TimelineImageSlot {
    fn new() -> Result<Self, String> {
        let (resize_tx, completed, worker) = spawn_resize_worker()?;
        Ok(Self {
            protocol: Some(ThreadProtocol::new(resize_tx, None)),
            completed,
            worker: Some(worker),
        })
    }
}

impl Drop for TimelineImageSlot {
    fn drop(&mut self) {
        self.protocol.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct TimelineImageBuffer {
    slots: [TimelineImageSlot; 2],
    active: Option<usize>,
    pending: Option<usize>,
}

impl TimelineImageBuffer {
    fn new() -> Result<Self, String> {
        Ok(Self {
            slots: [TimelineImageSlot::new()?, TimelineImageSlot::new()?],
            active: None,
            pending: None,
        })
    }

    fn replace_protocol(&mut self, protocol: ratatui_image::protocol::StatefulProtocol) {
        let slot = self
            .pending
            .unwrap_or_else(|| self.active.map_or(0, |active| 1 - active));
        if let Some(state) = &mut self.slots[slot].protocol {
            state.replace_protocol(protocol);
        }
        self.pending = Some(slot);
    }

    fn poll(&mut self) -> (bool, Option<String>) {
        let mut received = false;
        let mut error = None;
        for (slot_index, slot) in self.slots.iter_mut().enumerate() {
            while let Ok(response) = slot.completed.try_recv() {
                received = true;
                match response {
                    Ok(response) => {
                        let ready = slot
                            .protocol
                            .as_mut()
                            .is_some_and(|state| state.update_resized_protocol(response));
                        if ready && self.pending == Some(slot_index) {
                            self.active = Some(slot_index);
                            self.pending = None;
                        }
                    }
                    Err(message) => {
                        if self.pending == Some(slot_index) {
                            self.pending = None;
                        }
                        error = Some(message);
                    }
                }
            }
        }
        (received, error)
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(pending) = self.pending
            && let Some(state) = &mut self.slots[pending].protocol
        {
            frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Fit(Some(FilterType::Triangle))),
                area,
                state,
            );
        }
        if let Some(active) = self.active
            && self.pending != Some(active)
            && let Some(state) = &mut self.slots[active].protocol
        {
            frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Fit(Some(FilterType::Triangle))),
                area,
                state,
            );
        }
    }

    fn empty(&mut self) {
        for slot in &mut self.slots {
            if let Some(state) = &mut slot.protocol {
                state.empty_protocol();
            }
        }
        self.active = None;
        self.pending = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimelineRasterKey {
    width: u32,
    height: u32,
    visible: Range<usize>,
    retained: Range<usize>,
    playhead: usize,
    thumbnail_revision: u64,
    thumbnail_frames: Vec<usize>,
    smart_render: Vec<SmartRenderSpan>,
    objects: Vec<TimelineObjectLane>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimelinePreviewMapping {
    timeline: Range<usize>,
    source: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TimelineMonitorKey {
    canvas: (u32, u32),
    scope: MonitorScope,
    playhead: usize,
    active_signature: u64,
}

struct EditorHost<'a> {
    resize_tx: &'a mpsc::Sender<ResizeRequest>,
    picker: &'a Picker,
    base_directory: &'a Path,
    terminal_size: Size,
}

#[allow(clippy::too_many_lines)]
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
    let mut last_thumbnail_revision = None;
    loop {
        let now = Instant::now();
        let mut mmfx_changed = update_mmfx_preview(editor, session, host, now);
        mmfx_changed |= synchronize_project_compositor(editor, session, host);
        if let Some(app) = app.as_mut() {
            app.tick(now, false)?;
            editor.project_playhead = project_timeline_playhead(session, app);
            editor.timeline_playhead =
                displayed_timeline_playhead(session, app, editor.project_playhead);
            if monitor_uses_video(editor.monitor_scope, session, app) {
                let monitor_frame = monitor_playhead(editor);
                if editor
                    .project_compositor
                    .has_active_layers(timeline_frame_i64(monitor_frame))
                {
                    app.update_timeline_composition(&mut editor.project_compositor, monitor_frame)?;
                } else {
                    if app.composed_frame.take().is_some() {
                        app.image_frame = None;
                        if let Some(kitty) = &mut app.kitty {
                            kitty.discard_queued();
                        }
                    }
                    app.update_image(app.playback.frame_index())?;
                }
            } else {
                if app.kitty.is_some() && app.image_frame.is_some() {
                    app.clear_kitty()?;
                    app.image_frame = None;
                }
                mmfx_changed |= update_compositor_only_monitor(editor, session, host.picker);
            }
        } else {
            mmfx_changed |= synchronize_timeline_context(editor, session);
            mmfx_changed |= update_compositor_only_monitor(editor, session, host.picker);
        }
        let resized = app
            .as_mut()
            .is_some_and(|app| receive_resized_images(app, completed));
        let timeline_resized = receive_timeline_resized_image(editor);
        let current = app.as_ref().map(|app| app.playback.frame_index());
        if current != last_frame && current.is_some() {
            editor
                .timeline
                .sync_total_frames(local_timeline_frame_count(session));
            editor.timeline.reveal(editor.timeline_playhead);
        }
        let status = app.as_ref().map_or("empty", PreviewApp::status);
        let range = app.as_ref().map(|app| app.playback_range.clone());
        let thumbnail_revision = app.as_ref().map(PreviewApp::thumbnail_revision);
        if redraw
            || mmfx_changed
            || resized
            || timeline_resized
            || current != last_frame
            || status != last_status
            || range != last_range
            || thumbnail_revision != last_thumbnail_revision
        {
            terminal
                .draw(|frame| draw_editor(frame, app.as_mut(), session, editor, host.picker))
                .map_err(|error| format!("cannot draw terminal editor: {error}"))?;
            last_frame = current;
            status.clone_into(&mut last_status);
            last_range = range;
            last_thumbnail_revision = thumbnail_revision;
            redraw = false;
        }
        if let Some(app) = app.as_mut() {
            app.flush_kitty_frame()?;
        }

        if event::poll(EVENT_WAIT).map_err(|error| format!("cannot poll terminal: {error}"))? {
            redraw = true;
            for pending_event in read_pending_editor_events()? {
                match pending_event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        host.terminal_size = terminal.size().map_err(|error| {
                            format!("cannot read terminal size while opening media: {error}")
                        })?;
                        if handle_editor_key(
                            app,
                            session,
                            history,
                            editor,
                            key,
                            Instant::now(),
                            host,
                        )? {
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
                        focus_editor_pane_from_mouse(editor, mouse);
                        handle_inspector_mouse(editor, mouse);
                        if let Some(app) = app.as_mut() {
                            handle_editor_mouse(app, session, editor, mouse, Instant::now())?;
                        } else {
                            handle_fx_only_timeline_mouse(session, editor, mouse);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn update_mmfx_preview(
    editor: &mut EditorUi,
    session: &EditorSession,
    host: &EditorHost<'_>,
    now: Instant,
) -> bool {
    let mut changed = false;
    if let Some(document) = editor.mmfx.as_mut()
        && document.compile_due.is_some_and(|due| due <= now)
    {
        document.compile_due = None;
        document.compile_status = "compiling preview…".into();
        let base_directory = document
            .resource_base
            .as_deref()
            .or_else(|| session.project_file().and_then(Path::parent))
            .unwrap_or(host.base_directory)
            .to_path_buf();
        let request = MmfxCompileRequest {
            generation: document.generation,
            revision: document.revision,
            source: document.source.clone(),
            base_directory,
        };
        let sent = editor
            .mmfx_worker
            .as_ref()
            .and_then(|worker| worker.requests.as_ref())
            .is_some_and(|sender| sender.send(request).is_ok());
        if !sent {
            document.compile_status = "preview worker is unavailable".into();
            editor.message = "error: MMFX preview worker is unavailable".into();
        }
        changed = true;
    }

    let mut results = Vec::new();
    if let Some(worker) = &editor.mmfx_worker {
        while let Ok(result) = worker.results.try_recv() {
            results.push(result);
        }
    }
    for compiled in results {
        let Some(document) = editor.mmfx.as_mut() else {
            continue;
        };
        if compiled.generation != document.generation || compiled.revision != document.revision {
            continue;
        }
        match compiled.result {
            Ok(image) => {
                let dimensions = (image.width(), image.height());
                if let Some(buffer) = &mut editor.mmfx_image {
                    buffer.replace_protocol(host.picker.new_resize_protocol(image));
                }
                document.last_good_revision = Some(compiled.revision);
                document.compile_status = format!(
                    "preview ready — {}x{} — revision {}",
                    dimensions.0, dimensions.1, compiled.revision
                );
                editor.message = format!("ok: MMFX {}", document.compile_status);
            }
            Err(error) => {
                document.compile_status = format!("error: {error}");
                editor.message = format!("error: MMFX {error} (last valid preview retained)");
            }
        }
        changed = true;
    }
    if let Some(buffer) = &mut editor.mmfx_image {
        let (received, error) = buffer.poll();
        changed |= received;
        if let Some(error) = error {
            editor.message = format!("error: cannot resize MMFX preview: {error}");
        }
    }
    changed
}

fn focus_editor_pane_from_mouse(editor: &mut EditorUi, mouse: MouseEvent) {
    // Pointer motion must never steal keyboard focus. Besides making the command
    // prompt unpredictable, changing focus here hides the MMFX source pane as
    // soon as the pointer crosses its border.
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    let position = (mouse.column, mouse.row).into();
    if editor.prompt_area.contains(position) {
        editor.pane_focus = EditorPaneFocus::Command;
    } else if editor.code_area.contains(position)
        && editor.mmfx.is_some()
        && editor.inspector_focus == InspectorFocus::Mmfx
    {
        editor.pane_focus = EditorPaneFocus::Code;
    } else if editor.inspector_area.contains(position) {
        editor.pane_focus = EditorPaneFocus::Inspector;
    } else if editor.timeline_area.contains(position) {
        editor.pane_focus = EditorPaneFocus::Timeline;
    }
}

fn handle_inspector_mouse(editor: &mut EditorUi, mouse: MouseEvent) {
    if !editor
        .inspector_area
        .contains((mouse.column, mouse.row).into())
    {
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => scroll_inspector(editor, -3),
        MouseEventKind::ScrollDown => scroll_inspector(editor, 3),
        _ => {}
    }
}

fn scroll_inspector(editor: &mut EditorUi, amount: i32) {
    editor.inspector_scroll = if amount.is_negative() {
        editor
            .inspector_scroll
            .saturating_sub(u16::try_from(amount.unsigned_abs()).unwrap_or(u16::MAX))
    } else {
        editor
            .inspector_scroll
            .saturating_add(u16::try_from(amount).unwrap_or(u16::MAX))
            .min(editor.inspector_max_scroll)
    };
}

fn read_pending_editor_events() -> Result<Vec<Event>, String> {
    const MAX_PENDING_EVENTS: usize = 512;
    let mut events =
        vec![event::read().map_err(|error| format!("cannot read terminal input: {error}"))?];
    while events.len() < MAX_PENDING_EVENTS
        && event::poll(Duration::ZERO)
            .map_err(|error| format!("cannot poll pending terminal input: {error}"))?
    {
        events.push(
            event::read()
                .map_err(|error| format!("cannot read pending terminal input: {error}"))?,
        );
    }
    Ok(coalesce_editor_events(events))
}

fn coalesce_editor_events(events: Vec<Event>) -> Vec<Event> {
    let mut coalesced = Vec::with_capacity(events.len());
    for event in events {
        if is_scrub_mouse_event(&event) && coalesced.last().is_some_and(is_scrub_mouse_event) {
            if let Some(previous) = coalesced.last_mut() {
                *previous = event;
            }
        } else {
            coalesced.push(event);
        }
    }
    coalesced
}

fn is_scrub_mouse_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left),
            ..
        })
    )
}

fn receive_timeline_resized_image(editor: &mut EditorUi) -> bool {
    let Some(image) = &mut editor.timeline_image else {
        return false;
    };
    let (received, error) = image.poll();
    if let Some(error) = error {
        editor.timeline_raster_key = None;
        editor.message = format!("error: cannot resize timeline image: {error}");
    }
    received
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
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        let code_available = editor.mmfx.is_some();
        editor.pane_focus = if matches!(key.code, KeyCode::BackTab)
            || key.modifiers.contains(KeyModifiers::SHIFT)
        {
            editor.pane_focus.previous(code_available)
        } else {
            editor.pane_focus.next(code_available)
        };
        if editor.pane_focus == EditorPaneFocus::Code {
            editor.inspector_focus = InspectorFocus::Mmfx;
        }
        editor.message = format!("{} focused.", editor.pane_focus.label());
        return Ok(false);
    }

    if editor.pane_focus == EditorPaneFocus::Code {
        return Ok(handle_mmfx_editor_key(session, editor, key, now));
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q' | 'c') => {
                if session.is_dirty() {
                    editor.message = "error: project has unsaved changes, including embedded MMFX; save it or use quit --discard".into();
                    return Ok(false);
                }
                return Ok(true);
            }
            KeyCode::Char(' ') => match editor.pane_focus {
                EditorPaneFocus::Command => {
                    complete_editor_prompt(session, history, editor, host);
                }
                EditorPaneFocus::Timeline => {
                    if let Some(app) = app.as_mut() {
                        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), now)?;
                    } else {
                        editor.message = "No media loaded. Use import <media-file>.".into();
                    }
                }
                EditorPaneFocus::Inspector | EditorPaneFocus::Code => {}
            },
            KeyCode::Char('z') => {
                editor.input = "undo".into();
                return execute_editor_input(app, session, history, editor, now, host);
            }
            KeyCode::Char('y') => {
                editor.input = "redo".into();
                return execute_editor_input(app, session, history, editor, now, host);
            }
            KeyCode::Left | KeyCode::Right if editor.pane_focus == EditorPaneFocus::Timeline => {
                editor
                    .timeline
                    .sync_total_frames(local_timeline_frame_count(session));
                editor
                    .timeline
                    .pan_half_page(matches!(key.code, KeyCode::Right));
                editor.message = timeline_view_message_optional(app.as_ref(), &editor.timeline);
            }
            _ => {}
        }
        return Ok(false);
    }

    if editor.pane_focus == EditorPaneFocus::Inspector {
        let page = i32::from(editor.inspector_area.height.saturating_sub(2).max(1));
        match key.code {
            KeyCode::Up => scroll_inspector(editor, -1),
            KeyCode::Down => scroll_inspector(editor, 1),
            KeyCode::PageUp => scroll_inspector(editor, -page),
            KeyCode::PageDown => scroll_inspector(editor, page),
            KeyCode::Home => editor.inspector_scroll = 0,
            KeyCode::End => editor.inspector_scroll = editor.inspector_max_scroll,
            KeyCode::Esc => {
                editor.pane_focus = EditorPaneFocus::Command;
                editor.message = "Command focused.".into();
            }
            _ => {}
        }
        return Ok(false);
    }

    if editor.pane_focus == EditorPaneFocus::Timeline {
        match key.code {
            KeyCode::Char('+' | '-') => {
                editor
                    .timeline
                    .sync_total_frames(local_timeline_frame_count(session));
                let direction = if matches!(key.code, KeyCode::Char('+')) {
                    TimelineZoom::In
                } else {
                    TimelineZoom::Out
                };
                let anchor = if local_timeline_frame_count(session) == 0 {
                    let range = editor.timeline.visible_range();
                    range.start + editor.timeline.visible_frame_count() / 2
                } else {
                    editor.timeline_playhead
                };
                editor.timeline.zoom_around_frame(anchor, direction);
                editor.message = timeline_view_message_optional(app.as_ref(), &editor.timeline);
            }
            KeyCode::Char('0') => {
                editor
                    .timeline
                    .sync_total_frames(local_timeline_frame_count(session));
                editor.timeline.fit();
                editor.message = timeline_view_message_optional(app.as_ref(), &editor.timeline);
            }
            KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                editor
                    .timeline
                    .sync_total_frames(local_timeline_frame_count(session));
                editor
                    .timeline
                    .pan_half_page(matches!(key.code, KeyCode::Right));
                editor.message = timeline_view_message_optional(app.as_ref(), &editor.timeline);
            }
            KeyCode::Left | KeyCode::Right => {
                let amount = 1_usize;
                if let Some(app) = app.as_mut() {
                    let local = if matches!(key.code, KeyCode::Left) {
                        editor.timeline_playhead.saturating_sub(amount)
                    } else {
                        editor
                            .timeline_playhead
                            .saturating_add(amount)
                            .min(local_timeline_frame_count(session).saturating_sub(1))
                    };
                    editor.project_playhead = seek_local_timeline_frame(app, session, local, now)?;
                    editor.timeline_playhead =
                        displayed_timeline_playhead(session, app, editor.project_playhead);
                    editor.timeline.reveal(editor.timeline_playhead);
                    editor.message = project_scrub_message(session, editor.project_playhead);
                } else {
                    editor.timeline_playhead = if matches!(key.code, KeyCode::Left) {
                        editor.timeline_playhead.saturating_sub(amount)
                    } else {
                        editor
                            .timeline_playhead
                            .saturating_add(amount)
                            .min(local_timeline_frame_count(session).saturating_sub(1))
                    };
                    editor.project_playhead =
                        project_frame_for_local_timeline(session, editor.timeline_playhead);
                    editor.timeline.reveal(editor.timeline_playhead);
                    editor.message = format!("timeline frame {}", editor.timeline_playhead);
                }
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                if let Some(app) = app.as_mut() {
                    let amount = project_nominal_frames_per_second(session);
                    let local = if matches!(key.code, KeyCode::PageUp) {
                        editor.timeline_playhead.saturating_sub(amount)
                    } else {
                        editor
                            .timeline_playhead
                            .saturating_add(amount)
                            .min(local_timeline_frame_count(session).saturating_sub(1))
                    };
                    editor.project_playhead = seek_local_timeline_frame(app, session, local, now)?;
                    editor.timeline_playhead =
                        displayed_timeline_playhead(session, app, editor.project_playhead);
                    editor.timeline.reveal(editor.timeline_playhead);
                    editor.message = project_scrub_message(session, editor.project_playhead);
                } else {
                    let amount = project_nominal_frames_per_second(session);
                    editor.timeline_playhead = if matches!(key.code, KeyCode::PageUp) {
                        editor.timeline_playhead.saturating_sub(amount)
                    } else {
                        editor
                            .timeline_playhead
                            .saturating_add(amount)
                            .min(local_timeline_frame_count(session).saturating_sub(1))
                    };
                    editor.project_playhead =
                        project_frame_for_local_timeline(session, editor.timeline_playhead);
                    editor.timeline.reveal(editor.timeline_playhead);
                    editor.message = format!("timeline frame {}", editor.timeline_playhead);
                }
            }
            KeyCode::Home | KeyCode::End => {
                if let Some(app) = app.as_mut() {
                    let local = if matches!(key.code, KeyCode::Home) {
                        0
                    } else {
                        local_timeline_frame_count(session).saturating_sub(1)
                    };
                    editor.project_playhead = seek_local_timeline_frame(app, session, local, now)?;
                    editor.timeline_playhead =
                        displayed_timeline_playhead(session, app, editor.project_playhead);
                    editor.timeline.reveal(editor.timeline_playhead);
                    editor.message = project_scrub_message(session, editor.project_playhead);
                } else {
                    editor.timeline_playhead = if matches!(key.code, KeyCode::Home) {
                        0
                    } else {
                        local_timeline_frame_count(session).saturating_sub(1)
                    };
                    editor.project_playhead =
                        project_frame_for_local_timeline(session, editor.timeline_playhead);
                    editor.timeline.reveal(editor.timeline_playhead);
                    editor.message = format!("timeline frame {}", editor.timeline_playhead);
                }
            }
            KeyCode::Esc => {
                editor.pane_focus = EditorPaneFocus::Command;
                editor.message = "Command focused.".into();
            }
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Enter => execute_editor_input(app, session, history, editor, now, host),
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

fn handle_mmfx_editor_key(
    session: &mut EditorSession,
    editor: &mut EditorUi,
    key: KeyEvent,
    now: Instant,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => save_project_from_shortcut(session, editor),
            KeyCode::Char('z') => match session.apply(EditCommand::Undo) {
                Ok(_) => refresh_open_mmfx_from_project(session, editor, now),
                Err(error) => editor.message = format!("error: {error}"),
            },
            KeyCode::Char('y') => match session.apply(EditCommand::Redo) {
                Ok(_) => refresh_open_mmfx_from_project(session, editor, now),
                Err(error) => editor.message = format!("error: {error}"),
            },
            KeyCode::Char('q' | 'c') => {
                if session.is_dirty() {
                    editor.message = "error: project has unsaved changes, including embedded MMFX; save it or use quit --discard".into();
                } else {
                    return true;
                }
            }
            _ => {}
        }
        return false;
    }

    let page_lines = usize::from(editor.code_area.height.saturating_sub(2).max(1));
    let Some(document) = editor.mmfx.as_mut() else {
        editor.pane_focus = EditorPaneFocus::Command;
        return false;
    };
    let revision = document.revision;
    match key.code {
        KeyCode::Esc => {
            editor.pane_focus = EditorPaneFocus::Command;
            editor.message = "Command focused; MMFX source remains open.".into();
        }
        KeyCode::Left => {
            document.cursor = previous_char_boundary(&document.source, document.cursor);
        }
        KeyCode::Right => document.cursor = next_char_boundary(&document.source, document.cursor),
        KeyCode::Up => move_mmfx_cursor_vertical(document, -1),
        KeyCode::Down => move_mmfx_cursor_vertical(document, 1),
        KeyCode::PageUp => {
            move_mmfx_cursor_vertical(document, -isize::try_from(page_lines).unwrap_or(isize::MAX));
        }
        KeyCode::PageDown => {
            move_mmfx_cursor_vertical(document, isize::try_from(page_lines).unwrap_or(isize::MAX));
        }
        KeyCode::Home => document.cursor = line_start(&document.source, document.cursor),
        KeyCode::End => document.cursor = line_end(&document.source, document.cursor),
        KeyCode::Backspace => {
            let start = previous_char_boundary(&document.source, document.cursor);
            if start != document.cursor {
                let mut source = document.source.clone();
                source.replace_range(start..document.cursor, "");
                document.replace_source(source, start, now);
            }
        }
        KeyCode::Delete => {
            let end = next_char_boundary(&document.source, document.cursor);
            if end != document.cursor {
                let mut source = document.source.clone();
                source.replace_range(document.cursor..end, "");
                document.replace_source(source, document.cursor, now);
            }
        }
        KeyCode::Enter => insert_mmfx_text(document, "\n", now),
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            let mut encoded = [0_u8; 4];
            insert_mmfx_text(document, character.encode_utf8(&mut encoded), now);
        }
        _ => {}
    }
    if editor
        .mmfx
        .as_ref()
        .is_some_and(|document| document.revision != revision)
    {
        let source = editor.mmfx.as_ref().map(|document| MmfxSource {
            source: document.source.clone(),
            resource_base: document.resource_base.clone(),
        });
        if let Some(source) = source
            && let Err(error) = session.replace_current_mmfx_source(source)
        {
            editor.message = format!("error: cannot update embedded MMFX source: {error}");
        }
    }
    false
}

fn save_project_from_shortcut(session: &mut EditorSession, editor: &mut EditorUi) {
    let result = session
        .project_file()
        .map(Path::to_path_buf)
        .ok_or_else(|| "project has no file yet; use save as <project>".to_owned())
        .and_then(|path| crate::save_editor_project(session, &path, false));
    match result {
        Ok(path) => {
            editor.message = format!("ok: saved project and embedded MMFX to {}", path.display());
        }
        Err(error) => editor.message = format!("error: {error}"),
    }
}

fn refresh_open_mmfx_from_project(session: &EditorSession, editor: &mut EditorUi, now: Instant) {
    let Ok((media_id, payload)) = session.current_mmfx_source() else {
        editor.mmfx = None;
        if let Some(image) = &mut editor.mmfx_image {
            image.empty();
        }
        editor.pane_focus = EditorPaneFocus::Command;
        editor.inspector_focus = InspectorFocus::Context;
        editor.message = "Project history left the edited scene; source editor closed.".into();
        return;
    };
    let Some(document) = editor.mmfx.as_mut() else {
        return;
    };
    if document.media_id != media_id {
        return;
    }
    document.source.clone_from(&payload.source);
    document.resource_base.clone_from(&payload.resource_base);
    document.cursor = document.cursor.min(document.source.len());
    document.changed(now);
    editor.message = "Updated embedded MMFX source from project history.".into();
}

fn insert_mmfx_text(document: &mut MmfxDocument, text: &str, now: Instant) {
    let mut source = document.source.clone();
    source.insert_str(document.cursor, text);
    document.replace_source(source, document.cursor + text.len(), now);
}

fn previous_char_boundary(source: &str, cursor: usize) -> usize {
    source
        .get(..cursor)
        .and_then(|prefix| prefix.char_indices().next_back().map(|(index, _)| index))
        .unwrap_or(cursor)
}

fn next_char_boundary(source: &str, cursor: usize) -> usize {
    source
        .get(cursor..)
        .and_then(|suffix| {
            suffix
                .char_indices()
                .nth(1)
                .map(|(offset, _)| cursor + offset)
        })
        .unwrap_or(source.len())
}

fn line_start(source: &str, cursor: usize) -> usize {
    source[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(source: &str, cursor: usize) -> usize {
    source[cursor..]
        .find('\n')
        .map_or(source.len(), |offset| cursor + offset)
}

fn mmfx_cursor_line_column(source: &str, cursor: usize) -> (usize, usize) {
    let prefix = &source[..cursor];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, tail)| tail)
        .chars()
        .count();
    (line, column)
}

fn move_mmfx_cursor_vertical(document: &mut MmfxDocument, delta: isize) {
    let (line, column) = mmfx_cursor_line_column(&document.source, document.cursor);
    let line_count = document.source.lines().count().max(1);
    let target = if delta.is_negative() {
        line.saturating_sub(delta.unsigned_abs())
    } else {
        line.saturating_add(delta.unsigned_abs())
            .min(line_count.saturating_sub(1))
    };
    let mut offset = 0;
    for (index, text) in document.source.split('\n').enumerate() {
        if index == target {
            let byte_column = text
                .char_indices()
                .nth(column)
                .map_or(text.len(), |(byte, _)| byte);
            document.cursor = offset + byte_column;
            return;
        }
        offset += text.len() + 1;
    }
}

fn extract_mmfx_source(
    session: &EditorSession,
    editor: &mut EditorUi,
    locator: &str,
    base_directory: &Path,
) {
    let result = session
        .current_mmfx_source()
        .map_err(|error| error.to_string())
        .and_then(|(_, payload)| {
            let path = resolve_mmfx_output_path(base_directory, locator);
            std::fs::write(&path, &payload.source).map_err(|error| {
                format!("cannot save MMFX source '{}': {error}", path.display())
            })?;
            Ok(path)
        });
    match result {
        Ok(path) => {
            editor.message = format!("ok: extracted embedded MMFX source to {}", path.display());
        }
        Err(error) => editor.message = format!("error: {error}"),
    }
}

fn resolve_mmfx_output_path(base_directory: &Path, locator: &str) -> PathBuf {
    let requested = Path::new(locator);
    let mut path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base_directory.join(requested)
    };
    if path.extension().is_none() {
        path.set_extension("mmfx");
    }
    path
}

fn complete_editor_prompt(
    session: &EditorSession,
    history: &mut CommandHistory,
    editor: &mut EditorUi,
    host: &EditorHost<'_>,
) {
    let completion = prompt_completion::complete(&editor.input, session, host.base_directory);
    let changed = completion.replacement != editor.input;
    editor.input = completion.replacement;
    history.detach();
    editor.message = match completion.candidates.as_slice() {
        [] => "No completion matches this context.".into(),
        [candidate] if changed => format!("Completed: {candidate}"),
        candidates => completion_candidates_message(candidates),
    };
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
    session: &EditorSession,
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
    editor
        .timeline
        .sync_total_frames(local_timeline_frame_count(session));
    let relative = usize::from(mouse.column.saturating_sub(editor.timeline_area.x));
    let width = usize::from(editor.timeline_area.width);
    if width == 0 {
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
            let local_frame = editor.timeline.frame_at_column(relative, width);
            editor.project_playhead = seek_local_timeline_frame(app, session, local_frame, now)?;
            editor.timeline_playhead =
                displayed_timeline_playhead(session, app, editor.project_playhead);
            editor.message = project_scrub_message(session, editor.project_playhead);
        }
        MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::CONTROL) => {
            editor
                .timeline
                .zoom_at_column(relative, width, TimelineZoom::In);
            editor.message = timeline_view_message(app, &editor.timeline);
        }
        MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::CONTROL) => {
            editor
                .timeline
                .zoom_at_column(relative, width, TimelineZoom::Out);
            editor.message = timeline_view_message(app, &editor.timeline);
        }
        MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
            editor.timeline.pan_half_page(false);
            editor.message = timeline_view_message(app, &editor.timeline);
        }
        MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
            editor.timeline.pan_half_page(true);
            editor.message = timeline_view_message(app, &editor.timeline);
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

fn handle_fx_only_timeline_mouse(
    session: &EditorSession,
    editor: &mut EditorUi,
    mouse: MouseEvent,
) {
    if !editor
        .timeline_area
        .contains((mouse.column, mouse.row).into())
    {
        return;
    }
    editor
        .timeline
        .sync_total_frames(local_timeline_frame_count(session));
    let relative = usize::from(mouse.column.saturating_sub(editor.timeline_area.x));
    let width = usize::from(editor.timeline_area.width);
    if width == 0 {
        return;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
            editor.timeline_playhead = editor.timeline.frame_at_column(relative, width);
            editor.project_playhead =
                project_frame_for_local_timeline(session, editor.timeline_playhead);
            editor.message = format!("timeline frame {}", editor.timeline_playhead);
        }
        MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::CONTROL) => {
            editor
                .timeline
                .zoom_at_column(relative, width, TimelineZoom::In);
            editor.message = timeline_view_message_optional(None, &editor.timeline);
        }
        MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::CONTROL) => {
            editor
                .timeline
                .zoom_at_column(relative, width, TimelineZoom::Out);
            editor.message = timeline_view_message_optional(None, &editor.timeline);
        }
        MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
            editor.timeline.pan_half_page(false);
            editor.message = timeline_view_message_optional(None, &editor.timeline);
        }
        MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
            editor.timeline.pan_half_page(true);
            editor.message = timeline_view_message_optional(None, &editor.timeline);
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let amount = project_nominal_frames_per_second(session);
            editor.timeline_playhead = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                editor.timeline_playhead.saturating_sub(amount)
            } else {
                editor
                    .timeline_playhead
                    .saturating_add(amount)
                    .min(local_timeline_frame_count(session).saturating_sub(1))
            };
            editor.project_playhead =
                project_frame_for_local_timeline(session, editor.timeline_playhead);
            editor.timeline.reveal(editor.timeline_playhead);
            editor.message = format!("timeline frame {}", editor.timeline_playhead);
        }
        _ => {}
    }
}

fn timeline_view_message(app: &PreviewApp, timeline: &TimelineViewport) -> String {
    let range = timeline.visible_range();
    let mode = if timeline.is_fitted() { "fit" } else { "zoom" };
    format!(
        "timeline {mode}: {}..{} ({} frames)",
        app.timecode(range.start),
        app.timecode(range.end),
        timeline.visible_frame_count(),
    )
}

fn timeline_view_message_optional(app: Option<&PreviewApp>, timeline: &TimelineViewport) -> String {
    app.map_or_else(
        || {
            let range = timeline.visible_range();
            let mode = if timeline.is_fitted() { "fit" } else { "zoom" };
            format!(
                "timeline {mode}: {}..{} ({} frames)",
                range.start,
                range.end,
                timeline.visible_frame_count(),
            )
        },
        |app| timeline_view_message(app, timeline),
    )
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
        EditCommand::FxLoad { .. }
        | EditCommand::FxSave { .. }
        | EditCommand::FxEdit
        | EditCommand::FxClose => InspectorFocus::Mmfx,
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
    editor.inspector_scroll = 0;
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
        CommandOutput::MonitorRequested { target } => {
            let scope = match target {
                MonitorTarget::Project => MonitorScope::Project,
                MonitorTarget::Local => MonitorScope::Local,
                MonitorTarget::Toggle => match editor.monitor_scope {
                    MonitorScope::Project => MonitorScope::Local,
                    MonitorScope::Local => MonitorScope::Project,
                },
                _ => editor.monitor_scope,
            };
            editor.monitor_scope = scope;
            editor.project_compositor_state = None;
            editor.timeline_monitor_key = None;
            if let Some(image) = &mut editor.timeline_monitor_image {
                image.empty();
            }
            if let Some(app) = app.as_mut() {
                app.clear_kitty()?;
                app.image_frame = None;
                app.composed_frame = None;
            }
            let context = session
                .project()
                .display_path(session.path())
                .unwrap_or_else(|_| "/".into());
            editor.message = match scope {
                MonitorScope::Project => "ok: Project Monitor selected".into(),
                MonitorScope::Local => format!("ok: Local Monitor selected — {context}"),
            };
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
                    reset_editor_timeline(editor, loaded.frame_count());
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
                    reset_editor_timeline(editor, 0);
                    session.replace_new_project(project);
                    close_mmfx_pane(editor);
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
                    close_mmfx_pane(editor);
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
                            reset_editor_timeline(
                                editor,
                                loaded.as_ref().map_or(0, PreviewApp::frame_count),
                            );
                            *app = loaded;
                            editor.message = format!("ok: opened {}", path.display());
                        }
                        Err(error) => {
                            *app = None;
                            reset_editor_timeline(editor, 0);
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
                    if let Some(document) = editor.mmfx.as_mut()
                        && document.resource_base.is_none()
                    {
                        document.changed(now);
                    }
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
        CommandOutput::FxLoadRequested { locator } => {
            let result = crate::resolve_existing_path(host.base_directory, &locator, "MMFX source")
                .and_then(|path| {
                    let source = std::fs::read_to_string(&path).map_err(|error| {
                        format!("cannot read MMFX source '{}': {error}", path.display())
                    })?;
                    let resource_base = path.parent().map(Path::to_path_buf);
                    session
                        .replace_current_mmfx_source(MmfxSource {
                            source,
                            resource_base,
                        })
                        .map_err(|error| error.to_string())?;
                    Ok(path)
                });
            match result {
                Ok(path) => {
                    open_current_mmfx_editor(session, editor)?;
                    editor.message = format!(
                        "ok: loaded {} into the focused scene as embedded source",
                        path.display()
                    );
                }
                Err(error) => editor.message = format!("error: {error}"),
            }
            return Ok(false);
        }
        CommandOutput::FxSaveRequested { locator } => {
            extract_mmfx_source(session, editor, &locator, host.base_directory);
            if editor.mmfx.is_some() {
                editor.inspector_focus = InspectorFocus::Mmfx;
            }
            return Ok(false);
        }
        CommandOutput::FxEditRequested => {
            open_current_mmfx_editor(session, editor)?;
            editor.message = "Editing the focused scene's embedded MMFX source.".into();
            return Ok(false);
        }
        CommandOutput::FxCloseRequested => {
            if editor.mmfx.take().is_some() {
                if let Some(image) = &mut editor.mmfx_image {
                    image.empty();
                }
                editor.pane_focus = EditorPaneFocus::Command;
                editor.inspector_focus = InspectorFocus::Context;
                editor.message = "ok: closed MMFX source pane; source remains embedded".into();
            } else {
                editor.message = "No MMFX source was open.".into();
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
    sync_open_mmfx_context(session, editor, now);
    if changed && let Some(app) = app.as_mut() {
        if let Ok(range) = editor_source_range(session) {
            let current = app.playback.frame_index();
            let target = current.clamp(range.start, range.end - 1);
            app.set_playback_range(range, target, now)?;
        } else {
            app.pause_playback(now);
            editor
                .message
                .push_str("  (no previewable source; redo restores it)");
        }
    }
    Ok(false)
}

fn sync_open_mmfx_context(session: &EditorSession, editor: &mut EditorUi, now: Instant) {
    let Some(open_media_id) = editor.mmfx.as_ref().map(|document| document.media_id) else {
        return;
    };
    let Ok((media_id, payload)) = session.current_mmfx_source() else {
        editor.mmfx = None;
        if let Some(image) = &mut editor.mmfx_image {
            image.empty();
        }
        if editor.pane_focus == EditorPaneFocus::Code {
            editor.pane_focus = EditorPaneFocus::Command;
        }
        editor.inspector_focus = InspectorFocus::Context;
        editor
            .message
            .push_str("  (MMFX editor closed: hierarchy context changed)");
        return;
    };
    if media_id != open_media_id {
        editor.mmfx = None;
        if let Some(image) = &mut editor.mmfx_image {
            image.empty();
        }
        if editor.pane_focus == EditorPaneFocus::Code {
            editor.pane_focus = EditorPaneFocus::Command;
        }
        editor.inspector_focus = InspectorFocus::Context;
        editor
            .message
            .push_str("  (MMFX editor closed: another scene is focused)");
        return;
    }
    if let Some(document) = editor.mmfx.as_mut()
        && (document.source != payload.source || document.resource_base != payload.resource_base)
    {
        document.source.clone_from(&payload.source);
        document.resource_base.clone_from(&payload.resource_base);
        document.cursor = document.cursor.min(document.source.len());
        document.changed(now);
    }
}

fn open_current_mmfx_editor(session: &EditorSession, editor: &mut EditorUi) -> Result<(), String> {
    let (media_id, payload) = session
        .current_mmfx_source()
        .map_err(|error| error.to_string())?;
    let media = session
        .project()
        .media(media_id)
        .ok_or_else(|| "focused MMFX scene disappeared".to_owned())?;
    editor.mmfx_generation = editor.mmfx_generation.wrapping_add(1);
    editor.mmfx = Some(MmfxDocument::new(
        media_id,
        media.name.clone(),
        payload.source.clone(),
        payload.resource_base.clone(),
        editor.mmfx_generation,
    ));
    if let Some(image) = &mut editor.mmfx_image {
        image.empty();
    }
    editor.pane_focus = EditorPaneFocus::Code;
    editor.inspector_focus = InspectorFocus::Mmfx;
    Ok(())
}

fn close_mmfx_pane(editor: &mut EditorUi) {
    editor.mmfx = None;
    if let Some(image) = &mut editor.mmfx_image {
        image.empty();
    }
    if editor.pane_focus == EditorPaneFocus::Code {
        editor.pane_focus = EditorPaneFocus::Command;
    }
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
    let probe = media_probe::probe_media(&path, session.project().settings())?;
    let source = open_source(&path)?;
    let time_base = probe.frame_time_base()?;
    let duration = i64::try_from(probe.frame_count)
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
            kind: probe.kind,
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
        .find(|entry| matches!(entry.kind.as_str(), "video/mpeg2" | "video/h264"))
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
        app.tick(Instant::now(), true)?;
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

enum PreviewSource {
    Mpeg2(Mpeg2PlaybackSource),
    H264(H264PlaybackSource),
}

impl From<Mpeg2PlaybackSource> for PreviewSource {
    fn from(source: Mpeg2PlaybackSource) -> Self {
        Self::Mpeg2(source)
    }
}

impl From<H264PlaybackSource> for PreviewSource {
    fn from(source: H264PlaybackSource) -> Self {
        Self::H264(source)
    }
}

enum PreviewDecodeEvent {
    Frame {
        generation: u64,
        frame_index: usize,
        frame: Box<VideoFrame>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

enum ThumbnailCommand {
    Request(Vec<usize>),
    Stop,
}

enum ThumbnailResult {
    Ready { frame_index: usize, image: RgbImage },
    Error(String),
}

struct TimelineThumbnailer {
    commands: Option<mpsc::Sender<ThumbnailCommand>>,
    results: Receiver<ThumbnailResult>,
    worker: Option<JoinHandle<()>>,
    pending: BTreeSet<usize>,
}

impl TimelineThumbnailer {
    fn spawn(path: PathBuf) -> Result<Self, String> {
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("mmrecode-timeline-thumbnails".into())
            .spawn(move || timeline_thumbnail_worker(&path, &command_rx, &result_tx))
            .map_err(|error| format!("cannot start timeline thumbnail worker: {error}"))?;
        Ok(Self {
            commands: Some(command_tx),
            results: result_rx,
            worker: Some(worker),
            pending: BTreeSet::new(),
        })
    }

    fn request(&mut self, frames: Vec<usize>) {
        let pending = frames.iter().copied().collect::<BTreeSet<_>>();
        if self.pending == pending {
            return;
        }
        self.pending = pending;
        if let Some(commands) = &self.commands {
            let _ = commands.send(ThumbnailCommand::Request(frames));
        }
    }

    fn completed(&mut self, frame_index: usize) {
        self.pending.remove(&frame_index);
    }
}

impl Drop for TimelineThumbnailer {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(ThumbnailCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn timeline_thumbnail_worker(
    path: &Path,
    commands: &Receiver<ThumbnailCommand>,
    results: &mpsc::Sender<ThumbnailResult>,
) {
    let mut source = match open_source(path) {
        Ok(source) => source,
        Err(error) => {
            let _ = results.send(ThumbnailResult::Error(error));
            return;
        }
    };
    while let Ok(command) = commands.recv() {
        let ThumbnailCommand::Request(mut frames) = command else {
            return;
        };
        'batch: loop {
            let mut cursor = 0;
            while cursor < frames.len() {
                let frame_index = frames[cursor];
                let generation = match source.request(frame_index, 0) {
                    Ok(generation) => generation,
                    Err(error) => {
                        let _ = results.send(ThumbnailResult::Error(error));
                        cursor += 1;
                        continue;
                    }
                };
                loop {
                    match commands.try_recv() {
                        Ok(ThumbnailCommand::Request(new_frames)) => {
                            frames = new_frames;
                            continue 'batch;
                        }
                        Ok(ThumbnailCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                            return;
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    match source.try_event() {
                        Ok(Some(PreviewDecodeEvent::Frame {
                            generation: event_generation,
                            frame_index: event_frame,
                            frame,
                        })) if event_generation == generation && event_frame == frame_index => {
                            match video_frame_image(&frame, (THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT)) {
                                Ok(image) => {
                                    let _ = results.send(ThumbnailResult::Ready {
                                        frame_index,
                                        image: image.into_rgb8(),
                                    });
                                }
                                Err(error) => {
                                    let _ = results.send(ThumbnailResult::Error(error));
                                }
                            }
                            break;
                        }
                        Ok(Some(PreviewDecodeEvent::Error {
                            generation: event_generation,
                            message,
                        })) if event_generation == 0 || event_generation == generation => {
                            let _ = results.send(ThumbnailResult::Error(message));
                            break;
                        }
                        Ok(Some(_) | None) => thread::sleep(Duration::from_millis(2)),
                        Err(error) => {
                            let _ = results.send(ThumbnailResult::Error(error));
                            return;
                        }
                    }
                }
                cursor += 1;
            }
            break;
        }
    }
}

impl PreviewSource {
    fn frame_count(&self) -> usize {
        match self {
            Self::Mpeg2(source) => source.index().frame_count(),
            Self::H264(source) => source.index().frame_count(),
        }
    }

    fn playback_timeline(&self) -> Result<PlaybackTimeline, String> {
        match self {
            Self::Mpeg2(source) => {
                PlaybackTimeline::new(source.index().frame_rate(), source.index().frame_count())
                    .map_err(|error| error.to_string())
            }
            Self::H264(source) => source.index().playback_timeline(),
        }
    }

    fn media_start_time(&self) -> Duration {
        match self {
            Self::Mpeg2(_) => Duration::ZERO,
            Self::H264(source) => source
                .index()
                .frames()
                .first()
                .and_then(|frame| {
                    timestamp_seconds(frame.pts, source.index().time_base())
                        .is_sign_positive()
                        .then(|| {
                            Duration::from_secs_f64(timestamp_seconds(
                                frame.pts,
                                source.index().time_base(),
                            ))
                        })
                })
                .unwrap_or(Duration::ZERO),
        }
    }

    fn request(&mut self, frame_index: usize, look_ahead: usize) -> Result<u64, String> {
        match self {
            Self::Mpeg2(source) => source.request(frame_index, look_ahead),
            Self::H264(source) => source.request(frame_index, look_ahead),
        }
    }

    fn try_event(&self) -> Result<Option<PreviewDecodeEvent>, String> {
        match self {
            Self::Mpeg2(source) => source.try_event().map(|event| {
                event.map(|event| match event {
                    Mpeg2PlaybackEvent::Frame {
                        generation,
                        frame_index,
                        picture,
                    } => PreviewDecodeEvent::Frame {
                        generation,
                        frame_index,
                        frame: Box::new(picture.frame),
                    },
                    Mpeg2PlaybackEvent::Error {
                        generation,
                        message,
                    } => PreviewDecodeEvent::Error {
                        generation,
                        message,
                    },
                })
            }),
            Self::H264(source) => source.try_event().map(|event| {
                event.map(|event| match event {
                    H264PlaybackEvent::Frame {
                        generation,
                        frame_index,
                        frame,
                    } => PreviewDecodeEvent::Frame {
                        generation,
                        frame_index,
                        frame,
                    },
                    H264PlaybackEvent::Error {
                        generation,
                        message,
                    } => PreviewDecodeEvent::Error {
                        generation,
                        message,
                    },
                })
            }),
        }
    }

    fn is_intra_frame(&self, frame_index: usize) -> bool {
        match self {
            Self::Mpeg2(source) => source
                .index()
                .frames()
                .get(frame_index)
                .is_some_and(|frame| frame.picture_type == mmrecode_mpeg2::PictureType::I),
            Self::H264(source) => source
                .index()
                .frames()
                .get(frame_index)
                .is_some_and(|frame| frame.is_idr),
        }
    }

    fn timeline_pictures(&self, range: Range<usize>) -> Vec<TimelinePicture> {
        let start = range.start;
        match self {
            Self::Mpeg2(source) => source
                .index()
                .frames()
                .get(range)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(offset, frame)| TimelinePicture {
                    frame: start + offset,
                    kind: match frame.picture_type {
                        mmrecode_mpeg2::PictureType::I => TimelinePictureKind::I,
                        mmrecode_mpeg2::PictureType::P => TimelinePictureKind::P,
                        mmrecode_mpeg2::PictureType::B => TimelinePictureKind::B,
                        _ => TimelinePictureKind::Other,
                    },
                    random_access: frame.random_access != mmrecode_core::RandomAccessKind::None,
                    reference: !matches!(frame.picture_type, mmrecode_mpeg2::PictureType::B),
                })
                .collect(),
            Self::H264(source) => source
                .index()
                .frames()
                .get(range)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(offset, frame)| TimelinePicture {
                    frame: start + offset,
                    kind: match frame.picture_type {
                        mmrecode_h264::PictureType::I | mmrecode_h264::PictureType::Si => {
                            TimelinePictureKind::I
                        }
                        mmrecode_h264::PictureType::P | mmrecode_h264::PictureType::Sp => {
                            TimelinePictureKind::P
                        }
                        mmrecode_h264::PictureType::B => TimelinePictureKind::B,
                    },
                    random_access: frame.is_idr,
                    reference: frame.is_reference,
                })
                .collect(),
        }
    }

    fn dimensions(&self) -> (usize, usize) {
        match self {
            Self::Mpeg2(source) => source.index().frames().first().map_or((0, 0), |frame| {
                (frame.sequence.width, frame.sequence.height)
            }),
            Self::H264(source) => (
                source.index().display_width(),
                source.index().display_height(),
            ),
        }
    }

    fn is_progressive(&self) -> bool {
        match self {
            Self::Mpeg2(source) => source
                .index()
                .frames()
                .first()
                .is_some_and(|frame| frame.sequence.progressive_sequence),
            Self::H264(source) => source.index().is_progressive(),
        }
    }

    fn is_h264(&self) -> bool {
        matches!(self, Self::H264(_))
    }
}

#[allow(clippy::cast_precision_loss)]
fn timestamp_seconds(value: i64, time_base: Rational) -> f64 {
    value as f64 * time_base.numerator() as f64 / time_base.denominator() as f64
}

fn load_aac_source(
    path: &Path,
    video_start: Duration,
) -> (Option<AacPlaybackState>, Option<String>) {
    let Ok(bytes) = std::fs::read(path) else {
        return (None, None);
    };
    let Ok(mut source) = AacPlaybackSource::new(bytes) else {
        return (None, None);
    };
    let audio_minus_video = source.index().start_time().as_secs_f64() - video_start.as_secs_f64();
    match source.request_decode() {
        Ok(generation) => (
            Some(AacPlaybackState {
                source,
                generation,
                audio_minus_video,
                backend: None,
            }),
            None,
        ),
        Err(error) => (None, Some(error)),
    }
}

struct PreviewApp {
    source: PreviewSource,
    frames: BTreeMap<usize, Box<VideoFrame>>,
    generation: u64,
    requested_range: Range<usize>,
    playback: PlaybackController,
    aac: Option<AacPlaybackState>,
    audio_output: Option<AudioOutput>,
    audio_error: Option<String>,
    playback_range: Range<usize>,
    resume_when_buffered: bool,
    picker: Picker,
    image_state: Option<ThreadProtocol>,
    kitty: Option<KittyStreamer>,
    image_frame: Option<usize>,
    composed_frame: Option<(usize, u64)>,
    thumbnailer: Option<TimelineThumbnailer>,
    thumbnails: BTreeMap<usize, RgbImage>,
    timeline_thumbnail_frames: Vec<usize>,
    thumbnail_revision: u64,
    thumbnail_error: Option<String>,
    terminal_size: Size,
    preview_area: Rect,
    path: String,
    error: Option<String>,
}

#[derive(Debug)]
struct AacPlaybackState {
    source: AacPlaybackSource,
    generation: u64,
    audio_minus_video: f64,
    backend: Option<AacDecodeBackend>,
}

impl PreviewApp {
    fn new(
        source: impl Into<PreviewSource>,
        picker: Picker,
        resize_tx: mpsc::Sender<ResizeRequest>,
        path: &Path,
        terminal_size: Size,
    ) -> Result<Self, String> {
        let source = source.into();
        let timeline = source.playback_timeline()?;
        let video_start = source.media_start_time();
        let (aac, audio_error) = load_aac_source(path, video_start);
        let playback_range = 0..source.frame_count();
        let direct_kitty = picker.protocol_type() == ProtocolType::Kitty && !inside_tmux();
        let (thumbnailer, thumbnail_error) = match TimelineThumbnailer::spawn(path.to_path_buf()) {
            Ok(thumbnailer) => (Some(thumbnailer), None),
            Err(error) => (None, Some(error)),
        };
        Ok(Self {
            source,
            frames: BTreeMap::new(),
            generation: 0,
            requested_range: 0..0,
            playback: PlaybackController::new(timeline),
            aac,
            audio_output: None,
            audio_error,
            playback_range,
            resume_when_buffered: false,
            image_state: (!direct_kitty).then(|| ThreadProtocol::new(resize_tx, None)),
            kitty: direct_kitty.then(KittyStreamer::new).transpose()?,
            picker,
            image_frame: None,
            composed_frame: None,
            thumbnailer,
            thumbnails: BTreeMap::new(),
            timeline_thumbnail_frames: Vec::new(),
            thumbnail_revision: 0,
            thumbnail_error,
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

    fn tick(&mut self, now: Instant, update_image: bool) -> Result<(), String> {
        self.poll_decoder()?;
        self.poll_audio(now);
        self.poll_timeline_thumbnails();
        let playback_event = if self.playback.is_playing() {
            if let Some(audio) = &self.audio_output {
                self.playback.synchronize(audio.position(), now)
            } else {
                self.playback.advance(now)
            }
        } else {
            PlaybackEvent::None
        };
        if playback_event == PlaybackEvent::Looped
            && let Some(audio) = &self.audio_output
            && let Err(error) = audio.restart()
        {
            self.audio_error = Some(error);
        }
        let mut current = self.playback.frame_index();
        if current < self.playback_range.start || current >= self.playback_range.end {
            let was_playing = self.playback.is_playing();
            let target = if was_playing && self.playback.is_looping() {
                self.playback_range.start
            } else {
                self.playback_range.end - 1
            };
            self.pause_playback(now);
            self.playback
                .seek(self.playback.timeline().position_of_frame(target), now);
            if let Some(audio) = &self.audio_output
                && let Err(error) = audio.seek(self.playback.position())
            {
                self.audio_error = Some(error);
            }
            if was_playing && self.playback.is_looping() {
                self.play_playback(now);
            }
            current = target;
        }
        if self.playback.is_playing() && !self.frames.contains_key(&current) {
            self.pause_playback(now);
            self.resume_when_buffered = true;
        }
        self.request_frame(current)?;
        if self.resume_when_buffered && self.has_buffer(current) {
            self.resume_when_buffered = false;
            self.play_playback(now);
        }
        if update_image {
            self.update_image(current)
        } else {
            Ok(())
        }
    }

    fn thumbnail_revision(&self) -> u64 {
        self.thumbnail_revision
    }

    fn request_timeline_thumbnails(&mut self, visible: Range<usize>, pixel_width: u32) {
        let visible = visible.start.min(self.frame_count())..visible.end.min(self.frame_count());
        let span = visible.end.saturating_sub(visible.start);
        if span == 0 {
            self.timeline_thumbnail_frames.clear();
            return;
        }
        let target_count = usize::try_from(pixel_width.div_ceil(THUMBNAIL_WIDTH))
            .unwrap_or(usize::MAX)
            .clamp(1, 32)
            .min(span);
        let mut desired = BTreeSet::new();
        if target_count == 1 {
            desired.insert(visible.start + span / 2);
        } else {
            for slot in 0..target_count {
                let numerator = slot as u128 * (span - 1) as u128;
                let offset =
                    usize::try_from(numerator / (target_count - 1) as u128).unwrap_or(span - 1);
                desired.insert(visible.start + offset);
            }
        }
        self.timeline_thumbnail_frames = desired.iter().copied().collect();
        let missing = desired
            .into_iter()
            .filter(|frame| !self.thumbnails.contains_key(frame))
            .collect::<Vec<_>>();
        if let Some(thumbnailer) = &mut self.thumbnailer {
            thumbnailer.request(missing);
        }
    }

    fn poll_timeline_thumbnails(&mut self) {
        let Some(thumbnailer) = &mut self.thumbnailer else {
            return;
        };
        let mut changed = false;
        while let Ok(result) = thumbnailer.results.try_recv() {
            match result {
                ThumbnailResult::Ready { frame_index, image } => {
                    thumbnailer.completed(frame_index);
                    self.thumbnails.insert(frame_index, image);
                    changed = true;
                }
                ThumbnailResult::Error(error) => {
                    if self.thumbnail_error.as_deref() != Some(error.as_str()) {
                        self.thumbnail_error = Some(error);
                        changed = true;
                    }
                }
            }
        }
        if self.thumbnails.len() > MAX_TIMELINE_THUMBNAILS {
            let focus = self.playback.frame_index();
            let mut retained = self.thumbnails.keys().copied().collect::<Vec<_>>();
            retained.sort_by_key(|frame| frame.abs_diff(focus));
            for frame in retained.into_iter().skip(MAX_TIMELINE_THUMBNAILS) {
                self.thumbnails.remove(&frame);
            }
        }
        if changed {
            self.thumbnail_revision = self.thumbnail_revision.wrapping_add(1);
        }
    }

    fn smart_render_spans(&self, session: &EditorSession) -> Vec<SmartRenderSpan> {
        let retained = self.playback_range.clone();
        let settings = session.project().settings();
        let (width, height) = self.source.dimensions();
        let dimensions_match = usize::try_from(settings.width).ok() == Some(width)
            && usize::try_from(settings.height).ok() == Some(height);
        let rate_matches = settings.frame_rate == self.playback.timeline().frame_rate();
        let scan_matches = matches!(
            (settings.scan_mode, self.source.is_progressive()),
            (mmrecode_edit::ProjectScanMode::Progressive, true)
                | (mmrecode_edit::ProjectScanMode::Interlaced, false)
        );
        if !(dimensions_match && rate_matches && scan_matches) {
            return vec![SmartRenderSpan {
                frames: retained,
                state: SmartRenderState::FullRender,
            }];
        }

        let mut spans = vec![SmartRenderSpan {
            frames: retained.clone(),
            state: SmartRenderState::Copy,
        }];
        let boundary_state = if self.source.is_h264() {
            SmartRenderState::Review
        } else {
            SmartRenderState::Bridge
        };
        if retained.start > 0 && !self.source.is_intra_frame(retained.start) {
            let boundary_end = (retained.start + 1..retained.end)
                .find(|frame| self.source.is_intra_frame(*frame))
                .unwrap_or(retained.end);
            spans.push(SmartRenderSpan {
                frames: retained.start..boundary_end,
                state: boundary_state,
            });
        }
        if retained.end < self.frame_count() && !self.source.is_intra_frame(retained.end) {
            let boundary_start = (retained.start..retained.end)
                .rev()
                .find(|frame| self.source.is_intra_frame(*frame))
                .unwrap_or(retained.start);
            spans.push(SmartRenderSpan {
                frames: boundary_start..retained.end,
                state: boundary_state,
            });
        }
        spans
    }

    fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Result<bool, String> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char(' ') => {
                if self.playback.is_playing() || self.resume_when_buffered {
                    self.pause_playback(now);
                    self.resume_when_buffered = false;
                } else {
                    let mut current = self.playback.frame_index();
                    if current == self.playback_range.end - 1 && self.playback_range.start < current
                    {
                        current = self.playback_range.start;
                        self.seek_frame(current, now)?;
                    }
                    if self.has_buffer(current) {
                        self.play_playback(now);
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
        self.pause_playback(now);
        self.resume_when_buffered = false;
        self.playback
            .seek(self.playback.timeline().position_of_frame(frame), now);
        if let Some(audio) = &self.audio_output {
            audio.seek(self.playback.position())?;
        }
        self.request_frame(frame)
    }

    fn play_playback(&mut self, now: Instant) {
        self.playback.play(now);
        if let Some(audio) = &self.audio_output {
            audio.play();
        }
    }

    fn pause_playback(&mut self, now: Instant) {
        self.playback.pause(now);
        if let Some(audio) = &self.audio_output {
            audio.pause();
        }
    }

    fn poll_audio(&mut self, _now: Instant) {
        let Some(aac) = &mut self.aac else {
            return;
        };
        loop {
            let event = match aac.source.try_event() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    self.audio_error = Some(error);
                    break;
                }
            };
            match event {
                AacPlaybackEvent::Decoded {
                    generation,
                    audio,
                    backend,
                } if generation == aac.generation => {
                    aac.backend = Some(backend);
                    match AudioOutput::open(*audio, aac.audio_minus_video) {
                        Ok(output) => {
                            if let Err(error) = output.seek(self.playback.position()) {
                                self.audio_error = Some(error);
                                continue;
                            }
                            if self.playback.is_playing() {
                                output.play();
                            }
                            self.audio_output = Some(output);
                        }
                        Err(error) => self.audio_error = Some(error),
                    }
                }
                AacPlaybackEvent::Error {
                    generation,
                    message,
                } if generation == aac.generation => self.audio_error = Some(message),
                AacPlaybackEvent::Decoded { .. } | AacPlaybackEvent::Error { .. } => {}
            }
        }
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
                PreviewDecodeEvent::Frame {
                    generation,
                    frame_index,
                    frame,
                } if generation == self.generation => {
                    self.frames.insert(frame_index, frame);
                }
                PreviewDecodeEvent::Error {
                    generation,
                    message,
                } if generation == 0 || generation == self.generation => return Err(message),
                PreviewDecodeEvent::Frame { .. } | PreviewDecodeEvent::Error { .. } => {}
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
        let Some(video_frame) = self.frames.get(&frame_index) else {
            return Ok(());
        };
        let bounds = monitor_pixel_bounds(
            self.preview_area,
            self.picker.font_size(),
            (video_frame.width, video_frame.height),
            self.kitty.is_some(),
        );
        let image = video_frame_image(video_frame, bounds)?;
        if let Some(kitty) = &mut self.kitty {
            kitty.queue(frame_index, image.into_rgb8());
        } else if let Some(image_state) = &mut self.image_state {
            image_state.replace_protocol(self.picker.new_resize_protocol(image));
            self.image_frame = Some(frame_index);
        }
        Ok(())
    }

    fn update_image_with_compositor(
        &mut self,
        frame_index: usize,
        timeline_frame: usize,
        compositor: &mut ProjectCompositor,
    ) -> Result<(), String> {
        if self.image_frame == Some(frame_index)
            || self
                .kitty
                .as_ref()
                .is_some_and(|kitty| kitty.queued_frame() == Some(frame_index))
        {
            return Ok(());
        }
        let Some(video_frame) = self.frames.get(&frame_index) else {
            return Ok(());
        };
        let bounds = monitor_pixel_bounds(
            self.preview_area,
            self.picker.font_size(),
            (video_frame.width, video_frame.height),
            self.kitty.is_some(),
        );
        let mut image = video_frame_image(video_frame, bounds)?.to_rgba8();
        compositor
            .composite_rgba8_preview(timeline_frame_i64(timeline_frame), &mut image)
            .map_err(|error| error.to_string())?;
        let image = DynamicImage::ImageRgba8(image);
        if let Some(kitty) = &mut self.kitty {
            kitty.queue(frame_index, image.into_rgb8());
        } else if let Some(image_state) = &mut self.image_state {
            image_state.replace_protocol(self.picker.new_resize_protocol(image));
            self.image_frame = Some(frame_index);
        }
        Ok(())
    }

    fn update_timeline_composition(
        &mut self,
        compositor: &mut ProjectCompositor,
        timeline_frame: usize,
    ) -> Result<(), String> {
        let frame_index = self.playback.frame_index();
        let signature = compositor.active_signature(timeline_frame_i64(timeline_frame));
        if self.composed_frame == Some((frame_index, signature)) {
            return Ok(());
        }
        if !self.frames.contains_key(&frame_index) {
            self.composed_frame = None;
            return Ok(());
        }
        self.image_frame = None;
        if let Some(kitty) = &mut self.kitty {
            kitty.discard_queued();
        }
        self.update_image_with_compositor(frame_index, timeline_frame, compositor)?;
        self.composed_frame = Some((frame_index, signature));
        Ok(())
    }

    fn set_terminal_size(&mut self, size: Size) {
        if self.terminal_size != size {
            self.terminal_size = size;
            self.composed_frame = None;
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
        self.composed_frame = None;
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

fn reset_editor_timeline(editor: &mut EditorUi, total_frames: usize) {
    editor.timeline.reset(total_frames);
    editor.project_playhead = 0;
    editor.timeline_playhead = 0;
    editor.timeline_context = None;
    editor.timeline_raster_key = None;
    if let Some(image) = &mut editor.timeline_image {
        image.empty();
    }
}

fn local_timeline_frame_count(session: &EditorSession) -> usize {
    session
        .project()
        .resolve_path(session.path())
        .ok()
        .and_then(|media_id| session.project().media(media_id))
        .and_then(|media| usize::try_from(media.duration.value).ok())
        .unwrap_or(0)
}

fn project_nominal_frames_per_second(session: &EditorSession) -> usize {
    let rate = session.project().settings().frame_rate;
    let numerator = usize::try_from(rate.numerator()).unwrap_or(1);
    let denominator = usize::try_from(rate.denominator()).unwrap_or(1).max(1);
    numerator.div_ceil(denominator).max(1)
}

fn timeline_frame_i64(frame: usize) -> i64 {
    i64::try_from(frame).unwrap_or(i64::MAX)
}

fn synchronize_project_compositor(
    editor: &mut EditorUi,
    session: &EditorSession,
    host: &EditorHost<'_>,
) -> bool {
    // Source edits are already debounced for the worker preview. Do not make the
    // project compositor synchronously parse every intermediate keystroke too.
    if editor
        .mmfx
        .as_ref()
        .is_some_and(|document| document.compile_due.is_some())
    {
        return false;
    }
    let context = monitor_context(session, editor.monitor_scope);
    let state = (session.revision(), context);
    if editor.project_compositor_state == Some(state) {
        return false;
    }
    let project_directory = session.project_file().and_then(Path::parent);
    let sync =
        editor
            .project_compositor
            .synchronize(session.project(), context, |_, source, scene| {
                let base_directory = source
                    .resource_base
                    .as_deref()
                    .or(project_directory)
                    .unwrap_or(host.base_directory);
                crate::load_mmfx_resources(scene, base_directory)
            });
    if let Some(diagnostic) = sync.diagnostics.last() {
        editor.message = format!(
            "error: MMFX {} (last valid timeline preview retained)",
            diagnostic.message
        );
    }
    editor.project_compositor_state = Some(state);
    sync.changed
}

fn update_compositor_only_monitor(
    editor: &mut EditorUi,
    session: &EditorSession,
    picker: &Picker,
) -> bool {
    let mut changed = false;
    if editor.project_compositor.has_layers() {
        let settings = session.project().settings();
        let playhead = monitor_playhead(editor);
        let key = TimelineMonitorKey {
            canvas: (settings.width, settings.height),
            scope: editor.monitor_scope,
            playhead,
            active_signature: editor
                .project_compositor
                .active_signature(timeline_frame_i64(playhead)),
        };
        if editor.timeline_monitor_key.as_ref() != Some(&key) {
            let mut composed =
                monitor_background(settings.width, settings.height, editor.monitor_scope);
            if let Err(error) = editor
                .project_compositor
                .composite_rgba8(timeline_frame_i64(playhead), &mut composed)
            {
                editor.message = format!("error: cannot compose MMFX monitor: {error}");
            }
            if let Some(buffer) = &mut editor.timeline_monitor_image {
                buffer.replace_protocol(
                    picker.new_resize_protocol(DynamicImage::ImageRgba8(composed)),
                );
            }
            editor.timeline_monitor_key = Some(key);
            changed = true;
        }
    } else if editor.timeline_monitor_key.take().is_some() {
        if let Some(image) = &mut editor.timeline_monitor_image {
            image.empty();
        }
        changed = true;
    }
    if let Some(buffer) = &mut editor.timeline_monitor_image {
        let (received, error) = buffer.poll();
        changed |= received;
        if let Some(error) = error {
            editor.message = format!("error: cannot resize MMFX timeline preview: {error}");
        }
    }
    changed
}

fn monitor_context(session: &EditorSession, scope: MonitorScope) -> MediaId {
    match scope {
        MonitorScope::Project => session.project().root_id(),
        MonitorScope::Local => session
            .project()
            .resolve_path(session.path())
            .unwrap_or_else(|_| session.project().root_id()),
    }
}

const fn monitor_playhead(editor: &EditorUi) -> usize {
    match editor.monitor_scope {
        MonitorScope::Project => editor.project_playhead,
        MonitorScope::Local => editor.timeline_playhead,
    }
}

fn monitor_uses_video(scope: MonitorScope, session: &EditorSession, app: &PreviewApp) -> bool {
    scope == MonitorScope::Project || timeline_preview_mapping(session, app).is_some()
}

fn monitor_background(width: u32, height: u32, scope: MonitorScope) -> image::RgbaImage {
    if scope == MonitorScope::Project {
        return image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
    }
    image::RgbaImage::from_fn(width, height, |x, y| {
        let light = ((x / 24) + (y / 24)).is_multiple_of(2);
        let value = if light { 58 } else { 38 };
        image::Rgba([value, value, value, 255])
    })
}

fn timeline_object_lanes(
    session: &EditorSession,
    app: Option<&PreviewApp>,
) -> Vec<TimelineObjectLane> {
    let project = session.project();
    let mut objects = Vec::new();
    if let Some(link_id) = session.path().current_link()
        && let Some(link) = project.link(link_id)
        && let Some(media) = project.media(link.media_id)
        && let Ok(end) = usize::try_from(media.duration.value)
    {
        objects.push(TimelineObjectLane {
            name: link.alias.clone(),
            kind: media.kind.as_str().to_owned(),
            frames: 0..end,
            current: true,
            preview: app.is_some_and(|app| media_matches_preview(media, app)),
        });
    }
    if let Ok(entries) = project.list(session.path()) {
        for entry in entries {
            let Some(link) = project.link(entry.link_id) else {
                continue;
            };
            let Some(media) = project.media(link.media_id) else {
                continue;
            };
            let (Ok(start), Ok(end)) = (
                usize::try_from(entry.timeline_range.start.value),
                usize::try_from(entry.timeline_range.end.value),
            ) else {
                continue;
            };
            objects.push(TimelineObjectLane {
                name: entry.alias,
                kind: entry.kind.as_str().to_owned(),
                frames: start..end,
                current: false,
                preview: app.is_some_and(|app| media_matches_preview(media, app)),
            });
        }
    }
    if app.is_some()
        && !objects.iter().any(|object| object.preview)
        && let Some(object) = objects
            .iter_mut()
            .find(|object| object.kind.starts_with("video"))
    {
        object.preview = true;
    }
    objects
}

fn timeline_preview_mapping(
    session: &EditorSession,
    app: &PreviewApp,
) -> Option<TimelinePreviewMapping> {
    let project = session.project();
    let context = project.resolve_path(session.path()).ok()?;
    for placement in mmrecode_render::flatten_project_timeline(project, context).ok()? {
        let media = project.media(placement.media_id)?;
        if !media_matches_preview(media, app) {
            continue;
        }
        return Some(TimelinePreviewMapping {
            timeline: usize::try_from(placement.timeline_range.start).ok()?
                ..usize::try_from(placement.timeline_range.end).ok()?,
            source: usize::try_from(placement.source_range.start).ok()?
                ..usize::try_from(placement.source_range.end).ok()?,
        });
    }
    None
}

fn project_preview_mapping(
    session: &EditorSession,
    app: &PreviewApp,
) -> Option<TimelinePreviewMapping> {
    let project = session.project();
    for placement in mmrecode_render::flatten_project_timeline(project, project.root_id()).ok()? {
        let media = project.media(placement.media_id)?;
        if !media_matches_preview(media, app) {
            continue;
        }
        return Some(TimelinePreviewMapping {
            timeline: usize::try_from(placement.timeline_range.start).ok()?
                ..usize::try_from(placement.timeline_range.end).ok()?,
            source: usize::try_from(placement.source_range.start).ok()?
                ..usize::try_from(placement.source_range.end).ok()?,
        });
    }
    None
}

fn current_context_project_mapping(session: &EditorSession) -> Option<TimelinePreviewMapping> {
    if session.path().links().is_empty() {
        return None;
    }
    let project = session.project();
    mmrecode_render::flatten_project_timeline(project, project.root_id())
        .ok()?
        .into_iter()
        .find(|placement| placement.link_path == session.path().links())
        .and_then(|placement| {
            Some(TimelinePreviewMapping {
                timeline: usize::try_from(placement.timeline_range.start).ok()?
                    ..usize::try_from(placement.timeline_range.end).ok()?,
                source: usize::try_from(placement.source_range.start).ok()?
                    ..usize::try_from(placement.source_range.end).ok()?,
            })
        })
}

fn project_timeline_playhead(session: &EditorSession, app: &PreviewApp) -> usize {
    let source_frame = app.playback.frame_index();
    let Some(mapping) = project_preview_mapping(session, app) else {
        return source_frame;
    };
    timeline_frame_for_source(&mapping, source_frame)
}

fn local_timeline_playhead(session: &EditorSession, project_frame: usize) -> usize {
    current_context_project_mapping(session).map_or(project_frame, |mapping| {
        source_frame_for_timeline(&mapping, project_frame)
    })
}

fn displayed_timeline_playhead(
    session: &EditorSession,
    app: &PreviewApp,
    project_frame: usize,
) -> usize {
    let focused_is_preview = session
        .project()
        .resolve_path(session.path())
        .ok()
        .and_then(|media_id| session.project().media(media_id))
        .is_some_and(|media| media_matches_preview(media, app));
    if focused_is_preview {
        app.playback.frame_index()
    } else {
        local_timeline_playhead(session, project_frame)
    }
}

fn project_frame_for_local_timeline(session: &EditorSession, local_frame: usize) -> usize {
    current_context_project_mapping(session).map_or(local_frame, |mapping| {
        timeline_frame_for_source(&mapping, local_frame)
    })
}

fn seek_local_timeline_frame(
    app: &mut PreviewApp,
    session: &EditorSession,
    local_frame: usize,
    now: Instant,
) -> Result<usize, String> {
    let project_frame = project_frame_for_local_timeline(session, local_frame);
    let focused_is_preview = session
        .project()
        .resolve_path(session.path())
        .ok()
        .and_then(|media_id| session.project().media(media_id))
        .is_some_and(|media| media_matches_preview(media, app));
    let source_frame = if focused_is_preview {
        local_frame.clamp(
            app.playback_range.start,
            app.playback_range.end.saturating_sub(1),
        )
    } else {
        project_preview_mapping(session, app).map_or(project_frame, |mapping| {
            source_frame_for_timeline(&mapping, project_frame)
        })
    };
    app.seek_frame(source_frame, now)?;
    Ok(project_timeline_playhead(session, app))
}

fn project_scrub_message(session: &EditorSession, project_frame: usize) -> String {
    format!(
        "project scrub: {}",
        project_timecode(session, project_frame)
    )
}

fn project_timecode(session: &EditorSession, project_frame: usize) -> String {
    let project = session.project();
    project
        .media(project.root_id())
        .and_then(|root| {
            i64::try_from(project_frame)
                .ok()
                .and_then(|frame| format_compact_timecode(frame, root.time_base).ok())
        })
        .unwrap_or_else(|| format!("frame {project_frame}"))
}

fn synchronize_timeline_context(editor: &mut EditorUi, session: &EditorSession) -> bool {
    let Ok(context) = session.project().resolve_path(session.path()) else {
        return false;
    };
    if editor.timeline_context == Some(context) {
        return false;
    }
    editor.timeline_context = Some(context);
    editor.timeline_playhead = local_timeline_playhead(session, editor.project_playhead);
    editor.timeline.reset(local_timeline_frame_count(session));
    editor.timeline.reveal(editor.timeline_playhead);
    editor.timeline_raster_key = None;
    true
}

fn source_frame_for_timeline(mapping: &TimelinePreviewMapping, timeline_frame: usize) -> usize {
    let timeline_length = mapping.timeline.end.saturating_sub(mapping.timeline.start);
    let source_length = mapping.source.end.saturating_sub(mapping.source.start);
    if timeline_length == 0 || source_length == 0 {
        return mapping.source.start;
    }
    let offset = timeline_frame
        .saturating_sub(mapping.timeline.start)
        .min(timeline_length.saturating_sub(1));
    let source_offset =
        usize::try_from(offset as u128 * source_length as u128 / timeline_length as u128)
            .unwrap_or(source_length.saturating_sub(1));
    mapping
        .source
        .start
        .saturating_add(source_offset)
        .min(mapping.source.end.saturating_sub(1))
}

fn timeline_frame_for_source(mapping: &TimelinePreviewMapping, source_frame: usize) -> usize {
    let timeline_length = mapping.timeline.end.saturating_sub(mapping.timeline.start);
    let source_length = mapping.source.end.saturating_sub(mapping.source.start);
    if timeline_length == 0 || source_length == 0 {
        return mapping.timeline.start;
    }
    let offset = source_frame
        .saturating_sub(mapping.source.start)
        .min(source_length.saturating_sub(1));
    let timeline_offset =
        usize::try_from(offset as u128 * timeline_length as u128 / source_length as u128)
            .unwrap_or(timeline_length.saturating_sub(1));
    mapping
        .timeline
        .start
        .saturating_add(timeline_offset)
        .min(mapping.timeline.end.saturating_sub(1))
}

fn visible_thumbnail_source_range(
    viewport: &TimelineViewport,
    mapping: Option<&TimelinePreviewMapping>,
    fallback: Range<usize>,
) -> Range<usize> {
    let Some(mapping) = mapping else {
        return fallback;
    };
    let visible = viewport.visible_range();
    let start = visible.start.max(mapping.timeline.start);
    let end = visible.end.min(mapping.timeline.end);
    let timeline_length = mapping.timeline.end.saturating_sub(mapping.timeline.start);
    let source_length = mapping.source.end.saturating_sub(mapping.source.start);
    if start >= end || timeline_length == 0 || source_length == 0 {
        return 0..0;
    }
    let start_offset = start - mapping.timeline.start;
    let end_offset = end - mapping.timeline.start;
    let source_start_offset =
        usize::try_from(start_offset as u128 * source_length as u128 / timeline_length as u128)
            .unwrap_or(source_length);
    let source_end_offset = usize::try_from(
        (end_offset as u128 * source_length as u128).div_ceil(timeline_length as u128),
    )
    .unwrap_or(source_length);
    mapping.source.start.saturating_add(source_start_offset)
        ..mapping
            .source
            .start
            .saturating_add(source_end_offset)
            .min(mapping.source.end)
}

fn media_matches_preview(media: &mmrecode_edit::MediaNode, app: &PreviewApp) -> bool {
    let preview_path = Path::new(&app.path);
    match &media.origin {
        MediaOrigin::External { path } => preview_path == path,
        MediaOrigin::Managed { path } => preview_path.ends_with(path),
        _ => false,
    }
}

fn timeline_object_labels(objects: &[TimelineObjectLane]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "OBJECTS",
        Style::default().fg(Color::DarkGray),
    )];
    for object in objects {
        let marker = if object.current { "▸ " } else { "  " };
        let style = if object.current {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::styled(format!("{marker}{}", object.name), style));
        lines.push(Line::styled(
            format!("  {}", object.kind),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::default());
    }
    lines
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

    fn discard_queued(&mut self) {
        self.queued = None;
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
        self.queued = None;
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
    }
}

impl Drop for KittyStreamer {
    fn drop(&mut self) {
        self.clean_transfer_files();
        let _ = std::fs::remove_dir(&self.temp_directory);
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
        " {} | {} | Playhead {} | Duration {} | {}/{} fps | {} ",
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

    let visible_error = app.error.as_deref().or(app.audio_error.as_deref());
    let footer_text = visible_error.map_or_else(
        || "Space play/pause   ←/→ step   Home/End seek   l loop   q quit".to_owned(),
        |error| format!("{error}   (q to quit)"),
    );
    let footer_style = if visible_error.is_some() {
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
    picker: &Picker,
) {
    let mut app = app;
    let timeline_objects = timeline_object_lanes(session, app.as_deref());
    let maximum_timeline_height = frame.area().height.saturating_mul(2) / 5;
    let timeline_height =
        u16::try_from(10_usize.saturating_add(timeline_objects.len().saturating_mul(3)))
            .unwrap_or(u16::MAX)
            .clamp(10, maximum_timeline_height.max(10));
    let [header, workspace, timeline, result, prompt] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(10),
        Constraint::Length(timeline_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [preview, context] =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)])
            .areas(workspace);
    let breadcrumb = session.prompt().unwrap_or_else(|_| "Project".into());
    let dirty = if session.is_dirty() { "*" } else { "" };
    let title = app.as_deref().map_or_else(
        || format!(" MMRecode | {breadcrumb}{dirty} | no media | editing "),
        |app| {
            let rate = app.playback.timeline().frame_rate();
            format!(
                " MMRecode | {} | Playhead {} | Duration {} | {}/{} fps | {} | {} ",
                format_args!("{breadcrumb}{dirty}"),
                project_timecode(session, editor.project_playhead),
                app.timecode(app.frame_count()),
                rate.numerator(),
                rate.denominator(),
                app.status(),
                app.protocol_label(),
            )
        },
    );
    frame.render_widget(Paragraph::new(title), header);

    let monitor_context = session
        .project()
        .display_path(session.path())
        .unwrap_or_else(|_| "/".into());
    let monitor_name = match editor.monitor_scope {
        MonitorScope::Project => "Project Monitor".into(),
        MonitorScope::Local => format!("Local Monitor — {monitor_context}"),
    };
    let monitor_title = editor
        .mmfx
        .as_ref()
        .map_or(monitor_name.clone(), |document| {
            format!(
                "{monitor_name} — editing MMFX {}{}",
                document.display_name(),
                if session.is_dirty() { "*" } else { "" }
            )
        });
    let image_block = Block::default().borders(Borders::ALL).title(monitor_title);
    let image_area = image_block.inner(preview);
    if let Some(app) = app.as_deref_mut() {
        app.preview_area = image_area;
    }
    frame.render_widget(image_block, preview);
    let monitor_video = app
        .as_deref()
        .is_some_and(|app| monitor_uses_video(editor.monitor_scope, session, app));
    if let Some(image_state) = monitor_video
        .then(|| app.as_deref_mut().and_then(|app| app.image_state.as_mut()))
        .flatten()
    {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Fit(Some(FilterType::Triangle))),
            image_area,
            image_state,
        );
    } else if monitor_video && app.as_deref().is_some_and(|app| app.image_frame.is_none()) {
        frame.render_widget(
            Paragraph::new("Decoding edited frame…")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Yellow)),
            image_area,
        );
    } else if !monitor_video && editor.project_compositor.has_layers() {
        if let Some(state) = &mut editor.timeline_monitor_image {
            state.render(frame, image_area);
        }
    } else if editor.mmfx.is_some() {
        if let Some(state) = &mut editor.mmfx_image {
            state.render(frame, image_area);
        }
        if editor
            .mmfx
            .as_ref()
            .is_some_and(|document| document.last_good_revision.is_none())
        {
            let status = editor
                .mmfx
                .as_ref()
                .map_or("Compiling MMFX preview…", |document| {
                    document.compile_status.as_str()
                });
            frame.render_widget(
                Paragraph::new(status)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(if status.starts_with("error:") {
                        Color::Red
                    } else {
                        Color::Yellow
                    })),
                image_area,
            );
        }
    } else if app.is_none() {
        frame.render_widget(
            Paragraph::new(
                "No preview open\n\nimport <media-file> [as <alias>]\nor add scene <name> <duration>\n\nType help for commands",
            )
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
            image_area,
        );
    }

    editor.inspector_area = context;
    let show_mmfx_code = editor.mmfx.is_some()
        && editor.inspector_focus == InspectorFocus::Mmfx
        && editor.pane_focus != EditorPaneFocus::Inspector;
    if show_mmfx_code {
        draw_mmfx_source_editor(frame, context, session, editor);
    } else {
        editor.code_area = Rect::default();
        let inspector_focused = editor.pane_focus == EditorPaneFocus::Inspector;
        let inspector_title = format!(
            "{}{}",
            if inspector_focused { "▶ " } else { "" },
            editor_context_title(session, editor)
        );
        let inspector_text = editor_context_text(app.as_deref(), session, editor);
        let inspector_content = if editor.inspector_focus == InspectorFocus::Help {
            quick_help_rich_text(session)
        } else {
            Text::raw(inspector_text.as_str())
        };
        let inspector = Paragraph::new(inspector_content)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if inspector_focused {
                        Color::Cyan
                    } else {
                        Color::Gray
                    }))
                    .title(inspector_title),
            );
        let inspector_width = context.width.saturating_sub(2);
        let inspector_height = context.height.saturating_sub(2);
        editor.inspector_max_scroll = u16::try_from(
            wrapped_text_line_count(&inspector_text, inspector_width)
                .saturating_sub(usize::from(inspector_height)),
        )
        .unwrap_or(u16::MAX);
        editor.inspector_scroll = editor.inspector_scroll.min(editor.inspector_max_scroll);
        frame.render_widget(inspector.scroll((editor.inspector_scroll, 0)), context);
    }

    let local_frame_count = local_timeline_frame_count(session);
    editor.timeline.sync_total_frames(local_frame_count);
    editor.timeline_playhead = editor
        .timeline_playhead
        .min(local_frame_count.saturating_sub(1));
    let timeline_label = if editor.pane_focus == EditorPaneFocus::Timeline {
        "▶ Timeline"
    } else {
        "Timeline"
    };
    let timeline_title = if app.is_some() {
        let mode = if editor.timeline.is_fitted() {
            "fit"
        } else {
            "zoom"
        };
        let visible = editor.timeline.visible_range();
        let view = app.as_deref().map_or_else(
            || "—".into(),
            |app| {
                format!(
                    "{}..{}",
                    app.timecode(visible.start),
                    app.timecode(visible.end)
                )
            },
        );
        let playhead = project_timecode(session, editor.project_playhead);
        format!(" {timeline_label} • {breadcrumb} • Playhead {playhead} • {mode} {view} ")
    } else if timeline_objects.is_empty() {
        format!(" {timeline_label} • {breadcrumb} • empty project ")
    } else {
        let mode = if editor.timeline.is_fitted() {
            "fit"
        } else {
            "zoom"
        };
        format!(
            " {timeline_label} • {breadcrumb} • Frame {} • {mode} • {} object{} ",
            editor.timeline_playhead,
            timeline_objects.len(),
            if timeline_objects.len() == 1 { "" } else { "s" }
        )
    };
    let timeline_block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default().fg(if editor.pane_focus == EditorPaneFocus::Timeline {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        )
        .title(timeline_title);
    let timeline_inner = timeline_block.inner(timeline);
    frame.render_widget(timeline_block, timeline);
    if let Some(app) = app.as_deref_mut() {
        let [raster_area, legend_area] =
            Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(timeline_inner);
        let label_width = raster_area.width.saturating_sub(1).min(20);
        let [labels_area, image_area] =
            Layout::horizontal([Constraint::Length(label_width), Constraint::Min(1)])
                .areas(raster_area);
        editor.timeline_area = image_area;
        frame.render_widget(
            Paragraph::new(timeline_object_labels(&timeline_objects)),
            labels_area,
        );
        let font_size = app.picker.font_size();
        let (pixel_width, pixel_height) = timeline_pixel_dimensions(image_area, font_size);
        let preview_mapping = timeline_preview_mapping(session, app);
        let thumbnail_range = visible_thumbnail_source_range(
            &editor.timeline,
            preview_mapping.as_ref(),
            app.playback_range.clone(),
        );
        app.request_timeline_thumbnails(thumbnail_range, pixel_width);
        let smart_render = app.smart_render_spans(session);
        let key = TimelineRasterKey {
            width: pixel_width,
            height: pixel_height,
            visible: editor.timeline.visible_range(),
            retained: app.playback_range.clone(),
            playhead: editor.timeline_playhead,
            thumbnail_revision: app.thumbnail_revision(),
            thumbnail_frames: app.timeline_thumbnail_frames.clone(),
            smart_render: smart_render.clone(),
            objects: timeline_objects.clone(),
        };
        if editor.timeline_raster_key.as_ref() != Some(&key) {
            let pictures = app
                .source
                .timeline_pictures(editor.timeline.visible_range());
            let image = render_timeline(
                &TimelineRasterInput {
                    viewport: &editor.timeline,
                    playhead: editor.timeline_playhead,
                    retained: app.playback_range.clone(),
                    thumbnail_frames: &app.timeline_thumbnail_frames,
                    thumbnails: &app.thumbnails,
                    pictures: &pictures,
                    smart_render: &smart_render,
                    objects: &timeline_objects,
                    ruler_height: u32::from(font_size.height),
                    object_row_height: u32::from(font_size.height).saturating_mul(3),
                },
                pixel_width,
                pixel_height,
            );
            if let Some(state) = &mut editor.timeline_image {
                state.replace_protocol(
                    app.picker
                        .new_resize_protocol(DynamicImage::ImageRgb8(image)),
                );
            }
            editor.timeline_raster_key = Some(key);
        }
        if let Some(state) = &mut editor.timeline_image {
            state.render(frame, image_area);
        }
        frame.render_widget(Paragraph::new(timeline_legend(app)), legend_area);
    } else {
        let raster_area = timeline_inner;
        let label_width = raster_area.width.saturating_sub(1).min(20);
        let [labels_area, image_area] =
            Layout::horizontal([Constraint::Length(label_width), Constraint::Min(1)])
                .areas(raster_area);
        editor.timeline_area = image_area;
        frame.render_widget(
            Paragraph::new(timeline_object_labels(&timeline_objects)),
            labels_area,
        );
        let font_size = picker.font_size();
        let (pixel_width, pixel_height) = timeline_pixel_dimensions(image_area, font_size);
        let retained = 0..local_timeline_frame_count(session);
        let key = TimelineRasterKey {
            width: pixel_width,
            height: pixel_height,
            visible: editor.timeline.visible_range(),
            retained: retained.clone(),
            playhead: editor.timeline_playhead,
            thumbnail_revision: 0,
            thumbnail_frames: Vec::new(),
            smart_render: Vec::new(),
            objects: timeline_objects.clone(),
        };
        if editor.timeline_raster_key.as_ref() != Some(&key) {
            let image = render_timeline(
                &TimelineRasterInput {
                    viewport: &editor.timeline,
                    playhead: editor.timeline_playhead,
                    retained,
                    thumbnail_frames: &[],
                    thumbnails: &BTreeMap::new(),
                    pictures: &[],
                    smart_render: &[],
                    objects: &timeline_objects,
                    ruler_height: u32::from(font_size.height),
                    object_row_height: u32::from(font_size.height).saturating_mul(3),
                },
                pixel_width,
                pixel_height,
            );
            if let Some(state) = &mut editor.timeline_image {
                state.replace_protocol(picker.new_resize_protocol(DynamicImage::ImageRgb8(image)));
            }
            editor.timeline_raster_key = Some(key);
        }
        if let Some(state) = &mut editor.timeline_image {
            state.render(frame, image_area);
        }
    }

    let app_error = app.as_deref().and_then(|app| app.error.as_deref());
    let message = app_error.unwrap_or(&editor.message);
    let (marker, message, status_style) = if app_error.is_some() {
        (" × ", message, Style::default().fg(Color::Red))
    } else if let Some(message) = message.strip_prefix("error: ") {
        (" × ", message, Style::default().fg(Color::Red))
    } else if let Some(message) = message.strip_prefix("ok: ") {
        (" ✓ ", message, Style::default().fg(Color::Green))
    } else {
        (" · ", message, Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(marker, status_style.add_modifier(Modifier::BOLD)),
            Span::raw(message),
        ])),
        result,
    );
    editor.prompt_area = prompt;
    let command_focused = editor.pane_focus == EditorPaneFocus::Command;
    let prompt_prefix = format!(
        "{} {breadcrumb} > ",
        if command_focused { "▶" } else { " " }
    );
    frame.render_widget(
        Paragraph::new(format!("{prompt_prefix}{}", editor.input)).style(Style::default().fg(
            if command_focused {
                Color::White
            } else {
                Color::DarkGray
            },
        )),
        prompt,
    );
    let cursor_x = prompt
        .x
        .saturating_add(u16::try_from(prompt_prefix.chars().count()).unwrap_or(u16::MAX))
        .saturating_add(u16::try_from(editor.input.chars().count()).unwrap_or(u16::MAX))
        .min(prompt.right().saturating_sub(1));
    if command_focused {
        editor.cursor_position = (cursor_x, prompt.y);
    }
    if matches!(
        editor.pane_focus,
        EditorPaneFocus::Command | EditorPaneFocus::Code
    ) && let Some(cell) = frame.buffer_mut().cell_mut(editor.cursor_position)
    {
        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
    }
}

fn draw_mmfx_source_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &EditorSession,
    editor: &mut EditorUi,
) {
    let Some(document) = editor.mmfx.as_mut() else {
        return;
    };
    let focused = editor.pane_focus == EditorPaneFocus::Code;
    let (cursor_line, cursor_column) = mmfx_cursor_line_column(&document.source, document.cursor);
    let title = format!(
        "{}MMFX — {}{} — {}:{}",
        if focused { "▶ " } else { "" },
        document.display_name(),
        if session.is_dirty() { "*" } else { "" },
        cursor_line + 1,
        cursor_column + 1,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { Color::Cyan } else { Color::Gray }))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = usize::from(inner.height.max(1));
    if cursor_line < document.scroll {
        document.scroll = cursor_line;
    } else if cursor_line >= document.scroll.saturating_add(visible_height) {
        document.scroll = cursor_line.saturating_add(1).saturating_sub(visible_height);
    }
    let line_count = document.source.split('\n').count().max(1);
    let gutter_width = u16::try_from(line_count.to_string().len().saturating_add(1))
        .unwrap_or(8)
        .min(inner.width);
    let [gutter, source_area] =
        Layout::horizontal([Constraint::Length(gutter_width), Constraint::Min(1)]).areas(inner);
    let source_width = usize::from(source_area.width.max(1));
    if cursor_column < document.column_scroll {
        document.column_scroll = cursor_column;
    } else if cursor_column >= document.column_scroll.saturating_add(source_width) {
        document.column_scroll = cursor_column.saturating_add(1).saturating_sub(source_width);
    }

    let gutter_lines = (0..line_count)
        .map(|line| {
            Line::from(Span::styled(
                format!(
                    "{:>width$} ",
                    line + 1,
                    width = usize::from(gutter_width.saturating_sub(1))
                ),
                Style::default().fg(if line == cursor_line {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(gutter_lines)
            .scroll((u16::try_from(document.scroll).unwrap_or(u16::MAX), 0)),
        gutter,
    );
    let source_lines = document
        .source
        .split('\n')
        .enumerate()
        .map(|(line, text)| {
            Line::from(Span::styled(
                text.to_owned(),
                if line == cursor_line {
                    Style::default().bg(Color::Rgb(31, 39, 51))
                } else {
                    Style::default()
                },
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(source_lines).scroll((
            u16::try_from(document.scroll).unwrap_or(u16::MAX),
            u16::try_from(document.column_scroll).unwrap_or(u16::MAX),
        )),
        source_area,
    );
    editor.code_area = source_area;
    if focused {
        let cursor_x = source_area
            .x
            .saturating_add(
                u16::try_from(cursor_column.saturating_sub(document.column_scroll))
                    .unwrap_or(u16::MAX),
            )
            .min(source_area.right().saturating_sub(1));
        let cursor_y = source_area
            .y
            .saturating_add(
                u16::try_from(cursor_line.saturating_sub(document.scroll)).unwrap_or(u16::MAX),
            )
            .min(source_area.bottom().saturating_sub(1));
        editor.cursor_position = (cursor_x, cursor_y);
    }
}

fn wrapped_text_line_count(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum()
}

fn timeline_pixel_dimensions(area: Rect, font: ratatui_image::FontSize) -> (u32, u32) {
    (
        u32::from(area.width)
            .saturating_mul(u32::from(font.width))
            .max(1),
        u32::from(area.height)
            .saturating_mul(u32::from(font.height))
            .max(1),
    )
}

fn timeline_legend(app: &PreviewApp) -> Line<'static> {
    let mut spans = vec![
        Span::styled(" ■ ", Style::default().fg(Color::Green)),
        Span::raw("copy"),
        Span::styled("  ■ ", Style::default().fg(Color::Yellow)),
        Span::raw("bridge"),
        Span::styled("  ■ ", Style::default().fg(Color::Red)),
        Span::raw("render"),
        Span::styled("  ■ ", Style::default().fg(Color::DarkGray)),
        Span::raw("review"),
        Span::styled("   I ", Style::default().fg(Color::Yellow)),
        Span::styled("P ", Style::default().fg(Color::Blue)),
        Span::styled("B", Style::default().fg(Color::Magenta)),
    ];
    if app.thumbnail_error.is_some() {
        spans.push(Span::styled(
            "   thumbnail error",
            Style::default().fg(Color::Red),
        ));
    }
    if app.audio_error.is_some() {
        spans.push(Span::styled(
            "   audio unavailable",
            Style::default().fg(Color::Red),
        ));
    } else if let Some(aac) = &app.aac {
        spans.push(Span::raw(match aac.backend {
            Some(AacDecodeBackend::Native) => "   AAC: native Rust",
            Some(AacDecodeBackend::External) => "   AAC: FFmpeg fallback",
            None => "   AAC: decoding",
        }));
    }
    Line::from(spans)
}

fn editor_context_title(session: &EditorSession, editor: &EditorUi) -> String {
    match editor.inspector_focus {
        InspectorFocus::InPoint => "Inspector — In point".into(),
        InspectorFocus::OutPoint => "Inspector — Out point".into(),
        InspectorFocus::Help => format!(
            "Help — {}",
            session
                .project()
                .display_path(session.path())
                .unwrap_or_else(|_| "/".into())
        ),
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
        InspectorFocus::Mmfx => "MMFX Source".into(),
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
    if matches!(editor.inspector_focus, InspectorFocus::Mmfx) {
        return editor.mmfx.as_ref().map_or_else(
            || "No MMFX source pane is open.\n\nCreate one with `add scene <name> <duration>`, focus it with `cd <name>`, then use `edit`.".into(),
            |document| {
                format!(
                    "MMFX SCENE SOURCE\n\nObject     {}\nOwnership  embedded in project\nResources  {}\nProject    {}\nPreview    {}\n\n`save` persists the project and source.\n`scene save as <file>` extracts a copy.\nUse `man scene` for the complete workflow.",
                    document.name,
                    document.resource_base.as_ref().map_or_else(
                        || "project directory".into(),
                        |path| path.display().to_string()
                    ),
                    if session.is_dirty() { "modified" } else { "saved" },
                    document.compile_status,
                )
            },
        );
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

    if let Some(mmfx) = &media.mmfx {
        let _ = write!(
            text,
            "\nMMFX       embedded ({} bytes)\nResources  {}",
            mmfx.source.len(),
            mmfx.resource_base.as_ref().map_or_else(
                || "project directory".into(),
                |path| path.display().to_string()
            )
        );
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
        text.push_str("\n\nAvailable here\nimport <file>   add scene <name> <duration>   save   export plan\nproject info|match|preset|set   ls   cd <alias>");
    } else if media.kind.is_mmfx_scene() {
        text.push_str("\n\nAvailable here\nedit   scene load <scene.mmfx>   scene save as <file>\nscene close   save   cd ..   help   man edit");
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
    let local = if session.path().current_link().is_some() {
        "CURRENT OBJECT\n  left <time>   right <time>        Adjust trim"
    } else {
        "CURRENT LEVEL\n  import adds media at the project root"
    };
    format!(
        "PROJECT\n  new <name> [using <preset>]\n  open <project>   save [as <project>]\n  project info | match | presets | preset | set\n\nHIERARCHY & MEDIA\n  import <file> [as <name>]\n  pwd   ls   cd <path>\n  in <time>   out <time>   scale <mode>\n\nMMFX SCENES\n  add scene <name> <duration> [at <start>]\n  edit   scene load <scene.mmfx>\n  scene save as <file>   scene close\n  monitor project|local|toggle\n\nOUTPUT & HISTORY\n  export plan   export <file>\n  undo   redo   help   man <cmd>   quit\n\n{local}\n\nKEYS\n  Tab / Shift-Tab                 Move focus\n  Ctrl-Space                      Complete / play\n  Ctrl-S                          Save project\n  Ctrl-Z / Ctrl-Y                 Undo / redo\n  Inspector ↑/↓/PgUp/PgDn        Scroll\n  Timeline +/-   Shift-←/→   0    Zoom / pan / fit\n\nTime: S:FF or M:SS:FF"
    )
}

#[allow(clippy::too_many_lines)]
fn quick_help_rich_text(session: &EditorSession) -> Text<'static> {
    let section = |name: &'static str| -> Line<'static> {
        Line::from(Span::styled(
            name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let command = |name: &'static str| -> Span<'static> {
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD))
    };
    let args = |value: &'static str| -> Span<'static> {
        Span::styled(value, Style::default().fg(Color::Gray))
    };
    let description = |value: &'static str| -> Span<'static> {
        Span::styled(value, Style::default().fg(Color::DarkGray))
    };

    let mut lines = vec![
        section("PROJECT"),
        Line::from(vec![
            Span::raw("  "),
            command("new"),
            args(" <name> [using <preset>]"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("open"),
            args(" <project>   "),
            command("save"),
            args(" [as <project>]"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("project"),
            args(" info | match | presets | preset | set"),
        ]),
        Line::default(),
        section("HIERARCHY & MEDIA"),
        Line::from(vec![
            Span::raw("  "),
            command("import"),
            args(" <file> [as <name>]"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("pwd"),
            Span::raw("   "),
            command("ls"),
            Span::raw("   "),
            command("cd"),
            args(" <path>"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("in"),
            args(" <time>   "),
            command("out"),
            args(" <time>   "),
            command("scale"),
            args(" <mode>"),
        ]),
        Line::default(),
        section("MMFX SCENES"),
        Line::from(vec![
            Span::raw("  "),
            command("add scene"),
            args(" <name> <duration> [at <start>]"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("edit"),
            Span::raw("   "),
            command("scene load"),
            args(" <scene.mmfx>"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("scene save as"),
            args(" <file>   "),
            command("scene close"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("monitor"),
            args(" project | local | toggle"),
        ]),
        Line::default(),
        section("OUTPUT & HISTORY"),
        Line::from(vec![
            Span::raw("  "),
            command("export plan"),
            Span::raw("   "),
            command("export"),
            args(" <file>"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("undo"),
            Span::raw("   "),
            command("redo"),
            Span::raw("   "),
            command("help"),
            Span::raw("   "),
            command("man"),
            args(" <cmd>   "),
            command("quit"),
        ]),
        Line::default(),
    ];

    if session.path().current_link().is_some() {
        lines.extend([
            section("CURRENT OBJECT"),
            Line::from(vec![
                Span::raw("  "),
                command("left"),
                args(" <time>   "),
                command("right"),
                args(" <time>        "),
                description("Adjust trim"),
            ]),
        ]);
    } else {
        lines.extend([
            section("CURRENT LEVEL"),
            Line::from(vec![
                Span::raw("  "),
                command("import"),
                description(" adds media at the project root"),
            ]),
        ]);
    }

    lines.extend([
        Line::default(),
        section("KEYS"),
        Line::from(vec![
            Span::raw("  "),
            command("Tab / Shift-Tab"),
            description("                 Move focus"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("Ctrl-Space"),
            description("                      Complete / play"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("Ctrl-S"),
            description("                          Save project"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            command("Ctrl-Z / Ctrl-Y"),
            description("                 Undo / redo"),
        ]),
        Line::from(vec![
            Span::raw("  Inspector "),
            command("↑/↓/PgUp/PgDn"),
            description("        Scroll"),
        ]),
        Line::from(vec![
            Span::raw("  Timeline "),
            command("+/-"),
            Span::raw("   "),
            command("Shift-←/→"),
            Span::raw("   "),
            command("0"),
            description("    Zoom / pan / fit"),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled("Time: ", Style::default().add_modifier(Modifier::BOLD)),
            args("S:FF or M:SS:FF"),
        ]),
    ]);

    Text::from(lines)
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
    match &app.source {
        PreviewSource::Mpeg2(source) => {
            let Some(frame) = source
                .index()
                .frames()
                .get(app.playback.frame_index())
                .or_else(|| source.index().frames().first())
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
        PreviewSource::H264(source) => {
            let Some(frame) = source
                .index()
                .frames()
                .get(app.playback.frame_index())
                .or_else(|| source.index().frames().first())
            else {
                return;
            };
            let index = source.index();
            let scan = if index.is_progressive() {
                "progressive"
            } else {
                "interlaced/mixed"
            };
            let _ = write!(
                text,
                "\n\nVideo\nCodec      H.264/AVC\nDisplay    {}×{}\nScan       {scan}\nRate       {}/{} fps\nPicture    {:?}\nIDR        {}\nReference  {}\nPTS / DTS  {} / {}",
                index.display_width(),
                index.display_height(),
                index.frame_rate().numerator(),
                index.frame_rate().denominator(),
                frame.picture_type,
                frame.is_idr,
                frame.is_reference,
                frame.pts,
                frame.dts,
            );
        }
    }
}

fn timecode_or_unknown(frame: i64, time_base: Rational) -> String {
    format_compact_timecode(frame, time_base).unwrap_or_else(|_| "?:??".into())
}

#[cfg(test)]
fn editor_timeline_text(
    app: Option<&PreviewApp>,
    timeline: &TimelineViewport,
    width: u16,
    objects: &[TimelineObjectLane],
) -> String {
    let Some(app) = app else {
        let width = usize::from(width);
        if objects.is_empty() {
            return format!(
                "{}\n{}\n{}\n\nType import <media-file> or add scene <name> <duration>.",
                timeline_label_row(None, width),
                timeline_ruler(width),
                "░".repeat(width),
            );
        }
        let mut lines = vec![timeline_label_row(None, width), timeline_ruler(width)];
        for object in objects {
            lines.push(format!(
                "{}{} [{}]",
                if object.current { "▸ " } else { "  " },
                object.name,
                object.kind
            ));
            lines.push(timeline_object_text_bar(object, timeline, width));
        }
        lines.push("cd <name> focuses an object • edit opens a focused MMFX scene".into());
        return lines.join("\n");
    };
    let current = app.playback.frame_index();
    format!(
        "{}\n{}\n{}\n{}\nIN {}   PLAY {}   OUT {}   VIEW {}..{}",
        timeline_label_row(Some((app, timeline)), usize::from(width)),
        timeline_ruler(usize::from(width)),
        timeline_bar(app, timeline, usize::from(width)),
        timeline_picture_row(app, timeline, usize::from(width)),
        app.timecode(app.playback_range.start),
        app.timecode(current),
        app.timecode(app.playback_range.end),
        app.timecode(timeline.visible_range().start),
        app.timecode(timeline.visible_range().end),
    )
}

#[cfg(test)]
fn timeline_object_text_bar(
    object: &TimelineObjectLane,
    timeline: &TimelineViewport,
    width: usize,
) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = (0..width)
        .map(|column| {
            let frame = timeline.frame_at_column(column, width);
            if object.frames.contains(&frame) {
                '━'
            } else {
                '·'
            }
        })
        .collect::<Vec<_>>();
    if timeline.visible_range().contains(&object.frames.start) {
        let start = timeline.column_for_frame(object.frames.start, width);
        cells[start] = '┣';
    }
    if let Some(end) = object.frames.end.checked_sub(1)
        && timeline.visible_range().contains(&end)
    {
        let end = timeline.column_for_frame(end, width);
        cells[end] = '┫';
    }
    cells.into_iter().collect()
}

#[cfg(test)]
fn timeline_label_row(app: Option<(&PreviewApp, &TimelineViewport)>, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec![' '; width];
    let labels = app.map_or_else(
        || vec![(0, "PROJECT".into())],
        |(app, timeline)| {
            let range = timeline.visible_range();
            let middle = range.start + timeline.visible_frame_count() / 2;
            vec![
                (0, app.timecode(range.start)),
                (width / 2, app.timecode(middle)),
                (width.saturating_sub(1), app.timecode(range.end)),
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

#[cfg(test)]
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

#[cfg(test)]
fn timeline_picture_row(app: &PreviewApp, timeline: &TimelineViewport, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut cells = vec![' '; width];
    for index in timeline.visible_range() {
        if app.source.is_intra_frame(index) {
            let column = timeline.column_for_frame(index, width);
            cells[column] = 'I';
        }
    }
    if timeline
        .visible_range()
        .contains(&app.playback.frame_index())
    {
        let current = timeline.column_for_frame(app.playback.frame_index(), width);
        cells[current] = '▲';
    }
    cells.into_iter().collect()
}

#[cfg(test)]
fn timeline_bar(app: &PreviewApp, timeline: &TimelineViewport, width: usize) -> String {
    if width == 0 || app.frame_count() == 0 {
        return String::new();
    }
    let mut cells = (0..width)
        .map(|column| {
            let frame = timeline.frame_at_column(column, width);
            if app.playback_range.contains(&frame) {
                '━'
            } else {
                '·'
            }
        })
        .collect::<Vec<_>>();
    if timeline.visible_range().contains(&app.playback_range.start) {
        let start = timeline.column_for_frame(app.playback_range.start, width);
        cells[start] = '┣';
    }
    if timeline
        .visible_range()
        .contains(&(app.playback_range.end - 1))
    {
        let end = timeline.column_for_frame(app.playback_range.end - 1, width);
        cells[end] = '┫';
    }
    if timeline
        .visible_range()
        .contains(&app.playback.frame_index())
    {
        let current = timeline.column_for_frame(app.playback.frame_index(), width);
        cells[current] = '◆';
    }
    cells.into_iter().collect()
}

fn monitor_pixel_bounds(
    area: Rect,
    font: ratatui_image::FontSize,
    source: (usize, usize),
    kitty: bool,
) -> (u32, u32) {
    let width = u32::from(area.width.max(1)) * u32::from(font.width);
    let height = u32::from(area.height.max(1)) * u32::from(font.height);
    let protocol_cap = if kitty {
        (u32::MAX, u32::MAX)
    } else {
        (1_280, 720)
    };
    let source_width = u32::try_from(source.0).unwrap_or(u32::MAX);
    let source_height = u32::try_from(source.1).unwrap_or(u32::MAX);
    (
        width.min(source_width).min(protocol_cap.0).max(1),
        height.min(source_height).min(protocol_cap.1).max(1),
    )
}

fn video_frame_image(frame: &VideoFrame, bounds: (u32, u32)) -> Result<DynamicImage, String> {
    validate_frame(frame)?;
    let (width, height) = fitted_dimensions(frame.width, frame.height, bounds)?;
    let width_usize = width as usize;
    let height_usize = height as usize;
    let source_x = (0..width_usize)
        .map(|x| x.saturating_mul(frame.width) / width_usize)
        .collect::<Vec<_>>();
    let source_y = (0..height_usize)
        .map(|y| y.saturating_mul(frame.height) / height_usize)
        .collect::<Vec<_>>();
    let mut pixels = vec![0_u8; width_usize.saturating_mul(height_usize).saturating_mul(3)];
    match frame.format {
        PixelFormat::Gray8 => {
            let plane = &frame.planes[0];
            for (target_y, &source_y) in source_y.iter().enumerate() {
                let source_row = source_y * plane.stride;
                let output_row = target_y * width_usize * 3;
                for (target_x, &source_x) in source_x.iter().enumerate() {
                    let value = plane.data[source_row + source_x];
                    let output = output_row + target_x * 3;
                    pixels[output..output + 3].fill(value);
                }
            }
        }
        PixelFormat::Yuv420p8
        | PixelFormat::Yuv411p8
        | PixelFormat::Yuv422p8
        | PixelFormat::Yuv444p8 => {
            let luma = &frame.planes[0];
            let blue_chroma = &frame.planes[1];
            let red_chroma = &frame.planes[2];
            let blue_chroma_x = source_x
                .iter()
                .map(|&x| (x * blue_chroma.width / frame.width).min(blue_chroma.width - 1))
                .collect::<Vec<_>>();
            let red_chroma_x = source_x
                .iter()
                .map(|&x| (x * red_chroma.width / frame.width).min(red_chroma.width - 1))
                .collect::<Vec<_>>();
            for (target_y, &source_y) in source_y.iter().enumerate() {
                let luma_row =
                    (source_y * luma.height / frame.height).min(luma.height - 1) * luma.stride;
                let blue_chroma_row = (source_y * blue_chroma.height / frame.height)
                    .min(blue_chroma.height - 1)
                    * blue_chroma.stride;
                let red_chroma_row = (source_y * red_chroma.height / frame.height)
                    .min(red_chroma.height - 1)
                    * red_chroma.stride;
                let output_row = target_y * width_usize * 3;
                for target_x in 0..width_usize {
                    let rgb = ycbcr_to_rgb(
                        luma.data[luma_row + source_x[target_x]],
                        blue_chroma.data[blue_chroma_row + blue_chroma_x[target_x]],
                        red_chroma.data[red_chroma_row + red_chroma_x[target_x]],
                        frame.color.range,
                    );
                    let output = output_row + target_x * 3;
                    pixels[output..output + 3].copy_from_slice(&rgb);
                }
            }
        }
        PixelFormat::Rgb24 => {
            let plane = &frame.planes[0];
            for (target_y, &source_y) in source_y.iter().enumerate() {
                let source_row = source_y * plane.stride;
                let output_row = target_y * width_usize * 3;
                for (target_x, &source_x) in source_x.iter().enumerate() {
                    let source = source_row + source_x * 3;
                    let output = output_row + target_x * 3;
                    pixels[output..output + 3].copy_from_slice(&plane.data[source..source + 3]);
                }
            }
        }
        _ => unreachable!("validated terminal preview pixel format"),
    }
    let image = RgbImage::from_raw(width, height, pixels)
        .ok_or_else(|| "terminal preview RGB buffer has invalid dimensions".to_owned())?;
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

fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8, range: ColorRange) -> [u8; 3] {
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    let (red, green, blue) = if range == ColorRange::Limited {
        let y = (i32::from(y) - 16).max(0);
        (
            (298 * y + 409 * cr + 128) >> 8,
            (298 * y - 100 * cb - 208 * cr + 128) >> 8,
            (298 * y + 516 * cb + 128) >> 8,
        )
    } else {
        let y = i32::from(y);
        (
            y + ((359 * cr + 128) >> 8),
            y - ((88 * cb + 183 * cr + 128) >> 8),
            y + ((454 * cb + 128) >> 8),
        )
    };
    [
        u8::try_from(red.clamp(0, 255)).expect("clamped preview red"),
        u8::try_from(green.clamp(0, 255)).expect("clamped preview green"),
        u8::try_from(blue.clamp(0, 255)).expect("clamped preview blue"),
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
    fn pointer_motion_does_not_steal_mmfx_source_focus() {
        let mut editor = EditorUi {
            pane_focus: EditorPaneFocus::Code,
            inspector_focus: InspectorFocus::Mmfx,
            code_area: Rect::new(40, 2, 30, 20),
            inspector_area: Rect::new(39, 1, 32, 22),
            ..EditorUi::default()
        };
        focus_editor_pane_from_mouse(
            &mut editor,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 39,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(editor.pane_focus, EditorPaneFocus::Code);
        assert_eq!(editor.inspector_focus, InspectorFocus::Mmfx);
    }

    #[test]
    fn clearing_kitty_keeps_its_transfer_directory_reusable() {
        let mut streamer = KittyStreamer::new().unwrap();
        let directory = streamer.temp_directory.clone();
        let first = streamer.write_transfer_file(&[1, 2, 3]).unwrap();
        assert!(first.is_file());
        streamer.clear().unwrap();
        assert!(directory.is_dir());
        assert!(!first.exists());
        let second = streamer.write_transfer_file(&[4, 5, 6]).unwrap();
        assert!(second.is_file());
        drop(streamer);
        assert!(!directory.exists());
    }

    #[test]
    fn coalesces_each_burst_of_queued_scrub_events_to_its_latest_position() {
        let drag = |column| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column,
                row: 10,
                modifiers: KeyModifiers::NONE,
            })
        };
        let events = coalesce_editor_events(vec![
            drag(10),
            drag(20),
            drag(30),
            Event::Resize(80, 24),
            drag(40),
            drag(50),
        ]);

        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            Event::Mouse(MouseEvent { column: 30, .. })
        ));
        assert!(matches!(events[1], Event::Resize(80, 24)));
        assert!(matches!(
            events[2],
            Event::Mouse(MouseEvent { column: 50, .. })
        ));
    }

    #[test]
    fn timeline_rows_follow_the_current_hierarchy_level() {
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 30).unwrap()).unwrap());
        session
            .add_imported_media(&ImportedMedia {
                name: "clip.m2v".into(),
                alias: "Clip0".into(),
                kind: MediaKind::new("video/mpeg2").unwrap(),
                time_base: Rational::new(1, 30).unwrap(),
                duration: 120,
                origin: MediaOrigin::External {
                    path: PathBuf::from("clip.m2v"),
                },
            })
            .unwrap();

        let root = timeline_object_lanes(&session, None);
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "Clip0");
        assert!(!root[0].current);

        session
            .apply(EditCommand::Cd {
                path: "Clip0".into(),
            })
            .unwrap();
        let add = mmrecode_edit::parse_command("add text Title 0:20 at 0:10")
            .unwrap()
            .unwrap();
        session.apply(add).unwrap();

        let clip = timeline_object_lanes(&session, None);
        assert_eq!(
            clip.iter()
                .map(|object| object.name.as_str())
                .collect::<Vec<_>>(),
            ["Clip0", "Title"]
        );
        assert!(clip[0].current);
        assert!(!clip[1].current);
        assert_eq!(local_timeline_frame_count(&session), 120);
    }

    #[test]
    fn timeline_zoom_maps_to_a_narrower_thumbnail_source_window() {
        let mapping = TimelinePreviewMapping {
            timeline: 100..200,
            source: 20..70,
        };
        let mut viewport = TimelineViewport::default();
        viewport.reset(300);
        viewport.zoom_around_frame(150, TimelineZoom::In);
        assert_eq!(
            visible_thumbnail_source_range(&viewport, Some(&mapping), 0..300),
            20..70
        );
        viewport.zoom_around_frame(150, TimelineZoom::In);
        assert_eq!(
            visible_thumbnail_source_range(&viewport, Some(&mapping), 0..300),
            26..64
        );
    }

    #[test]
    fn completed_thumbnail_does_not_restart_the_remaining_batch() {
        let (command_tx, command_rx) = mpsc::channel();
        let (_result_tx, result_rx) = mpsc::channel();
        let mut thumbnailer = TimelineThumbnailer {
            commands: Some(command_tx),
            results: result_rx,
            worker: None,
            pending: BTreeSet::new(),
        };
        thumbnailer.request(vec![10, 20, 30]);
        assert!(matches!(
            command_rx.try_recv(),
            Ok(ThumbnailCommand::Request(frames)) if frames == vec![10, 20, 30]
        ));
        thumbnailer.completed(10);
        thumbnailer.request(vec![20, 30]);
        assert!(matches!(
            command_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

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
    fn timeline_surface_uses_the_complete_terminal_area() {
        assert_eq!(
            timeline_pixel_dimensions(
                Rect::new(0, 0, 190, 32),
                ratatui_image::FontSize::new(15, 30),
            ),
            (2_850, 960),
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
            app.tick(Instant::now(), true).expect("poll decoder");
            assert!(Instant::now() < deadline, "preview preroll timed out");
            thread::sleep(Duration::from_millis(1));
        }

        let start = Instant::now();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), start)
            .expect("start playback");
        assert!(app.playback.is_playing());
        app.tick(start + Duration::from_millis(200), true)
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
        editor
            .timeline
            .sync_total_frames(app.as_ref().unwrap().frame_count());
        let timeline = editor_timeline_text(app.as_ref(), &editor.timeline, 80, &[]);
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
        assert_eq!(editor.pane_focus, EditorPaneFocus::Command);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.pane_focus, EditorPaneFocus::Timeline);

        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(app.as_ref().unwrap().playback.frame_index(), 8);

        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Char('+'), KeyModifiers::SHIFT),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.timeline.visible_frame_count(), 6);
        assert_eq!(editor.timeline.visible_range(), 5..11);
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
        assert_eq!(editor.timeline.visible_range(), 6..12);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.timeline.visible_range(), 3..9);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.timeline.visible_range(), 6..12);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert!(editor.timeline.is_fitted());
        editor.inspector_area = Rect::new(0, 0, 20, 6);
        editor.inspector_max_scroll = 20;
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.pane_focus, EditorPaneFocus::Inspector);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.inspector_scroll, 4);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.pane_focus, EditorPaneFocus::Command);
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Char('+'), KeyModifiers::SHIFT),
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.input, "+");

        editor.timeline_area = Rect::new(10, 5, 101, 4);
        handle_editor_mouse(
            app.as_mut().unwrap(),
            &session,
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
    fn empty_editor_still_has_help_inspector_and_timeline() {
        let session = EditorSession::new(
            MediaProject::new("Untitled", Rational::new(1, 25).unwrap()).unwrap(),
        );
        let editor = EditorUi {
            inspector_focus: InspectorFocus::Help,
            ..EditorUi::default()
        };

        let help = editor_context_text(None, &session, &editor);
        assert!(help.starts_with("PROJECT\n"));
        assert!(help.contains("HIERARCHY & MEDIA"));
        assert!(help.contains("MMFX SCENES"));
        assert!(help.contains("OUTPUT & HISTORY"));
        assert!(help.contains("KEYS"));
        assert!(help.contains("open <project>"));
        assert!(help.contains("import <file>"));
        assert!(help.contains("man <cmd>"));
        let timeline = editor_timeline_text(None, &editor.timeline, 40, &[]);
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

    #[test]
    fn scene_editing_keeps_the_monitor_on_the_project_timeline() {
        let (resize_tx, _resize_rx) = mpsc::channel();
        let picker = Picker::halfblocks();
        let host = EditorHost {
            resize_tx: &resize_tx,
            picker: &picker,
            base_directory: Path::new("."),
            terminal_size: Size::new(80, 24),
        };
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 30).unwrap()).unwrap());
        session
            .apply(EditCommand::Add {
                kind: MediaKind::new("scene").unwrap(),
                alias: "Overlay".into(),
                duration: mmrecode_edit::FrameValue::Frames {
                    frames: 60,
                    relative: false,
                },
                start: mmrecode_edit::FrameValue::Frames {
                    frames: 30,
                    relative: false,
                },
            })
            .unwrap();
        session
            .apply(EditCommand::Cd {
                path: "Overlay".into(),
            })
            .unwrap();

        assert_eq!(local_timeline_playhead(&session, 45), 15);
        assert_eq!(project_frame_for_local_timeline(&session, 20), 50);

        let mut editor = EditorUi {
            project_playhead: 45,
            ..EditorUi::default()
        };
        assert!(synchronize_timeline_context(&mut editor, &session));
        assert_eq!(editor.timeline_playhead, 15);
        assert!(synchronize_project_compositor(&mut editor, &session, &host));
        assert!(!editor.project_compositor.has_active_layers(15));
        assert!(editor.project_compositor.has_active_layers(45));

        let mut app = None;
        let mut history = CommandHistory::default();
        editor.input = "monitor local".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            Instant::now(),
            &host,
        )
        .unwrap();
        assert_eq!(editor.monitor_scope, MonitorScope::Local);
        assert!(editor.message.contains("Local Monitor selected"));
        assert!(synchronize_project_compositor(&mut editor, &session, &host));
        assert!(editor.project_compositor.has_active_layers(15));

        let checker = monitor_background(48, 24, MonitorScope::Local);
        assert_ne!(checker.get_pixel(0, 0), checker.get_pixel(24, 0));

        editor.input = "monitor toggle".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            Instant::now(),
            &host,
        )
        .unwrap();
        assert_eq!(editor.monitor_scope, MonitorScope::Project);
        assert_eq!(editor.project_playhead, 45);
        assert_eq!(editor.timeline_playhead, 15);
    }

    #[test]
    fn mmfx_starter_and_file_resources_compile_for_live_preview() {
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 30).unwrap()).unwrap());
        session
            .apply(EditCommand::Add {
                kind: MediaKind::new("fx").unwrap(),
                alias: "LivePreview".into(),
                duration: mmrecode_edit::FrameValue::Frames {
                    frames: 30,
                    relative: false,
                },
                start: mmrecode_edit::FrameValue::Frames {
                    frames: 0,
                    relative: false,
                },
            })
            .unwrap();
        session
            .apply(EditCommand::Cd {
                path: "LivePreview".into(),
            })
            .unwrap();
        let starter = &session.current_mmfx_source().unwrap().1.source;
        assert!(starter.contains("@font Inter"));
        assert!(starter.contains("@text title"));
        assert!(starter.contains("content: \"LivePreview\""));
        let starter_image = compile_mmfx_preview(starter, Path::new("."))
            .expect("compile the internal starter scene");
        assert_eq!(
            (starter_image.width(), starter_image.height()),
            (1_920, 1_080)
        );

        let module =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/mmfx/lower-third.mmfx");
        let source = std::fs::read_to_string(&module).expect("read checked-in MMFX example");
        let image = compile_mmfx_preview(&source, module.parent().unwrap())
            .expect("compile file-backed scene with relative font");
        assert_eq!((image.width(), image.height()), (1_280, 720));

        let motion =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/mmfx/motion-layout.mmfx");
        let source = std::fs::read_to_string(&motion).expect("read Scene 0.2 example");
        let scene = mmrecode_mmfx::parse_scene(&source).expect("parse Scene 0.2 example");
        let resources = crate::load_mmfx_resources(&scene, motion.parent().unwrap())
            .expect("load example image and built-in font");
        let rendered = mmrecode_mmfx::render_frame_with_resources(
            &scene,
            &resources,
            mmrecode_mmfx::SceneTime::new(23, 60),
        )
        .expect("render documented animation frame");
        assert_eq!((rendered.width(), rendered.height()), (960, 540));
        assert!(
            rendered
                .to_rgba8()
                .chunks_exact(4)
                .any(|pixel| pixel[3] > 0)
        );
        let documented = image::open(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/static/img/mmfx/motion-layout-023.png"),
        )
        .expect("open documented CPU reference output")
        .to_rgba8();
        assert_eq!(documented.into_raw(), rendered.to_rgba8());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn timeline_mmfx_source_edits_embed_extract_and_close() {
        let (resize_tx, _resize_rx) = mpsc::channel();
        let picker = Picker::halfblocks();
        let host = EditorHost {
            resize_tx: &resize_tx,
            picker: &picker,
            base_directory: Path::new("."),
            terminal_size: Size::new(80, 24),
        };
        let mut session =
            EditorSession::new(MediaProject::new("Film", Rational::new(1, 30).unwrap()).unwrap());
        let mut editor = EditorUi {
            input: "add scene LowerThird 2:00".into(),
            ..EditorUi::default()
        };
        let mut history = CommandHistory::default();
        let mut app = None;
        let now = Instant::now();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert!(editor.mmfx.is_none());
        editor
            .timeline
            .sync_total_frames(local_timeline_frame_count(&session));
        let root_objects = timeline_object_lanes(&session, None);
        let root_timeline = editor_timeline_text(None, &editor.timeline, 80, &root_objects);
        assert!(root_timeline.contains("LowerThird [scene/mmfx]"));
        assert!(synchronize_project_compositor(&mut editor, &session, &host));
        assert!(!synchronize_project_compositor(
            &mut editor,
            &session,
            &host
        ));
        assert!(editor.project_compositor.has_active_layers(0));
        assert!(update_compositor_only_monitor(
            &mut editor,
            &session,
            &picker
        ));
        assert_eq!(
            editor
                .timeline_monitor_key
                .as_ref()
                .map(|key| key.active_signature),
            Some(editor.project_compositor.active_signature(0))
        );
        editor.input = "cd LowerThird".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert_eq!(
            session.project().display_path(session.path()).unwrap(),
            "/LowerThird"
        );
        // Hierarchy navigation does not change the root project monitor composition.
        assert!(!synchronize_project_compositor(
            &mut editor,
            &session,
            &host
        ));
        let focused_objects = timeline_object_lanes(&session, None);
        let focused_timeline = editor_timeline_text(None, &editor.timeline, 80, &focused_objects);
        assert!(focused_timeline.contains("▸ LowerThird [scene/mmfx]"));
        editor.input = "edit".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert_eq!(editor.pane_focus, EditorPaneFocus::Code);
        assert!(
            editor
                .mmfx
                .as_ref()
                .unwrap()
                .source
                .contains("@scene LowerThird")
        );

        editor.pane_focus = EditorPaneFocus::Code;
        let cursor = editor.mmfx.as_ref().unwrap().source.len();
        editor.mmfx.as_mut().unwrap().cursor = cursor;
        handle_editor_key(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            now,
            &host,
        )
        .unwrap();
        assert!(
            session
                .current_mmfx_source()
                .unwrap()
                .1
                .source
                .ends_with(' ')
        );
        assert!(session.is_dirty());

        editor.pane_focus = EditorPaneFocus::Command;
        editor.input = "scene close".into();
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        assert!(editor.mmfx.is_none());
        assert!(
            session
                .current_mmfx_source()
                .unwrap()
                .1
                .source
                .ends_with(' ')
        );

        let destination =
            std::env::temp_dir().join(format!("mmrecode-live-mmfx-{}", std::process::id()));
        editor.input = format!("scene save as {}", destination.display());
        execute_editor_input(
            &mut app,
            &mut session,
            &mut history,
            &mut editor,
            now,
            &host,
        )
        .unwrap();
        let saved = destination.with_extension("mmfx");
        assert_eq!(
            std::fs::read_to_string(&saved).unwrap(),
            session.current_mmfx_source().unwrap().1.source
        );
        let _ = std::fs::remove_file(saved);
    }
}
