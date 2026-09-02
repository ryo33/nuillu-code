use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use nuillu_code_module::{
    GitControlCommand, GitControlHandle, GitUiEvent, GitUiState, GitWorkspace, WorkspaceMode,
};
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
    let open = GitWorkspace::open(&args.cwd)?;
    let workspace = open.workspace;
    let git = open.git;
    let controls = open.controls;
    let control_receiver = open.control_receiver;
    let git_events = open.ui_events;
    let state_dir = open.state_dir;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build startup validation runtime")?
        .block_on(workspace.verify_state_dir_is_ignored())?;
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

    let registrars = vec![nuillu_code_module::registrar(
        workspace.clone(),
        git.clone(),
        control_receiver,
    )];
    let runtime = Server::new(config)
        .module_registrars(registrars)
        .spawn()
        .context("start embedded Nuillu runtime")?;
    let (server_messages, client_messages) = runtime.visualizer_channels();
    let app_runtime = runtime.clone();
    let native_options = native_options();
    let title = format!(
        "Nuillu Code — {}",
        state_dir.parent().unwrap_or(&state_dir).display()
    );
    let result = eframe::run_native(
        &title,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(NuilluCodeApp::new(
                server_messages,
                client_messages,
                app_runtime,
                controls,
                git_events,
            )))
        }),
    );
    let _ = runtime.shutdown();
    match runtime.join_timeout(Duration::from_secs(2)) {
        Ok(true) => {}
        Ok(false) => eprintln!("embedded Nuillu runtime did not stop within two seconds"),
        Err(error) => eprintln!("embedded Nuillu runtime failed during shutdown: {error:#}"),
    }
    let _ = git.cleanup();
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
    controls: GitControlHandle,
    git_events: Receiver<GitUiEvent>,
    git_state: Option<GitUiState>,
    errors: Vec<String>,
    expanded_commits: BTreeSet<String>,
    patch_window_open: bool,
    stopped: bool,
}

impl NuilluCodeApp {
    fn new(
        server_messages: Receiver<VisualizerServerMessage>,
        client_messages: Sender<VisualizerClientMessage>,
        runtime: ServerRuntimeHandle,
        controls: GitControlHandle,
        git_events: Receiver<GitUiEvent>,
    ) -> Self {
        Self {
            visualizer: Visualizer::with_config(
                eframe::egui::Id::new("nuillu-code-visualizer"),
                VisualizerConfig::standalone(),
            ),
            server_messages,
            client_messages,
            runtime,
            controls,
            git_events,
            git_state: None,
            errors: Vec::new(),
            expanded_commits: BTreeSet::new(),
            patch_window_open: false,
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

    fn drain_git_events(&mut self, context: &eframe::egui::Context) {
        while let Ok(event) = self.git_events.try_recv() {
            match event {
                GitUiEvent::State(state) => {
                    if !state.commits.is_empty() {
                        self.patch_window_open = true;
                    }
                    self.git_state = Some(state);
                }
                GitUiEvent::Error(message) => {
                    self.errors.push(message);
                    self.patch_window_open = true;
                }
                GitUiEvent::Sensory(content) => {
                    if let Err(error) =
                        self.runtime
                            .send_one_shot("vision", Some("workspace".to_owned()), content)
                    {
                        self.errors.push(format!(
                            "failed to publish workspace sensory input: {error:#}"
                        ));
                    }
                }
            }
            context.request_repaint();
        }
    }

    fn stop(&mut self) {
        let _ = self.runtime.shutdown();
        self.stopped = true;
    }

    fn controls(&mut self, ui: &mut eframe::egui::Ui) {
        ui.horizontal(|ui| {
            let current = self.git_state.as_ref().map(|state| state.mode);
            for (mode, label) in [
                (WorkspaceMode::ReadOnly, "Read-only"),
                (WorkspaceMode::Review, "Review"),
                (WorkspaceMode::Write, "Write"),
            ] {
                if ui
                    .add_enabled(
                        !self.stopped && current.is_some(),
                        eframe::egui::Button::new(label).selected(current == Some(mode)),
                    )
                    .clicked()
                    && let Err(error) = self.controls.send(GitControlCommand::SetMode(mode))
                {
                    self.errors.push(format!("{error:#}"));
                }
            }
            if let Some(state) = &self.git_state {
                ui.monospace(format!(
                    "{} · {} pending",
                    state.branch,
                    state.commits.len()
                ));
            }
            if ui
                .selectable_label(self.patch_window_open, "Patches")
                .clicked()
            {
                self.patch_window_open = !self.patch_window_open;
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
        let commits = self
            .git_state
            .as_ref()
            .map(|state| state.commits.clone())
            .unwrap_or_default();
        if !commits.is_empty()
            && ui.button("Apply all").clicked()
            && let Err(error) = self.controls.send(GitControlCommand::ApplyAll)
        {
            self.errors.push(format!("{error:#}"));
        }
        eframe::egui::ScrollArea::vertical().show(ui, |ui| {
            for commit in commits.iter().rev() {
                ui.strong(&commit.purpose);
                ui.monospace(&commit.id);
                ui.label(commit.changed_paths.join(", "));
                ui.horizontal(|ui| {
                    let expanded = self.expanded_commits.contains(&commit.id);
                    if ui.button(if expanded { "Hide" } else { "View" }).clicked() {
                        if expanded {
                            self.expanded_commits.remove(&commit.id);
                        } else {
                            self.expanded_commits.insert(commit.id.clone());
                        }
                    }
                    if ui.button("Apply").clicked()
                        && let Err(error) = self
                            .controls
                            .send(GitControlCommand::Apply(commit.id.clone()))
                    {
                        self.errors.push(format!("{error:#}"));
                    }
                    if ui.button("Discard").clicked()
                        && let Err(error) = self
                            .controls
                            .send(GitControlCommand::Discard(commit.id.clone()))
                    {
                        self.errors.push(format!("{error:#}"));
                    }
                });
                if self.expanded_commits.contains(&commit.id) {
                    ui.monospace(&commit.diff);
                }
                ui.separator();
            }
            for error in self.errors.iter().rev() {
                ui.colored_label(eframe::egui::Color32::RED, error);
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
        self.drain_git_events(ui.ctx());
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
