use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText, Stroke, TextureHandle, TextureOptions};
use mmrecode_mjpeg::{HuffmanTableClass, Marker, QuantizationPrecision, SegmentData};
use mmrecode_playback::{PlaybackController, PlaybackEvent, PlaybackTimeline};

use crate::{
    audio::AudioOutput,
    display::{self, DisplayMode},
    document::{Document, DvInspection, JpegInspection, Mpeg2Inspection, TransportInspection},
};

pub(crate) struct ViewerApp {
    path_input: String,
    document: Option<Document>,
    frame_index: usize,
    display_mode: DisplayMode,
    texture: Option<TextureHandle>,
    texture_generation: u64,
    fit_to_window: bool,
    zoom: f32,
    block_grid: bool,
    structure_view: StructureView,
    pixel_description: Option<String>,
    status: Status,
    playback: Option<PlaybackController>,
    audio_output: Option<AudioOutput>,
    audio_unavailable: bool,
    volume: f32,
}

enum Status {
    Ready,
    Info(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StructureView {
    #[default]
    Image,
    DifMap,
    MacroblockMap,
}

impl ViewerApp {
    pub(crate) fn new(
        context: &eframe::CreationContext<'_>,
        initial_path: Option<PathBuf>,
    ) -> Self {
        context.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            path_input: initial_path
                .as_deref()
                .map_or_else(String::new, |path| path.display().to_string()),
            document: None,
            frame_index: 0,
            display_mode: DisplayMode::Composite,
            texture: None,
            texture_generation: 0,
            fit_to_window: true,
            zoom: 1.0,
            block_grid: false,
            structure_view: StructureView::Image,
            pixel_description: None,
            status: Status::Ready,
            playback: None,
            audio_output: None,
            audio_unavailable: false,
            volume: 0.8,
        };
        if let Some(path) = initial_path {
            app.open_path(&context.egui_ctx, &path);
        }
        app
    }

    fn open_path(&mut self, context: &egui::Context, path: &Path) {
        match Document::load(path) {
            Ok(document) => {
                let frame_count = document.frames.len();
                let kind = document.kind.label();
                let timeline = match PlaybackTimeline::new(document.frame_rate, frame_count) {
                    Ok(timeline) => timeline,
                    Err(error) => {
                        self.status = Status::Error(error.to_string());
                        return;
                    }
                };
                self.path_input = document.path.display().to_string();
                self.audio_output = None;
                self.audio_unavailable = false;
                self.playback = Some(PlaybackController::new(timeline));
                self.document = Some(document);
                self.frame_index = 0;
                self.display_mode = DisplayMode::Composite;
                self.structure_view = StructureView::Image;
                self.pixel_description = None;
                self.status = Status::Info(format!("Loaded {frame_count} {kind} frame(s)"));
                self.refresh_texture(context);
                let title = path.file_name().map_or_else(
                    || "MMRecode Viewer".to_owned(),
                    |name| format!("{} — MMRecode Viewer", name.to_string_lossy()),
                );
                context.send_viewport_cmd(egui::ViewportCommand::Title(title));
            }
            Err(error) => {
                self.status = Status::Error(error);
            }
        }
    }

    fn refresh_texture(&mut self, context: &egui::Context) {
        let Some(record) = self
            .document
            .as_ref()
            .and_then(|document| document.frames.get(self.frame_index))
        else {
            self.texture = None;
            return;
        };
        let frame = display_frame(record, self.structure_view);
        match display::color_image(frame, self.display_mode) {
            Ok(image) => {
                self.texture_generation += 1;
                let name = format!("mmrecode-frame-{}", self.texture_generation);
                self.texture = Some(context.load_texture(name, image, TextureOptions::NEAREST));
            }
            Err(error) => {
                self.texture = None;
                self.status = Status::Error(error);
            }
        }
    }

    fn move_frame(&mut self, delta: isize, context: &egui::Context) {
        let Some(document) = &self.document else {
            return;
        };
        let last = document.frames.len() - 1;
        let next = self.frame_index.saturating_add_signed(delta).min(last);
        if next != self.frame_index {
            self.pause_playback();
            self.seek_frame(next, context);
        }
    }

    fn seek_frame(&mut self, frame_index: usize, context: &egui::Context) {
        let Some(playback) = &mut self.playback else {
            return;
        };
        let frame_index = frame_index.min(playback.timeline().frame_count() - 1);
        let position = playback.timeline().position_of_frame(frame_index);
        playback.seek(position, Instant::now());
        if let Some(audio) = &self.audio_output
            && let Err(error) = audio.seek(position)
        {
            self.status = Status::Error(error);
            self.audio_output = None;
            self.audio_unavailable = true;
        }
        self.frame_index = frame_index;
        self.pixel_description = None;
        self.refresh_texture(context);
    }

    fn pause_playback(&mut self) {
        if let Some(playback) = &mut self.playback {
            playback.pause(Instant::now());
        }
        if let Some(audio) = &self.audio_output {
            audio.pause();
        }
    }

    fn toggle_playback(&mut self, context: &egui::Context) {
        let playing = self
            .playback
            .as_ref()
            .is_some_and(PlaybackController::is_playing);
        if playing {
            self.pause_playback();
            return;
        }
        let has_animation = self
            .document
            .as_ref()
            .is_some_and(|document| document.frames.len() > 1);
        if !has_animation {
            return;
        }
        if self.audio_output.is_none() && !self.audio_unavailable {
            let track = self
                .document
                .as_ref()
                .and_then(|document| document.audio.clone());
            if let Some(track) = track {
                match AudioOutput::open(track, self.volume) {
                    Ok(output) => self.audio_output = Some(output),
                    Err(error) => {
                        self.audio_unavailable = true;
                        self.status = Status::Error(format!(
                            "{error}; continuing with silent video playback"
                        ));
                    }
                }
            }
        }
        let now = Instant::now();
        if let Some(playback) = &mut self.playback {
            playback.play(now);
        }
        let position = self
            .playback
            .as_ref()
            .map_or(Duration::ZERO, PlaybackController::position);
        if let Some(audio) = &self.audio_output {
            if let Err(error) = audio.seek(position) {
                self.status =
                    Status::Error(format!("{error}; continuing with silent video playback"));
                self.audio_output = None;
                self.audio_unavailable = true;
            } else {
                audio.play();
            }
        }
        context.request_repaint();
    }

    fn stop_playback(&mut self, context: &egui::Context) {
        self.pause_playback();
        self.seek_frame(0, context);
    }

    fn tick_playback(&mut self, context: &egui::Context) {
        let Some(playback) = &mut self.playback else {
            return;
        };
        if !playback.is_playing() {
            return;
        }
        let now = Instant::now();
        let event = if let Some(audio) = &self.audio_output {
            if audio.is_finished() {
                playback.advance(now)
            } else {
                playback.synchronize(audio.position(), now)
            }
        } else {
            playback.advance(now)
        };
        let next_frame = playback.frame_index();
        if next_frame != self.frame_index {
            self.frame_index = next_frame;
            self.pixel_description = None;
            self.refresh_texture(context);
        }
        match event {
            PlaybackEvent::None => {}
            PlaybackEvent::Ended => {
                if let Some(audio) = &self.audio_output {
                    audio.pause();
                }
            }
            PlaybackEvent::Looped => {
                if let Some(audio) = &self.audio_output
                    && let Err(error) = audio.restart()
                {
                    self.status = Status::Error(error);
                    self.audio_output = None;
                    self.audio_unavailable = true;
                }
            }
        }
        if self
            .playback
            .as_ref()
            .is_some_and(PlaybackController::is_playing)
        {
            context.request_repaint_after(Duration::from_millis(5));
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("File");
            let path_response = ui.add(
                egui::TextEdit::singleline(&mut self.path_input)
                    .desired_width(360.0)
                    .hint_text("MPEG-TS, MPEG-2 Video, raw DV, JPEG/MJPEG, or Y4M path"),
            );
            let open_requested = ui.button("Open").clicked()
                || (path_response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if open_requested {
                let path = PathBuf::from(self.path_input.trim());
                self.open_path(ui.ctx(), &path);
            }

            ui.separator();
            self.playback_toolbar(ui);

            ui.separator();
            let old_mode = self.display_mode;
            let current_record = self
                .document
                .as_ref()
                .and_then(|document| document.frames.get(self.frame_index));
            let current_frame =
                current_record.map(|record| display_frame(record, self.structure_view));
            let plane_count = current_frame.map_or(0, |frame| frame.planes.len());
            let packed_rgb = current_frame
                .is_some_and(|frame| frame.format == mmrecode_core::PixelFormat::Rgb24);
            let has_dv = current_record.is_some_and(|record| record.dv.is_some());
            let has_mpeg2 = current_record.is_some_and(|record| record.mpeg2.is_some());
            for mode in DisplayMode::ALL {
                let enabled = match mode {
                    DisplayMode::Composite => plane_count >= 1,
                    DisplayMode::Luma => plane_count >= 1 && !packed_rgb,
                    DisplayMode::ChromaBlue => plane_count >= 2,
                    DisplayMode::ChromaRed => plane_count >= 3,
                };
                ui.add_enabled_ui(enabled, |ui| {
                    ui.selectable_value(&mut self.display_mode, mode, mode.label());
                });
            }
            if old_mode != self.display_mode {
                self.pixel_description = None;
                self.refresh_texture(ui.ctx());
            }

            ui.separator();
            ui.checkbox(&mut self.fit_to_window, "Fit");
            ui.add_enabled(
                !self.fit_to_window,
                egui::Slider::new(&mut self.zoom, 0.25..=16.0)
                    .logarithmic(true)
                    .suffix("×"),
            );
            ui.checkbox(&mut self.block_grid, "8×8 grid");
            let old_structure = self.structure_view;
            ui.add_enabled_ui(has_dv, |ui| {
                let mut enabled = self.structure_view == StructureView::DifMap;
                if ui.checkbox(&mut enabled, "DIF map").changed() {
                    self.structure_view = if enabled {
                        StructureView::DifMap
                    } else {
                        StructureView::Image
                    };
                }
            });
            ui.add_enabled_ui(has_mpeg2, |ui| {
                let mut enabled = self.structure_view == StructureView::MacroblockMap;
                if ui.checkbox(&mut enabled, "Macroblock map").changed() {
                    self.structure_view = if enabled {
                        StructureView::MacroblockMap
                    } else {
                        StructureView::Image
                    };
                }
            });
            if old_structure != self.structure_view {
                self.display_mode = DisplayMode::Composite;
                self.refresh_texture(ui.ctx());
            }
        });
    }

    fn playback_toolbar(&mut self, ui: &mut egui::Ui) {
        let playing = self
            .playback
            .as_ref()
            .is_some_and(PlaybackController::is_playing);
        let can_play = self
            .document
            .as_ref()
            .is_some_and(|document| document.frames.len() > 1);
        if ui
            .add_enabled(can_play, egui::Button::new(if playing { "⏸" } else { "▶" }))
            .on_hover_text(if playing {
                "Pause (Space)"
            } else {
                "Play (Space)"
            })
            .clicked()
        {
            self.toggle_playback(ui.ctx());
        }
        if ui
            .add_enabled(can_play, egui::Button::new("■"))
            .on_hover_text("Stop")
            .clicked()
        {
            self.stop_playback(ui.ctx());
        }
        if let Some(playback) = &mut self.playback {
            let mut looping = playback.is_looping();
            if ui.checkbox(&mut looping, "Loop").changed() {
                playback.set_looping(looping);
            }
        }

        ui.separator();
        let has_previous = self.frame_index > 0;
        let has_next = self
            .document
            .as_ref()
            .is_some_and(|document| self.frame_index + 1 < document.frames.len());
        if ui
            .add_enabled(has_previous, egui::Button::new("◀"))
            .on_hover_text("Previous frame")
            .clicked()
        {
            self.move_frame(-1, ui.ctx());
        }
        if ui
            .add_enabled(has_next, egui::Button::new("▶"))
            .on_hover_text("Next frame")
            .clicked()
        {
            self.move_frame(1, ui.ctx());
        }

        if self
            .document
            .as_ref()
            .is_some_and(|document| document.audio.is_some())
        {
            ui.separator();
            ui.label("Volume");
            if ui
                .add(egui::Slider::new(&mut self.volume, 0.0..=1.5).show_value(false))
                .changed()
                && let Some(audio) = &self.audio_output
            {
                audio.set_volume(self.volume);
            }
        }
    }

    fn timeline(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(document) = &self.document {
                let old_index = self.frame_index;
                if document.frames.len() > 1 {
                    ui.add(
                        egui::Slider::new(&mut self.frame_index, 0..=document.frames.len() - 1)
                            .show_value(false),
                    );
                }
                ui.monospace(format!(
                    "Frame {} / {}",
                    self.frame_index + 1,
                    document.frames.len()
                ));
                if old_index != self.frame_index {
                    self.pixel_description = None;
                    self.seek_frame(self.frame_index, ui.ctx());
                }
                if let Some(playback) = &self.playback {
                    ui.separator();
                    ui.monospace(format!(
                        "{} / {}",
                        format_time(playback.position()),
                        format_time(playback.timeline().duration())
                    ));
                }
            } else {
                ui.label(
                    "Drop an MPEG-TS, MPEG-2 Video, raw DV, JPEG/MJPEG, or Y4M file here, or enter its path above.",
                );
            }

            ui.separator();
            if let Some(description) = &self.pixel_description {
                ui.monospace(description);
            } else {
                self.show_status(ui);
            }
        });
    }

    fn show_status(&self, ui: &mut egui::Ui) {
        match &self.status {
            Status::Ready => {
                ui.label("Ready");
            }
            Status::Info(message) => {
                ui.label(message);
            }
            Status::Error(message) => {
                ui.label(RichText::new(message).color(Color32::LIGHT_RED));
            }
        }
    }

    fn inspector(&self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            ui.heading("Inspector");
            ui.label("No media loaded.");
            return;
        };
        let record = &document.frames[self.frame_index];
        ui.heading("Inspector");
        ui.monospace(document.path.display().to_string());
        ui.separator();
        show_document_info(ui, document, record);

        ui.separator();
        ui.label(RichText::new("Planes").strong());
        for (index, plane) in record.frame.planes.iter().enumerate() {
            let name = ["Y", "Cb", "Cr"].get(index).copied().unwrap_or("?");
            ui.monospace(format!(
                "{name}: {}×{}, stride {}, {} bytes",
                plane.width,
                plane.height,
                plane.stride,
                plane.data.len()
            ));
        }

        ui.separator();
        ui.small("Display conversion: BT.601 coefficients; unspecified range is treated as full. Raw plane views are unconverted.");
        if let Some(transport) = &document.transport {
            show_transport_inspection(ui, transport);
        }
        if let Some(jpeg) = &record.jpeg {
            show_jpeg_inspection(ui, jpeg);
        }
        if let Some(dv) = &record.dv {
            show_dv_inspection(ui, dv);
        }
        if let Some(mpeg2) = &record.mpeg2 {
            show_mpeg2_inspection(ui, mpeg2);
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn viewport(&mut self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Drop media here to inspect it").size(22.0));
            });
            return;
        };
        let record = &document.frames[self.frame_index];
        let frame = display_frame(record, self.structure_view);
        let Some((width, height)) = display::dimensions(frame, self.display_mode) else {
            return;
        };
        let Some(texture) = &self.texture else {
            return;
        };
        let available = ui.available_size();
        let scale = if self.fit_to_window {
            ((available.x - 12.0) / width as f32)
                .min((available.y - 12.0) / height as f32)
                .clamp(0.01, 64.0)
        } else {
            self.zoom
        };
        let image_size = egui::vec2(width as f32 * scale, height as f32 * scale);
        let block_grid = self.block_grid;
        let mut pixel_description = None;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let response = ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(image_size)
                        .texture_options(TextureOptions::NEAREST)
                        .sense(egui::Sense::hover()),
                );
                if block_grid && scale >= 1.0 {
                    paint_block_grid(ui, response.rect, width, height);
                }
                if let Some(position) = response.hover_pos() {
                    let relative_x = ((position.x - response.rect.left()) / response.rect.width())
                        .clamp(0.0, 0.999_999);
                    let relative_y = ((position.y - response.rect.top()) / response.rect.height())
                        .clamp(0.0, 0.999_999);
                    let x = (relative_x * width as f32) as usize;
                    let y = (relative_y * height as f32) as usize;
                    pixel_description =
                        Some(display::pixel_description(frame, self.display_mode, x, y));
                }
            });
        self.pixel_description = pixel_description;
    }

    fn handle_dropped_file(&mut self, context: &egui::Context) {
        let path = context.input(|input| {
            input
                .raw
                .dropped_files
                .first()
                .and_then(|file| file.path.clone())
        });
        if let Some(path) = path {
            self.open_path(context, &path);
        }
    }

    fn handle_keyboard(&mut self, context: &egui::Context) {
        if context.egui_wants_keyboard_input() {
            return;
        }
        let direction = context.input(|input| {
            isize::from(input.key_pressed(egui::Key::ArrowRight))
                - isize::from(input.key_pressed(egui::Key::ArrowLeft))
        });
        if direction != 0 {
            self.move_frame(direction, context);
        }
        if context.input(|input| input.key_pressed(egui::Key::Space)) {
            self.toggle_playback(context);
        }
    }
}

fn display_frame(
    record: &crate::document::FrameRecord,
    structure_view: StructureView,
) -> &mmrecode_core::VideoFrame {
    match (structure_view, &record.dv, &record.mpeg2) {
        (StructureView::DifMap, Some(dv), _) => &dv.dif_map,
        (StructureView::MacroblockMap, _, Some(mpeg2)) => &mpeg2.macroblock_map,
        _ => &record.frame,
    }
}

fn format_time(duration: Duration) -> String {
    let total_millis = duration.as_millis();
    let minutes = total_millis / 60_000;
    let seconds = total_millis / 1_000 % 60;
    let millis = total_millis % 1_000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

fn show_document_info(
    ui: &mut egui::Ui,
    document: &Document,
    record: &crate::document::FrameRecord,
) {
    egui::Grid::new("document-info")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            ui.label("Type");
            ui.label(document.kind.label());
            ui.end_row();
            ui.label("File bytes");
            ui.monospace(document.byte_length.to_string());
            ui.end_row();
            ui.label("Dimensions");
            ui.monospace(format!("{} × {}", record.frame.width, record.frame.height));
            ui.end_row();
            ui.label("Format");
            ui.monospace(format!("{:?}", record.frame.format));
            ui.end_row();
            ui.label("Range");
            ui.monospace(format!("{:?}", record.frame.color.range));
            ui.end_row();
            ui.label("Primaries");
            ui.monospace(
                record
                    .frame
                    .color
                    .primaries
                    .as_deref()
                    .unwrap_or("unspecified"),
            );
            ui.end_row();
            ui.label("Transfer");
            ui.monospace(
                record
                    .frame
                    .color
                    .transfer
                    .as_deref()
                    .unwrap_or("unspecified"),
            );
            ui.end_row();
            ui.label("Matrix");
            ui.monospace(
                record
                    .frame
                    .color
                    .matrix
                    .as_deref()
                    .unwrap_or("unspecified"),
            );
            ui.end_row();
            ui.label("Field order");
            ui.monospace(format!("{:?}", record.frame.field_order));
            ui.end_row();
            ui.label("Playback rate");
            let assumption = if document.frame_rate_assumed {
                " (assumed)"
            } else {
                ""
            };
            ui.monospace(format!(
                "{}/{} fps{assumption}",
                document.frame_rate.numerator(),
                document.frame_rate.denominator()
            ));
            ui.end_row();
            ui.label("Playback audio");
            if let Some(audio) = &document.audio {
                ui.monospace(format!(
                    "{} Hz, {} ch, {}",
                    audio.sample_rate,
                    audio.channels,
                    format_time(audio.duration())
                ));
            } else {
                ui.monospace("none");
            }
            ui.end_row();
        });
}

fn show_transport_inspection(ui: &mut egui::Ui, transport: &TransportInspection) {
    ui.separator();
    ui.label(RichText::new("MPEG-2 Transport").strong());
    ui.monospace(format!(
        "{} TS packets, {} PAT, {} PMT, {} PES, {} PCR",
        transport.packet_count,
        transport.pat_count,
        transport.pmt_count,
        transport.pes_count,
        transport.pcr_count
    ));
    for program in &transport.programs {
        ui.monospace(format!(
            "Program {}: PMT 0x{:04x}, PCR 0x{:04x}",
            program.program_number, program.pmt_pid, program.pcr_pid
        ));
        for &(pid, stream_type) in &program.streams {
            ui.monospace(format!(
                "  PID 0x{pid:04x}, stream type 0x{stream_type:02x}"
            ));
        }
    }
    if let Some(audio) = &transport.mpeg_audio {
        ui.monospace(format!(
            "MPEG Layer II: {} frames, {} Hz, {} ch, {} bit/s",
            audio.frame_count, audio.sample_rate, audio.channels, audio.bit_rate
        ));
    }
    ui.small("Picture byte ranges below are relative to the demultiplexed elementary stream.");
}

fn show_mpeg2_inspection(ui: &mut egui::Ui, mpeg2: &Mpeg2Inspection) {
    ui.separator();
    ui.label(RichText::new("MPEG-2 structure").strong());
    ui.monospace(format!(
        "source 0x{:08x}..0x{:08x} ({} bytes)",
        mpeg2.source_range.start,
        mpeg2.source_range.end,
        mpeg2.source_range.len()
    ));
    ui.monospace(format!(
        "{:?} picture: temporal {}, decode {}, display {}",
        mpeg2.picture_type, mpeg2.temporal_reference, mpeg2.decode_order, mpeg2.presentation_order
    ));
    ui.monospace(format!(
        "{:?}, random access {:?}, references {:?}",
        mpeg2.picture_structure, mpeg2.random_access, mpeg2.references
    ));
    ui.monospace(format!(
        "{}×{} {:?}, {}/{} fps, profile/level 0x{:02x}",
        mpeg2.sequence.width,
        mpeg2.sequence.height,
        mpeg2.sequence.chroma_format,
        mpeg2.sequence.frame_rate.numerator(),
        mpeg2.sequence.frame_rate.denominator(),
        mpeg2.sequence.profile_and_level_indication
    ));
    ui.monospace(format!(
        "{} slice(s), {} macroblock(s), VBV {} bits",
        mpeg2.slice_count,
        mpeg2.macroblocks.len(),
        mpeg2.sequence.vbv_buffer_size_bits
    ));
    ui.monospace(format!(
        "progressive {}, top first {}, repeat first {}",
        mpeg2.progressive_frame, mpeg2.top_field_first, mpeg2.repeat_first_field
    ));
    let intra = mpeg2
        .macroblocks
        .iter()
        .filter(|macroblock| macroblock.coding == mmrecode_mpeg2::MacroblockCoding::Intra)
        .count();
    let predicted = mpeg2
        .macroblocks
        .iter()
        .filter(|macroblock| macroblock.coding == mmrecode_mpeg2::MacroblockCoding::Predicted)
        .count();
    let skipped = mpeg2.macroblocks.len().saturating_sub(intra + predicted);
    ui.monospace(format!(
        "macroblocks: {intra} intra, {predicted} predicted, {skipped} skipped"
    ));
    ui.small(
        "Macroblock map: intra green, P prediction amber, B prediction violet, skipped blue/dark violet, field prediction cyan/magenta.",
    );
}

fn show_dv_inspection(ui: &mut egui::Ui, dv: &DvInspection) {
    ui.separator();
    ui.label(RichText::new("DV structure").strong());
    let rate = dv.profile.frame_rate();
    ui.monospace(format!(
        "source 0x{:08x}..0x{:08x} ({} bytes)",
        dv.source_range.start,
        dv.source_range.end,
        dv.source_range.len()
    ));
    ui.monospace(format!(
        "{:?}: {}×{} {:?}, {}/{} fps",
        dv.profile.system,
        dv.profile.width,
        dv.profile.height,
        dv.profile.pixel_format,
        rate.numerator(),
        rate.denominator()
    ));
    ui.monospace(format!(
        "{} DIF sequences, {} packs, {} issue(s), {} concealed segment(s)",
        dv.profile.dif_sequences,
        dv.pack_count,
        dv.issues.len(),
        dv.concealed_video_segments
    ));
    if let Some(timecode) = dv.timecode {
        let separator = if timecode.drop_frame { ';' } else { ':' };
        ui.monospace(format!(
            "TC {:02}:{:02}:{:02}{separator}{:02}",
            timecode.hours, timecode.minutes, timecode.seconds, timecode.frames
        ));
    }
    if let Some((pairs, rate, samples)) = dv.audio {
        ui.monospace(format!(
            "Audio: {pairs} stereo pair(s), {rate} Hz, {samples} samples/ch"
        ));
    }
    ui.small("DIF map: header blue, subcode violet, VAUX cyan, audio amber, video green, reserved/error red.");
    for issue in dv.issues.iter().take(8) {
        ui.monospace(format!("0x{:08x}: {:?}", issue.offset, issue.kind));
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.tick_playback(ui.ctx());
        self.handle_dropped_file(ui.ctx());
        self.handle_keyboard(ui.ctx());

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("timeline").show(ui, |ui| self.timeline(ui));
        egui::Panel::right("inspector")
            .default_size(310.0)
            .min_size(240.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui));
            });
        egui::CentralPanel::default().show(ui, |ui| self.viewport(ui));
    }
}

#[allow(clippy::cast_precision_loss)]
fn paint_block_grid(ui: &egui::Ui, rect: egui::Rect, width: usize, height: usize) {
    let painter = ui.painter_at(rect);
    let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 190, 40, 150));
    for x in (8..width).step_by(8) {
        let screen_x = rect.left() + x as f32 / width as f32 * rect.width();
        painter.line_segment(
            [
                egui::pos2(screen_x, rect.top()),
                egui::pos2(screen_x, rect.bottom()),
            ],
            stroke,
        );
    }
    for y in (8..height).step_by(8) {
        let screen_y = rect.top() + y as f32 / height as f32 * rect.height();
        painter.line_segment(
            [
                egui::pos2(rect.left(), screen_y),
                egui::pos2(rect.right(), screen_y),
            ],
            stroke,
        );
    }
}

fn show_jpeg_inspection(ui: &mut egui::Ui, jpeg: &JpegInspection) {
    ui.separator();
    ui.label(RichText::new("JPEG structure").strong());
    ui.monospace(format!(
        "source 0x{:08x}..0x{:08x} ({} bytes)",
        jpeg.source_range.start,
        jpeg.source_range.end,
        jpeg.source_range.len()
    ));
    for segment in &jpeg.image.segments {
        let label = marker_label(segment.marker);
        egui::CollapsingHeader::new(format!("0x{:06x}  {label}", segment.offset))
            .default_open(matches!(
                segment.marker,
                Marker::StartOfFrameBaseline | Marker::StartOfScan
            ))
            .show(ui, |ui| {
                if let Some(payload_offset) = segment.payload_offset {
                    ui.monospace(format!(
                        "payload 0x{payload_offset:06x}, {} bytes",
                        segment.payload_length
                    ));
                }
                show_segment_data(ui, &segment.data);
            });
    }
    for (index, scan) in jpeg.image.entropy_scans.iter().enumerate() {
        ui.monospace(format!(
            "Scan {}: 0x{:06x}, {} bytes, {} restart markers",
            index + 1,
            scan.data_offset,
            scan.data_length,
            scan.restart_markers.len()
        ));
    }
}

fn marker_label(marker: Marker) -> String {
    match marker {
        Marker::Application(number) => format!("APP{number}"),
        Marker::Restart(number) => format!("RST{number}"),
        Marker::Other(code) => format!("0x{code:02x}"),
        marker => marker.name().to_owned(),
    }
}

fn show_segment_data(ui: &mut egui::Ui, data: &SegmentData) {
    match data {
        SegmentData::Frame(frame) => {
            ui.monospace(format!(
                "{}×{}, {} bit, {} components",
                frame.width,
                frame.height,
                frame.sample_precision,
                frame.components.len()
            ));
            for component in &frame.components {
                ui.monospace(format!(
                    "component {}: {}×{}, Q{}",
                    component.id,
                    component.horizontal_sampling,
                    component.vertical_sampling,
                    component.quantization_table
                ));
            }
        }
        SegmentData::QuantizationTables(tables) => {
            for table in tables {
                let bits = match table.precision {
                    QuantizationPrecision::EightBit => 8,
                    QuantizationPrecision::SixteenBit => 16,
                };
                ui.monospace(format!("Q{}: {bits} bit", table.id));
            }
        }
        SegmentData::HuffmanTables(tables) => {
            for table in tables {
                let class = match table.class {
                    HuffmanTableClass::Dc => "DC",
                    HuffmanTableClass::Ac => "AC",
                };
                ui.monospace(format!(
                    "{class}{}: {} symbols",
                    table.id,
                    table.symbols.len()
                ));
            }
        }
        SegmentData::RestartInterval(interval) => {
            ui.monospace(format!("{interval} MCU(s)"));
        }
        SegmentData::Scan(scan) => {
            ui.monospace(format!("{} component(s)", scan.components.len()));
        }
        SegmentData::Jfif(jfif) => {
            ui.monospace(format!(
                "JFIF {}.{:02}, density {}×{} (unit {})",
                jfif.version_major,
                jfif.version_minor,
                jfif.density_x,
                jfif.density_y,
                jfif.density_units
            ));
        }
        SegmentData::Application(application) => {
            ui.monospace(format!("{} opaque bytes", application.data.len()));
        }
        SegmentData::Comment(comment) => {
            ui.monospace(format!("{} comment bytes", comment.len()));
        }
        SegmentData::Unknown(data) => {
            ui.monospace(format!("{} opaque bytes", data.len()));
        }
        SegmentData::Empty => {
            ui.label("No payload");
        }
    }
}
