import { Monitor, Moon, Sun } from "lucide-react";
import { ICON_SM } from "./components/icons";
import { SessionCounter } from "./connections/SessionCounter";
import { useConnections } from "./connections/store";
import { getAppInfo, NoCoreError } from "./ipc";
import { errorMessage, useAsync } from "./useAsync";
import {
  setThemePreference,
  type ThemePreference,
  useThemePreference,
} from "./workbench/theme-bridge";

const THEMES: { value: ThemePreference; label: string; Icon: typeof Moon }[] = [
  { value: "dark", label: "Dark theme", Icon: Moon },
  { value: "light", label: "Light theme", Icon: Sun },
  { value: "system", label: "Follow the system theme", Icon: Monitor },
];

function ThemeSwitch() {
  const current = useThemePreference();
  return (
    <fieldset className="theme-switch">
      <legend className="visually-hidden">Theme</legend>
      {THEMES.map(({ value, label, Icon }) => (
        <button
          key={value}
          type="button"
          aria-pressed={current === value}
          aria-label={label}
          title={label}
          className="theme-switch__option"
          onClick={() => setThemePreference(value)}
        >
          <Icon {...ICON_SM} />
        </button>
      ))}
    </fieldset>
  );
}

export function StatusBar() {
  const info = useAsync(getAppInfo);
  const { sessions } = useConnections();
  const open = sessions.length;

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
    <footer className="status-bar" data-testid="status-bar">
      <span className="status-bar__item" data-testid="open-connections" aria-live="polite">
        <span
          className={open > 0 ? "status-dot" : "status-dot status-dot--idle"}
          aria-hidden="true"
        />
        {open === 1 ? "1 open connection" : `${open} open connections`}
      </span>
      <SessionCounter />
      <span className="status-bar__spacer" />
      <span className="status-bar__item" data-testid="core-version">
        {text}
      </span>
      <ThemeSwitch />
    </footer>
  );
}
