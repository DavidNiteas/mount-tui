mod app;
mod disk;
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

    // Load devices before entering the alternate screen: a startup failure must
    // stay readable on the normal screen and be shown inside the TUI.
    let mut app = match disk::get_block_devices(&runner) {
        Ok(devices) => App::new(devices),
        Err(error) => {
            let mut app = App::new(Vec::new());
            app.action = ActionStatus::Error(format!("Failed to list block devices: {}", error));
            app
        }
    };

    let mut session = Session::new().context("Failed to initialise terminal")?;
    let result = run(&mut session, &mut app, &runner);

    // Restore the terminal before the error (if any) is printed.
    session.suspend()?;
    drop(session);

    if let Err(error) = result {
        eprintln!("Error: {}", error);
    }

    Ok(())
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
            'f' => app.force_unmount_selected(),
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
fn run_interactive(
    session: &mut Session,
    app: &mut App,
    mut request: InteractiveRequest,
    runner: &dyn runner::CommandRunner,
) -> anyhow::Result<()> {
    session.suspend()?;
    let outcome = execute_request(&request);

    if !outcome.ok {
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
