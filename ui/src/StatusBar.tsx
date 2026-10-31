import { getAppInfo, NoCoreError } from "./ipc";
import { errorMessage, useAsync } from "./useAsync";

export function StatusBar() {
  const info = useAsync(getAppInfo);

  let text: string;
  switch (info.status) {
    case "loading":
      text = "Connecting to core…";
      break;
    case "ok":
      text = `oxbase core ${info.value.version} · ${info.value.platform}/${info.value.arch}`;
      break;
    case "error":
      text =
        info.error instanceof NoCoreError
          ? "Core unavailable (browser preview)"
          : `Core error: ${errorMessage(info.error)}`;
      break;
  }

  return (
    <footer className="status-bar">
      <span className="status-bar__item" data-testid="core-version">
        {text}
      </span>
    </footer>
  );
}
