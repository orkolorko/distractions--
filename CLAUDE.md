# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`distractions--` blocks distracting websites on Linux in a way that is **intentionally irreversible** until a user-set timer expires. Irreversibility is a core feature, not a bug — be careful before "fixing" anything that looks like a missing escape hatch. The block/persistence logic lives entirely in `distractions--.sh`; everything else is a presentation layer over it.

## Three layers

1. **`distractions--.sh`** — bash script that does the actual work (hosts edit, `chattr`, systemd, `at`). Two front-ends are built into it, picked by flags:
   - **Interactive zenity** (no flags): the default. Shows a form, then a progress dialog.
   - **Non-interactive / wrapper mode** (`--no-gui --duration … --preset … [--sites …]`): suppresses every zenity dialog, implies `--yes`, skips the optional countdown, and emits machine-readable progress lines on stdout.
2. **`distractions_gui.py`** — GTK3 (PyGObject) frontend. Runs unprivileged, collects inputs, and shells out to the bash script via `pkexec --no-gui …`. Polkit handles the auth prompt; the GUI itself never needs root. Covered in detail under *GTK frontend* below.

## Running

```bash
chmod +x distractions--.sh
sudo ./distractions--.sh                                    # interactive zenity
sudo ./distractions--.sh --duration 2h --preset social --yes # CLI, sees zenity errors
sudo ./distractions--.sh --no-gui --duration 2h --preset all # wrapper mode
./distractions_gui.py                                       # GTK frontend (no sudo)
```

There is no build step, no test suite, and no linter configured. For local syntax checks use `bash -n distractions--.sh` or `shellcheck distractions--.sh`; the Python frontend can be parse-checked with `python3 -c "import ast; ast.parse(open('distractions_gui.py').read())"`.

Runtime dependencies (checked at startup): `at`, `chattr`, `systemd-run`, `date` — plus `zenity` unless `--no-gui` is set. Optional: `yad` (countdown window), `notify-send` (desktop notifications), `systemd-resolve`/`resolvectl` (DNS cache flush). The GTK frontend additionally needs `python3-gi` + `gir1.2-gtk-3.0` + `pkexec`.

## Architecture

The script does its work in one linear pass; understanding the *layered persistence* is the only thing that needs cross-file context (and there are no other files):

1. **Hosts-file block.** Domains chosen via the zenity form get appended to `/etc/hosts` between `# HARDBLOCK START` / `# HARDBLOCK END` markers, mapped to `127.0.0.1` and `::1`. The original is backed up to `/etc/hosts.hardblock.bak` first.
2. **Immutability.** `chattr +i /etc/hosts` makes the file unmodifiable even by root — this is what enforces irreversibility within the kernel.
3. **State directory.** `/var/lib/hardblock/` holds `block_active` (sentinel), `end_time` (unix timestamp), `unblock.sh` (the unblocker), and optionally `countdown.sh`.
4. **Two redundant unblock triggers** (both must work for the block to lift cleanly):
   - `hardblock-unblock.timer` (systemd) fires `hardblock-unblock.service` every 60s, which runs `/var/lib/hardblock/unblock.sh`. This survives reboots via `WantedBy=timers.target`.
   - An `at` job scheduled for `now + DURATION_SECONDS` as a backup.
   `unblock.sh` itself re-checks `end_time` before acting, so early firings are no-ops.
5. **Active-block guard.** On startup the script reads `/var/lib/hardblock/end_time` and refuses to start a new block if one is still in progress.

### Cleanup trap — important nuance

The `cleanup()` function (trapped on `SIGINT`/`SIGTERM`/`SIGHUP`) restores the system *only if interrupted before the script finishes successful setup*. State flags `HOSTS_MODIFIED`, `HOSTS_IMMUTABLE`, `SERVICES_CREATED` gate which steps it undoes. The trap is explicitly cleared (`trap - SIGINT SIGTERM SIGHUP`) at the end of the script, because once the block is active, undoing it would defeat the entire point. Do not extend cleanup to run post-activation.

### Block presets

Domains live in `blocklists.txt` next to the script, parsed at startup into `BLOCKS_SOCIAL` / `BLOCKS_ADULT` / `BLOCKS_TIMEWASTERS` by `load_blocklists()`. Format is INI-style: `[social]`/`[adult]`/`[timewasters]` headers, one domain per line, `#` comments and blank lines ignored, unknown sections silently dropped. If the file is missing the script falls back to a hardcoded copy of the same lists so it still works standalone — keep that fallback in sync with the file when adding new categories.

The file path is resolved via `readlink -f "$0"` (script directory), so it works regardless of CWD or whether the script was invoked through `pkexec`. Edits between blocks take effect on the next activation; edits during a block do not affect the in-progress one (its `/etc/hosts` entries were baked in at activation).

### Wrapper protocol (`--no-gui`)

When `--no-gui` is set, `report_error`, `report_info`, and `emit_progress` swap their zenity calls for line-oriented stdio so a wrapper can drive the script:

- `PROGRESS:<pct>:<msg>\n` on **stdout** during setup (10/20/30/.../100).
- `INFO: <title>: <message>\n` on **stdout** for the success line.
- `ERROR: <title>: <message>\n` on **stderr** for any failure.

`--no-gui` implies `--yes` (final confirmation suppressed) and skips the optional yad countdown — wrappers are expected to render their own. `--no-gui` without `--duration` is rejected with exit 2.

If you add a new error/info path, route it through `report_error`/`report_info` rather than calling `zenity` directly, otherwise wrapper mode will block on a dialog that never paints.

## GTK frontend (`distractions_gui.py`)

PyGObject + GTK3, single file, runs as the unprivileged user. Two `Gtk.Stack` pages:

- **Setup** (`SetupView`): duration spinner + unit dropdown, preset combo with an "Edit list..." button (opens `BlocklistEditor` — a TextView dialog that loads/saves `blocklists.txt`), custom-sites entry, activate button with confirmation dialog. On activate it spawns `pkexec ./distractions--.sh --no-gui --duration … --preset … [--sites …]` via `Gio.Subprocess`, parses `PROGRESS:` lines into the inline progress bar, and buffers stderr to surface in an error dialog if the subprocess fails (which includes the user dismissing the polkit prompt).
- **Countdown** (`CountdownView`): polls `/var/lib/hardblock/end_time` every second via `GLib.timeout_add_seconds`. Picks itself when the window opens with a block already active, and is switched in by `MainWindow` after a successful activation.

The GUI never reads or writes anything in `/var/lib/hardblock` itself — `end_time` is created world-readable by the bash script (default umask 022 under root), so the unprivileged poll loop just works. The script is the only privileged surface; the GUI never asks for root for itself (in fact `main()` refuses to start as root). All design choices follow from the constraint of not touching the tested block-setup code path.

## Conventions

- All user-facing dialogs go through `zenity` (forms, progress, info, error, question). The countdown window uses `yad` because zenity has no live-updating label.
- Heredocs that should expand variables at write time use unquoted `EOF`; the embedded `unblock.sh` uses quoted `'EOF'` so its `$VAR`s are literal and resolved at run time inside the unblocker.
