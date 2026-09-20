# SQLight

A convenient terminal (TUI) client for SQLite — a friendlier alternative to the
`sqlite3` CLI, with multiline editing, `TAB` autocomplete and a scrollable
results grid.

## Install / run

```sh
cargo build --release
./target/release/sqlight my_database.db
```

The DB file must already exist. If it is missing, inaccessible or locked,
SQLight exits with a message on stderr (it never creates the file for you).

## Usage

You get a `# ` prompt. Type SQL ending with `;` and hit `Enter` to run it.
Without a trailing `;`, `Enter` just adds a new line (multiline statements):

```
# select * from users;
# update users set
    name = "greg",
    age = 30
  where id = 10;
```

Internal commands (no `;` needed):

- `.tables` — list tables, one per row
- `.schema TABLE_NAME` — show `CREATE TABLE` + index statements for the table

### Keys

| Key | Action |
|---|---|
| `Enter` | Run (if `;`-terminated or dot-command) else newline |
| `Tab` / `Shift+Tab` | Autocomplete next/previous (needs ≥2 chars) |
| `Up` / `Down` | History (whole multiline entry, caret to end) or move within buffer |
| `Left` / `Right`, `Backspace`, `Delete` | Edit |
| `Esc` | Close autocomplete popup, or exit table view back to prompt |
| `w` (in table) | Wrap/unwrap long values (wrap shows up to 8 lines) |
| `Up`/`Down`/`Left`/`Right`, `PgUp`/`PgDn` (in table) | Scroll grid |
| `Ctrl+C` / `Ctrl+D` | Quit anywhere |

`SELECT` results open a full-screen scrollable grid. Long values are truncated
with `...`; press `w` to wrap them. Writes print green `Affected N rows` / `OK`;
errors are light-red, warnings (e.g. truncation, empty DB) orange.

## Compile-time config

Tweak `src/config.rs` (`MIN_COL_WIDTH`, `MAX_COL_WIDTH`, `MAX_ROWS`, etc.)
and rebuild. There is no live config file.

## Tech

Rust + Ratatui (crossterm backend) + rusqlite (bundled SQLite).
Each statement opens its own short-lived connection (`busy_timeout`), so the
DB file is locked only for the duration of the query.
