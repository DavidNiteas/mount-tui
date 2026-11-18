mod app;
mod config;
mod disk;
mod mount;
mod runner;
mod ui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use anyhow::Context;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{ActionStatus, App, InteractiveRequest};
use runner::SystemRunner;

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

/// Idle interval between automatic device refreshes.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Exit code for usage / configuration errors, reported before the TUI starts.
const EXIT_USAGE: i32 = 2;

fn main() -> anyhow::Result<()> {
    install_panic_hook();

    // The single production command runner; every non-interactive system
    // command goes through it.
    let runner = SystemRunner;

    let cli = match config::parse_args(program_args()) {
        Ok(config::Args::Run(overrides)) => overrides,
        Ok(config::Args::Help) => {
            print!(
                "{}",
                config::help_text(config::current_config_path().as_deref())
            );
            return Ok(());
        }
        Ok(config::Args::Version) => {
            println!("mount-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(message) => {
            eprintln!("mount-tui: {}", message);
            eprintln!("Try 'mount-tui --help' for more information.");
            std::process::exit(EXIT_USAGE);
        }
    };

    // A broken configuration must be readable on the normal screen: it is
    // reported before the terminal is switched to the alternate screen.
    let config = match config::load(&cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mount-tui: configuration error: {:#}", error);
            std::process::exit(EXIT_USAGE);
        }
    };

    // Load devices before entering the alternate screen: a startup failure must
    // stay readable on the normal screen and be shown inside the TUI.
    let mut app = match disk::get_block_devices(&runner) {
        Ok(devices) => App::with_config(devices, config.clone()),
        Err(error) => {
            let mut app = App::with_config(Vec::new(), config);
            app.action = ActionStatus::Error(format!("Failed to list block devices: {}", error));
            app
        }
    };

    let mut session = Session::new().context("Failed to initialise terminal")?;
    let result = run(&mut session, &mut app, &runner);

    // Restore the terminal before the error (if any) is printed: the error is
    // returned to the runtime, which prints it and exits non-zero. A runtime
    // error must never be swallowed into a successful exit status.
    session.suspend()?;
    drop(session);

    result
}

/// Command line arguments, without the program name.
fn program_args() -> impl Iterator<Item = String> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
}

/// Owns the terminal and guarantees a restore on drop.
struct Session {
    terminal: Tui,
    raw_mode: bool,
    alternate_screen: bool,
}

/// Restores the terminal if `Session::new` fails after raw mode was enabled.
struct InitGuard {
    armed: bool,
}

impl InitGuard {
    fn new() -> Self {
        Self { armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InitGuard {
    fn drop(&mut self) {
        if self.armed {
            restore_terminal();
        }
    }
}

impl Session {
    fn new() -> anyhow::Result<Self> {
        enable_raw_mode().context("Failed to enable raw mode")?;
        // From here on every failure has to restore raw mode / the alternate
        // screen: `Session` does not exist yet, so its Drop cannot help.
        let mut guard = InitGuard::new();
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide).context("Failed to enter alternate screen")?;
        let terminal =
            Terminal::new(CrosstermBackend::new(stdout)).context("Failed to create terminal")?;
        let mut session = Self {
            terminal,
            raw_mode: true,
            alternate_screen: true,
        };
        // Ownership moved to `Session`; its Drop now handles restoration.
        guard.disarm();
        session
            .terminal
            .clear()
            .context("Failed to clear terminal")?;
        Ok(session)
    }

    /// Hand the terminal back to the shell (used around sudo commands).
    fn suspend(&mut self) -> anyhow::Result<()> {
        if self.raw_mode {
            disable_raw_mode().context("Failed to disable raw mode")?;
            self.raw_mode = false;
        }
        if self.alternate_screen {
            execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show)
                .context("Failed to leave alternate screen")?;
            self.alternate_screen = false;
        }
        Ok(())
    }

    /// Take the terminal back after an interactive command.
    fn resume(&mut self) -> anyhow::Result<()> {
        enable_raw_mode().context("Failed to enable raw mode")?;
        self.raw_mode = true;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen, Hide)
            .context("Failed to enter alternate screen")?;
        self.alternate_screen = true;
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

/// Restore the terminal before the default panic output so a crash never leaves
/// the shell in raw mode / the alternate screen.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

fn run(
    session: &mut Session,
    app: &mut App,
    runner: &dyn runner::CommandRunner,
) -> anyhow::Result<()> {
    let mut needs_draw = true;
    while app.running {
        if needs_draw {
            session.terminal.draw(|frame| ui::draw(frame, app))?;
            needs_draw = false;
        }

        // Poll instead of blocking so hotplugged devices show up on their own;
        // a timeout triggers the silent automatic refresh.
        if event::poll(REFRESH_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(request) = handle_key(app, key, runner) {
                        run_interactive(session, app, request, runner)?;
                    }
                    needs_draw = true;
                }
                Event::Resize(_, _) => {
                    session.terminal.clear()?;
                    needs_draw = true;
                }
                _ => {}
            }
        } else {
            needs_draw = app.auto_refresh(runner);
        }
    }
    Ok(())
}

fn handle_key(
    app: &mut App,
    key: KeyEvent,
    runner: &dyn runner::CommandRunner,
) -> Option<InteractiveRequest> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.running = false;
            None
        }
        KeyCode::Char('q') | KeyCode::Esc => {
            app.running = false;
            None
        }
        KeyCode::Up => {
            app.navigate_up();
            None
        }
        KeyCode::Down => {
            app.navigate_down();
            None
        }
        KeyCode::Home => {
            app.select_first();
            None
        }
        KeyCode::End => {
            app.select_last();
            None
        }
        KeyCode::PageUp => {
            app.page_up();
            None
        }
        KeyCode::PageDown => {
            app.page_down();
            None
        }
        KeyCode::Left | KeyCode::Right => {
            app.toggle_collapse();
            None
        }
        KeyCode::Enter => app.enter_selected(runner),
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'm' => app.mount_selected(runner),
            'u' => app.unmount_selected(runner),
            'f' => app.force_unmount_selected(runner),
            'r' => {
                app.refresh(runner);
                None
            }
            _ => None,
        },
        _ => None,
    }
}

struct RequestOutcome {
    ok: bool,
    detail: String,
}

/// Run the request with the terminal handed back to the shell.
///
/// The mount point of a sudo mount is created here, right before the handoff:
/// a request that never runs (cancelled or failed) must not leave a directory
/// behind, so a directory created by this call is removed again when the command
/// did not succeed.
fn run_interactive(
    session: &mut Session,
    app: &mut App,
    mut request: InteractiveRequest,
    runner: &dyn runner::CommandRunner,
) -> anyhow::Result<()> {
    let created = match prepare_mount_target(&request) {
        Ok(created) => created,
        Err(message) => {
            // Nothing to hand over: report the failure without suspending.
            request.context = message;
            app.finish(runner, request, false);
            return Ok(());
        }
    };

    // The directory now belongs to this call: every early return below (a
    // failing `suspend`, a failing `resume`, ...) drops the guard and removes
    // it again. Only a command that actually succeeded disarms it.
    let mut cleanup = CreatedDirGuard::new(created);

    session.suspend()?;
    let outcome = execute_request(&request);

    if outcome.ok {
        cleanup.disarm();
    } else {
        let mut stdout = io::stdout();
        println!();
        println!("{}", outcome.detail);
        println!();
        print!("Press Enter to return to Mount-TUI... ");
        let _ = stdout.flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        request.context = if request.context.is_empty() {
            outcome.detail.clone()
        } else {
            format!("{} ({})", request.context, outcome.detail)
        };
    }

    session.resume()?;

    // Remove a target the failed command left empty *before* the status bar is
    // updated; a disarmed guard has nothing left to do.
    drop(cleanup);

    app.finish(runner, request, outcome.ok);
    Ok(())
}

/// Removes the mount point created for an interactive request unless the
/// command succeeded.
///
/// The guard covers every early return of [`run_interactive`] after the target
/// exists: a failing `suspend`, `resume` or a panicking command must not leave a
/// directory behind. A successful mount calls [`CreatedDirGuard::disarm`], so
/// the mount point is kept.
struct CreatedDirGuard {
    created: Option<PathBuf>,
}

impl CreatedDirGuard {
    fn new(created: Option<PathBuf>) -> Self {
        Self { created }
    }

    /// Keep the directory (the mount succeeded).
    fn disarm(&mut self) {
        self.created = None;
    }
}

impl Drop for CreatedDirGuard {
    fn drop(&mut self) {
        if let Some(path) = self.created.take() {
            remove_empty_dir(&path);
        }
    }
}

/// Create the mount target of an interactive request, if it has one.
///
/// Returns the path when this call created it (the caller removes it again
/// after a failed command); `None` when the directory already existed.
///
/// The path is checked twice: once before `create_dir_all` and once right after
/// it. The second check closes the window between the check and the creation, in
/// which the path could have been swapped for a symbolic link or something that
/// is not a directory.
fn prepare_mount_target(request: &InteractiveRequest) -> Result<Option<PathBuf>, String> {
    let Some(target) = request.mount_target.as_deref() else {
        return Ok(None);
    };
    let existed = target.exists();
    check_mount_target(target)?;
    std::fs::create_dir_all(target).map_err(|error| {
        format!(
            "failed to create mount point {}: {}",
            target.display(),
            error
        )
    })?;
    // TOCTOU re-check: never hand a swapped path to `sudo mount`. A path that
    // was replaced in this window is reported, not deleted: it is not ours.
    check_mount_target(target)?;
    Ok(if existed {
        None
    } else {
        Some(target.to_path_buf())
    })
}

/// Refuse a mount target that exists but is not a plain directory.
///
/// A symbolic link would redirect the mount somewhere unexpected; any other
/// non-directory means the mount cannot work, so it is reported before the
/// terminal is handed over. A missing or unreadable path is left to
/// `create_dir_all`, which reports the real error.
fn check_mount_target(target: &Path) -> Result<(), String> {
    let Ok(metadata) = std::fs::symlink_metadata(target) else {
        return Ok(());
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(format!(
            "mount point {} is a symbolic link",
            target.display()
        ));
    }
    if !file_type.is_dir() {
        return Err(format!(
            "mount point {} is not a directory",
            target.display()
        ));
    }
    Ok(())
}

/// Remove a mount point created by a failed run.
///
/// `remove_dir` refuses a non-empty directory, so content that appeared there
/// (a mount that partially succeeded) is never destroyed.
fn remove_empty_dir(path: &Path) {
    let _ = std::fs::remove_dir(path);
}

fn execute_request(request: &InteractiveRequest) -> RequestOutcome {
    let mut attempts: Vec<&[String]> = vec![request.args.as_slice()];
    if let Some(fallback) = request.fallback_args.as_deref() {
        attempts.push(fallback);
    }

    let mut last = RequestOutcome {
        ok: false,
        detail: format!("could not run {}", request.program),
    };

    for args in attempts {
        match Command::new(&request.program).args(args).status() {
            Ok(status) if status.success() => {
                return RequestOutcome {
                    ok: true,
                    detail: String::new(),
                };
            }
            Ok(status) => {
                last = RequestOutcome {
                    ok: false,
                    detail: format!(
                        "{} {} exited with {}",
                        request.program,
                        args.join(" "),
                        describe_status(&status)
                    ),
                };
            }
            Err(error) => {
                last = RequestOutcome {
                    ok: false,
                    detail: format!("failed to run {}: {}", request.program, error),
                };
            }
        }
    }

    last
}

fn describe_status(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {}", code),
        None => "a signal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::{InteractiveOp, InteractiveRequest};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique scratch directory; removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "mount-tui-main-{}-{}-{}",
                tag,
                std::process::id(),
                unique
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mount_request(target: Option<PathBuf>) -> InteractiveRequest {
        InteractiveRequest {
            program: "sudo".into(),
            args: vec!["mount".into(), "/dev/sdb1".into(), "/tmp/x".into()],
            op: InteractiveOp::Mount,
            device_name: "sdb1".into(),
            context: String::new(),
            fallback_args: None,
            mount_target: target,
        }
    }

    #[test]
    fn a_request_without_target_creates_nothing() {
        let request = mount_request(None);
        assert_eq!(prepare_mount_target(&request), Ok(None));
    }

    #[test]
    fn preparing_creates_the_target_and_reports_it_created() {
        let root = TempDir::new("create");
        let target = root.path().join("usb").join("USBKEY");
        let request = mount_request(Some(target.clone()));

        assert_eq!(
            prepare_mount_target(&request),
            Ok(Some(target.clone())),
            "a freshly created target is reported"
        );
        assert!(target.is_dir());
        // The parents (`usb`) are created as well.
        assert!(root.path().join("usb").is_dir());
    }

    #[test]
    fn preparing_reuses_an_existing_directory() {
        let root = TempDir::new("reuse");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();

        let request = mount_request(Some(target.clone()));
        assert_eq!(
            prepare_mount_target(&request),
            Ok(None),
            "an existing directory must not be reported as created"
        );
        assert!(target.is_dir());
    }

    #[test]
    fn preparing_reports_a_failure_instead_of_panicking() {
        let root = TempDir::new("blocked");
        let file = root.path().join("plain-file");
        std::fs::write(&file, b"x").unwrap();

        let request = mount_request(Some(file.join("nested")));
        let error = prepare_mount_target(&request).unwrap_err();
        assert!(error.contains("failed to create mount point"), "{}", error);
    }

    #[test]
    fn preparing_refuses_a_symbolic_link() {
        let root = TempDir::new("symlink");
        let real = root.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.path().join("USBKEY");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let request = mount_request(Some(link.clone()));
        let error = prepare_mount_target(&request).unwrap_err();
        assert!(error.contains("is a symbolic link"), "{}", error);
        assert!(link.is_symlink(), "the link must be left untouched");
    }

    #[test]
    fn preparing_refuses_a_non_directory() {
        let root = TempDir::new("file-target");
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();

        let request = mount_request(Some(file.clone()));
        let error = prepare_mount_target(&request).unwrap_err();
        assert!(error.contains("is not a directory"), "{}", error);
        assert!(file.is_file(), "the existing file must survive");
    }

    #[test]
    fn the_target_checker_accepts_a_missing_path_and_a_real_directory() {
        let root = TempDir::new("checker");
        assert_eq!(check_mount_target(&root.path().join("missing")), Ok(()));

        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(check_mount_target(&nested), Ok(()));
    }

    #[test]
    fn the_cleanup_guard_removes_the_created_directory_on_drop() {
        let root = TempDir::new("guard-drop");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();

        {
            let _guard = CreatedDirGuard::new(Some(target.clone()));
            // An early return (`suspend` / `resume` failure) leaves the scope
            // without disarming.
        }
        assert!(
            !target.exists(),
            "a request that never mounted removes its mount point"
        );
    }

    #[test]
    fn the_cleanup_guard_keeps_the_directory_after_disarm() {
        let root = TempDir::new("guard-disarm");
        let target = root.path().join("USBKEY");
        std::fs::create_dir_all(&target).unwrap();

        {
            let mut guard = CreatedDirGuard::new(Some(target.clone()));
            guard.disarm();
        }
        assert!(target.is_dir(), "a successful mount keeps its mount point");
    }

    #[test]
    fn the_cleanup_guard_ignores_a_reused_directory_and_content() {
        let root = TempDir::new("guard-none");
        // Nothing was created by us: dropping the guard is a no-op.
        drop(CreatedDirGuard::new(None));

        // Content that appeared in the meantime (a partially successful mount)
        // must never be destroyed: only empty directories are removed.
        let occupied = root.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("data"), b"x").unwrap();
        drop(CreatedDirGuard::new(Some(occupied.clone())));
        assert!(occupied.join("data").is_file());
    }

    #[test]
    fn cleanup_removes_only_empty_directories() {
        let root = TempDir::new("cleanup");

        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        remove_empty_dir(&empty);
        assert!(!empty.exists(), "an empty created mount point is removed");

        let occupied = root.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("data"), b"x").unwrap();
        remove_empty_dir(&occupied);
        assert!(occupied.is_dir(), "a non-empty directory must survive");

        // A missing path is a no-op, not a panic.
        remove_empty_dir(&root.path().join("missing"));
    }
}
