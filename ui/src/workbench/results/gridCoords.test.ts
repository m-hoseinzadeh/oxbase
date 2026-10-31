import { describe, expect, it } from "vitest";
import { dataCellOf, ROW_MARKER_COLUMNS } from "./gridCoords";

describe("dataCellOf", () => {
  it("maps the overlay editor's grid column to the data column", () => {
    // Grid column 2 is the second data column ("code" after "id"), not the third ("name").
    expect(dataCellOf([2, 3])).toEqual([1, 3]);
    expect(dataCellOf([ROW_MARKER_COLUMNS, 0])).toEqual([0, 0]);
  });

  it("has no data cell for the row-marker column", () => {
    expect(dataCellOf([0, 5])).toBeNull();
  });
});
