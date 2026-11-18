//! Injectable command layer for every **non-interactive** system command.
//!
//! `udisksctl`, `lsblk`, `fuser` and `lsof` are all executed through a
//! [`CommandRunner`]. Production uses [`SystemRunner`]; tests use the fake
//! runner in [`fake`] so command success / failure / retry / busy branches can
//! be exercised without touching the real system.
//!
//! Interactive commands (`sudo` with a password prompt) need inherited stdio
//! and are intentionally **not** part of this layer; `main::execute_request`
//! keeps running them directly.

use std::process::Command;

/// A fully described non-interactive command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Build a spec from string literals at the call site; arguments are
    /// copied into `String`s so dynamically built arguments can be appended.
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }
}

/// Result of a non-interactive command, deliberately free of
/// [`std::process::ExitStatus`] so tests can construct one directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Executes non-interactive commands.
pub trait CommandRunner {
    fn output(&self, spec: &CommandSpec) -> std::io::Result<CommandOutput>;
}

/// Production runner backed by [`std::process::Command::output`].
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn output(&self, spec: &CommandSpec) -> std::io::Result<CommandOutput> {
        let output = Command::new(&spec.program).args(&spec.args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Test double: replays a fixed sequence of results and records every call.
#[cfg(test)]
pub(crate) mod fake {
    use super::{CommandOutput, CommandRunner, CommandSpec};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;

    #[derive(Default)]
    pub(crate) struct FakeRunner {
        calls: RefCell<Vec<CommandSpec>>,
        results: RefCell<VecDeque<io::Result<CommandOutput>>>,
    }

    impl FakeRunner {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Queue a successful run with `stdout`.
        pub(crate) fn push_ok(mut self, stdout: &str) -> Self {
            self.results.get_mut().push_back(Ok(CommandOutput {
                success: true,
                code: Some(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            }));
            self
        }

        /// Queue a failed run (exit status 1) with `stderr`.
        pub(crate) fn push_fail(self, stderr: &str) -> Self {
            self.push_fail_with(1, stderr)
        }

        /// Queue a failed run with an explicit exit code and `stderr`.
        pub(crate) fn push_fail_with(mut self, code: i32, stderr: &str) -> Self {
            self.results.get_mut().push_back(Ok(CommandOutput {
                success: false,
                code: Some(code),
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            }));
            self
        }

        /// Queue an `io::Error` (the program could not be spawned).
        pub(crate) fn push_err(mut self, message: &str) -> Self {
            self.results
                .get_mut()
                .push_back(Err(io::Error::new(io::ErrorKind::NotFound, message)));
            self
        }

        /// Every command that reached the runner, in call order.
        pub(crate) fn calls(&self) -> Vec<CommandSpec> {
            self.calls.borrow().clone()
        }

        pub(crate) fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl CommandRunner for FakeRunner {
        fn output(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
            self.calls.borrow_mut().push(spec.clone());
            match self.results.borrow_mut().pop_front() {
                Some(result) => result,
                None => panic!("FakeRunner: unexpected command {:?}", spec),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRunner;

    #[test]
    fn spec_keeps_program_and_args() {
        let spec = CommandSpec::new("lsblk", &["-o", "NAME", "--json"]);
        assert_eq!(spec.program, "lsblk");
        assert_eq!(spec.args, vec!["-o", "NAME", "--json"]);
    }

    #[test]
    fn fake_runner_replays_results_and_records_calls() {
        let runner = FakeRunner::new()
            .push_ok("mounted")
            .push_fail("busy")
            .push_err("no binary");

        let first = runner
            .output(&CommandSpec::new("udisksctl", &["mount"]))
            .unwrap();
        assert!(first.success);
        assert_eq!(first.stdout, b"mounted");

        let second = runner
            .output(&CommandSpec::new("udisksctl", &["unmount"]))
            .unwrap();
        assert!(!second.success);
        assert_eq!(second.code, Some(1));
        assert_eq!(second.stderr, b"busy");

        assert!(runner.output(&CommandSpec::new("fuser", &["-vm"])).is_err());

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].program, "udisksctl");
        assert_eq!(calls[1].args, vec!["unmount"]);
        assert_eq!(calls[2].program, "fuser");
        assert_eq!(runner.call_count(), 3);
    }
}
