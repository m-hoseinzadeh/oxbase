/**
 * Display text for result cells, read straight from the Arrow columns of a page.
 *
 * Temporal and decimal values are formatted from their raw integers (not through JS numbers),
 * so microseconds and 38-digit decimals are shown exactly. NULL is reported separately so the
 * grid can style it apart from the text "NULL".
 */
import { type Data, type DataType, DateUnit, TimeUnit, Type, type Vector } from "apache-arrow";
import type { ColumnInfo } from "../../api";

/** Arrow's canonical extension name key (field metadata). */
const EXTENSION_NAME = "ARROW:extension:name";
/** Longest text drawn in a grid cell; the full value is in the cell overlay. */
const MAX_CELL_TEXT = 2000;
/** Bytes of a binary value shown as hex. */
const MAX_HEX_BYTES = 64;

export interface FormattedCell {
  /** Full text of the value ("" for NULL). */
  text: string;
  isNull: boolean;
  /** Right-aligned in the grid. */
  numeric: boolean;
}

const NULL_CELL: FormattedCell = { text: "", isNull: true, numeric: false };

/** The chunk of `vector` that holds row `index`, and the row's index inside it. */
function locate(vector: Vector, index: number): { data: Data; i: number } | null {
  let i = index;
  for (const data of vector.data) {
    if (i < data.length) return { data, i };
    i -= data.length;
  }
  return null;
}

function pad(n: number | bigint | string, width: number): string {
  return n.toString().padStart(width, "0");
}

const UNIT_DIGITS: Record<number, number> = {
  [TimeUnit.SECOND]: 0,
  [TimeUnit.MILLISECOND]: 3,
  [TimeUnit.MICROSECOND]: 6,
  [TimeUnit.NANOSECOND]: 9,
};

/** Splits a count of `unit`s into whole seconds and the fraction digits (trailing zeros cut). */
function splitSeconds(raw: bigint, unit: number): { seconds: bigint; fraction: string } {
  const digits = UNIT_DIGITS[unit] ?? 0;
  const scale = 10n ** BigInt(digits);
  let seconds = raw / scale;
  let rest = raw % scale;
  if (rest < 0n) {
    seconds -= 1n;
    rest += scale;
  }
  const fraction = digits === 0 ? "" : pad(rest, digits).replace(/0+$/, "");
  return { seconds, fraction };
}

const DAY_SECONDS = 86_400n;

/** Floor division (rounds toward minus infinity, unlike `Math.trunc`). */
function floorDiv(a: number, b: number): number {
  return Math.floor(a / b);
}

/**
 * The proleptic Gregorian date `days` after 1970-01-01, by integer arithmetic (JS `Date` stops
 * at ±275,760 years and has no BC formatting). Year 0 is 1 BC, as in Arrow.
 */
export function civilFromDays(days: number): { year: number; month: number; day: number } {
  const z = days + 719_468;
  const era = floorDiv(z, 146_097);
  const doe = z - era * 146_097; // [0, 146096]
  const yoe = floorDiv(
    doe - floorDiv(doe, 1460) + floorDiv(doe, 36_524) - floorDiv(doe, 146_096),
    365,
  );
  const doy = doe - (365 * yoe + floorDiv(yoe, 4) - floorDiv(yoe, 100)); // [0, 365]
  const mp = floorDiv(5 * doy + 2, 153); // [0, 11], March first
  const day = doy - floorDiv(153 * mp + 2, 5) + 1;
  const month = mp < 10 ? mp + 3 : mp - 9;
  const year = yoe + era * 400 + (month <= 2 ? 1 : 0);
  return { year, month, day };
}

/** `YYYY-MM-DD` (more year digits when needed) and whether the date is BC. */
function dateParts(days: number): { text: string; bc: boolean } {
  const { year, month, day } = civilFromDays(days);
  const bc = year <= 0;
  const shown = bc ? 1 - year : year;
  return { text: `${pad(shown, 4)}-${pad(month, 2)}-${pad(day, 2)}`, bc };
}

/** A date as the databases print it: `2024-10-31`, `0044-03-15 BC`. */
export function dateFromDays(days: number): string {
  if (!Number.isFinite(days)) return String(days);
  const { text, bc } = dateParts(days);
  return bc ? `${text} BC` : text;
}

function timeOfDay(seconds: number, fraction: string): string {
  const hh = Math.floor(seconds / 3600);
  const mm = Math.floor((seconds % 3600) / 60);
  return `${pad(hh, 2)}:${pad(mm, 2)}:${pad(seconds % 60, 2)}${fraction ? `.${fraction}` : ""}`;
}

export function timestampText(
  raw: bigint,
  unit: number,
  timezone: string | null | undefined,
): string {
  const { seconds, fraction } = splitSeconds(raw, unit);
  let days = seconds / DAY_SECONDS;
  let rest = seconds % DAY_SECONDS;
  if (rest < 0n) {
    days -= 1n;
    rest += DAY_SECONDS;
  }
  const date = dateParts(Number(days));
  const text = `${date.text} ${timeOfDay(Number(rest), fraction)}${timezone ? "Z" : ""}`;
  return date.bc ? `${text} BC` : text;
}

function timeText(raw: bigint, unit: number): string {
  const { seconds, fraction } = splitSeconds(raw, unit);
  return timeOfDay(Number(seconds), fraction);
}

/** A 128-bit (or 256-bit) little-endian two's complement integer from 32-bit words. */
function wordsToBigInt(words: Uint32Array): bigint {
  let value = 0n;
  for (let w = words.length - 1; w >= 0; w -= 1) value = (value << 32n) | BigInt(words[w] ?? 0);
  const bits = BigInt(words.length * 32);
  if ((words[words.length - 1] ?? 0) & 0x8000_0000) value -= 1n << bits;
  return value;
}

export function decimalText(unscaled: bigint, scale: number): string {
  if (scale <= 0) return (unscaled * 10n ** BigInt(-scale)).toString();
  const negative = unscaled < 0n;
  const digits = (negative ? -unscaled : unscaled).toString().padStart(scale + 1, "0");
  const text = `${digits.slice(0, -scale)}.${digits.slice(-scale)}`;
  return negative ? `-${text}` : text;
}

function hexText(bytes: Uint8Array): string {
  let out = "0x";
  const shown = Math.min(bytes.length, MAX_HEX_BYTES);
  for (let i = 0; i < shown; i += 1) out += pad((bytes[i] ?? 0).toString(16), 2);
  return bytes.length > shown ? `${out}… (${bytes.length} bytes)` : out;
}

function uuidText(bytes: Uint8Array): string {
  const hex = Array.from(bytes, (b) => pad(b.toString(16), 2)).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function jsonReplacer(_key: string, value: unknown): unknown {
  if (typeof value === "bigint") return value.toString();
  if (value instanceof Uint8Array) return hexText(value);
  return value;
}

/** Text of a nested or unknown value (lists, structs, maps). */
function genericText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  if (typeof value === "string") return value;
  if (typeof value === "bigint" || typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (value instanceof Uint8Array) return hexText(value);
  const withJson = value as { toJSON?: () => unknown; toArray?: () => unknown };
  try {
    if (typeof withJson.toJSON === "function") {
      return JSON.stringify(withJson.toJSON(), jsonReplacer);
    }
    if (typeof withJson.toArray === "function") {
      return JSON.stringify(Array.from(withJson.toArray() as ArrayLike<unknown>), jsonReplacer);
    }
    return JSON.stringify(value, jsonReplacer);
  } catch {
    return String(value);
  }
}

function isNumericType(type: DataType): boolean {
  return type.typeId === Type.Int || type.typeId === Type.Float || type.typeId === Type.Decimal;
}

/** The display text of row `index` of `vector`. `column` gives extension metadata (UUID). */
export function formatCell(vector: Vector, index: number, column?: ColumnInfo): FormattedCell {
  if (!vector.isValid(index)) return NULL_CELL;
  const type = vector.type as DataType;
  const numeric = isNumericType(type);
  const loc = locate(vector, index);
  const values = loc?.data.values as unknown;
  const i = loc?.i ?? 0;
  let text: string;
  switch (type.typeId) {
    case Type.Timestamp: {
      const t = type as DataType & { unit: number; timezone?: string | null };
      const raw = values instanceof BigInt64Array ? values[i] : undefined;
      text =
        raw === undefined
          ? genericText(vector.get(index))
          : timestampText(raw, t.unit, t.timezone ?? null);
      break;
    }
    case Type.Date: {
      const t = type as DataType & { unit: number };
      if (t.unit === DateUnit.DAY && values instanceof Int32Array) {
        text = dateFromDays(values[i] ?? 0);
      } else {
        // Date64: milliseconds (Arrow JS may hand back a number or a Date).
        const value = vector.get(index) as unknown;
        const ms = value instanceof Date ? value.getTime() : Number(value);
        text = Number.isFinite(ms) ? dateFromDays(Math.floor(ms / 86_400_000)) : genericText(value);
      }
      break;
    }
    case Type.Time: {
      const t = type as DataType & { unit: number };
      const raw =
        values instanceof BigInt64Array
          ? values[i]
          : values instanceof Int32Array
            ? BigInt(values[i] ?? 0)
            : undefined;
      text = raw === undefined ? genericText(vector.get(index)) : timeText(raw, t.unit);
      break;
    }
    case Type.Decimal: {
      const t = type as DataType & { scale: number };
      const stride = loc?.data.stride || 4;
      text =
        values instanceof Uint32Array
          ? decimalText(wordsToBigInt(values.subarray(i * stride, (i + 1) * stride)), t.scale)
          : genericText(vector.get(index));
      break;
    }
    case Type.FixedSizeBinary: {
      const bytes = vector.get(index) as Uint8Array;
      text =
        column?.metadata?.[EXTENSION_NAME] === "arrow.uuid" && bytes.length === 16
          ? uuidText(bytes)
          : hexText(bytes);
      break;
    }
    case Type.Binary:
    case Type.LargeBinary:
      text = hexText(vector.get(index) as Uint8Array);
      break;
    case Type.Bool:
      text = vector.get(index) ? "true" : "false";
      break;
    default:
      text = genericText(vector.get(index));
  }
  return { text, isNull: false, numeric };
}

/** One line of at most `MAX_CELL_TEXT` characters, for drawing in a cell. */
export function cellDisplayText(text: string): string {
  const short = text.length > MAX_CELL_TEXT ? `${text.slice(0, MAX_CELL_TEXT)}…` : text;
  return short.replace(/\r?\n/g, "↵ ").replace(/\t/g, " ");
}

/** The column's native type when the driver reported one (`oxbase.<engine>.type`), else Arrow's. */
export function columnTypeLabel(column: ColumnInfo): string {
  for (const [key, value] of Object.entries(column.metadata ?? {})) {
    if (/^oxbase\.[a-z0-9_]+\.type$/.test(key)) return value;
  }
  return column.dataType;
}

/** A starting width (px) for a column from its type. */
export function initialColumnWidth(column: ColumnInfo): number {
  const t = column.dataType;
  if (/^(Int|UInt)(8|16)$|^Boolean$/.test(t)) return 90;
  if (/^(Int|UInt|Float)\d+$|^Decimal/.test(t)) return 120;
  if (/^(Timestamp|Date64)/.test(t)) return 190;
  if (/^(Date|Time)/.test(t)) return 120;
  if (column.metadata?.[EXTENSION_NAME] === "arrow.uuid") return 290;
  return Math.min(320, Math.max(120, column.name.length * 9 + 40));
}
