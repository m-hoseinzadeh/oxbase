import { Database, FileInput, History, type LucideIcon, Plug, SquarePen } from "lucide-react";
import { ICON, ICON_LG } from "../components/icons";
import { requestNewConnection } from "../connections/requests";
import { fetchDemoPage, NoCoreError } from "../ipc";
import { errorMessage, useAsync } from "../useAsync";
import { openQueryEditor, showHistory } from "../workbench/layout";
import { openImport } from "../workbench/transfer/store";

function DemoPageStatus() {
  const page = useAsync(fetchDemoPage);
  switch (page.status) {
    case "loading":
      return <span>Loading demo result page…</span>;
    case "ok":
      return (
        <span>
          Demo Arrow page decoded:{" "}
          <strong data-testid="demo-row-count">{page.value.numRows}</strong> rows ×{" "}
          {page.value.numCols} columns
        </span>
      );
    case "error":
      return (
        <span>
          {page.error instanceof NoCoreError
            ? "Demo Arrow page needs the desktop shell."
            : `Demo Arrow page failed: ${errorMessage(page.error)}`}
        </span>
      );
  }
}

interface Action {
  title: string;
  hint: string;
  Icon: LucideIcon;
  run: () => void;
  primary?: boolean;
}

const ACTIONS: Action[] = [
  {
    title: "New connection",
    hint: "PostgreSQL, MySQL, MariaDB, SQLite or SQL Server",
    Icon: Plug,
    run: () => requestNewConnection(),
    primary: true,
  },
  {
    title: "New query",
    hint: "Open a SQL editor tab",
    Icon: SquarePen,
    run: () => openQueryEditor(),
  },
  {
    title: "Import data",
    hint: "CSV, JSON, Excel or Parquet into a table",
    Icon: FileInput,
    run: () => openImport(),
  },
  {
    title: "Query history",
    hint: "Search and reopen past runs",
    Icon: History,
    run: showHistory,
  },
];

const SHORTCUTS: { keys: string[]; label: string }[] = [
  { keys: ["Ctrl", "Enter"], label: "Run the statement at the cursor" },
  { keys: ["Ctrl", "Shift", "Enter"], label: "Run the whole script" },
  { keys: ["Ctrl", "Space"], label: "Show completions" },
  { keys: ["Shift", "Alt", "F"], label: "Format SQL" },
];

export function WelcomePanel() {
  return (
    <section className="panel panel--welcome" aria-label="Welcome">
      <div className="welcome">
        <header className="welcome__hero">
          <span className="welcome__mark" aria-hidden="true">
            <Database size={24} strokeWidth={2} />
          </span>
          <div>
            <h1>Welcome to Oxbase</h1>
            <p>A database client with a Rust core. Add a connection to get started.</p>
          </div>
        </header>

        <section aria-labelledby="welcome-start">
          <h2 id="welcome-start" className="welcome__section-title">
            Start
          </h2>
          <div className="welcome__actions">
            {ACTIONS.map(({ title, hint, Icon, run, primary }) => (
              <button
                key={title}
                type="button"
                className={primary ? "action-card action-card--primary" : "action-card"}
                onClick={run}
              >
                <span className="action-card__icon">
                  <Icon {...(primary ? ICON_LG : ICON)} />
                </span>
                <span>
                  <span className="action-card__title">{title}</span>
                  <span className="action-card__hint">{hint}</span>
                </span>
              </button>
            ))}
          </div>
        </section>

        <section aria-labelledby="welcome-keys">
          <h2 id="welcome-keys" className="welcome__section-title">
            Keyboard
          </h2>
          <dl className="shortcut-list">
            {SHORTCUTS.map(({ keys, label }) => (
              <div key={label} className="shortcut-list__row">
                <dt>
                  {keys.map((key) => (
                    <kbd key={key} className="kbd">
                      {key}
                    </kbd>
                  ))}
                </dt>
                <dd>{label}</dd>
              </div>
            ))}
          </dl>
        </section>

        <p className="welcome__demo" data-testid="demo-page">
          <DemoPageStatus />
        </p>
      </div>
    </section>
  );
}
