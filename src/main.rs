use std::collections::HashSet;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, error::ErrorKind};
use color_eyre::eyre::{Context, Result, bail, eyre};
use eframe::egui::{self, Color32, RichText};
use egui_term::{BackendCommand, BackendSettings, PtyEvent, TerminalBackend, TerminalView};
use serde_json::Value;
use tempfile::tempfile;

fn main() {
    if let Err(err) = run() {
        println!("{err:?}");
        if !should_suppress_error_dialog(&err) {
            show_error_dialog(&err.to_string());
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    color_eyre::install()?;

    let args = parse_args()?;
    let tasks = resolve_project_tasks(&args.project)?;
    let project_name = project_display_name(&args.project);

    match run_build_window(&args.project, &project_name, &tasks.build_task)? {
        BuildOutcome::Succeeded => {}
        BuildOutcome::Failed(code) => return Err(BuildTaskFailed { exit_code: code }.into()),
        BuildOutcome::Aborted => bail!("Build cancelled."),
    }

    if let Some(quick_failure) =
        run_task_with_quick_failure_capture(&args.project, &tasks.run_task)?
    {
        show_run_failure_window(
            &project_name,
            quick_failure.exit_code,
            &quick_failure.output,
        )?;
        bail!(
            "Run task exited with non-zero code {} within 2 seconds.",
            quick_failure.exit_code
        );
    }

    Ok(())
}

#[derive(Debug)]
struct BuildTaskFailed {
    exit_code: i32,
}

impl fmt::Display for BuildTaskFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Build task failed with exit code {}.", self.exit_code)
    }
}

impl std::error::Error for BuildTaskFailed {}

fn should_suppress_error_dialog(err: &color_eyre::Report) -> bool {
    err.downcast_ref::<BuildTaskFailed>().is_some()
}

fn show_error_dialog(message: &str) {
    let title = "Launch Director - Error";
    let app = ErrorDialogApp::new(message.to_string());
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 240.0])
            .with_min_inner_size([520.0, 180.0]),
        ..Default::default()
    };
    let _ = eframe::run_native(title, native_options, Box::new(|_cc| Ok(Box::new(app))));
}

fn parse_args() -> Result<CliArgs> {
    match CliArgs::try_parse() {
        Ok(args) => Ok(args),
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                println!("{err}");
                std::process::exit(0);
            }
            _ => bail!("{err}"),
        },
    }
}

struct ErrorDialogApp {
    message: String,
}

impl ErrorDialogApp {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl eframe::App for ErrorDialogApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::bottom("error_dialog_controls").show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(RichText::new("Launch Director encountered an error:").strong());
            ui.separator();

            ui.add_sized(
                ui.available_size(),
                egui::TextEdit::multiline(&mut self.message)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .interactive(false),
            );
        });
    }
}

/// Launch and monitor locally developed programs via project-defined `mise` tasks, showing build output UI only for builds longer than 2s.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Project directory with `mise` tasks: prefer `_launch_director_build`/`_launch_director_run`, fall back to `build`/`run`.
    #[arg(short, long, value_name = "PATH", value_parser = parse_project_dir)]
    project: PathBuf,
}

fn parse_project_dir(value: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_dir() {
        return Err(format!(
            "Project path '{}' is not a directory.",
            path.display()
        ));
    }
    Ok(path)
}

fn project_display_name(project: &Path) -> String {
    project
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| project.display().to_string())
}

struct ProjectTasks {
    build_task: String,
    run_task: String,
}

fn resolve_project_tasks(project: &Path) -> Result<ProjectTasks> {
    let tasks = list_mise_tasks(project)?;
    let build_task = resolve_task_name(&tasks, &["_launch_director_build", "build"], "build")?;
    let run_task = resolve_task_name(&tasks, &["_launch_director_run", "run"], "run")?;
    Ok(ProjectTasks {
        build_task,
        run_task,
    })
}

fn resolve_task_name(
    tasks: &HashSet<String>,
    candidates: &[&str],
    task_kind: &str,
) -> Result<String> {
    for candidate in candidates {
        if tasks.contains(*candidate) {
            return Ok((*candidate).to_string());
        }
    }

    let mut sorted: Vec<_> = tasks.iter().cloned().collect();
    sorted.sort();
    bail!(
        "Could not find a {} task. Define one of: {}. Discovered tasks: {}",
        task_kind,
        candidates.join(", "),
        sorted.join(", ")
    );
}

fn list_mise_tasks(project: &Path) -> Result<HashSet<String>> {
    let output = Command::new("mise")
        .args(["tasks", "ls", "--local", "--json"])
        .current_dir(project)
        .output()
        .wrap_err("Failed to run `mise tasks ls --local --json`")?;

    if !output.status.success() {
        bail!(
            "Failed to list mise tasks: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let raw = String::from_utf8(output.stdout).wrap_err("mise task output was not valid UTF-8")?;
    let value: Value = serde_json::from_str(&raw).wrap_err("Failed to parse mise task JSON")?;
    let mut names = HashSet::new();
    collect_task_names(&value, &mut names);

    if names.is_empty() {
        for name in [
            "_launch_director_build",
            "_launch_director_run",
            "build",
            "run",
        ] {
            if raw.contains(&format!("\"{name}\"")) {
                names.insert(name.to_string());
            }
        }
    }

    Ok(names)
}

fn collect_task_names(value: &Value, names: &mut HashSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_task_names(item, names);
            }
        }
        Value::Object(map) => {
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                names.insert(name.to_string());
            }

            if let Some(tasks) = map.get("tasks") {
                collect_task_names(tasks, names);
            }

            for (key, val) in map {
                if let Value::Object(task_like) = val {
                    if task_like.contains_key("run")
                        || task_like.contains_key("alias")
                        || task_like.contains_key("description")
                    {
                        names.insert(key.clone());
                    }
                }

                if val.is_object() || val.is_array() {
                    collect_task_names(val, names);
                }
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy)]
enum BuildOutcome {
    Succeeded,
    Failed(i32),
    Aborted,
}

fn run_build_window(project: &Path, project_name: &str, build_task: &str) -> Result<BuildOutcome> {
    let session = BuildTerminalSession::start(project.to_path_buf(), build_task.to_string())?;
    let start = Instant::now();
    let mut pre_ui_failure_code = None;
    loop {
        while let Ok((_, event)) = session.pty_event_rx.try_recv() {
            match event {
                PtyEvent::ChildExit(code) => {
                    if code == 0 {
                        return Ok(BuildOutcome::Succeeded);
                    }
                    pre_ui_failure_code = Some(code);
                    break;
                }
                PtyEvent::Exit => {
                    pre_ui_failure_code = Some(-1);
                    break;
                }
                _ => {}
            }
        }

        if pre_ui_failure_code.is_some() || start.elapsed() >= Duration::from_secs(2) {
            break;
        }

        thread::sleep(Duration::from_millis(50));
    }

    let result = Arc::new(Mutex::new(None));
    let result_for_ui = Arc::clone(&result);
    let project_name_for_app = project_name.to_string();
    let title = format!("Launch Director - {project_name} - Build");

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        &title,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(BuildWindowApp::new(
                project_name_for_app,
                session,
                pre_ui_failure_code,
                result_for_ui,
            )))
        }),
    )
    .map_err(|err| eyre!("Failed to open build output window: {err}"))?;

    let final_result = result
        .lock()
        .map(|mut guard| guard.take().unwrap_or(BuildOutcome::Aborted))
        .unwrap_or(BuildOutcome::Aborted);

    Ok(final_result)
}

#[derive(Debug)]
struct QuickRunFailure {
    exit_code: i32,
    output: String,
}

fn run_task_with_quick_failure_capture(
    project: &Path,
    run_task: &str,
) -> Result<Option<QuickRunFailure>> {
    let mut output_file = tempfile().wrap_err("Failed to create temporary output file")?;
    let stdout_file = output_file
        .try_clone()
        .wrap_err("Failed to clone temp output file for stdout")?;
    let stderr_file = output_file
        .try_clone()
        .wrap_err("Failed to clone temp output file for stderr")?;

    let mut child = Command::new("mise")
        .arg("run")
        .arg(run_task)
        .current_dir(project)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .wrap_err_with(|| format!("Failed to launch `{run_task}` task"))?;

    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .wrap_err_with(|| format!("Failed while waiting for `{run_task}`"))?
        {
            if status.success() {
                return Ok(None);
            }

            let mut output = String::new();
            let _ = output_file.seek(SeekFrom::Start(0));
            let _ = output_file.read_to_string(&mut output);
            return Ok(Some(QuickRunFailure {
                exit_code: exit_code(status),
                output,
            }));
        }

        if start.elapsed() >= Duration::from_secs(2) {
            return Ok(None);
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn show_run_failure_window(project_name: &str, exit_code: i32, output: &str) -> Result<()> {
    let app = FailureWindowApp::new(project_name.to_string(), exit_code, output.to_string());
    let title = format!("Launch Director - {project_name} - Run Failure");
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(&title, native_options, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|err| eyre!("Failed to open run failure window: {err}"))?;

    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

struct BuildTerminalSession {
    terminal_backend: TerminalBackend,
    pty_event_rx: Receiver<(u64, PtyEvent)>,
}

impl BuildTerminalSession {
    fn start(project: PathBuf, build_task: String) -> Result<Self> {
        let (pty_event_tx, pty_event_rx) = mpsc::channel();
        let terminal_backend = TerminalBackend::new(
            0,
            egui::Context::default(),
            pty_event_tx,
            BackendSettings {
                shell: "mise".to_string(),
                args: vec!["run".to_string(), build_task],
                working_directory: Some(project),
            },
        )
        .wrap_err("Failed to start build task in terminal backend")?;

        Ok(Self {
            terminal_backend,
            pty_event_rx,
        })
    }
}

struct BuildWindowApp {
    project_name: String,
    terminal_backend: TerminalBackend,
    pty_event_rx: Receiver<(u64, PtyEvent)>,
    final_result: Arc<Mutex<Option<BuildOutcome>>>,
    failure_code: Option<i32>,
    build_exited: bool,
}

impl BuildWindowApp {
    fn new(
        project_name: String,
        session: BuildTerminalSession,
        initial_failure_code: Option<i32>,
        final_result: Arc<Mutex<Option<BuildOutcome>>>,
    ) -> Self {
        let BuildTerminalSession {
            terminal_backend,
            pty_event_rx,
        } = session;
        let app = Self {
            project_name,
            terminal_backend,
            pty_event_rx,
            final_result,
            failure_code: initial_failure_code,
            build_exited: initial_failure_code.is_some(),
        };
        if let Some(code) = initial_failure_code {
            app.set_result_once(BuildOutcome::Failed(code));
        }
        app
    }

    fn set_result_once(&self, outcome: BuildOutcome) {
        if let Ok(mut guard) = self.final_result.lock() {
            if guard.is_none() {
                *guard = Some(outcome);
            }
        }
    }
}

impl Drop for BuildWindowApp {
    fn drop(&mut self) {
        self.set_result_once(BuildOutcome::Aborted);
    }
}

impl eframe::App for BuildWindowApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        while let Ok((_, event)) = self.pty_event_rx.try_recv() {
            match event {
                PtyEvent::ChildExit(code) => {
                    self.build_exited = true;
                    if code == 0 {
                        self.set_result_once(BuildOutcome::Succeeded);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        self.failure_code = Some(code);
                        self.set_result_once(BuildOutcome::Failed(code));
                    }
                }
                PtyEvent::Exit if !self.build_exited && !close_requested => {
                    self.build_exited = true;
                    self.failure_code = Some(-1);
                    self.set_result_once(BuildOutcome::Failed(-1));
                }
                _ => {}
            }
        }

        egui::TopBottomPanel::bottom("build_status").show(ctx, |ui| {
            let row_height = ui.spacing().interact_size.y;
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), row_height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if let Some(code) = self.failure_code {
                        ui.label(RichText::new("X").color(Color32::RED).strong());
                        ui.label(
                            RichText::new(format!(
                                "Build {} exited with code {}.",
                                self.project_name, code
                            ))
                            .color(Color32::RED),
                        );
                    } else {
                        ui.add(egui::Spinner::new());
                        ui.label(format!("Building {}...", self.project_name));
                    }

                    let remaining = ui.available_size_before_wrap();
                    ui.allocate_ui_with_layout(
                        egui::vec2(remaining.x.max(0.0), row_height),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if self.failure_code.is_some() {
                                if ui.button("Close").clicked() {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            } else if !self.build_exited && ui.button("Cancel").clicked() {
                                self.terminal_backend
                                    .process_command(BackendCommand::Write(vec![3]));
                            }
                        },
                    );
                },
            );
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let terminal = TerminalView::new(ui, &mut self.terminal_backend)
                .set_focus(true)
                .set_size(ui.available_size());
            ui.add(terminal);
        });

        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

struct FailureWindowApp {
    project_name: String,
    output: String,
    exit_code: i32,
}

impl FailureWindowApp {
    fn new(project_name: String, exit_code: i32, output: String) -> Self {
        Self {
            project_name,
            output,
            exit_code,
        }
    }
}

impl eframe::App for FailureWindowApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.output)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .interactive(false),
                    );
                });
        });

        egui::TopBottomPanel::bottom("run_failure_status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("X").color(Color32::RED).strong());
                ui.label(
                    RichText::new(format!(
                        "{} exited with code {}",
                        self.project_name, self.exit_code
                    ))
                    .color(Color32::RED),
                );
            });
        });
    }
}
