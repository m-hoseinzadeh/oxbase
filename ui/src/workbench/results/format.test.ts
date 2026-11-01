import {
  Bool,
  DateDay,
  Decimal,
  FixedSizeBinary,
  Int64,
  makeData,
  makeVector,
  TimeMicrosecond,
  TimestampMicrosecond,
  TimeUnit,
  tableFromArrays,
  Utf8,
  vectorFromArray,
} from "apache-arrow";
import { describe, expect, it } from "vitest";
import {
  civilFromDays,
  columnTypeLabel,
  dateFromDays,
  decimalText,
  formatCell,
  timestampText,
} from "./format";

describe("formatCell", () => {
  it("reports NULL apart from text", () => {
    const v = vectorFromArray(["NULL", null], new Utf8());
    expect(formatCell(v, 0)).toEqual({ text: "NULL", isNull: false, numeric: false });
    expect(formatCell(v, 1).isNull).toBe(true);
  });

  it("formats integers, booleans and 64-bit values exactly", () => {
    expect(formatCell(vectorFromArray([2n ** 62n], new Int64()), 0)).toEqual({
      text: "4611686018427387904",
      isNull: false,
      numeric: true,
    });
    expect(formatCell(vectorFromArray([true], new Bool()), 0).text).toBe("true");
  });

  it("formats timestamps and times with microseconds", () => {
    const micros = 1_730_368_800_123_456n; // 2024-10-31 10:00:00.123456 UTC
    const ts = makeVector(
      makeData({
        type: new TimestampMicrosecond(),
        length: 1,
        data: BigInt64Array.from([micros]),
      }),
    );
    expect(formatCell(ts, 0).text).toBe("2024-10-31 10:00:00.123456");
    const tz = makeVector(
      makeData({
        type: new TimestampMicrosecond("UTC"),
        length: 1,
        data: BigInt64Array.from([-1n]),
      }),
    );
    expect(formatCell(tz, 0).text).toBe("1969-12-31 23:59:59.999999Z");
    const time = makeVector(
      makeData({
        type: new TimeMicrosecond(),
        length: 1,
        data: BigInt64Array.from([(13n * 3600n + 5n * 60n + 9n) * 1_000_000n + 500n]),
      }),
    );
    expect(formatCell(time, 0).text).toBe("13:05:09.0005");
    const date = makeVector(
      makeData({ type: new DateDay(), length: 1, data: Int32Array.from([20_027]) }),
    );
    expect(formatCell(date, 0).text).toBe("2024-10-31");
  });

  it("formats dates and timestamps past JS Date's range and BC", () => {
    expect(civilFromDays(0)).toEqual({ year: 1970, month: 1, day: 1 });
    expect(civilFromDays(-719_162)).toEqual({ year: 1, month: 1, day: 1 });
    expect(dateFromDays(20_027)).toBe("2024-10-31");
    expect(dateFromDays(-719_528)).toBe("0001-01-01 BC"); // year 0
    expect(dateFromDays(-735_160)).toBe("0044-03-15 BC"); // year -43
    expect(dateFromDays(2_145_042_905)).toBe("5874897-12-31");
    // 290000-01-01 00:00:00.5 (microseconds), beyond Date's ±8.64e15 ms.
    const micros = 105_200_797n * 86_400n * 1_000_000n + 500_000n;
    expect(timestampText(micros, TimeUnit.MICROSECOND, null)).toBe("290000-01-01 00:00:00.5");
    // One second before 0001-01-01 is 1 BC.
    const beforeEra = (-719_162n * 86_400n - 1n) * 1_000_000n;
    expect(timestampText(beforeEra, TimeUnit.MICROSECOND, "UTC")).toBe("0001-12-31 23:59:59Z BC");
    const ts = makeVector(
      makeData({
        type: new TimestampMicrosecond(),
        length: 1,
        data: BigInt64Array.from([micros]),
      }),
    );
    expect(() => formatCell(ts, 0)).not.toThrow();
  });

  it("formats decimals with their scale", () => {
    expect(decimalText(12345n, 2)).toBe("123.45");
    expect(decimalText(-5n, 3)).toBe("-0.005");
    expect(decimalText(7n, 0)).toBe("7");
    // -1.50 as a Decimal128(10, 2): two's complement in four 32-bit words.
    const words = Uint32Array.from([0xffff_ff6a, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff]);
    const dec = makeVector(makeData({ type: new Decimal(2, 10, 128), length: 1, data: words }));
    expect(formatCell(dec, 0)).toEqual({ text: "-1.50", isNull: false, numeric: true });
  });

  it("shows UUIDs and binary values", () => {
    const bytes = Uint8Array.from({ length: 16 }, (_, i) => i);
    const uuid = makeVector(makeData({ type: new FixedSizeBinary(16), length: 1, data: bytes }));
    const column = {
      name: "id",
      dataType: "FixedSizeBinary(16)",
      nullable: false,
      metadata: { "ARROW:extension:name": "arrow.uuid" },
    };
    expect(formatCell(uuid, 0, column).text).toBe("00010203-0405-0607-0809-0a0b0c0d0e0f");
    expect(formatCell(uuid, 0).text).toBe("0x000102030405060708090a0b0c0d0e0f");
  });

  it("reads values from sliced tables", () => {
    const table = tableFromArrays({ n: Float64Array.from([1, 2, 3, 4]) }).slice(2, 4);
    const v = table.getChildAt(0);
    expect(v && formatCell(v, 1).text).toBe("4");
  });

  it("labels columns with the driver's native type", () => {
    expect(
      columnTypeLabel({
        name: "c",
        dataType: "Utf8",
        nullable: true,
        metadata: { "oxbase.pg.type": "varchar" },
      }),
    ).toBe("varchar");
    expect(columnTypeLabel({ name: "c", dataType: "Int32", nullable: true })).toBe("Int32");
    // MySQL and SQL Server report the type family under one key.
    expect(
      columnTypeLabel({
        name: "c",
        dataType: "FixedSizeBinary[16]",
        nullable: true,
        metadata: { "oxbase.native_type": "uniqueidentifier" },
      }),
    ).toBe("uniqueidentifier");
  });
});
