import {
  DockviewReact,
  type DockviewReadyEvent,
  type DockviewTheme,
  type IDockviewPanelProps,
  themeLight,
} from "dockview-react";
import type { FunctionComponent } from "react";
import { ConnectionsPanel } from "./panels/ConnectionsPanel";
import { WelcomePanel } from "./panels/WelcomePanel";
import { StatusBar } from "./StatusBar";

const components: Record<string, FunctionComponent<IDockviewPanelProps>> = {
  connections: () => <ConnectionsPanel />,
  welcome: () => <WelcomePanel />,
};

// Structural variables come from dockview's light theme; colours are overridden in
// theme.css with our own CSS variables, which follow the OS light/dark preference.
const theme: DockviewTheme = { ...themeLight, name: "oxbase" };

function onReady(event: DockviewReadyEvent) {
  const welcome = event.api.addPanel({ id: "welcome", component: "welcome", title: "Welcome" });
  event.api.addPanel({
    id: "connections",
    component: "connections",
    title: "Connections",
    position: { referencePanel: welcome, direction: "left" },
    initialWidth: 260,
  });
  welcome.api.setActive();
}

export function App() {
  return (
    <div className="app">
      <main className="app__workspace">
        <DockviewReact components={components} onReady={onReady} theme={theme} />
      </main>
      <StatusBar />
    </div>
  );
}
