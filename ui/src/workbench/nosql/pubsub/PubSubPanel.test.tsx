import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PubSubEvent } from "../../../api";
import { tauriCore } from "../../../test/tauriMock";
import { bufferEvent, EMPTY_BUFFER, nameList, PubSubPanel } from "./PubSubPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../../test/tauriMock")).tauriCore);

let onEvent: ((event: PubSubEvent) => void) | null = null;

function message(payload: string, channel = "m7:news", pattern?: string): PubSubEvent {
  return {
    type: "message",
    channel: { encoding: "utf8", text: channel },
    ...(pattern ? { pattern: { encoding: "utf8", text: pattern } } : {}),
    payload: { encoding: "utf8", text: payload },
    receivedAtMs: 1_700_000_000_000,
  };
}

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "pubsub_subscribe":
        onEvent = (a.onEvent as { onmessage: (e: PubSubEvent) => void }).onmessage;
        return "sub-1";
      case "pubsub_unsubscribe":
        return true;
      case "pubsub_publish":
        return 2;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

function calls(cmd: string) {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, args]) => args as Record<string, unknown>);
}

describe("pub/sub buffer", () => {
  it("keeps the newest messages and counts what was dropped", () => {
    let b = EMPTY_BUFFER;
    for (let i = 0; i < 5; i += 1) b = bufferEvent(b, message(`m${i}`), 3);
    b = bufferEvent(b, { type: "dropped", count: 7 }, 3);
    expect(b.messages.map((m) => m.payload)).toEqual(["m2", "m3", "m4"]);
    expect(b.trimmed).toBe(2);
    expect(b.droppedByCore).toBe(7);
    const bin = bufferEvent(EMPTY_BUFFER, {
      ...message(""),
      payload: { encoding: "hex", hex: "00ff" },
    } as PubSubEvent);
    expect(bin.messages[0]).toMatchObject({ payload: "\\x00\\xff", binary: true });
  });

  it("splits channel lists", () => {
    expect(nameList(" a, ,b ")).toEqual([
      { encoding: "utf8", text: "a" },
      { encoding: "utf8", text: "b" },
    ]);
  });
});

describe("PubSubPanel", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    onEvent = null;
    mockCore();
  });

  it("subscribes, lists messages, publishes and unsubscribes when closed", async () => {
    const { unmount } = render(<PubSubPanel sessionId="s-1" />);
    fireEvent.change(screen.getByTestId("pubsub-channels"), { target: { value: "m7:news" } });
    fireEvent.change(screen.getByPlaceholderText("news:*"), { target: { value: "m7:*" } });
    fireEvent.click(screen.getByTestId("pubsub-subscribe"));
    await waitFor(() => expect(calls("pubsub_subscribe")).toHaveLength(1));
    expect(calls("pubsub_subscribe")[0]).toMatchObject({
      sessionId: "s-1",
      request: {
        channels: [{ encoding: "utf8", text: "m7:news" }],
        patterns: [{ encoding: "utf8", text: "m7:*" }],
      },
    });
    act(() => {
      onEvent?.({
        type: "subscribed",
        channels: [{ encoding: "utf8", text: "m7:news" }],
        patterns: [{ encoding: "utf8", text: "m7:*" }],
      });
      onEvent?.(message("hello"));
      onEvent?.(message("hello", "m7:news", "m7:*"));
      onEvent?.({ type: "dropped", count: 4 });
    });
    expect(screen.getByTestId("pubsub-status")).toHaveTextContent("Listening to m7:news, m7:*");
    const rows = screen.getAllByTestId("pubsub-message");
    expect(rows).toHaveLength(2);
    expect(rows[1]).toHaveTextContent("m7:news via m7:*hello");
    expect(screen.getByTestId("pubsub-dropped")).toHaveTextContent("4 messages were not kept");

    fireEvent.change(screen.getByTestId("publish-channel"), { target: { value: "m7:news" } });
    fireEvent.change(screen.getByTestId("publish-payload"), { target: { value: "ping" } });
    fireEvent.click(screen.getByTestId("publish-button"));
    expect(await screen.findByText("Published to 2 subscribers.")).toBeInTheDocument();
    expect(calls("pubsub_publish")[0]).toMatchObject({
      sessionId: "s-1",
      channel: { encoding: "utf8", text: "m7:news" },
      payload: { encoding: "utf8", text: "ping" },
    });

    unmount();
    await waitFor(() => expect(calls("pubsub_unsubscribe")).toEqual([{ subscriptionId: "sub-1" }]));
  });

  it("holds messages while paused", async () => {
    render(<PubSubPanel sessionId="s-1" />);
    fireEvent.change(screen.getByTestId("pubsub-channels"), { target: { value: "c" } });
    fireEvent.click(screen.getByTestId("pubsub-subscribe"));
    await waitFor(() => expect(calls("pubsub_subscribe")).toHaveLength(1));
    await waitFor(() => expect(screen.getByRole("button", { name: "Pause" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "Pause" }));
    act(() => onEvent?.(message("later", "c")));
    expect(screen.queryAllByTestId("pubsub-message")).toHaveLength(0);
    fireEvent.click(screen.getByRole("button", { name: /Resume/ }));
    expect(screen.getAllByTestId("pubsub-message")).toHaveLength(1);
  });

  it("needs a channel or pattern", () => {
    render(<PubSubPanel sessionId="s-1" />);
    fireEvent.click(screen.getByTestId("pubsub-subscribe"));
    expect(screen.getByRole("alert")).toHaveTextContent("Enter at least one channel or pattern.");
    expect(calls("pubsub_subscribe")).toHaveLength(0);
  });
});
