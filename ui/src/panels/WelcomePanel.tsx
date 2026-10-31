import { fetchDemoPage, NoCoreError } from "../ipc";
import { errorMessage, useAsync } from "../useAsync";

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

export function WelcomePanel() {
  return (
    <section className="panel panel--welcome" aria-label="Welcome">
      <h1>Welcome to oxbase</h1>
      <p>A database client with a Rust core. Add a connection to get started.</p>
      <p className="welcome__demo" data-testid="demo-page">
        <DemoPageStatus />
      </p>
    </section>
  );
}
