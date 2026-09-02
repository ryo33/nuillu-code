use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use nuillu_code_module::{PatchDecision, PatchGate, PatchUiEvent, PendingPatch, Workspace};
use nuillu_server::{
    RuntimeModule, Server, ServerBootConfig, ServerRunOptions, ServerRuntimeHandle,
    load_server_config_from_options,
};
use nuillu_visualizer_egui::{Visualizer, VisualizerConfig};
use nuillu_visualizer_protocol::{
    VisualizerClientMessage, VisualizerCommand, VisualizerServerMessage,
};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};

const SERVER_MESSAGES_PER_FRAME: usize = 256;

fn main() -> Result<()> {
    install_trace_subscriber()?;
    let args = Args::parse();
    let workspace = Workspace::open(&args.cwd)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build startup validation runtime")?
        .block_on(workspace.verify_state_dir_is_ignored())?;
    let state_dir = workspace.state_dir();
    let mut config = load_server_config_from_options(ServerRunOptions {
        state_dir: state_dir.clone(),
        run_id: None,
        session_id: None,
        llm_log_root: state_dir.join("llm-logs"),
        model_set: None,
        disabled_modules: Vec::new(),
        participants: Vec::new(),
        fresh_agent_db: args.fresh_agent_db,
        agent_db: None,
    })?;
    config.boot_config = coding_agent_boot_config();
    config.disabled_modules.clear();

    let (patch_gate, patch_events) = PatchGate::new();
    let registrars = vec![nuillu_code_module::registrar(
        workspace.clone(),
        patch_gate.clone(),
    )];
    let runtime = Server::new(config)
        .module_registrars(registrars)
        .spawn()
        .context("start embedded Nuillu runtime")?;
    let (server_messages, client_messages) = runtime.visualizer_channels();
    let app_runtime = runtime.clone();
    let native_options = native_options();
    let title = format!("Nuillu Code — {}", workspace.root().display());
    let result = eframe::run_native(
        &title,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(NuilluCodeApp::new(
                server_messages,
                client_messages,
                app_runtime,
                patch_gate,
                patch_events,
            )))
        }),
    );
    let _ = runtime.shutdown();
    match runtime.join_timeout(Duration::from_secs(2)) {
        Ok(true) => {}
        Ok(false) => eprintln!("embedded Nuillu runtime did not stop within two seconds"),
        Err(error) => eprintln!("embedded Nuillu runtime failed during shutdown: {error:#}"),
    }
    result.map_err(anyhow::Error::msg)
}

fn install_trace_subscriber() -> Result<()> {
    tracing_log::LogTracer::init().context("install log-to-tracing bridge")?;
    let console = tracing_subscriber::fmt::layer().with_filter(LevelFilter::WARN);
    let subscriber = tracing_subscriber::registry()
        .with(lutum_trace::layer())
        .with(console);
    tracing::subscriber::set_global_default(subscriber).context("install global tracing subscriber")
}

fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_active(true)
            .with_visible(true)
            .with_fullscreen(true),
        ..eframe::NativeOptions::default()
    }
}

/// The only runtime modules this coding agent boots.
const CODING_AGENT_MODULES: [RuntimeModule; 10] = [
    RuntimeModule::Sensory,
    RuntimeModule::CognitionGate,
    RuntimeModule::Allocation,
    RuntimeModule::Action,
    RuntimeModule::AttentionSchema,
    RuntimeModule::Interpreter,
    RuntimeModule::SelfModel,
    RuntimeModule::QueryMemory,
    RuntimeModule::Memory,
    RuntimeModule::Speak,
];

fn coding_agent_boot_config() -> ServerBootConfig {
    let mut config = ServerBootConfig::default();
    config
        .modules
        .retain(|module| CODING_AGENT_MODULES.contains(&module.id));
    config.actions.clear();
    config
}

struct NuilluCodeApp {
    visualizer: Visualizer,
    server_messages: Receiver<VisualizerServerMessage>,
    client_messages: Sender<VisualizerClientMessage>,
    runtime: ServerRuntimeHandle,
    patch_gate: PatchGate,
    patch_events: Receiver<PatchUiEvent>,
    pending_patch: Option<PendingPatch>,
    patch_log: Vec<PatchLogEntry>,
    patch_window_open: bool,
    write_enabled: bool,
    stopped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PatchLogKind {
    Applying,
    Approved,
    Applied,
    Rejected,
    Failed,
}

impl PatchLogKind {
    fn label(self) -> &'static str {
        match self {
            Self::Applying => "Applying",
            Self::Approved => "Approved",
            Self::Applied => "Applied",
            Self::Rejected => "Rejected",
            Self::Failed => "Failed",
        }
    }
}

struct PatchLogEntry {
    kind: PatchLogKind,
    purpose: String,
    body: String,
}

impl NuilluCodeApp {
    fn new(
        server_messages: Receiver<VisualizerServerMessage>,
        client_messages: Sender<VisualizerClientMessage>,
        runtime: ServerRuntimeHandle,
        patch_gate: PatchGate,
        patch_events: Receiver<PatchUiEvent>,
    ) -> Self {
        patch_gate.set_write_enabled(false);
        Self {
            visualizer: Visualizer::with_config(
                eframe::egui::Id::new("nuillu-code-visualizer"),
                VisualizerConfig::standalone(),
            ),
            server_messages,
            client_messages,
            runtime,
            patch_gate,
            patch_events,
            pending_patch: None,
            patch_log: Vec::new(),
            patch_window_open: false,
            write_enabled: false,
            stopped: false,
        }
    }

    fn drain_server_messages(&mut self, context: &eframe::egui::Context) {
        for _ in 0..SERVER_MESSAGES_PER_FRAME {
            match self.server_messages.try_recv() {
                Ok(message) => self.visualizer.apply_server_message(message),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.visualizer.mark_disconnected();
                    return;
                }
            }
        }
        context.request_repaint();
    }

    fn drain_patch_events(&mut self, context: &eframe::egui::Context) {
        while let Ok(event) = self.patch_events.try_recv() {
            match event {
                PatchUiEvent::Pending(pending) => {
                    if self.write_enabled {
                        self.patch_log.push(PatchLogEntry {
                            kind: PatchLogKind::Applying,
                            purpose: pending.purpose.clone(),
                            body: pending.diff.clone(),
                        });
                        pending.decide(PatchDecision::Apply);
                    } else {
                        self.pending_patch = Some(pending);
                        self.patch_window_open = true;
                    }
                }
                PatchUiEvent::Applied { purpose, diff } => {
                    self.patch_log.push(PatchLogEntry {
                        kind: PatchLogKind::Applied,
                        purpose,
                        body: diff,
                    });
                }
                PatchUiEvent::Failed { purpose, message } => {
                    self.patch_log.push(PatchLogEntry {
                        kind: PatchLogKind::Failed,
                        purpose,
                        body: message,
                    });
                }
            }
            context.request_repaint();
        }
    }

    fn set_write_enabled(&mut self, enabled: bool) {
        self.write_enabled = enabled;
        self.patch_gate.set_write_enabled(enabled);
        if enabled && let Some(pending) = self.pending_patch.take() {
            self.patch_log.push(PatchLogEntry {
                kind: PatchLogKind::Approved,
                purpose: pending.purpose.clone(),
                body: pending.diff.clone(),
            });
            pending.decide(PatchDecision::Apply);
        }
    }

    fn reject_pending(&mut self) {
        if let Some(pending) = self.pending_patch.take() {
            self.patch_log.push(PatchLogEntry {
                kind: PatchLogKind::Rejected,
                purpose: pending.purpose.clone(),
                body: pending.diff.clone(),
            });
            pending.decide(PatchDecision::Reject);
        }
    }

    fn stop(&mut self) {
        self.reject_pending();
        self.set_write_enabled(false);
        let _ = self.runtime.shutdown();
        self.stopped = true;
    }

    fn controls(&mut self, ui: &mut eframe::egui::Ui) {
        ui.horizontal(|ui| {
            let mut enabled = self.write_enabled;
            if ui
                .add_enabled(
                    !self.stopped,
                    eframe::egui::Checkbox::new(&mut enabled, "Write mode"),
                )
                .changed()
            {
                self.set_write_enabled(enabled);
            }
            let status = if self.stopped {
                "stopped"
            } else if self.pending_patch.is_some() {
                "waiting for write mode"
            } else if self.write_enabled {
                "automatic writes enabled"
            } else {
                "read only"
            };
            ui.monospace(status);
            if ui
                .selectable_label(self.patch_window_open, "Patches")
                .clicked()
            {
                self.patch_window_open = !self.patch_window_open;
            }
            if ui
                .add_enabled(
                    self.pending_patch.is_some(),
                    eframe::egui::Button::new("Reject"),
                )
                .clicked()
            {
                self.reject_pending();
            }
            if ui
                .add_enabled(!self.stopped, eframe::egui::Button::new("Stop"))
                .clicked()
            {
                self.stop();
            }
        });
    }

    fn patch_panel(&mut self, ui: &mut eframe::egui::Ui) {
        if let Some(pending) = &self.pending_patch {
            ui.colored_label(eframe::egui::Color32::YELLOW, "Waiting for write mode");
            ui.label(&pending.purpose);
            eframe::egui::ScrollArea::vertical()
                .max_height(280.0)
                .show(ui, |ui| {
                    ui.monospace(&pending.diff);
                });
            ui.separator();
        }
        eframe::egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in self.patch_log.iter().rev() {
                ui.strong(format!("{}: {}", entry.kind.label(), entry.purpose));
                ui.monospace(&entry.body);
                ui.separator();
            }
        });
    }

    fn patch_window(&mut self, context: &eframe::egui::Context) {
        if !self.patch_window_open {
            return;
        }
        let mut open = true;
        eframe::egui::Window::new("Patches")
            .id(eframe::egui::Id::new("nuillu-code-patches"))
            .open(&mut open)
            .default_width(640.0)
            .default_height(480.0)
            .resizable(true)
            .show(context, |ui| self.patch_panel(ui));
        self.patch_window_open = open;
    }
}

impl eframe::App for NuilluCodeApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_server_messages(ui.ctx());
        self.drain_patch_events(ui.ctx());
        self.controls(ui);
        ui.separator();
        for message in self.visualizer.show(ui).into_messages() {
            if let Err(error) = self.client_messages.send(message) {
                self.visualizer
                    .record_send_failure(format!("failed to send visualizer message: {error}"));
                break;
            }
        }
        self.patch_window(ui.ctx());
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_millis(1500)
    }

    fn on_exit(&mut self) {
        self.stop();
        let _ = self.client_messages.send(VisualizerClientMessage::Command {
            command: VisualizerCommand::Shutdown,
        });
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "nuillu-code",
    about = "A minimal, safety-bounded Nuillu coding agent"
)]
struct Args {
    /// Workspace root. All coding tools are confined below this directory.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,

    /// Back up the existing agent database, then start with a fresh one.
    #[arg(long)]
    fresh_agent_db: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `retain` silently drops a module that upstream stopped providing, so the
    /// count matters as much as the membership.
    #[test]
    fn boot_config_keeps_every_required_module() {
        let modules = coding_agent_boot_config()
            .modules
            .iter()
            .map(|module| module.id)
            .collect::<Vec<_>>();
        for required in CODING_AGENT_MODULES {
            assert!(
                modules.contains(&required),
                "upstream ServerBootConfig no longer provides {required:?}"
            );
        }
        assert_eq!(
            modules.len(),
            CODING_AGENT_MODULES.len(),
            "unexpected modules survived: {modules:?}"
        );
    }
}
