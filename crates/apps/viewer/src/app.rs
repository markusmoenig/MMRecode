use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText, Stroke, TextureHandle, TextureOptions};
use mmrecode_mjpeg::{HuffmanTableClass, Marker, QuantizationPrecision, SegmentData};

use crate::{
    display::{self, DisplayMode},
    document::{Document, DvInspection, JpegInspection},
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
    dv_structure: bool,
    pixel_description: Option<String>,
    status: Status,
}

enum Status {
    Ready,
    Info(String),
    Error(String),
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
            dv_structure: false,
            pixel_description: None,
            status: Status::Ready,
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
                self.path_input = document.path.display().to_string();
                self.document = Some(document);
                self.frame_index = 0;
                self.display_mode = DisplayMode::Composite;
                self.dv_structure = false;
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
        let frame = display_frame(record, self.dv_structure);
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
            self.frame_index = next;
            self.pixel_description = None;
            self.refresh_texture(context);
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("File");
            let path_response = ui.add(
                egui::TextEdit::singleline(&mut self.path_input)
                    .desired_width(360.0)
                    .hint_text("Raw DV, JPEG, MJPEG, or Y4M path"),
            );
            let open_requested = ui.button("Open").clicked()
                || (path_response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if open_requested {
                let path = PathBuf::from(self.path_input.trim());
                self.open_path(ui.ctx(), &path);
            }

            ui.separator();
            let has_previous = self.frame_index > 0;
            let has_next = self
                .document
                .as_ref()
                .is_some_and(|document| self.frame_index + 1 < document.frames.len());
            if ui
                .add_enabled(has_previous, egui::Button::new("◀"))
                .clicked()
            {
                self.move_frame(-1, ui.ctx());
            }
            if ui.add_enabled(has_next, egui::Button::new("▶")).clicked() {
                self.move_frame(1, ui.ctx());
            }

            ui.separator();
            let old_mode = self.display_mode;
            let current_record = self
                .document
                .as_ref()
                .and_then(|document| document.frames.get(self.frame_index));
            let current_frame =
                current_record.map(|record| display_frame(record, self.dv_structure));
            let plane_count = current_frame.map_or(0, |frame| frame.planes.len());
            let packed_rgb = current_frame
                .is_some_and(|frame| frame.format == mmrecode_core::PixelFormat::Rgb24);
            let has_dv = current_record.is_some_and(|record| record.dv.is_some());
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
            let old_structure = self.dv_structure;
            ui.add_enabled_ui(has_dv, |ui| {
                ui.checkbox(&mut self.dv_structure, "DIF map");
            });
            if old_structure != self.dv_structure {
                self.display_mode = DisplayMode::Composite;
                self.refresh_texture(ui.ctx());
            }
        });
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
                    self.refresh_texture(ui.ctx());
                }
            } else {
                ui.label("Drop a raw DV, JPEG, MJPEG, or Y4M file here, or enter its path above.");
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
                ui.label("Field order");
                ui.monospace(format!("{:?}", record.frame.field_order));
                ui.end_row();
            });

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
        if let Some(jpeg) = &record.jpeg {
            show_jpeg_inspection(ui, jpeg);
        }
        if let Some(dv) = &record.dv {
            show_dv_inspection(ui, dv);
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
        let frame = display_frame(record, self.dv_structure);
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
    }
}

fn display_frame(
    record: &crate::document::FrameRecord,
    dv_structure: bool,
) -> &mmrecode_core::VideoFrame {
    if dv_structure && let Some(dv) = &record.dv {
        &dv.dif_map
    } else {
        &record.frame
    }
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
