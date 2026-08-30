//! Visual inspection application for `MMRecode` media.

fn main() -> eframe::Result {
    let initial_path = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MMRecode Viewer",
        options,
        Box::new(move |context| Ok(Box::new(app::ViewerApp::new(context, initial_path)))),
    )
}

mod app;
mod audio;
mod display;
mod document;
