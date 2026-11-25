use crate::{model::when, phase, session::Interface};
use anyhow::{Context, Result, anyhow, ensure};
use eframe::egui::{self, Button, Color32, Frame, Key, Modifiers, RichText, Stroke, Ui};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub(crate) struct SaveCopy {
    pub device: String,
    pub time: i64,
    pub files: u64,
    pub bytes: u64,
    pub changes: String,
    pub backup: i64,
}

#[derive(Serialize, Deserialize)]
enum Decision {
    Prompt {
        title: String,
        message: String,
        choices: Vec<String>,
        #[serde(default)]
        details: String,
    },
    Conflict {
        game: String,
        copies: [SaveCopy; 2],
    },
}

const DISMISSED: usize = 9;

#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled")
    }
}
impl std::error::Error for Cancelled {}

impl Decision {
    fn ask(self, count: usize) -> Result<usize> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("_decision")
            .stdin(Stdio::piped())
            .spawn()?;
        serde_json::to_writer(child.stdin.take().context("open decision window")?, &self)?;
        let status = child.wait()?;
        if status.code() == Some(DISMISSED as i32) {
            return Err(Cancelled.into());
        }
        let code = status.code().unwrap_or(-1);
        ensure!(
            (10..10 + count as i32).contains(&code),
            "Buddy’s dialog closed unexpectedly ({status})"
        );
        Ok((code - 10) as usize)
    }
}

pub fn choose(title: &str, message: &str, options: &[&str]) -> Result<usize> {
    choose_with_details(title, message, options, "")
}

pub(crate) fn choose_with_details(
    title: &str,
    message: &str,
    options: &[&str],
    details: &str,
) -> Result<usize> {
    Decision::Prompt {
        title: title.into(),
        message: message.into(),
        choices: options.iter().map(|s| s.to_string()).collect(),
        details: details.to_owned(),
    }
    .ask(options.len())
}

pub(crate) fn choose_conflict(game: &str, copies: [SaveCopy; 2]) -> Result<usize> {
    Decision::Conflict {
        game: game.into(),
        copies,
    }
    .ask(3)
}

fn window_frame() -> Frame {
    Frame::new().fill(phase::INK).inner_margin(20)
}

fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(28.).strong().color(phase::TEXT));
}

fn label(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).color(phase::MUTED));
}

fn masthead(ui: &mut Ui, game: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        phase::brand(ui);
        if let Some(game) = game {
            ui.separator();
            label(ui, game);
        }
    });
    ui.add_space(6.);
    ui.separator();
    ui.add_space(8.);
}

fn arrow(ui: &Ui, current: usize, count: usize) -> Option<usize> {
    ui.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
            Some((current + count - 1) % count)
        } else if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
            Some((current + 1) % count)
        } else {
            None
        }
    })
}

fn prompt(
    ui: &mut Ui,
    title: &str,
    message: &str,
    choices: &[String],
    details: &str,
    focus: &mut usize,
    initial: bool,
) -> Option<usize> {
    let navigation = arrow(ui, *focus, choices.len());
    if let Some(index) = navigation {
        *focus = index;
    }
    masthead(ui, None);
    heading(ui, title);
    ui.add_space(6.);
    let remaining = (ui.available_height() - 56.).max(28.);
    egui::ScrollArea::vertical()
        .id_salt("prompt-body")
        .max_height(remaining)
        .show(ui, |ui| {
            ui.label(message);
            if !details.is_empty() && details != message {
                ui.collapsing("Details", |ui| ui.monospace(details));
            }
        });
    ui.add_space(15.);
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        for (i, option) in choices.iter().enumerate() {
            let response = ui.add(if *focus == i {
                phase::primary(option)
            } else {
                Button::new(option)
            });
            if initial || navigation.is_some() {
                if *focus == i {
                    response.request_focus();
                }
            } else if response.has_focus() {
                *focus = i;
            }
            if response.clicked() {
                picked = Some(i);
            }
        }
    });
    picked
}

fn byte_size(bytes: u64) -> String {
    for (unit, scale) in [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

// A radio control provides native keyboard focus; the entire card is a mouse target.
fn copy_card(ui: &mut Ui, copy: &SaveCopy, caption: &str, selected: bool) -> bool {
    let width = ui.available_width();
    let result = phase::panel()
        .inner_margin(14)
        .stroke(Stroke::new(
            1.0_f32,
            if selected { phase::ORANGE } else { phase::LINE },
        ))
        .fill(if selected {
            Color32::from_rgb(35, 28, 24)
        } else {
            phase::PANEL
        })
        .show(ui, |ui| {
            ui.set_width((width - 30.).max(1.));
            let radio = ui.radio(selected, caption);
            ui.add_space(5.);
            ui.label(RichText::new(&copy.device).size(24.).strong());
            ui.label(
                RichText::new(when(copy.time))
                    .monospace()
                    .color(phase::MUTED),
            );
            ui.separator();
            let noun = if copy.files == 1 { "file" } else { "files" };
            ui.label(format!(
                "{} {noun}  ·  {}",
                copy.files,
                byte_size(copy.bytes)
            ))
            .on_hover_text(format!("{} bytes", copy.bytes));
            label(ui, &copy.changes);
            label(ui, format!("Backup #{}", copy.backup));
            radio.clicked()
        });
    result.inner || result.response.interact(egui::Sense::click()).clicked()
}

fn conflict(
    ui: &mut Ui,
    game: &str,
    copies: &[SaveCopy; 2],
    selected: &mut usize,
    initial: bool,
) -> Option<usize> {
    let navigation = arrow(ui, *selected, 3);
    if let Some(index) = navigation {
        *selected = index;
    }
    masthead(ui, Some(game));
    heading(ui, "Save conflict");
    ui.add_space(9.);
    let mut new_selection = None;
    let mut card = |ui: &mut Ui, i: usize| {
        let caption = ["This device", "Server copy"][i];
        if copy_card(ui, &copies[i], caption, *selected == i) {
            new_selection = Some(i);
        }
    };
    if ui.available_width() >= 520. {
        ui.columns(2, |cols| {
            card(&mut cols[0], 0);
            card(&mut cols[1], 1);
        });
    } else {
        card(ui, 0);
        card(ui, 1);
    }
    if let Some(index) = new_selection {
        *selected = index;
    }
    ui.add_space(8.);
    label(ui, "Both versions have permanent backups.");
    ui.add_space(8.);
    ui.horizontal_wrapped(|ui| {
        let later = ui.add(phase::quiet("Decide later"));
        let apply = ui.add_enabled(*selected < 2, phase::primary("Use this save"));
        if initial || navigation.is_some() || new_selection.is_some() {
            if *selected == 2 {
                later.request_focus();
            } else {
                apply.request_focus();
            }
        }
        if later.clicked() {
            Some(2)
        } else if apply.clicked() {
            Some(*selected)
        } else {
            None
        }
    })
    .inner
}

pub fn decision_window() -> Result<i32> {
    let question: Decision = serde_json::from_reader(io::stdin().lock())?;
    let (title, size, mut focus) = match &question {
        Decision::Prompt { title, choices, .. } => {
            ensure!(
                (1..=245).contains(&choices.len()),
                "too many choices for one dialog"
            );
            let safe = choices
                .iter()
                .position(|s| {
                    matches!(
                        s.as_str(),
                        "Cancel" | "Decide later" | "Leave pending" | "Later"
                    )
                })
                .unwrap_or(0);
            (title.clone(), [690., 330.], safe)
        }
        Decision::Conflict { .. } => (
            "Which save should Buddy continue with?".into(),
            [760., 506.],
            2,
        ),
    };
    let result = Arc::new(AtomicUsize::new(DISMISSED));
    let choice = result.clone();
    let mut initial = true;
    eframe::run_simple_native(&title, super::native(size), move |ctx, _| {
        if initial {
            phase::style(ctx);
        }
        egui::CentralPanel::default()
            .frame(window_frame())
            .show(ctx, |ui| {
                if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                let picked = match &question {
                    Decision::Prompt {
                        title,
                        message,
                        choices,
                        details,
                    } if choices.len() <= 3 => {
                        prompt(ui, title, message, choices, details, &mut focus, initial)
                    }
                    Decision::Prompt {
                        title,
                        message,
                        choices,
                        details,
                    } => {
                        egui::ScrollArea::vertical()
                            .show(ui, |ui| {
                                prompt(ui, title, message, choices, details, &mut focus, initial)
                            })
                            .inner
                    }
                    Decision::Conflict { game, copies } => {
                        egui::ScrollArea::vertical()
                            .show(ui, |ui| conflict(ui, game, copies, &mut focus, initial))
                            .inner
                    }
                };
                if let Some(i) = picked {
                    choice.store(i + 10, Ordering::Relaxed);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        // These dialogs use Left/Right to cycle choices, including across the ends.
        // Let Tab navigate normally, but prevent egui from moving twice on an arrow.
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        ..Default::default()
                    },
                );
            }
        });
        initial = false;
    })
    .map_err(|e| anyhow!(e.to_string()))?;
    Ok(result.load(Ordering::Relaxed) as i32)
}

#[derive(Serialize, Deserialize)]
enum Activity {
    Stage(String),
    Upload { completed: u64, total: u64 },
    Download { completed: u64, total: u64 },
}

fn transfer(ui: &mut Ui, upload: bool, completed: u64, total: u64) {
    let fraction = (completed as f64 / total.max(1) as f64).clamp(0., 1.) as f32;
    heading(
        ui,
        if upload {
            "Uploading save"
        } else {
            "Downloading save"
        },
    );
    ui.add_space(7.);
    ui.horizontal(|ui| {
        label(ui, if upload { "This device" } else { "Server" });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            label(ui, if upload { "Server" } else { "This device" });
        });
    });
    let (rail, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.), egui::Sense::hover());
    phase::traces(ui.painter(), rail, Some(fraction));
    ui.horizontal(|ui| {
        let units = format!("{} / {}", byte_size(completed), byte_size(total));
        ui.label(RichText::new(units).monospace().color(phase::MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{}%", (fraction * 100.).floor() as u32)).strong());
        });
    });
}

pub fn progress_window() -> Result<()> {
    let (tx, rx) = mpsc::channel::<(String, Activity)>();
    thread::spawn(move || {
        for line in io::stdin()
            .lock()
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Ok(update) = serde_json::from_str(&line)
                && tx.send(update).is_err()
            {
                break;
            }
        }
        // Steam may remove the wrapper while the compositor is stalled. EOF must close
        // this process even if the UI never receives another redraw.
        std::process::exit(0);
    });
    let mut game = String::new();
    let mut status = Activity::Stage("Checking game files…".into());
    let mut initial = true;
    eframe::run_simple_native("Buddy sync", super::native([465., 285.]), move |ctx, _| {
        if initial {
            phase::style(ctx);
            initial = false;
        }
        for (title, update) in rx.try_iter() {
            game = title;
            status = update;
        }
        egui::CentralPanel::default()
            .frame(window_frame())
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    masthead(ui, (!game.is_empty()).then_some(game.as_str()));
                    match &status {
                        Activity::Stage(message) => {
                            ui.horizontal_wrapped(|ui| {
                                ui.spinner();
                                ui.label(message);
                            });
                        }
                        Activity::Upload { completed, total } => {
                            transfer(ui, true, *completed, *total)
                        }
                        Activity::Download { completed, total } => {
                            transfer(ui, false, *completed, *total)
                        }
                    }
                });
            });
        ctx.request_repaint_after(Duration::from_millis(120));
    })
    .map_err(|e| anyhow!(e.to_string()))
}

// The pipe ties the indicator to this process even when Steam terminates the wrapper.
#[derive(Default)]
struct ProgressChild {
    stopped: bool,
    child: Option<Child>,
}
pub struct Progress {
    tx: Option<mpsc::Sender<Activity>>,
    child: Arc<Mutex<ProgressChild>>,
}
impl Drop for Progress {
    fn drop(&mut self) {
        self.tx.take();
        let mut state = self.child.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        if let Some(mut child) = state.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Progress {
    pub fn start(ui: Interface, game: &str, message: &str) -> Self {
        let command = if matches!(ui, Interface::Window) {
            std::env::current_exe().ok().map(|binary| {
                let mut command = Command::new(binary);
                command.arg("_progress");
                command
            })
        } else {
            None
        };
        Self::start_with(command, game, message, Duration::from_secs(1))
    }
    fn start_with(command: Option<Command>, game: &str, message: &str, delay: Duration) -> Self {
        let child = Arc::new(Mutex::new(ProgressChild::default()));
        let Some(mut command) = command else {
            return Self { tx: None, child };
        };
        let (tx, rx) = mpsc::channel();
        let game = game.to_owned();
        let mut status = Activity::Stage(message.to_owned());
        let worker_child = Arc::clone(&child);
        thread::spawn(move || {
            let deadline = Instant::now() + delay;
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(next) => status = next,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
            let mut pipe = {
                let mut state = worker_child.lock().unwrap_or_else(|e| e.into_inner());
                if state.stopped {
                    return;
                }
                let Ok(mut process) = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    return;
                };
                let pipe = process.stdin.take();
                state.child = Some(process);
                pipe
            };
            if let Some(ref mut pipe) = pipe {
                for update in std::iter::once(status).chain(rx) {
                    if serde_json::to_writer(&mut *pipe, &(&game, update)).is_err()
                        || writeln!(pipe).is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            tx: Some(tx),
            child,
        }
    }
    pub fn uploaded(&self, completed: u64, total: u64) {
        if total > 0
            && let Some(tx) = &self.tx
        {
            let _ = tx.send(Activity::Upload { completed, total });
        }
    }
    pub fn downloaded(&self, completed: u64, total: u64) {
        if total > 0
            && let Some(tx) = &self.tx
        {
            let _ = tx.send(Activity::Download { completed, total });
        }
    }
    pub fn status(&self, text: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Activity::Stage(text.into()));
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod progress_tests {
    use super::*;

    #[test]
    fn dropping_progress_stops_helper_even_when_pipe_is_stalled() {
        let progress = Progress::start_with(
            Some({
                let mut command = Command::new("sleep");
                command.arg("30");
                command
            }),
            "game",
            "syncing",
            Duration::ZERO,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = progress.child.lock().unwrap().child.as_ref().map(Child::id) {
                break pid;
            }
            assert!(Instant::now() < deadline, "progress helper did not start");
            thread::yield_now();
        };
        progress.status(&"x".repeat(1_000_000));
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        drop(progress);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn dropping_fast_progress_never_starts_helper() {
        let progress = Progress::start_with(
            Some(Command::new("sleep")),
            "game",
            "syncing",
            Duration::from_millis(150),
        );
        let child = Arc::clone(&progress.child);
        drop(progress);
        thread::sleep(Duration::from_millis(250));
        let state = child.lock().unwrap();
        assert!(state.stopped && state.child.is_none());
    }
}
