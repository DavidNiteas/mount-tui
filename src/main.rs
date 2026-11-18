mod app;
mod cli;
mod config;
mod disk;
mod mount;
mod runner;
mod ui;

use std::io::{self, Write};
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

fn main() -> anyhow::Result<()> {
    install_panic_hook();

    // The single production command runner; every non-interactive system
    // command goes through it.
    let runner = SystemRunner;

    let invocation = match cli::parse(program_args()) {
        Ok(cli::Parsed::Run(invocation)) => invocation,
        Ok(cli::Parsed::Help) => {
            print!(
                "{}",
                config::help_text(config::current_config_path().as_deref())
            );
            return Ok(());
        }
        Ok(cli::Parsed::Version) => {
            println!("mount-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(message) => {
            eprintln!("mount-tui: {}", message);
            eprintln!("Try 'mount-tui --help' for more information.");
            std::process::exit(cli::EXIT_USAGE);
        }
    };

    // A broken configuration must be readable on the normal screen: it is
    // reported before the terminal is switched to the alternate screen.
    let config = match config::load(&invocation.overrides) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mount-tui: configuration error: {:#}", error);
            std::process::exit(cli::EXIT_USAGE);
        }
    };

    // A CLI command never touches terminal state: no raw mode, no alternate
    // screen, no TUI main loop. It finishes with its own exit code.
    if invocation.command != cli::Command::Tui {
        let interactive = cli::SystemInteractiveRunner;
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        let code = cli::run(
            &config,
            &invocation.command,
            &runner,
            &interactive,
            &mut stdout,
            &mut stderr,
        );
        let _ = stdout.flush();
        std::process::exit(code);
    }

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
    // The search input line owns the keyboard: typed characters belong to the
    // query (`q` included) and no global shortcut fires. Only Ctrl-C still
    // quits, so a lost input line can never trap the user.
    if app.search_editing {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.running = false;
            }
            KeyCode::Enter => app.confirm_search(),
            KeyCode::Esc => app.cancel_search(),
            KeyCode::Backspace => app.search_backspace(),
            KeyCode::Char(character) => app.search_push_char(character),
            _ => {}
        }
        return None;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.running = false;
            None
        }
        KeyCode::Char('q') => {
            app.running = false;
            None
        }
        KeyCode::Esc => {
            // An applied filter is cleared first: Esc only quits the list view.
            if app.search.is_empty() {
                app.running = false;
            } else {
                app.clear_search();
            }
            None
        }
        KeyCode::Char('/') => {
            app.begin_search();
            None
        }
        KeyCode::Char('[') => {
            app.scroll_details_up();
            None
        }
        KeyCode::Char(']') => {
            app.scroll_details_down();
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
            'l' => {
                app.toggle_loops();
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
    let created = match mount::prepare_mount_target(request.mount_target.as_deref()) {
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
    let mut cleanup = mount::CreatedDirGuard::new(created);

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
