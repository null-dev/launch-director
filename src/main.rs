use std::collections::HashSet;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Context, Result, bail, eyre};
use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use tempfile::NamedTempFile;

fn main() -> Result<()> {
    color_eyre::install()?;

    let args = CliArgs::parse()?;
    ensure_required_tasks(&args.project)?;
    let project_name = project_display_name(&args.project);

    match run_build_window(&args.project, &project_name)? {
        BuildOutcome::Succeeded => {}
        BuildOutcome::Failed(code) => bail!("Build task failed with exit code {code}."),
        BuildOutcome::Aborted => bail!("Build window was closed before the build finished."),
    }

    if let Some(quick_failure) = run_task_with_quick_failure_capture(&args.project)? {
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
struct CliArgs {
    project: PathBuf,
}

impl CliArgs {
    fn parse() -> Result<Self> {
        let mut args = env::args().skip(1);
        let parsed = match (args.next(), args.next(), args.next()) {
            (Some(flag), Some(project), None) if flag == "--project" => Self {
                project: PathBuf::from(project),
            },
            _ => bail!("Usage: launch-director --project /path/to/project"),
        };

        if !parsed.project.is_dir() {
            bail!(
                "Project path '{}' is not a directory.",
                parsed.project.display()
            );
        }

        Ok(parsed)
    }
}

fn project_display_name(project: &Path) -> String {
    project
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| project.display().to_string())
}

fn ensure_required_tasks(project: &Path) -> Result<()> {
    let tasks = list_mise_tasks(project)?;
    for required in ["_launch_director_build", "_launch_director_run"] {
        if !tasks.contains(required) {
            let mut sorted: Vec<_> = tasks.into_iter().collect();
            sorted.sort();
            bail!(
                "Required task '{}' not found. Discovered tasks: {}",
                required,
                sorted.join(", ")
            );
        }
    }
    Ok(())
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
        for name in ["_launch_director_build", "_launch_director_run"] {
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

fn run_build_window(project: &Path, project_name: &str) -> Result<BuildOutcome> {
    let process = RunningProcess::spawn(project, "_launch_director_build")?;
    let result = Arc::new(Mutex::new(None));
    let result_for_ui = Arc::clone(&result);
    let app = BuildWindowApp::new(project_name.to_string(), process, result_for_ui);

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Launch Director - Build",
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
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

fn run_task_with_quick_failure_capture(project: &Path) -> Result<Option<QuickRunFailure>> {
    let output_file = NamedTempFile::new().wrap_err("Failed to create temporary output file")?;
    let stdout_file = OpenOptions::new()
        .append(true)
        .open(output_file.path())
        .wrap_err("Failed to open temp output file for stdout")?;
    let stderr_file = stdout_file
        .try_clone()
        .wrap_err("Failed to clone temp output file for stderr")?;

    let mut child = Command::new("mise")
        .arg("run")
        .arg("_launch_director_run")
        .current_dir(project)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .wrap_err("Failed to launch `_launch_director_run` task")?;

    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .wrap_err("Failed while waiting for `_launch_director_run`")?
        {
            if status.success() {
                return Ok(None);
            }

            let output = fs::read_to_string(output_file.path()).unwrap_or_default();
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
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Launch Director - Run Failure",
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|err| eyre!("Failed to open run failure window: {err}"))?;

    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

struct RunningProcess {
    child: Child,
    output_rx: Receiver<String>,
}

impl RunningProcess {
    fn spawn(project: &Path, task: &str) -> Result<Self> {
        let mut child = Command::new("mise")
            .arg("run")
            .arg(task)
            .current_dir(project)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err_with(|| format!("Failed to start `{task}` task"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre!("Failed to capture stdout for spawned task"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| eyre!("Failed to capture stderr for spawned task"))?;

        let (tx, rx) = mpsc::channel();
        spawn_reader(stdout, tx.clone());
        spawn_reader(stderr, tx);

        Ok(Self {
            child,
            output_rx: rx,
        })
    }
}

fn spawn_reader<R: Read + Send + 'static>(reader: R, tx: Sender<String>) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            match line {
                Ok(line) => {
                    let _ = tx.send(format!("{line}\n"));
                }
                Err(err) => {
                    let _ = tx.send(format!("<<failed to read output: {err}>>\n"));
                    break;
                }
            }
        }
    });
}

struct BuildWindowApp {
    project_name: String,
    output: String,
    process: Option<RunningProcess>,
    final_result: Arc<Mutex<Option<BuildOutcome>>>,
    failure_code: Option<i32>,
}

impl BuildWindowApp {
    fn new(
        project_name: String,
        process: RunningProcess,
        final_result: Arc<Mutex<Option<BuildOutcome>>>,
    ) -> Self {
        Self {
            project_name,
            output: String::new(),
            process: Some(process),
            final_result,
            failure_code: None,
        }
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
        if let Some(process) = self.process.as_mut() {
            let _ = process.child.kill();
            let _ = process.child.wait();
            self.set_result_once(BuildOutcome::Aborted);
        }
    }
}

impl eframe::App for BuildWindowApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(process) = self.process.as_mut() {
            while let Ok(chunk) = process.output_rx.try_recv() {
                self.output.push_str(&chunk);
            }

            match process.child.try_wait() {
                Ok(Some(status)) => {
                    let code = exit_code(status);
                    if status.success() {
                        self.set_result_once(BuildOutcome::Succeeded);
                        self.process = None;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        self.failure_code = Some(code);
                        self.set_result_once(BuildOutcome::Failed(code));
                        self.process = None;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    self.output
                        .push_str(&format!("<<failed while waiting for build: {err}>>\n"));
                    self.failure_code = Some(-1);
                    self.set_result_once(BuildOutcome::Failed(-1));
                    self.process = None;
                }
            }
        }

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

        egui::TopBottomPanel::bottom("build_status").show(ctx, |ui| {
            ui.horizontal(|ui| {
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
            });
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
