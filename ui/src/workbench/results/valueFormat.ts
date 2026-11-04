/**
 * Formats of a cell value in the value panel: detection and beautifiers.
 *
 * The beautifiers only re-indent: JSON is re-laid out token by token (never through
 * `JSON.parse`, so a number like 12345678901234567890 keeps every digit and key order stays),
 * XML tag by tag with text, comments and CDATA unchanged. Input they cannot read is left alone
 * (`null`), so the panel shows the value as it is.
 */

export type ValueFormat = "text" | "json" | "xml" | "sql";

/** The format a value most likely is: JSON and XML by their shape, everything else text. */
export function detectFormat(text: string): ValueFormat {
  const t = text.trim();
  if ((t.startsWith("{") && t.endsWith("}")) || (t.startsWith("[") && t.endsWith("]"))) {
    if (prettyJson(t) !== null) return "json";
  }
  if (t.startsWith("<") && t.endsWith(">") && prettyXml(t) !== null) return "xml";
  return "text";
}

/** `text` re-indented as JSON (2 spaces), or `null` when it is not valid JSON. */
export function prettyJson(text: string, indent = "  "): string | null {
  try {
    JSON.parse(text);
  } catch {
    return null;
  }
  let out = "";
  let depth = 0;
  let i = 0;
  const n = text.length;
  const newline = () => `\n${indent.repeat(depth)}`;
  while (i < n) {
    const c = text[i] as string;
    if (c === '"') {
      // A string, escapes included, copied as it is.
      let j = i + 1;
      while (j < n && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
      out += text.slice(i, j + 1);
      i = j + 1;
      continue;
    }
    if (c === " " || c === "\t" || c === "\n" || c === "\r") {
      i += 1;
      continue;
    }
    if (c === "{" || c === "[") {
      // An empty object or array stays on one line.
      let j = i + 1;
      while (j < n && /\s/.test(text[j] as string)) j += 1;
      const close = c === "{" ? "}" : "]";
      if (text[j] === close) {
        out += c + close;
        i = j + 1;
        continue;
      }
      depth += 1;
      out += c + newline();
      i += 1;
      continue;
    }
    if (c === "}" || c === "]") {
      depth -= 1;
      out += newline() + c;
      i += 1;
      continue;
    }
    if (c === ",") {
      out += `,${newline()}`;
      i += 1;
      continue;
    }
    if (c === ":") {
      out += ": ";
      i += 1;
      continue;
    }
    // Numbers, true, false, null: copied as they are.
    let j = i;
    while (j < n && !/[\s,:{}[\]"]/.test(text[j] as string)) j += 1;
    out += text.slice(i, j);
    i = j;
  }
  return out;
}

/** `text` re-indented as XML (2 spaces per level), or `null` when its tags do not balance. */
export function prettyXml(text: string, indent = "  "): string | null {
  const tokens: string[] = [];
  let i = 0;
  const n = text.length;
  while (i < n) {
    if (text.startsWith("<!--", i)) {
      const end = text.indexOf("-->", i + 4);
      if (end < 0) return null;
      tokens.push(text.slice(i, end + 3));
      i = end + 3;
    } else if (text.startsWith("<![CDATA[", i)) {
      const end = text.indexOf("]]>", i + 9);
      if (end < 0) return null;
      tokens.push(text.slice(i, end + 3));
      i = end + 3;
    } else if (text[i] === "<") {
      // A tag; `>` inside quoted attribute values does not end it.
      let j = i + 1;
      let quote: string | null = null;
      while (j < n) {
        const c = text[j];
        if (quote) {
          if (c === quote) quote = null;
        } else if (c === '"' || c === "'") quote = c;
        else if (c === ">") break;
        j += 1;
      }
      if (j >= n) return null;
      tokens.push(text.slice(i, j + 1));
      i = j + 1;
    } else {
      const end = text.indexOf("<", i);
      const chunk = text.slice(i, end < 0 ? n : end);
      tokens.push(chunk);
      i = end < 0 ? n : end;
    }
  }

  const lines: string[] = [];
  const open: string[] = [];
  let depth = 0;
  const nameOf = (tag: string) => /^<\/?\s*([^\s/>]+)/.exec(tag)?.[1] ?? "";
  for (let k = 0; k < tokens.length; k += 1) {
    const token = tokens[k] as string;
    if (!token.startsWith("<")) {
      const t = token.trim();
      if (t) lines.push(indent.repeat(depth) + t);
      continue;
    }
    const special = token.startsWith("<?") || token.startsWith("<!");
    if (special) {
      lines.push(indent.repeat(depth) + token);
      continue;
    }
    if (token.startsWith("</")) {
      const name = nameOf(token);
      if (open.pop() !== name) return null;
      depth -= 1;
      lines.push(indent.repeat(depth) + token);
      continue;
    }
    if (token.endsWith("/>")) {
      lines.push(indent.repeat(depth) + token);
      continue;
    }
    // <a>text</a> stays on one line.
    const next = tokens[k + 1];
    const after = tokens[k + 2];
    const name = nameOf(token);
    if (
      next !== undefined &&
      !next.startsWith("<") &&
      !next.includes("\n") &&
      after?.startsWith("</") &&
      nameOf(after) === name
    ) {
      lines.push(indent.repeat(depth) + token + next.trim() + after);
      k += 2;
      continue;
    }
    lines.push(indent.repeat(depth) + token);
    open.push(name);
    depth += 1;
  }
  if (open.length > 0) return null;
  return lines.join("\n");
}
