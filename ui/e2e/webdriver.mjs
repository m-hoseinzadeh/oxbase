// A minimal W3C WebDriver client over `fetch` (Node 18+), enough for the webview smoke test
// (ui/e2e/smoke.mjs). No dependencies. It talks to `tauri-driver`, which forwards to the
// platform's native driver (WebKitWebDriver on Linux, msedgedriver on Windows).
//
// Request bodies are never logged: `sendKeys` carries the fixture password.

/** The W3C web element identifier key. */
const ELEMENT_KEY = "element-6066-11e4-a07c-4c0f6cbc39ec";

/** W3C key codes used by the smoke test. */
export const Keys = {
  enter: "",
  escape: "",
  control: "",
  backspace: "",
};

export class WebDriverError extends Error {
  constructor(method, path, status, error, message) {
    super(`${method} ${path}: ${status} ${error}: ${message}`);
    this.name = "WebDriverError";
    this.status = status;
    this.error = error;
  }
}

/** True when `value` is a serialized web element. */
export function isElement(value) {
  return value !== null && typeof value === "object" && ELEMENT_KEY in value;
}

export class WebDriver {
  /**
   * @param {string} baseUrl e.g. `http://127.0.0.1:4444`
   * @param {{ requestTimeoutMs?: number }} [options]
   */
  constructor(baseUrl, options = {}) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.sessionId = null;
    this.requestTimeoutMs = options.requestTimeoutMs ?? 120_000;
  }

  async request(method, path, body) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.requestTimeoutMs);
    let response;
    try {
      response = await fetch(`${this.baseUrl}${path}`, {
        method,
        headers: body === undefined ? {} : { "Content-Type": "application/json; charset=utf-8" },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
      });
    } catch (error) {
      throw new WebDriverError(method, path, 0, "transport", String(error?.message ?? error));
    } finally {
      clearTimeout(timer);
    }
    const text = await response.text();
    let json = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      json = null;
    }
    const value = json && typeof json === "object" && "value" in json ? json.value : json;
    if (!response.ok || (value && typeof value === "object" && "error" in value && value.error)) {
      const error = value?.error ?? "unknown error";
      const message = value?.message ?? text.slice(0, 500);
      throw new WebDriverError(method, path, response.status, error, message);
    }
    return value;
  }

  /** Waits until the driver answers `GET /status`. */
  static async waitReady(baseUrl, timeoutMs = 30_000) {
    const deadline = Date.now() + timeoutMs;
    let last = "no answer";
    while (Date.now() < deadline) {
      try {
        const response = await fetch(`${baseUrl}/status`);
        if (response.ok) return;
        last = `HTTP ${response.status}`;
      } catch (error) {
        last = String(error?.message ?? error);
      }
      await sleep(250);
    }
    throw new Error(`WebDriver at ${baseUrl} not ready after ${timeoutMs} ms (${last})`);
  }

  async newSession(capabilities) {
    const value = await this.request("POST", "/session", {
      capabilities: { alwaysMatch: capabilities, firstMatch: [{}] },
    });
    this.sessionId = value.sessionId;
    return value;
  }

  async deleteSession() {
    if (!this.sessionId) return;
    const id = this.sessionId;
    this.sessionId = null;
    await this.request("DELETE", `/session/${id}`);
  }

  session(path) {
    if (!this.sessionId) throw new Error("no WebDriver session");
    return `/session/${this.sessionId}${path}`;
  }

  setTimeouts(timeouts) {
    return this.request("POST", this.session("/timeouts"), timeouts);
  }

  title() {
    return this.request("GET", this.session("/title"));
  }

  /** Runs `script` (a function body; `arguments` holds `args`) and returns its result. */
  execute(script, ...args) {
    return this.request("POST", this.session("/execute/sync"), { script, args });
  }

  /** Like {@link execute}, but the script calls `arguments[arguments.length - 1](result)`. */
  executeAsync(script, ...args) {
    return this.request("POST", this.session("/execute/async"), { script, args });
  }

  async findAll(css) {
    return this.request("POST", this.session("/elements"), {
      using: "css selector",
      value: css,
    });
  }

  /** The first element matching `css`, or `null`. */
  async find(css) {
    const all = await this.findAll(css);
    return all.length > 0 ? all[0] : null;
  }

  elementId(element) {
    if (!isElement(element)) throw new Error("not a web element");
    return element[ELEMENT_KEY];
  }

  click(element) {
    return this.request("POST", this.session(`/element/${this.elementId(element)}/click`), {});
  }

  sendKeys(element, text) {
    return this.request("POST", this.session(`/element/${this.elementId(element)}/value`), {
      text,
    });
  }

  clear(element) {
    return this.request("POST", this.session(`/element/${this.elementId(element)}/clear`), {});
  }

  text(element) {
    return this.request("GET", this.session(`/element/${this.elementId(element)}/text`));
  }

  /** Low-level input: `actions` is the W3C `actions` array. */
  performActions(actions) {
    return this.request("POST", this.session("/actions"), { actions });
  }

  releaseActions() {
    return this.request("DELETE", this.session("/actions"));
  }

  /** Presses `key` while holding Control (e.g. select all with "a"). */
  async chord(modifier, key) {
    await this.performActions([
      {
        type: "key",
        id: "keyboard",
        actions: [
          { type: "keyDown", value: modifier },
          { type: "keyDown", value: key },
          { type: "keyUp", value: key },
          { type: "keyUp", value: modifier },
        ],
      },
    ]);
    await this.releaseActions();
  }

  /** A PNG screenshot of the window, base64. */
  screenshot() {
    return this.request("GET", this.session("/screenshot"));
  }
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
