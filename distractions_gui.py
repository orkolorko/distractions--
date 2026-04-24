#!/usr/bin/env python3
"""
GTK3 frontend for distractions--.sh.

Runs as the unprivileged user. Activation shells out to the bash script via
pkexec so the kernel/systemd work happens as root, while this process only
collects inputs, parses PROGRESS:<pct>:<msg> lines from the script, and polls
/var/lib/hardblock/end_time for the countdown view. The block/persistence
logic stays entirely in distractions--.sh.
"""

import os
import sys
import time
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402

SCRIPT_PATH = Path(__file__).resolve().parent / "distractions--.sh"
BLOCKLIST_FILE = Path(__file__).resolve().parent / "blocklists.txt"
STATE_DIR = Path("/var/lib/hardblock")
END_TIME_FILE = STATE_DIR / "end_time"
ACTIVE_FILE = STATE_DIR / "block_active"

PRESETS = [
    ("none", "No preset (use custom sites only)"),
    ("all", "All categories"),
    ("social", "Social media only"),
    ("adult", "Adult content only"),
    ("timewasters", "Time wasters only (YouTube, Netflix, ...)"),
]

DURATION_UNITS = [("minutes", "m"), ("hours", "h"), ("days", "d")]


def read_end_time():
    try:
        return int(END_TIME_FILE.read_text().strip())
    except (OSError, ValueError):
        return 0


def block_is_active():
    if not ACTIVE_FILE.exists():
        return False
    end = read_end_time()
    return end > time.time()


def format_remaining(seconds):
    if seconds <= 0:
        return "00:00:00"
    days, rem = divmod(seconds, 86400)
    hours, rem = divmod(rem, 3600)
    minutes, secs = divmod(rem, 60)
    if days:
        return f"{days}d {hours:02d}:{minutes:02d}:{secs:02d}"
    return f"{hours:02d}:{minutes:02d}:{secs:02d}"


class SetupView(Gtk.Box):
    """Form + activate button + inline progress."""

    def __init__(self, on_activate, blocklist_path):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        self.set_border_width(16)
        self._on_activate = on_activate
        self._blocklist_path = blocklist_path

        warning = Gtk.Label()
        warning.set_markup(
            '<span foreground="#cc0000" weight="bold" size="large">'
            "WARNING: activation cannot be reversed until the timer expires."
            "</span>"
        )
        warning.set_line_wrap(True)
        warning.set_xalign(0)
        self.pack_start(warning, False, False, 0)

        self.pack_start(self._build_duration_row(), False, False, 0)
        self.pack_start(self._build_preset_row(), False, False, 0)
        self.pack_start(self._labeled("Additional sites (space-separated):", self._build_sites()), False, False, 0)

        self.progress_label = Gtk.Label(xalign=0)
        self.progress_bar = Gtk.ProgressBar()
        self.progress_bar.set_show_text(True)
        self.pack_start(self.progress_label, False, False, 0)
        self.pack_start(self.progress_bar, False, False, 0)
        self.progress_label.hide()
        self.progress_bar.hide()

        button_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        button_row.set_halign(Gtk.Align.END)
        self.activate_btn = Gtk.Button(label="Activate Block")
        self.activate_btn.get_style_context().add_class("destructive-action")
        self.activate_btn.connect("clicked", self._on_activate_clicked)
        button_row.pack_start(self.activate_btn, False, False, 0)
        self.pack_end(button_row, False, False, 0)

    @staticmethod
    def _labeled(text, widget):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        label = Gtk.Label(label=text, xalign=0)
        box.pack_start(label, False, False, 0)
        box.pack_start(widget, False, False, 0)
        return box

    def _build_duration_row(self):
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        box.pack_start(Gtk.Label(label="Duration:", xalign=0), False, False, 0)
        self.duration_value = Gtk.SpinButton.new_with_range(1, 999, 1)
        self.duration_value.set_value(1)
        box.pack_start(self.duration_value, False, False, 0)
        self.duration_unit = Gtk.ComboBoxText()
        for label, _suffix in DURATION_UNITS:
            self.duration_unit.append_text(label)
        self.duration_unit.set_active(1)  # hours
        box.pack_start(self.duration_unit, False, False, 0)
        return box

    def _build_preset_row(self):
        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        outer.pack_start(Gtk.Label(label="Preset category:", xalign=0),
                         False, False, 0)
        row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self.preset_combo = Gtk.ComboBoxText()
        for key, label in PRESETS:
            self.preset_combo.append(key, label)
        self.preset_combo.set_active_id("all")
        row.pack_start(self.preset_combo, True, True, 0)
        edit_btn = Gtk.Button(label="Edit list...")
        edit_btn.set_tooltip_text(
            "Edit blocklists.txt — changes apply to the next activation."
        )
        edit_btn.connect("clicked", self._on_edit_blocklists)
        row.pack_start(edit_btn, False, False, 0)
        outer.pack_start(row, False, False, 0)
        return outer

    def _on_edit_blocklists(self, _btn):
        parent = self.get_toplevel()
        editor = BlocklistEditor(parent, self._blocklist_path)
        error = editor.run_and_save()
        if error:
            dialog = Gtk.MessageDialog(
                transient_for=parent,
                modal=True,
                message_type=Gtk.MessageType.ERROR,
                buttons=Gtk.ButtonsType.OK,
                text="Could not save blocklist",
            )
            dialog.format_secondary_text(error)
            dialog.run()
            dialog.destroy()

    def _build_sites(self):
        self.sites_entry = Gtk.Entry()
        self.sites_entry.set_placeholder_text(
            "e.g. news.ycombinator.com hn.algolia.com"
        )
        return self.sites_entry

    def _duration_spec(self):
        unit_label = self.duration_unit.get_active_text()
        suffix = dict(DURATION_UNITS)[unit_label]
        return f"{int(self.duration_value.get_value())}{suffix}"

    def _formatted_duration(self):
        n = int(self.duration_value.get_value())
        unit = self.duration_unit.get_active_text()
        return f"{n} {unit if n != 1 else unit.rstrip('s')}"

    def _on_activate_clicked(self, _btn):
        dialog = Gtk.MessageDialog(
            transient_for=self.get_toplevel(),
            modal=True,
            message_type=Gtk.MessageType.WARNING,
            buttons=Gtk.ButtonsType.OK_CANCEL,
            text="Activate the block?",
        )
        dialog.format_secondary_markup(
            f"Distracting sites will be blocked for "
            f"<b>{GLib.markup_escape_text(self._formatted_duration())}</b>.\n\n"
            "<b>This action cannot be undone</b> — even rebooting will not "
            "lift the block until the timer expires."
        )
        response = dialog.run()
        dialog.destroy()
        if response != Gtk.ResponseType.OK:
            return

        self.activate_btn.set_sensitive(False)
        self.progress_label.show()
        self.progress_bar.show()
        self.progress_bar.set_fraction(0.0)
        self.progress_bar.set_text("0%")
        self.progress_label.set_text("Requesting authorization...")
        self._on_activate(
            self._duration_spec(),
            self.preset_combo.get_active_id(),
            self.sites_entry.get_text().strip(),
        )

    def update_progress(self, pct, msg):
        self.progress_label.set_text(msg)
        self.progress_bar.set_fraction(max(0, min(100, pct)) / 100.0)
        self.progress_bar.set_text(f"{pct}%")

    def reset_after_failure(self):
        self.activate_btn.set_sensitive(True)
        self.progress_label.set_text("Activation failed or was canceled.")

    def reset_for_new_block(self):
        self.activate_btn.set_sensitive(True)
        self.progress_label.set_text("")
        self.progress_label.hide()
        self.progress_bar.set_fraction(0.0)
        self.progress_bar.set_text("0%")
        self.progress_bar.hide()


class BlocklistEditor(Gtk.Dialog):
    """Embedded text editor for blocklists.txt. Returns an error string on
    save failure so the caller can surface it; None means success or cancel."""

    def __init__(self, parent, path):
        super().__init__(
            title="Edit blocklists.txt",
            transient_for=parent,
            modal=True,
        )
        self._path = path
        self.set_default_size(580, 540)
        self.add_button("Cancel", Gtk.ResponseType.CANCEL)
        save_btn = self.add_button("Save", Gtk.ResponseType.OK)
        save_btn.get_style_context().add_class("suggested-action")

        content = self.get_content_area()
        content.set_spacing(8)
        content.set_border_width(12)

        info = Gtk.Label(xalign=0)
        info.set_markup(
            "<small>Edits apply to the <b>next</b> activation, not to a "
            "block already in progress.</small>"
        )
        content.pack_start(info, False, False, 0)

        scroll = Gtk.ScrolledWindow()
        scroll.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        self._textview = Gtk.TextView()
        self._textview.set_monospace(True)
        self._textview.set_left_margin(8)
        self._textview.set_right_margin(8)
        self._textview.set_top_margin(8)
        self._textview.set_bottom_margin(8)
        try:
            initial = path.read_text()
        except FileNotFoundError:
            initial = (
                "# blocklists.txt did not exist; will be created on save.\n"
                "[social]\n\n[adult]\n\n[timewasters]\n"
            )
        except OSError as err:
            initial = f"# Failed to read {path}: {err}\n"
        self._textview.get_buffer().set_text(initial)
        scroll.add(self._textview)
        content.pack_start(scroll, True, True, 0)

        self.show_all()

    def run_and_save(self):
        response = self.run()
        error = None
        if response == Gtk.ResponseType.OK:
            buf = self._textview.get_buffer()
            text = buf.get_text(buf.get_start_iter(), buf.get_end_iter(), True)
            try:
                self._path.write_text(text)
            except OSError as err:
                error = f"{self._path}: {err}"
        self.destroy()
        return error


class CountdownView(Gtk.Box):
    """Live countdown driven by /var/lib/hardblock/end_time."""

    def __init__(self, on_finished):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        self.set_border_width(24)
        self.set_valign(Gtk.Align.CENTER)
        self._on_finished = on_finished

        title = Gtk.Label()
        title.set_markup(
            '<span size="x-large" weight="bold">Block Active</span>'
        )
        self.pack_start(title, False, False, 0)

        subtitle = Gtk.Label(label="Time remaining until websites unblock:")
        self.pack_start(subtitle, False, False, 0)

        self.timer_label = Gtk.Label()
        self.pack_start(self.timer_label, False, False, 0)

        self.end_label = Gtk.Label()
        self.pack_start(self.end_label, False, False, 0)

        self._timer_id = None

    def start(self):
        self._tick()
        if self._timer_id is None:
            self._timer_id = GLib.timeout_add_seconds(1, self._tick)

    def _tick(self):
        end = read_end_time()
        if end <= 0 or not ACTIVE_FILE.exists():
            self.timer_label.set_markup(
                '<span size="x-large" foreground="#2e7d32" weight="bold">'
                "Block ended.</span>"
            )
            self.end_label.set_text("")
            self._timer_id = None
            # Hold the "Block ended" message for a beat, then hand control
            # back to the caller so it can drop the user on the setup page.
            GLib.timeout_add_seconds(2, self._fire_finished)
            return False
        remaining = max(0, end - int(time.time()))
        self.timer_label.set_markup(
            f'<span size="xx-large" foreground="#cc0000" weight="bold">'
            f"{format_remaining(remaining)}</span>"
        )
        self.end_label.set_text(
            "Ends at " + time.strftime("%a %b %d %H:%M:%S", time.localtime(end))
        )
        if remaining <= 0:
            self._timer_id = None
            return False
        return True

    def _fire_finished(self):
        self._on_finished()
        return False


class MainWindow(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="distractions--")
        self.set_default_size(540, 460)

        self.stack = Gtk.Stack()
        self.add(self.stack)

        self.setup_view = SetupView(self.run_block, BLOCKLIST_FILE)
        self.countdown_view = CountdownView(self._return_to_setup)

        self.stack.add_named(self.setup_view, "setup")
        self.stack.add_named(self.countdown_view, "countdown")

        if block_is_active():
            self.stack.set_visible_child_name("countdown")
            self.countdown_view.start()
        else:
            self.stack.set_visible_child_name("setup")

        self.show_all()

    def run_block(self, duration, preset, sites):
        argv = [
            "pkexec",
            str(SCRIPT_PATH),
            "--no-gui",
            "--duration", duration,
            "--preset", preset,
        ]
        if sites:
            argv += ["--sites", sites]

        flags = (
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE
        )
        try:
            proc = Gio.Subprocess.new(argv, flags)
        except GLib.Error as err:
            self._show_error(f"Failed to launch script: {err.message}")
            self.setup_view.reset_after_failure()
            return

        self._stderr_buffer = []
        stdout = Gio.DataInputStream.new(proc.get_stdout_pipe())
        stderr = Gio.DataInputStream.new(proc.get_stderr_pipe())
        self._read_line(stdout, self._handle_stdout_line)
        self._read_line(stderr, self._handle_stderr_line)
        proc.wait_async(None, self._on_proc_finished)

    def _read_line(self, stream, handler):
        stream.read_line_async(
            GLib.PRIORITY_DEFAULT, None, self._on_line_ready, handler
        )

    def _on_line_ready(self, stream, result, handler):
        try:
            line, _length = stream.read_line_finish_utf8(result)
        except GLib.Error:
            return
        if line is None:
            return  # EOF
        handler(line)
        self._read_line(stream, handler)

    def _handle_stdout_line(self, line):
        if line.startswith("PROGRESS:"):
            try:
                _tag, pct, msg = line.split(":", 2)
                self.setup_view.update_progress(int(pct), msg)
            except ValueError:
                pass

    def _handle_stderr_line(self, line):
        self._stderr_buffer.append(line)

    def _on_proc_finished(self, proc, result):
        try:
            proc.wait_finish(result)
        except GLib.Error as err:
            self._show_error(err.message)
            self.setup_view.reset_after_failure()
            return

        if proc.get_successful():
            self.stack.set_visible_child_name("countdown")
            self.countdown_view.start()
        else:
            err_text = "\n".join(self._stderr_buffer).strip() or (
                "Script exited with non-zero status. "
                "(Possible polkit cancellation.)"
            )
            self._show_error(err_text)
            self.setup_view.reset_after_failure()

    def _return_to_setup(self):
        self.setup_view.reset_for_new_block()
        self.stack.set_visible_child_name("setup")

    def _show_error(self, message):
        dialog = Gtk.MessageDialog(
            transient_for=self,
            modal=True,
            message_type=Gtk.MessageType.ERROR,
            buttons=Gtk.ButtonsType.OK,
            text="Activation failed",
        )
        dialog.format_secondary_text(message)
        dialog.run()
        dialog.destroy()


class App(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="org.distractions.gui")

    def do_activate(self):
        win = self.props.active_window
        if not win:
            win = MainWindow(self)
        win.present()


def main():
    if not SCRIPT_PATH.exists():
        sys.stderr.write(
            f"distractions--.sh not found next to {__file__}\n"
        )
        return 1
    if os.geteuid() == 0:
        sys.stderr.write(
            "Run this GUI as your normal user; pkexec handles privilege "
            "escalation for the bash script.\n"
        )
        return 1
    return App().run([])


if __name__ == "__main__":
    raise SystemExit(main())
