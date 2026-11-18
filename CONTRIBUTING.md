# Contributing

Thanks for taking a look at mount-tui.

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

End-to-end tests use mocked system tools and `tmux`; they never touch a real
device, mount or `sudo` prompt:

```bash
cargo build
tests/e2e/run-all.sh
```

Requirements for the e2e suite: Linux, `tmux`, `python3`. See
[`tests/e2e/README.md`](tests/e2e/README.md).

## Changes

- Keep the TUI and CLI modes consistent: safety checks, mount-target policy and
  configuration precedence are shared behavior.
- Add or update tests for behavior changes. Prefer unit tests with the injected
  command runner; add end-to-end coverage for interactive/CLI paths.
- Run `cargo fmt` and keep `cargo clippy --all-targets -- -D warnings` clean.
- Do not commit build artifacts or test state; `target/` and e2e temporary
  directories are ignored.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as `MIT OR Apache-2.0`, without any additional terms or
conditions. See [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).
