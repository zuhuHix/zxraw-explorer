//! Automation hook for UI verification and screenshots in docs/CI.
//!
//! * `ZXRAW_CMDS=ViewGrid,ToggleDualPane` — run commands (by `Cmd` variant name) once the
//!   first folder has loaded.
//! * `ZXRAW_SELECT=name.jpg` — select that entry in the focused pane before the commands.
//! * `ZXRAW_SCREENSHOT=out.png` — after the UI settles, capture the rendered frame via
//!   egui's own screenshot path (works even where OS screen capture can't see GPU
//!   swapchains), save it, and exit.

use std::path::PathBuf;

use crate::app::ZxrawApp;
use crate::commands::Cmd;

#[derive(Default)]
pub struct TestHook {
    screenshot: Option<PathBuf>,
    cmds: Vec<Cmd>,
    select: Option<String>,
    frames: u32,
    ran_cmds: bool,
    requested: bool,
}

impl TestHook {
    pub fn from_env() -> Self {
        let cmds = std::env::var("ZXRAW_CMDS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|n| {
                Cmd::ALL
                    .iter()
                    .find(|c| format!("{c:?}") == n.trim())
                    .copied()
            })
            .collect();
        Self {
            screenshot: std::env::var_os("ZXRAW_SCREENSHOT").map(PathBuf::from),
            cmds,
            select: std::env::var("ZXRAW_SELECT").ok(),
            ..Default::default()
        }
    }

    pub fn active(&self) -> bool {
        self.screenshot.is_some() || !self.cmds.is_empty() || self.select.is_some()
    }
}

pub fn tick(app: &mut ZxrawApp, ctx: &egui::Context) {
    if !app.testhook.active() {
        return;
    }
    let loading = app.tabs.iter().any(|t| t.left.is_loading());
    let hook = &mut app.testhook;
    hook.frames += 1;
    if !hook.ran_cmds && !loading && hook.frames > 5 {
        hook.ran_cmds = true;
        if let Some(name) = hook.select.take() {
            app.pane_mut().select_name(&name);
        }
        let hook = &mut app.testhook;
        let cmds = std::mem::take(&mut hook.cmds);
        for c in cmds {
            app.run(c, ctx);
        }
    }
    let hook = &mut app.testhook;
    // Give async listings/thumbnails a moment to land, then capture.
    if hook.ran_cmds && hook.frames > 90 && !hook.requested {
        if hook.screenshot.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            hook.requested = true;
        }
    }
    let shot = ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let (Some(img), Some(path)) = (shot, hook.screenshot.clone()) {
        let [w, h] = img.size;
        let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        match image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
            Some(buf) => {
                if let Err(e) = buf.save(&path) {
                    tracing::error!("screenshot save failed: {e}");
                }
            }
            None => tracing::error!("screenshot buffer size mismatch"),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    ctx.request_repaint();
}
