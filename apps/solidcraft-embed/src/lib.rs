//! SolidCraft hosted inside Workbench's side panel: the same app as the desktop build, drawn into a view
//! Workbench owns, with Workbench's look and only the 3D view showing (Hands off) until the user asks for the
//! menus. Workbench (and Claude) drive it through its control channel (docs/control-protocol.md), plus a few
//! requests of Workbench's own: `file.open {path}`, `file.new`, `file.saveAs {path}`, `view.workbench` and
//! `view.handsOff`. See `craft-embed`.
// craft-embed's `export_embed!` expands to the C functions the host calls.
#![allow(unsafe_code)]

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use craft_embed::{EmbedApp, egui, egui_wgpu};
use serde_json::{Value, json};
use solidcraft_control::{ControlRequest, ControlResponse};
use solidcraft_engine::Session;
use solidcraft_ui_egui::{Services, SolveApp};

/// The scene's depth buffer, as the browser build draws it.
const DEPTH_BITS: u8 = 32;

struct Solid {
    app: SolveApp,
    /// Requests go to the app's own control queue, which it answers in its frame logic.
    sender: Sender<ControlRequest>,
    /// The reply still to come for the last request (a rebuild or a file loading answers frames later).
    waiting: Option<Receiver<ControlResponse>>,
}

fn reply_ok(result: Value) -> String {
    json!({"ok": true, "result": result}).to_string()
}

fn reply_err(error: &str) -> String {
    json!({"ok": false, "error": error}).to_string()
}

impl Solid {
    /// Workbench's own view requests: answered here, between frames.
    fn view(&mut self, command: &str, params: &Value) -> Option<String> {
        let on = |current: bool| params.get("on").and_then(Value::as_bool).unwrap_or(!current);
        match command {
            "view.workbench" => {
                let on = on(solidcraft_theme_on());
                self.app.set_workbench(on, params.get("accent").and_then(Value::as_str));
                Some(reply_ok(json!({"on": on})))
            }
            "view.handsOff" => {
                let on = on(self.app.ui.hands_off);
                self.app.set_hands_off(on);
                Some(reply_ok(json!({"on": on})))
            }
            _ => None,
        }
    }
}

fn solidcraft_theme_on() -> bool {
    solidcraft_theme::workbench().is_some()
}

impl EmbedApp for Solid {
    fn logic(&mut self, ctx: &egui::Context) {
        self.app.logic(ctx);
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.app.ui(ui);
    }
    fn control(&mut self, ctx: &egui::Context, request: &str) -> String {
        let Ok(req) = serde_json::from_str::<Value>(request) else { return reply_err("the request is not JSON") };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        let path = || params.get("path").and_then(Value::as_str).map(str::to_owned);
        let (method, params) = match method {
            "view.workbench" | "view.handsOff" => return self.view(method, &params).unwrap_or_default(),
            "engine.execute" | "command" => {
                let command = params.get("command").and_then(Value::as_str).unwrap_or("");
                let inner = params.get("params").cloned().unwrap_or_else(|| json!({}));
                if let Some(reply) = self.view(command, &inner) {
                    return reply;
                }
                (method.to_owned(), params)
            }
            // Workbench's file requests, by real path: the app reads and writes the files itself.
            "file.open" => {
                let Some(path) = path() else { return reply_err("file.open needs a path") };
                if !std::path::Path::new(&path).is_file() {
                    return reply_err(&format!("{path}: no such file"));
                }
                ("engine.execute".into(), json!({"command": "doc.open", "params": {"path": path}}))
            }
            "file.new" => ("engine.execute".into(), json!({"command": "file.new", "params": {}})),
            "file.save" => ("engine.execute".into(), json!({"command": "file.save", "params": {}})),
            "file.saveAs" => {
                let Some(path) = path() else { return reply_err("file.saveAs needs a path") };
                ("engine.execute".into(), json!({"command": "file.save_as", "params": {"path": path}}))
            }
            // The app's own screenshots wait for the window's capture, which the host doesn't run.
            "ui.screenshot" => return reply_err("Workbench takes the screenshot of the panel itself"),
            "" => return reply_err("the request has no method"),
            other => (other.to_owned(), params),
        };
        let (request, reply) = ControlRequest::new(method, params);
        if self.sender.send(request).is_err() {
            return reply_err("SolidCraft is not running");
        }
        self.waiting = Some(reply);
        ctx.request_repaint();
        reply_ok(json!({"pending": true}))
    }
    fn settle(&mut self) -> Option<String> {
        let reply = match self.waiting.as_ref()?.try_recv() {
            Ok(reply) => reply.to_string(),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => reply_err("SolidCraft dropped the request"),
        };
        self.waiting = None;
        Some(reply)
    }
    fn gpu(&mut self, rs: &egui_wgpu::RenderState) {
        // The 3D view's pipelines, drawn into the host's pass as the browser build does.
        solidcraft_ui_egui::gpu::install_picture_renderer(rs);
        let backend = rs.adapter.get_info().backend;
        let depth = egui_wgpu::depth_format_from_bits(DEPTH_BITS, 0);
        let supported = egui_wgpu::supported_msaa_samples(&rs.adapter, &rs.device, rs.target_format, depth);
        let requested = solidcraft_ui_egui::gpu::scene_samples(backend, self.app.preferences.msaa, &supported);
        self.app.set_wgpu_split(rs, DEPTH_BITS, 0, requested);
        self.app.msaa_supported = solidcraft_ui_egui::gpu::scene_sample_choices(backend, supported);
    }
}

fn make(ctx: &egui::Context) -> Box<dyn EmbedApp> {
    solidcraft_ui_egui::i18n::install();
    solidcraft_look::install_tables();
    solidcraft_ui_egui::workspace::install_tabs();
    solidcraft_ui_egui::navigation_slots::install();
    solidcraft_ui_egui::fonts::setup_bundled_fonts(ctx);
    let (sender, receiver) = channel();
    let mut app = SolveApp::new(Session::default(), Services::default()).with_control(receiver);
    // One design, no start page: Workbench opens and starts designs itself.
    app.embed_editor = true;
    solidcraft_ui_egui::prefs::apply_new_design(&mut app);
    // Workbench's look and Hands off from the first frame; Workbench sets the mode's accent right after
    app.set_workbench(true, None);
    app.set_hands_off(true);
    app.apply_preferences(ctx);
    Box::new(Solid { app, sender, waiting: None })
}

craft_embed::export_embed!(make);
