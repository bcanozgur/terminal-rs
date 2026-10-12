//! terminal-rs: a keyboard-driven, Bloomberg-style market terminal with GPU charts and
//! congressional (STOCK Act) trade tracking.

mod anim;
mod app;
mod chart_view;
mod command;
mod congress_view;
mod dock;
mod layout;
mod layouts;
mod markets_view;
mod palette;
mod prefs;
mod theme;
mod themes;
mod widgets;
mod worker;

use std::time::Instant;

fn main() -> eframe::Result {
    let t0 = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();

    // MSAA smooths chart lines and drawings; 4x by default on macOS (TR_MSAA overrides).
    let msaa: u16 = std::env::var("TR_MSAA")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| matches!(n, 1 | 2 | 4 | 8))
        .unwrap_or(if cfg!(target_os = "macos") { 4 } else { 1 });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("terminal-rs")
        .with_app_id("terminal-rs")
        .with_inner_size([1600.0, 960.0])
        .with_min_inner_size([900.0, 560.0]);
    if cfg!(target_os = "macos") {
        // Content extends under a transparent title bar; tr-macos adds the unified
        // toolbar (inset traffic lights) and sidebar vibrancy on the first frame.
        viewport = viewport
            .with_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_shown(false)
            .with_titlebar_shown(false);
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport,
        multisampling: if msaa > 1 { msaa } else { 0 },
        ..Default::default()
    };
    eframe::run_native(
        "terminal-rs",
        options,
        Box::new(move |cc| {
            let rs = cc.wgpu_render_state.as_ref().ok_or("terminal-rs requires the wgpu renderer")?;
            rs.renderer.write().callback_resources.insert(tr_chart::gpu::ChartRenderer::new(
                &rs.device,
                rs.target_format,
                u32::from(msaa),
            ));
            let provider = tr_data::from_env();
            let name = provider.name().to_string();
            let worker = worker::Worker::spawn(cc.egui_ctx.clone(), provider)?;
            tracing::info!("window + GPU ready in {:?} (adapter: {})", t0.elapsed(), rs.adapter.get_info().name);
            Ok(Box::new(app::TerminalApp::new(&cc.egui_ctx, worker, name)))
        }),
    )
}
