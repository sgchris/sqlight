# SQLight

A convenient terminal (TUI) client for **SQLite** and **PostgreSQL** — a
friendlier alternative to the `sqlite3` / `psql` CLIs, with multiline editing,
`TAB` autocomplete and a scrollable results grid.

More database backends are planned; connection types that are not yet implemented
are rejected with a clear error and do not affect other entries in your config.

<img width="1115" height="628" alt="image" src="https://github.com/user-attachments/assets/f97e4a9e-642d-4d00-9e72-b47786ee1816" />


## Supported databases

| Backend      | How you connect |
|--------------|-----------------|
| **SQLite**   | Path to an existing `.db` file (or any SQLite database path) |
| **PostgreSQL** | Named entry in `~/.config/sqlight/connections.json` |

## Install / run

```sh
cargo build --release
./target/release/sqlight my_database.db
```

The DB file must already exist. If it is missing, inaccessible or locked,
SQLight exits with a message on stderr (it never creates the file for you).

### PostgreSQL (named connections)

```sh
./target/release/sqlight prod1
```

If the argument is not an existing file, SQLight looks it up by name in
`~/.config/sqlight/connections.json` (a file always wins over a same-named
connection). Running `sqlight` with no argument prints the full expected path.

```json
{
  "prod1":    {"type": "postgresql", "host": "db.example.test", "port": 5432,
               "database": "app", "user": "reader", "password": "..."},
  "staging1": {"type": "postgresql", "host": "staging.example.test", "port": 5432,
               "database": "app", "user": "reader"}
}
```

- `port` is an integer; the other fields are strings.
- Without a `password` key you are prompted for it (hidden input).
  `"password": ""` means an empty password.
- Use `"type": "postgresql"` for Postgres; additional `type` values will be
  added over time.
- TLS is preferred: the connection is encrypted when the server supports it
  (certificates are not verified, like libpq `sslmode=prefer`), else plain.
- Failed connections exit with an error before the UI starts. Everything else
  (autocomplete, `.tables`, `.schema`, grid, history) works as with SQLite.
  `.schema` reconstructs DDL from the catalog; tables outside `public` are
  listed as `schema.table`.

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
- `.clear` — clear all output

### Keys

| Key | Action |
|---|---|
| `Enter` | Run (if `;`-terminated or dot-command) else newline |
| `Tab` / `Shift+Tab` | Autocomplete next/previous (needs ≥2 chars) |
| `Up` / `Down` | History (whole multiline entry, caret to end) or move within buffer |
| `Shift+Up` / `Shift+Down`, `PgUp` / `PgDn` (in prompt) | Scroll output (latest shown by default) |
| `Left` / `Right`, `Backspace`, `Delete` | Edit |
| `Esc` | Close autocomplete popup, or exit table view back to prompt |
| `w` (in table) | Wrap/unwrap long values (wrap shows up to 8 lines) |
| `r` (in table) | Refresh: re-run the query |
| `Up`/`Down`/`Left`/`Right`, `h`/`j`/`k`/`l`, `PgUp`/`PgDn` (in table) | Scroll grid (`h` left, `j` down, `k` up, `l` right) |
| `Ctrl+C` (in prompt, empty input) | Quit (press twice within 3s to confirm) |
| `Ctrl+C` (in prompt, with input) | Clear input (all lines) |
| `Ctrl+C` (in table) | Back to prompt |
| `Ctrl+D` | Quit anywhere |

`SELECT` results open a full-screen scrollable grid. Long values are truncated
with `...`; press `w` to wrap them. Writes print green `Affected N rows` / `OK`;
errors are light-red, warnings (e.g. truncation, empty DB) orange.

Command history (`Up`/`Down`, up to 200 entries) persists between sessions in
a per-user file: `%APPDATA%\sqlight\history` on Windows,
`~/Library/Application Support/sqlight/history` on macOS, and
`$XDG_DATA_HOME/sqlight/history` (or `~/.local/share/sqlight/history`) on Linux.

## Compile-time config

Tweak `src/config.rs` (`MIN_COL_WIDTH`, `MAX_COL_WIDTH`, `MAX_ROWS`, etc.)
and rebuild. There is no live config file.

## Tech

Rust + Ratatui (crossterm backend) + rusqlite (bundled SQLite) + `postgres`.
For SQLite each statement opens its own short-lived connection
(`busy_timeout`), so the DB file is locked only for the duration of the query.
PostgreSQL keeps one session open; `SELECT`s are read through a server-side
cursor capped at `MAX_ROWS`.
