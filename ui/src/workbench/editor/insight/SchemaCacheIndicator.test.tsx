import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CacheStatus } from "../../../api";
import { tauriCore } from "../../../test/tauriMock";
import { SchemaCacheIndicator } from "./SchemaCacheIndicator";
import { CACHE_POLL_MS, useSchemaCache } from "./useSchemaCache";

vi.mock("@tauri-apps/api/core", async () => (await import("../../../test/tauriMock")).tauriCore);

function status(extra: Partial<CacheStatus> = {}): CacheStatus {
  return {
    key: { profileId: "p-1", database: "shop" },
    state: "ready",
    source: "live",
    building: false,
    generation: 7,
    readAt: "2026-11-01T10:00:00Z",
    relationCount: 1204,
    columnCount: 18_000,
    routineCount: 31,
    truncated: false,
    lastBuildMs: 840,
    lastError: null,
    ...extra,
  };
}

function Harness({ sessionId }: { sessionId: string | null }) {
  const view = useSchemaCache(sessionId, "shop");
  return <SchemaCacheIndicator view={view} connected={sessionId !== null} />;
}

function calls(cmd: string) {
  return tauriCore.invoke.mock.calls.filter(([c]) => c === cmd).map(([, a]) => a);
}

async function settle() {
  await act(async () => {
    for (let i = 0; i < 5; i += 1) await Promise.resolve();
  });
}

beforeEach(() => {
  tauriCore.invoke.mockReset();
  tauriCore.isTauri.mockReturnValue(true);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("schema cache indicator", () => {
  it("shows the cache state and its details", async () => {
    tauriCore.invoke.mockResolvedValue(status());
    render(<Harness sessionId="s-1" />);
    await settle();
    const indicator = screen.getByTestId("schema-cache-status");
    expect(indicator).toHaveTextContent("Schema: 1,204 tables");
    expect(indicator).toHaveAttribute("data-state", "ready");
    expect(indicator.getAttribute("title")).toContain("18,000 columns, 31 routines");
    expect(calls("schema_cache_status")).toEqual([{ sessionId: "s-1", database: "shop" }]);
  });

  it("polls every 2 s while a build runs", async () => {
    vi.useFakeTimers();
    tauriCore.invoke.mockResolvedValueOnce(
      status({ state: "missing", building: true, relationCount: 0 }),
    );
    render(<Harness sessionId="s-1" />);
    await settle();
    expect(screen.getByTestId("schema-cache-status")).toHaveTextContent("Schema: loading…");
    tauriCore.invoke.mockResolvedValueOnce(status({ state: "missing", building: true }));
    await act(async () => {
      vi.advanceTimersByTime(CACHE_POLL_MS);
    });
    await settle();
    expect(calls("schema_cache_status")).toHaveLength(2);
    tauriCore.invoke.mockResolvedValueOnce(status({ relationCount: 3 }));
    await act(async () => {
      vi.advanceTimersByTime(CACHE_POLL_MS);
    });
    await settle();
    expect(screen.getByTestId("schema-cache-status")).toHaveTextContent("Schema: 3 tables");
    await act(async () => {
      vi.advanceTimersByTime(CACHE_POLL_MS * 3);
    });
    await settle();
    expect(calls("schema_cache_status")).toHaveLength(3);
  });

  it("refreshes the whole database on demand", async () => {
    tauriCore.invoke.mockImplementation(async (cmd) => {
      if (cmd === "schema_cache_status") return status({ state: "stale" });
      if (cmd === "schema_cache_refresh") return status({ relationCount: 1205, generation: 8 });
      throw new Error(`unexpected ${cmd}`);
    });
    render(<Harness sessionId="s-1" />);
    await settle();
    expect(screen.getByTestId("schema-cache-status")).toHaveTextContent("(stale)");
    fireEvent.click(screen.getByRole("button", { name: "Refresh schema cache" }));
    await settle();
    const [args] = calls("schema_cache_refresh") as Record<string, unknown>[];
    expect(args).toMatchObject({ sessionId: "s-1", database: "shop", scope: { type: "full" } });
    expect(typeof args?.requestId).toBe("string");
    expect(screen.getByTestId("schema-cache-status")).toHaveTextContent("Schema: 1,205 tables");
  });

  it("says when the cache cannot be read and when there is no connection", async () => {
    tauriCore.invoke.mockRejectedValue({ category: "internal", message: "no cache service" });
    const view = render(<Harness sessionId="s-1" />);
    await settle();
    const indicator = screen.getByTestId("schema-cache-status");
    expect(indicator).toHaveTextContent("Schema: unavailable");
    expect(indicator).toHaveAttribute("title", "no cache service");
    view.rerender(<Harness sessionId={null} />);
    await settle();
    expect(screen.getByTestId("schema-cache-status")).toHaveTextContent("Schema: no connection");
    expect(screen.getByRole("button", { name: "Refresh schema cache" })).toBeDisabled();
  });

  it("reports a failed build", async () => {
    tauriCore.invoke.mockResolvedValue(
      status({ state: "failed", lastError: { category: "auth", message: "denied" } }),
    );
    render(<Harness sessionId="s-1" />);
    await settle();
    const indicator = screen.getByTestId("schema-cache-status");
    expect(indicator).toHaveTextContent("Schema: failed");
    expect(indicator.getAttribute("title")).toContain("Last error: denied");
  });
});
