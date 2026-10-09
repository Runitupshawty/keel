fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("Keel"),
        ..Default::default()
    };
    eframe::run_native("Keel", opts, Box::new(|_cc| Ok(Box::new(App))))
}

struct App;

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("Keel");
        });
    }
}
