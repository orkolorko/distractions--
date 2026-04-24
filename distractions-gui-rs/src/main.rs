//! GTK3 frontend for distractions--.sh — Rust port of distractions_gui.py.
//!
//! Runs unprivileged. Shells out to the bash script via pkexec with the
//! `--no-gui` wrapper protocol (PROGRESS:<pct>:<msg> on stdout, ERROR lines
//! on stderr). Never touches /etc/hosts, chattr, systemd, or /var/lib/hardblock
//! itself — the bash script owns all privileged work.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gio::prelude::*;
use gio::{Cancellable, DataInputStream, Subprocess, SubprocessFlags};
use glib::{clone, ControlFlow, Priority};
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, ButtonsType, ComboBoxText,
    Dialog, DialogFlags, Entry, Label, MessageDialog, MessageType, Orientation, PolicyType,
    ProgressBar, ResponseType, ScrolledWindow, SpinButton, Stack, TextView,
};

const APP_ID: &str = "org.distractions.gui";
const STATE_DIR: &str = "/var/lib/hardblock";
const SCRIPT_NAME: &str = "distractions--.sh";
const BLOCKLIST_NAME: &str = "blocklists.txt";

const PRESETS: &[(&str, &str)] = &[
    ("none", "No preset (use custom sites only)"),
    ("all", "All categories"),
    ("social", "Social media only"),
    ("adult", "Adult content only"),
    ("timewasters", "Time wasters only (YouTube, Netflix, ...)"),
];

const DURATION_UNITS: &[(&str, &str)] = &[
    ("minutes", "m"),
    ("hours", "h"),
    ("days", "d"),
];

extern "C" {
    fn geteuid() -> u32;
}

fn is_root() -> bool {
    unsafe { geteuid() == 0 }
}

fn end_time_path() -> PathBuf {
    PathBuf::from(STATE_DIR).join("end_time")
}

fn active_file_path() -> PathBuf {
    PathBuf::from(STATE_DIR).join("block_active")
}

fn read_end_time() -> i64 {
    std::fs::read_to_string(end_time_path())
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    glib::DateTime::now_local()
        .map(|dt| dt.to_unix())
        .unwrap_or(0)
}

fn block_is_active() -> bool {
    active_file_path().exists() && read_end_time() > now_unix()
}

fn format_remaining(seconds: i64) -> String {
    if seconds <= 0 {
        return "00:00:00".to_string();
    }
    let days = seconds / 86400;
    let rem = seconds % 86400;
    let hours = rem / 3600;
    let rem = rem % 3600;
    let minutes = rem / 60;
    let secs = rem % 60;
    if days > 0 {
        format!("{}d {:02}:{:02}:{:02}", days, hours, minutes, secs)
    } else {
        format!("{:02}:{:02}:{:02}", hours, minutes, secs)
    }
}

/// Locate distractions--.sh. Search order: $DISTRACTIONS_SCRIPT env var,
/// then walk up from the binary's location, then common install paths.
fn find_script() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DISTRACTIONS_SCRIPT") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            let candidate = d.join(SCRIPT_NAME);
            if candidate.exists() {
                return Some(candidate);
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    for p in ["/usr/local/bin", "/usr/bin"] {
        let pb = Path::new(p).join(SCRIPT_NAME);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

/// Snap-installed VS Code leaks GTK_PATH / GIO_MODULE_DIR / GSETTINGS_SCHEMA_DIR
/// into child processes, which drags in snap's older GTK modules and breaks the
/// loader. Drop them before GTK initializes.
fn scrub_snap_env() {
    for var in ["GTK_PATH", "GIO_MODULE_DIR", "GSETTINGS_SCHEMA_DIR"] {
        if let Ok(v) = std::env::var(var) {
            if v.contains("/snap/") {
                std::env::remove_var(var);
            }
        }
    }
}

struct Ui {
    window: ApplicationWindow,
    stack: Stack,
    duration_value: SpinButton,
    duration_unit: ComboBoxText,
    preset_combo: ComboBoxText,
    sites_entry: Entry,
    progress_label: Label,
    progress_bar: ProgressBar,
    activate_btn: Button,
    timer_label: Label,
    end_label: Label,
    script_path: PathBuf,
    blocklist_path: PathBuf,
    stderr_buffer: RefCell<Vec<String>>,
    countdown_running: RefCell<bool>,
}

fn build_ui(app: &Application, script_path: PathBuf) {
    let blocklist_path = script_path
        .parent()
        .map(|p| p.join(BLOCKLIST_NAME))
        .unwrap_or_else(|| PathBuf::from(BLOCKLIST_NAME));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("distractions--")
        .default_width(540)
        .default_height(460)
        .build();

    let stack = Stack::new();
    window.add(&stack);

    // ---- Setup view ----
    let setup_box = GtkBox::new(Orientation::Vertical, 12);
    setup_box.set_border_width(16);

    let warning = Label::new(None);
    warning.set_markup(
        "<span foreground=\"#cc0000\" weight=\"bold\" size=\"large\">\
WARNING: activation cannot be reversed until the timer expires.\
</span>",
    );
    warning.set_line_wrap(true);
    warning.set_xalign(0.0);
    setup_box.pack_start(&warning, false, false, 0);

    let dur_row = GtkBox::new(Orientation::Horizontal, 8);
    let dur_lbl = Label::new(Some("Duration:"));
    dur_lbl.set_xalign(0.0);
    dur_row.pack_start(&dur_lbl, false, false, 0);
    let duration_value = SpinButton::with_range(1.0, 999.0, 1.0);
    duration_value.set_value(1.0);
    dur_row.pack_start(&duration_value, false, false, 0);
    let duration_unit = ComboBoxText::new();
    for (label, _) in DURATION_UNITS {
        duration_unit.append_text(label);
    }
    duration_unit.set_active(Some(1));
    dur_row.pack_start(&duration_unit, false, false, 0);
    setup_box.pack_start(&dur_row, false, false, 0);

    let preset_outer = GtkBox::new(Orientation::Vertical, 4);
    let preset_lbl = Label::new(Some("Preset category:"));
    preset_lbl.set_xalign(0.0);
    preset_outer.pack_start(&preset_lbl, false, false, 0);
    let preset_row = GtkBox::new(Orientation::Horizontal, 8);
    let preset_combo = ComboBoxText::new();
    for (key, label) in PRESETS {
        preset_combo.append(Some(key), label);
    }
    preset_combo.set_active_id(Some("all"));
    preset_row.pack_start(&preset_combo, true, true, 0);
    let edit_btn = Button::with_label("Edit list...");
    edit_btn.set_tooltip_text(Some(
        "Edit blocklists.txt \u{2014} changes apply to the next activation.",
    ));
    preset_row.pack_start(&edit_btn, false, false, 0);
    preset_outer.pack_start(&preset_row, false, false, 0);
    setup_box.pack_start(&preset_outer, false, false, 0);

    let sites_box = GtkBox::new(Orientation::Vertical, 4);
    let sites_lbl = Label::new(Some("Additional sites (space-separated):"));
    sites_lbl.set_xalign(0.0);
    sites_box.pack_start(&sites_lbl, false, false, 0);
    let sites_entry = Entry::new();
    sites_entry.set_placeholder_text(Some("e.g. news.ycombinator.com hn.algolia.com"));
    sites_box.pack_start(&sites_entry, false, false, 0);
    setup_box.pack_start(&sites_box, false, false, 0);

    let progress_label = Label::new(None);
    progress_label.set_xalign(0.0);
    let progress_bar = ProgressBar::new();
    progress_bar.set_show_text(true);
    setup_box.pack_start(&progress_label, false, false, 0);
    setup_box.pack_start(&progress_bar, false, false, 0);

    let btn_row = GtkBox::new(Orientation::Horizontal, 8);
    btn_row.set_halign(Align::End);
    let activate_btn = Button::with_label("Activate Block");
    activate_btn.style_context().add_class("destructive-action");
    btn_row.pack_start(&activate_btn, false, false, 0);
    setup_box.pack_end(&btn_row, false, false, 0);

    stack.add_named(&setup_box, "setup");

    // ---- Countdown view ----
    let countdown_box = GtkBox::new(Orientation::Vertical, 12);
    countdown_box.set_border_width(24);
    countdown_box.set_valign(Align::Center);
    let title = Label::new(None);
    title.set_markup("<span size=\"x-large\" weight=\"bold\">Block Active</span>");
    countdown_box.pack_start(&title, false, false, 0);
    let subtitle = Label::new(Some("Time remaining until websites unblock:"));
    countdown_box.pack_start(&subtitle, false, false, 0);
    let timer_label = Label::new(None);
    countdown_box.pack_start(&timer_label, false, false, 0);
    let end_label = Label::new(None);
    countdown_box.pack_start(&end_label, false, false, 0);
    stack.add_named(&countdown_box, "countdown");

    let ui = Rc::new(Ui {
        window: window.clone(),
        stack: stack.clone(),
        duration_value,
        duration_unit,
        preset_combo,
        sites_entry,
        progress_label,
        progress_bar,
        activate_btn: activate_btn.clone(),
        timer_label,
        end_label,
        script_path,
        blocklist_path,
        stderr_buffer: RefCell::new(Vec::new()),
        countdown_running: RefCell::new(false),
    });

    edit_btn.connect_clicked(clone!(@strong ui => move |_| {
        open_blocklist_editor(&ui);
    }));

    activate_btn.connect_clicked(clone!(@strong ui => move |_| {
        on_activate_clicked(&ui);
    }));

    if block_is_active() {
        ui.stack.set_visible_child_name("countdown");
        start_countdown(&ui);
    } else {
        ui.stack.set_visible_child_name("setup");
    }

    window.show_all();
    // Progress widgets hidden until activation starts (show_all overrides
    // the hide-on-construction that the Python version does before show_all).
    ui.progress_label.hide();
    ui.progress_bar.hide();
}

fn duration_spec(ui: &Ui) -> String {
    let n = ui.duration_value.value() as i64;
    let unit_label = ui
        .duration_unit
        .active_text()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "hours".to_string());
    let suffix = DURATION_UNITS
        .iter()
        .find(|(label, _)| *label == unit_label.as_str())
        .map(|(_, s)| *s)
        .unwrap_or("h");
    format!("{}{}", n, suffix)
}

fn formatted_duration(ui: &Ui) -> String {
    let n = ui.duration_value.value() as i64;
    let unit = ui
        .duration_unit
        .active_text()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "hours".to_string());
    if n != 1 {
        format!("{} {}", n, unit)
    } else {
        format!("{} {}", n, unit.trim_end_matches('s'))
    }
}

fn on_activate_clicked(ui: &Rc<Ui>) {
    let confirm = MessageDialog::new(
        Some(&ui.window),
        DialogFlags::MODAL,
        MessageType::Warning,
        ButtonsType::OkCancel,
        "Activate the block?",
    );
    confirm.set_secondary_text(Some(&format!(
        "Distracting sites will be blocked for {}.\n\n\
This action cannot be undone \u{2014} even rebooting will not lift the block \
until the timer expires.",
        formatted_duration(ui)
    )));
    let response = confirm.run();
    confirm.close();
    if response != ResponseType::Ok {
        return;
    }

    ui.activate_btn.set_sensitive(false);
    ui.progress_label.show();
    ui.progress_bar.show();
    ui.progress_bar.set_fraction(0.0);
    ui.progress_bar.set_text(Some("0%"));
    ui.progress_label.set_text("Requesting authorization...");

    run_block(ui);
}

fn run_block(ui: &Rc<Ui>) {
    let duration = duration_spec(ui);
    let preset = ui
        .preset_combo
        .active_id()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string());
    let sites = ui.sites_entry.text().trim().to_string();

    let mut argv: Vec<OsString> = vec![
        OsString::from("pkexec"),
        ui.script_path.clone().into_os_string(),
        OsString::from("--no-gui"),
        OsString::from("--duration"),
        OsString::from(duration),
        OsString::from("--preset"),
        OsString::from(preset),
    ];
    if !sites.is_empty() {
        argv.push(OsString::from("--sites"));
        argv.push(OsString::from(sites));
    }
    let argv_refs: Vec<&std::ffi::OsStr> = argv.iter().map(OsString::as_os_str).collect();

    let flags = SubprocessFlags::STDOUT_PIPE | SubprocessFlags::STDERR_PIPE;
    let proc = match Subprocess::newv(&argv_refs, flags) {
        Ok(p) => p,
        Err(e) => {
            show_error(ui, &format!("Failed to launch script: {}", e));
            reset_after_failure(ui);
            return;
        }
    };

    ui.stderr_buffer.borrow_mut().clear();

    if let Some(stdout) = proc.stdout_pipe() {
        kick_read_loop(Rc::clone(ui), DataInputStream::new(&stdout), true);
    }
    if let Some(stderr) = proc.stderr_pipe() {
        kick_read_loop(Rc::clone(ui), DataInputStream::new(&stderr), false);
    }

    proc.wait_check_async(
        None::<&Cancellable>,
        clone!(@strong ui => move |result| {
            on_proc_finished(&ui, result);
        }),
    );
}

fn kick_read_loop(ui: Rc<Ui>, stream: DataInputStream, is_stdout: bool) {
    let stream_clone = stream.clone();
    stream.read_line_async(
        Priority::default(),
        None::<&Cancellable>,
        move |result| match result {
            Ok(bytes) if !bytes.is_empty() => {
                let line = String::from_utf8_lossy(&bytes)
                    .trim_end_matches(|c| c == '\n' || c == '\r')
                    .to_string();
                if is_stdout {
                    handle_stdout_line(&ui, &line);
                } else {
                    ui.stderr_buffer.borrow_mut().push(line);
                }
                kick_read_loop(ui, stream_clone, is_stdout);
            }
            _ => {
                // EOF or error — stop reading.
            }
        },
    );
}

fn handle_stdout_line(ui: &Rc<Ui>, line: &str) {
    if let Some(rest) = line.strip_prefix("PROGRESS:") {
        let mut parts = rest.splitn(2, ':');
        if let (Some(pct_str), Some(msg)) = (parts.next(), parts.next()) {
            if let Ok(pct) = pct_str.parse::<i32>() {
                let clamped = pct.clamp(0, 100);
                ui.progress_label.set_text(msg);
                ui.progress_bar.set_fraction(clamped as f64 / 100.0);
                ui.progress_bar.set_text(Some(&format!("{}%", pct)));
            }
        }
    }
}

fn on_proc_finished(ui: &Rc<Ui>, result: Result<(), glib::Error>) {
    match result {
        Ok(()) => {
            ui.stack.set_visible_child_name("countdown");
            start_countdown(ui);
        }
        Err(_) => {
            let msg = {
                let buf = ui.stderr_buffer.borrow();
                let joined = buf.join("\n");
                let trimmed = joined.trim();
                if trimmed.is_empty() {
                    "Script exited with non-zero status. (Possible polkit cancellation.)"
                        .to_string()
                } else {
                    trimmed.to_string()
                }
            };
            show_error(ui, &msg);
            reset_after_failure(ui);
        }
    }
}

fn show_error(ui: &Rc<Ui>, message: &str) {
    let dlg = MessageDialog::new(
        Some(&ui.window),
        DialogFlags::MODAL,
        MessageType::Error,
        ButtonsType::Ok,
        "Activation failed",
    );
    dlg.set_secondary_text(Some(message));
    dlg.run();
    dlg.close();
}

fn reset_after_failure(ui: &Rc<Ui>) {
    ui.activate_btn.set_sensitive(true);
    ui.progress_label.set_text("Activation failed or was canceled.");
}

fn return_to_setup(ui: &Rc<Ui>) {
    ui.activate_btn.set_sensitive(true);
    ui.progress_label.set_text("");
    ui.progress_label.hide();
    ui.progress_bar.set_fraction(0.0);
    ui.progress_bar.set_text(Some("0%"));
    ui.progress_bar.hide();
    ui.stack.set_visible_child_name("setup");
}

fn start_countdown(ui: &Rc<Ui>) {
    if *ui.countdown_running.borrow() {
        return;
    }
    *ui.countdown_running.borrow_mut() = true;
    tick_countdown(ui);
    glib::timeout_add_seconds_local(
        1,
        clone!(@strong ui => move || {
            if tick_countdown(&ui) {
                ControlFlow::Continue
            } else {
                *ui.countdown_running.borrow_mut() = false;
                ControlFlow::Break
            }
        }),
    );
}

fn tick_countdown(ui: &Rc<Ui>) -> bool {
    let end = read_end_time();
    if end <= 0 || !active_file_path().exists() {
        ui.timer_label.set_markup(
            "<span size=\"x-large\" foreground=\"#2e7d32\" weight=\"bold\">\
Block ended.</span>",
        );
        ui.end_label.set_text("");
        // Hold the "Block ended" message for a beat, then drop the user back
        // on the setup page so they can start a new block.
        glib::timeout_add_seconds_local_once(
            2,
            clone!(@strong ui => move || {
                return_to_setup(&ui);
            }),
        );
        return false;
    }
    let remaining = (end - now_unix()).max(0);
    ui.timer_label.set_markup(&format!(
        "<span size=\"xx-large\" foreground=\"#cc0000\" weight=\"bold\">{}</span>",
        format_remaining(remaining)
    ));
    if let Ok(dt) = glib::DateTime::from_unix_local(end) {
        if let Ok(formatted) = dt.format("%a %b %d %H:%M:%S") {
            ui.end_label.set_text(&format!("Ends at {}", formatted));
        }
    }
    remaining > 0
}

fn open_blocklist_editor(ui: &Rc<Ui>) {
    let dialog = Dialog::builder()
        .title("Edit blocklists.txt")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(580)
        .default_height(540)
        .build();
    dialog.add_button("Cancel", ResponseType::Cancel);
    let save_btn = dialog.add_button("Save", ResponseType::Ok);
    save_btn.style_context().add_class("suggested-action");

    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_border_width(12);

    let info = Label::new(None);
    info.set_markup(
        "<small>Edits apply to the <b>next</b> activation, not to a \
block already in progress.</small>",
    );
    info.set_xalign(0.0);
    content.pack_start(&info, false, false, 0);

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .build();
    let textview = TextView::new();
    textview.set_monospace(true);
    textview.set_left_margin(8);
    textview.set_right_margin(8);
    textview.set_top_margin(8);
    textview.set_bottom_margin(8);

    let initial = match std::fs::read_to_string(&ui.blocklist_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            "# blocklists.txt did not exist; will be created on save.\n\
[social]\n\n[adult]\n\n[timewasters]\n"
                .to_string()
        }
        Err(e) => format!("# Failed to read {}: {}\n", ui.blocklist_path.display(), e),
    };
    let buffer = textview.buffer().expect("TextView always has a buffer");
    buffer.set_text(&initial);
    scroll.add(&textview);
    content.pack_start(&scroll, true, true, 0);
    dialog.show_all();

    let response = dialog.run();
    let save_error = if response == ResponseType::Ok {
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .map(|g| g.to_string())
            .unwrap_or_default();
        match std::fs::write(&ui.blocklist_path, text) {
            Ok(()) => None,
            Err(e) => Some(format!("{}: {}", ui.blocklist_path.display(), e)),
        }
    } else {
        None
    };
    dialog.close();

    if let Some(err) = save_error {
        let dlg = MessageDialog::new(
            Some(&ui.window),
            DialogFlags::MODAL,
            MessageType::Error,
            ButtonsType::Ok,
            "Could not save blocklist",
        );
        dlg.set_secondary_text(Some(&err));
        dlg.run();
        dlg.close();
    }
}

fn main() {
    scrub_snap_env();

    if is_root() {
        eprintln!(
            "Run this GUI as your normal user; pkexec handles privilege \
escalation for the bash script."
        );
        std::process::exit(1);
    }

    let script_path = match find_script() {
        Some(p) => p,
        None => {
            eprintln!(
                "{} not found. Set DISTRACTIONS_SCRIPT=/path/to/{} or place \
it next to the binary.",
                SCRIPT_NAME, SCRIPT_NAME
            );
            std::process::exit(1);
        }
    };

    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        build_ui(app, script_path.clone());
    });

    let exit = app.run();
    std::process::exit(i32::from(exit));
}
