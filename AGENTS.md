# AGENTS.md — SQLight contributor guide

This file is for AI coding agents (Claude, Codex, Cursor, Antigravity,
OpenCode, etc.) and human contributors. Keep it harness-agnostic.

## Project map

- `src/main.rs` — startup (arg/file preflight), terminal init/restore, event loop.
- `src/cli.rs` — clap arg parsing (exactly one `DB_PATH`).
- `src/config.rs` — ALL tunable constants (prompt, widths, row caps, colors).
  Change values here, never hardcode elsewhere.
- `src/db.rs` — SQLite access. Open-per-statement (`Connection::open` +
  `busy_timeout`), never hold a global connection.
- `src/parser.rs` — trailing-`;` detection (string/comment aware),
  statement classification, dot-command parsing. Pure functions, unit-test them.
- `src/editor.rs` — `InputBuffer` (multiline, char-based cursor), `History`,
  `Completer` (TAB cycling, ≥2 char gate).
- `src/app.rs` — `Mode::{Input, Table}`, scrollback, key dispatch, execution glue.
- `src/ui.rs` — Ratatui rendering + color helpers (`line_ok/line_err/line_warn`).
- `src/table_view.rs` — grid state, truncation (`...`), wrap (max 8 lines), scrolling.
- `.temp/` — local scratch (demo DBs, scripts). NEVER commit; it is git-ignored.

## Commands

```sh
cargo fmt --check   # or: cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
cargo run -- .temp/demo.db
```

Run `fmt` + `clippy` + `test` before every commit. Keep `cargo build` warning-free.

## Conventions

- Rust 2024 edition, latest stable toolchain.
- Ratatui is immediate-mode: build stateless `render()` from `App` each frame;
  keep cursor/scroll in state structs, handle resize by redrawing.
- rusqlite with `bundled` feature; map `rusqlite::Error` to user-facing
  `Error: ...` lines, never panic on SQL errors. File IO failures pre-TUI exit
  via stderr + non-zero code.
- Unicode: use `unicode-width` for widths; cursor math on `char`s, never bytes.
- Colors: success green, error light-red, warning orange (see `config.rs`);
  always keep a text prefix (`Error:`, `Warning:`), color is not the only signal.
- Internal commands: only `.tables` and `.schema TABLE`. Unknown `.foo` is an error.
- Tests: unit-test `parser`/`editor`/`table_view` logic; integration tests use
  `tempfile` or `.temp/` DBs, never the repo root.

## Workflow

- Small focused commits, conventional messages (`feat:`, `fix:`, `docs:`, ...).
- Do not add dependencies without need; prefer std + current deps.
- Do not commit `.temp/`, `*.db`, `target/`, secrets, or remote config.
- Local commits only unless the user explicitly asks for push/PR.
