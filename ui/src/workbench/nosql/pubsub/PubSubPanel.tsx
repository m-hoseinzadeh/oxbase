/**
 * Publish / subscribe (Redis, docs/m7-contracts.md §6): subscribe to channels and patterns on a
 * dedicated connection (`pubsub_subscribe`, events on a channel), watch the messages arrive in
 * a bounded list, and publish (`pubsub_publish`). The core drops messages when the UI falls
 * behind and says how many (`dropped`); this list keeps the newest {@link MAX_MESSAGES} and
 * counts the ones it let go. Closing the tab unsubscribes.
 */
import { CircleAlert, Pause, Play, Radio, Send, Trash2, Unplug } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  type BinaryText,
  displayBinary,
  type PubSubEvent,
  pubsubPublish,
  pubsubSubscribe,
  pubsubUnsubscribe,
  utf8,
} from "../../../api";
import { ICON, ICON_LG, ICON_SM } from "../../../components/icons";
import { toCommandError } from "../../../connections/api";
import type { PubSubPanelParams } from "../../layout";

/** Messages the list keeps (older ones are dropped and counted). */
export const MAX_MESSAGES = 1000;

export interface ShownMessage {
  id: number;
  channel: string;
  pattern?: string;
  payload: string;
  binary: boolean;
  receivedAtMs: number;
}

export interface MessageBuffer {
  messages: ShownMessage[];
  /** Dropped by the core (a full sink) plus trimmed here. */
  droppedByCore: number;
  trimmed: number;
  nextId: number;
}

export const EMPTY_BUFFER: MessageBuffer = {
  messages: [],
  droppedByCore: 0,
  trimmed: 0,
  nextId: 1,
};

/** Folds one event into the bounded buffer. */
export function bufferEvent(
  buffer: MessageBuffer,
  event: PubSubEvent,
  max = MAX_MESSAGES,
): MessageBuffer {
  if (event.type === "dropped")
    return { ...buffer, droppedByCore: buffer.droppedByCore + event.count };
  if (event.type !== "message") return buffer;
  const message: ShownMessage = {
    id: buffer.nextId,
    channel: displayBinary(event.channel),
    ...(event.pattern ? { pattern: displayBinary(event.pattern) } : {}),
    payload: displayBinary(event.payload),
    binary: event.payload.encoding === "hex",
    receivedAtMs: event.receivedAtMs,
  };
  const messages = [...buffer.messages, message];
  const over = Math.max(0, messages.length - max);
  return {
    messages: over > 0 ? messages.slice(over) : messages,
    droppedByCore: buffer.droppedByCore,
    trimmed: buffer.trimmed + over,
    nextId: buffer.nextId + 1,
  };
}

/** `a, b ,c` → the names as `BinaryText`s (empty entries left out). */
export function nameList(text: string): BinaryText[] {
  return text
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "")
    .map(utf8);
}

type SubState =
  | { status: "idle" }
  | { status: "subscribing" }
  | { status: "subscribed"; id: string; channels: string[]; patterns: string[] }
  | { status: "closed"; error?: string };

function time(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

export function PubSubPanel({ sessionId }: PubSubPanelParams) {
  const [channels, setChannels] = useState("");
  const [patterns, setPatterns] = useState("");
  const [sub, setSub] = useState<SubState>({ status: "idle" });
  const [buffer, setBuffer] = useState<MessageBuffer>(EMPTY_BUFFER);
  const [paused, setPaused] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [publishChannel, setPublishChannel] = useState("");
  const [payload, setPayload] = useState("");
  const [published, setPublished] = useState<string | null>(null);
  const subId = useRef<string | null>(null);
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const held = useRef<PubSubEvent[]>([]);

  const unsubscribe = useCallback(async () => {
    const id = subId.current;
    subId.current = null;
    if (id) await pubsubUnsubscribe(id).catch(() => false);
  }, []);

  useEffect(
    () => () => {
      void unsubscribe();
    },
    [unsubscribe],
  );

  async function subscribe() {
    const request = { channels: nameList(channels), patterns: nameList(patterns) };
    if (request.channels.length === 0 && request.patterns.length === 0) {
      setError("Enter at least one channel or pattern.");
      return;
    }
    setError(null);
    await unsubscribe();
    setSub({ status: "subscribing" });
    const mine = { id: null as string | null };
    try {
      const id = await pubsubSubscribe(sessionId, request, (event) => {
        if (mine.id !== null && subId.current !== mine.id) return;
        if (event.type === "subscribed") {
          setSub((s) =>
            s.status === "subscribed"
              ? s
              : {
                  status: "subscribed",
                  id: mine.id ?? "",
                  channels: event.channels.map(displayBinary),
                  patterns: event.patterns.map(displayBinary),
                },
          );
        } else if (event.type === "closed") {
          subId.current = null;
          setSub({ status: "closed", ...(event.error ? { error: event.error } : {}) });
        } else if (pausedRef.current && event.type === "message") {
          held.current.push(event);
          // The held messages are bounded like the list.
          if (held.current.length > MAX_MESSAGES) held.current.shift();
        } else {
          setBuffer((b) => bufferEvent(b, event));
        }
      });
      mine.id = id;
      subId.current = id;
      setSub((s) =>
        s.status === "subscribed"
          ? { ...s, id }
          : {
              status: "subscribed",
              id,
              channels: request.channels.map(displayBinary),
              patterns: request.patterns.map(displayBinary),
            },
      );
    } catch (err) {
      setSub({ status: "idle" });
      setError(toCommandError(err).message);
    }
  }

  async function stopSubscription() {
    await unsubscribe();
    setSub({ status: "closed" });
  }

  function resume() {
    setPaused(false);
    const events = held.current;
    held.current = [];
    setBuffer((b) => events.reduce((acc, e) => bufferEvent(acc, e), b));
  }

  async function publish() {
    if (publishChannel.trim() === "") {
      setError("Enter the channel to publish on.");
      return;
    }
    setError(null);
    try {
      const receivers = await pubsubPublish(sessionId, utf8(publishChannel.trim()), utf8(payload));
      setPublished(`Published to ${receivers} ${receivers === 1 ? "subscriber" : "subscribers"}.`);
    } catch (err) {
      setPublished(null);
      setError(toCommandError(err).message);
    }
  }

  const subscribed = sub.status === "subscribed";
  const lost = buffer.droppedByCore + buffer.trimmed;
  return (
    <section className="pubsub" aria-label="Publish and subscribe" data-testid="pubsub-panel">
      <form
        className="toolbar table-data__toolbar pubsub__bar"
        aria-label="Subscription"
        onSubmit={(e) => {
          e.preventDefault();
          void subscribe();
        }}
      >
        <label className="pubsub__field">
          <span>Channels</span>
          <input
            type="text"
            spellCheck={false}
            placeholder="news, alerts"
            data-testid="pubsub-channels"
            value={channels}
            onChange={(e) => setChannels(e.target.value)}
          />
        </label>
        <label className="pubsub__field">
          <span>Patterns</span>
          <input
            type="text"
            spellCheck={false}
            placeholder="news:*"
            value={patterns}
            onChange={(e) => setPatterns(e.target.value)}
          />
        </label>
        <button
          type="submit"
          className="button button--primary"
          data-testid="pubsub-subscribe"
          disabled={sub.status === "subscribing"}
        >
          <Radio {...ICON_SM} />
          {subscribed ? "Resubscribe" : "Subscribe"}
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={!subscribed}
          onClick={() => void stopSubscription()}
        >
          <Unplug {...ICON_SM} />
          Unsubscribe
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--ghost"
          aria-pressed={paused}
          disabled={!subscribed}
          onClick={() => (paused ? resume() : setPaused(true))}
          title={paused ? "Show the messages that arrived meanwhile" : "Hold new messages"}
        >
          {paused ? <Play {...ICON_SM} /> : <Pause {...ICON_SM} />}
          {paused ? `Resume${held.current.length ? ` (${held.current.length})` : ""}` : "Pause"}
        </button>
        <button
          type="button"
          className="icon-button"
          aria-label="Clear messages"
          title="Clear the list"
          disabled={buffer.messages.length === 0}
          onClick={() => setBuffer(EMPTY_BUFFER)}
        >
          <Trash2 {...ICON} />
        </button>
        <span
          className="query-editor__status table-data__status"
          role="status"
          data-testid="pubsub-status"
        >
          {sub.status === "subscribing"
            ? "Subscribing…"
            : subscribed
              ? `Listening to ${[...sub.channels, ...sub.patterns].join(", ")}`
              : sub.status === "closed"
                ? sub.error
                  ? `Closed: ${sub.error}`
                  : "Not subscribed"
                : "Not subscribed"}
        </span>
      </form>
      {error ? (
        <div className="banner banner--error" role="alert">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <p>{error}</p>
        </div>
      ) : null}
      {lost > 0 ? (
        <div className="banner banner--warning" role="status" data-testid="pubsub-dropped">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <p>
            {lost.toLocaleString("en-US")} {lost === 1 ? "message was" : "messages were"} not kept
            {buffer.droppedByCore > 0
              ? ` (${buffer.droppedByCore.toLocaleString("en-US")} arrived faster than they could be shown)`
              : ""}
            ; the list keeps the newest {MAX_MESSAGES.toLocaleString("en-US")}.
          </p>
        </div>
      ) : null}
      <div
        className="pubsub__messages"
        role="log"
        aria-label="Messages"
        data-testid="pubsub-messages"
      >
        {buffer.messages.length === 0 ? (
          <div className="empty-state">
            <span className="empty-state__icon" aria-hidden="true">
              <Radio {...ICON_LG} />
            </span>
            <p className="empty-state__title">
              {subscribed ? "Waiting for messages" : "No messages"}
            </p>
            <p className="empty-state__hint">
              Subscribe to channels or patterns; messages published there appear here.
            </p>
          </div>
        ) : (
          <table className="pubsub__table">
            <thead>
              <tr>
                <th scope="col">Time</th>
                <th scope="col">Channel</th>
                <th scope="col">Message</th>
              </tr>
            </thead>
            <tbody>
              {buffer.messages.map((m) => (
                <tr key={m.id} data-testid="pubsub-message">
                  <td className="pubsub__time">{time(m.receivedAtMs)}</td>
                  <td className="pubsub__channel">
                    {m.channel}
                    {m.pattern ? <span className="pubsub__pattern"> via {m.pattern}</span> : null}
                  </td>
                  <td className={`pubsub__payload${m.binary ? " pubsub__payload--binary" : ""}`}>
                    {m.payload}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <form
        className="pubsub__publish"
        aria-label="Publish"
        onSubmit={(e) => {
          e.preventDefault();
          void publish();
        }}
      >
        <label className="pubsub__field">
          <span>Channel</span>
          <input
            type="text"
            spellCheck={false}
            data-testid="publish-channel"
            value={publishChannel}
            onChange={(e) => setPublishChannel(e.target.value)}
          />
        </label>
        <label className="pubsub__field pubsub__field--wide">
          <span>Message</span>
          <input
            type="text"
            spellCheck={false}
            data-testid="publish-payload"
            value={payload}
            onChange={(e) => setPayload(e.target.value)}
          />
        </label>
        <button type="submit" className="button" data-testid="publish-button">
          <Send {...ICON_SM} />
          Publish
        </button>
        {published ? (
          <span className="pubsub__published" role="status">
            {published}
          </span>
        ) : null}
      </form>
    </section>
  );
}
