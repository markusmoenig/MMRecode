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
use mmrecode_core::{ColorRange, PixelFormat, Plane, VideoFrame};
use mmrecode_mpeg2::DecodedMpeg2Picture;
use mmrecode_playback::{
    Mpeg2PlaybackEvent, Mpeg2PlaybackSource, PlaybackController, PlaybackTimeline,
};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    layout::{Alignment, Constraint, Layout, Rect, Size},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
};
use ratatui_image::{
    FilterType, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    thread::{ResizeRequest, ResizeResponse, ThreadProtocol},
};

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

fn open_source(path: &Path) -> Result<Mpeg2PlaybackSource, String> {
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

fn run_initialized(
    terminal: &mut DefaultTerminal,
    source: Mpeg2PlaybackSource,
    path: &Path,
) -> Result<(), String> {
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let protocol = picker.protocol_type();
    let (resize_tx, resize_rx) = mpsc::channel::<ResizeRequest>();
    let (complete_tx, complete_rx) = mpsc::channel::<Result<ResizeResponse, String>>();
    let resize_worker = thread::Builder::new()
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

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut PreviewApp,
    completed: &Receiver<Result<ResizeResponse, String>>,
) -> Result<(), String> {
    loop {
        app.tick(Instant::now())?;
        while let Ok(response) = completed.try_recv() {
            match response {
                Ok(response) => {
                    if let Some(image_state) = &mut app.image_state {
                        image_state.update_resized_protocol(response);
                    }
                }
                Err(error) => app.error = Some(error),
            }
        }
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|error| format!("cannot draw terminal preview: {error}"))?;
        app.flush_kitty_frame()?;

        if event::poll(EVENT_WAIT).map_err(|error| format!("cannot poll terminal: {error}"))? {
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
    resume_when_buffered: bool,
    picker: Picker,
    image_state: Option<ThreadProtocol>,
    kitty: Option<KittyStreamer>,
    image_frame: Option<usize>,
    terminal_size: Size,
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
        let direct_kitty = picker.protocol_type() == ProtocolType::Kitty && !inside_tmux();
        Ok(Self {
            source,
            frames: BTreeMap::new(),
            generation: 0,
            requested_range: 0..0,
            playback: PlaybackController::new(timeline),
            resume_when_buffered: false,
            image_state: (!direct_kitty).then(|| ThreadProtocol::new(resize_tx, None)),
            kitty: direct_kitty.then(KittyStreamer::new).transpose()?,
            picker,
            image_frame: None,
            terminal_size,
            path: path.display().to_string(),
            error: None,
        })
    }

    fn tick(&mut self, now: Instant) -> Result<(), String> {
        self.poll_decoder()?;
        self.playback.advance(now);
        let current = self.playback.frame_index();
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
                    let current = self.playback.frame_index();
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
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta.unsigned_abs())
                .min(self.frame_count() - 1)
        };
        self.seek_frame(target, now)
    }

    fn seek_frame(&mut self, frame: usize, now: Instant) -> Result<(), String> {
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
        let end = start.saturating_add(BUFFER_FRAMES).min(self.frame_count());
        (start..end).all(|index| self.frames.contains_key(&index))
    }

    const fn frame_count(&self) -> usize {
        self.playback.timeline().frame_count()
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
        let displayed = kitty.flush(self.terminal_size, self.picker.font_size())?;
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
        terminal_size: Size,
        font_size: ratatui_image::FontSize,
    ) -> Result<Option<usize>, String> {
        let Some(frame) = self.queued.take() else {
            self.update_placement(terminal_size, font_size)?;
            return Ok(None);
        };
        let width = frame.image.width();
        let height = frame.image.height();
        let placement = kitty_placement(width, height, terminal_size, font_size);
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
        terminal_size: Size,
        font_size: ratatui_image::FontSize,
    ) -> Result<(), String> {
        let Some((width, height)) = self.image_size else {
            return Ok(());
        };
        let placement = kitty_placement(width, height, terminal_size, font_size);
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
    terminal: Size,
    font: ratatui_image::FontSize,
) -> KittyPlacement {
    let available = Rect::new(
        1,
        2,
        terminal.width.saturating_sub(2),
        terminal.height.saturating_sub(4),
    );
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
        " {} | {} | frame {}/{} | {}/{} fps | {} ",
        app.path,
        protocol,
        current + 1,
        app.frame_count(),
        rate.numerator(),
        rate.denominator(),
        app.status()
    );
    frame.render_widget(Paragraph::new(title), header);

    let image_block = Block::default().borders(Borders::ALL).title("Preview");
    let image_area = image_block.inner(preview);
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
    use mmrecode_core::{ColorDescription, FieldOrder, FrameTiming};
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
            kitty_placement(1_920, 1_080, Size::new(100, 40), font),
            KittyPlacement {
                column: 2,
                row: 7,
                columns: 98,
                rows: 28,
            }
        );
        assert_eq!(
            kitty_placement(1_080, 1_920, Size::new(100, 40), font),
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
}
