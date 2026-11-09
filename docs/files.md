# Files, saved queries and the workspace (1.3)

How Oxbase opens and saves query files, keeps saved queries per connection, remembers which
connection a file runs on, brings tabs back after a restart, and opens files from the operating
system.

## Saving queries

| Action | Keys | What happens |
|---|---|---|
| Save | Ctrl+S, toolbar **Save** | A tab that shows a file or a saved query writes to it. A new tab on a saved connection asks for a name and saves a **saved query** of that connection (or **Save to a file instead…**). A new tab without a connection asks for a file. |
| Save as file | Ctrl+Shift+S, **Save ▾ → Save as file…** | Always a file, chosen in the native save dialog. A name typed without an extension gets `.sql` (unless that file already exists). |
| Save as saved query | **Save ▾ → Save as saved query…** | A new saved query of the tab's connection. |

- A dot before a tab's title marks unsaved changes. Undoing back to the saved text clears it.
- Closing such a tab (close button, middle click, **Close**, **Close others**, **Close all** on
  the tab's right-click menu) asks: **Save**, **Don't save** or **Cancel**. **Close others** and
  **Close all** leave the side panels (Connections, Objects, Queries, History) open.
- Files are UTF-8. A file that starts with a byte order mark keeps it, and line endings stay as
  they are. Files larger than 16 MB are not opened.
- Saving replaces the file atomically where the system allows it (a temporary file next to it,
  renamed over it), keeping its permissions. A symbolic link stays a link; the file it points to
  is replaced.

## Saved queries

A saved query belongs to a saved connection. The **Queries** panel (next to Objects; the app
bar's **Queries** button shows it again) lists the saved queries of every connection, connected
or not. Double-click or Enter opens one in a query tab on its connection; F2 renames; Delete
deletes (after asking). A tab that shows a deleted query keeps its text, unsaved.

Each saved query is a plain `.sql` file, so other tools can open it too:

| Platform | Folder |
|---|---|
| Linux | `~/.local/share/oxbase/queries/<connection id>/` |
| macOS | `~/Library/Application Support/dev.oxbase.app/queries/<connection id>/` |
| Windows | `%LOCALAPPDATA%\oxbase\queries\<connection id>\` |

`OXBASE_DATA_DIR` moves the whole data folder. Names are 1 to 100 characters, without
`/ \ : * ? " < > |`, not starting or ending with a dot, and unique per connection (ignoring case).

## Which connection a file runs on

When you run a file on a connection, Oxbase remembers that connection and database for the file
(in the app database, not in the file). The next time the file opens (Ctrl+O, a recent file, a
double-click) it opens on that connection. When the connection is not open, the tab says so and
offers **Connect**. A file that never ran anywhere opens with no connection chosen. A saved
query always opens on its own connection.

Nothing runs when a file opens, wherever it came from.

## Recent files

**Open ▾** in the app bar lists the last 20 files opened or saved (files that no longer exist
are left out) and **Clear recent files**. Saved queries are not in the list; they are in the
Queries panel.

## The workspace

When Oxbase quits it keeps the open query tabs: their order, the active tab, the connection
and database of each, and their text when it is not the file's (unsaved changes, or a tab
without a file). The next start brings them back: files are read again from disk, unsaved
changes come back as they were (still marked unsaved). A file that was moved or deleted is
skipped, and a notice names it. Connections are not opened by themselves; a tab on a connection
that is not open offers **Connect**.

The workspace is kept a second after every change, and once more when the app closes, so a
crash loses at most the last second. Up to 200 tabs and 64 MB of unsaved text are kept. Other
tabs (table data, designer, sync) are not kept.

## Opening files from the operating system

The installers register two file types:

| Files | Opens as |
|---|---|
| `.sql` | A query tab |
| `.sqlite`, `.sqlite3`, `.db`, `.db3` | A SQLite connection: the saved connection of that file, or a new one named after it. A `.db` file that is not a SQLite database is refused. |

- Windows (`.msi`, `-setup.exe`), macOS (`.dmg`) and Linux `.deb` / `.rpm` register them. The
  AppImage cannot register anything; open files from it with **Open…** or the command line.
- Whether Oxbase becomes the default app for a type is up to the system (Windows and macOS may
  keep another default; choose Oxbase in **Open with**).
- Only one Oxbase runs at a time. Opening a file while it runs (double-click, `oxbase file.sql`)
  hands the file to the running window, which comes to the front. On Linux this uses the D-Bus
  session bus; without one, each launch is its own instance.
- Command line: `oxbase report.sql data.sqlite` (the program is `app-tauri` / `Oxbase`).
  Arguments starting with `-` are ignored; relative paths are relative to the current folder.
  At most 32 files per launch.

Developers: a debug build (`pnpm tauri dev`) shares the single-instance lock with an installed
Oxbase. Quit the installed app first, or the dev build hands its arguments to it and exits.

## How it works

- The webview never sends a file path. A tab that shows a file holds an opaque document id,
  which Rust maps to the path for the app run. Native dialogs are shown from Rust. The UI gets
  a file's name and, for display only, its folder (home shown as `~`).
- Rust (`crates/app-tauri/src/documents.rs`, `shell.rs`) and the store
  (`crates/oxbase-store/src/documents.rs`) do the file work; the UI is in
  `ui/src/workbench/documents/`.
- Files opened by the system and quit requests reach the UI through a Tauri `Channel`
  (`app_events_subscribe`), so the webview needs no event permission. Quitting waits up to
  3 seconds for the UI to keep the workspace.
- Recent files, file links and the workspace are JSON values in the app database's `settings`
  table (`documents.recent`, `documents.links`, `workspace`).
