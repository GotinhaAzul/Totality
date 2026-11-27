//! Terminal setup and restore: raw mode, the alternate screen, and a panic
//! hook that always gives the user their shell back.

use std::io::Write as _;

use super::*;

/// Room for a full repaint of a large terminal, so a frame leaves in one
/// write instead of the many small ones a line-buffered stdout makes (which
/// shows as tearing over SSH).
pub(super) const OUTPUT_BUFFER: usize = 256 * 1024;

pub(super) type Tty = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

pub(super) static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set while another program owns the terminal; the input thread stops
/// reading so it cannot steal that program's keystrokes.
pub(super) static INPUT_PAUSED: AtomicBool = AtomicBool::new(false);

/// Set by the input thread once it has seen [`INPUT_PAUSED`] and is no
/// longer inside a poll/read.
pub(super) static INPUT_STOPPED: AtomicBool = AtomicBool::new(false);

/// How long the input thread may sit in one `poll` before it rechecks
/// [`INPUT_PAUSED`].
pub(super) const INPUT_POLL: Duration = Duration::from_millis(50);

/// Hand the terminal to `run` (an external editor): leave raw mode and the
/// alternate screen, pause the input thread, and take everything back after.
/// Without an active TUI (tests) `run` is simply called.
pub(super) fn suspend_terminal<T>(run: impl FnOnce() -> T) -> T {
    if !TERMINAL_ACTIVE.load(Ordering::SeqCst) {
        return run();
    }
    INPUT_PAUSED.store(true, Ordering::SeqCst);
    // Wait for a poll already in flight to return, rather than assuming a
    // fixed sleep is long enough under load.
    for _ in 0..25 {
        if INPUT_STOPPED.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(INPUT_POLL / 5);
    }
    let mouse = MOUSE_CAPTURE.load(Ordering::SeqCst);
    restore_terminal();
    let result = run();
    // The TUI's own guard still restores on exit; this one must not.
    if let Ok(guard) = TerminalGuard::enter(mouse) {
        std::mem::forget(guard);
    }
    INPUT_PAUSED.store(false, Ordering::SeqCst);
    result
}

/// Whether the TUI captured the mouse, so a resume restores the same state.
static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(false);

/// The editor command for Ctrl+G: `$VISUAL`, then `$EDITOR`.
pub(super) fn external_editor() -> Option<String> {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// Write `text` to a private temporary file, run `editor` on it through the
/// shell (so `code --wait` works), and return what was saved, minus the
/// trailing newline editors add.
pub(super) fn edit_in_editor(editor: &str, text: &str) -> Result<String> {
    let file = DraftFile::create(text)?;
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("tot-editor")
        .arg(file.path())
        .spawn()
        .with_context(|| format!("cannot run {editor}"))?;
    // The editor shares our foreground process group, so a Ctrl+C in it also
    // reaches this process. Ignore SIGINT while it runs; otherwise the queued
    // signal quits Totality as soon as the editor exits. Spawn first so the
    // child inherits the normal disposition and Ctrl+C still works there.
    let previous = ignore_sigint();
    let status = child.wait();
    if let Some(previous) = previous {
        restore_sigint(&previous);
    }
    let status = status.with_context(|| format!("cannot run {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}; the prompt is unchanged");
    }
    let mut edited = std::fs::read_to_string(file.path()).context("cannot read the draft back")?;
    if edited.ends_with('\n') {
        edited.pop();
        if edited.ends_with('\r') {
            edited.pop();
        }
    }
    Ok(edited)
}

/// Ignore SIGINT for this process while an external editor runs.
#[cfg(unix)]
fn ignore_sigint() -> Option<nix::sys::signal::SigAction> {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    // SAFETY: sigaction only stores the action; the mask and flags are valid.
    unsafe { sigaction(Signal::SIGINT, &action).ok() }
}

#[cfg(not(unix))]
fn ignore_sigint() -> Option<()> {
    None
}

#[cfg(unix)]
fn restore_sigint(previous: &nix::sys::signal::SigAction) {
    use nix::sys::signal::{Signal, sigaction};
    // SAFETY: `previous` came from sigaction on this process.
    unsafe {
        let _ = sigaction(Signal::SIGINT, previous);
    }
}

#[cfg(not(unix))]
fn restore_sigint(_previous: &()) {}

pub(super) fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

pub(super) fn restore_terminal() {
    if TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            EndSynchronizedUpdate,
            DisableMouseCapture,
            DisableFocusChange,
            LeaveAlternateScreen,
            DisableBracketedPaste
        );
        // Give the shell its title back (xterm title stack; ignored where
        // unsupported).
        let _ = out.write_all(b"\x1b[23;0t");
        let _ = out.flush();
    }
}

pub(super) struct TerminalGuard;

impl TerminalGuard {
    pub(super) fn enter(mouse: bool) -> Result<Self> {
        install_panic_hook();
        MOUSE_CAPTURE.store(mouse, Ordering::SeqCst);
        enable_raw_mode().context("cannot enable raw mode")?;
        let mut out = std::io::stdout();
        // Focus reports let the UI repaint after the terminal was covered
        // or a multiplexer reattached; terminals without them ignore it.
        let entered = if mouse {
            execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange,
                EnableMouseCapture
            )
        } else {
            execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange
            )
        };
        if let Err(error) = entered {
            let _ = disable_raw_mode();
            return Err(error).context("cannot enter the alternate screen");
        }
        // Save the shell's title so leaving restores it.
        let _ = out.write_all(b"\x1b[22;0t");
        let _ = out.flush();
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// A private (0600, create-new) temporary file removed on drop.
struct DraftFile(PathBuf);

impl DraftFile {
    fn create(text: &str) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or_default();
        let path =
            std::env::temp_dir().join(format!("tot-prompt-{}-{nanos:08x}.md", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("cannot create a temporary file")?;
        let draft = DraftFile(path);
        file.write_all(text.as_bytes())
            .context("cannot write the draft")?;
        Ok(draft)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for DraftFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
