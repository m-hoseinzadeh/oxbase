/**
 * Which workbench an engine's objects open in. SQL engines use the M2/M3 tabs (query editor,
 * table data, designer); MongoDB's collections open in the collection tab and its shell is the
 * query editor with the mongo-shell language; Redis keys open in the key browser and its
 * console runs one command per line. The core decides everything else (tree, languages).
 */
import type { Engine } from "../../connections/types";

export type EngineFamily = "sql" | "document" | "keyvalue";

const FAMILY: Partial<Record<Engine, EngineFamily>> = {
  mongodb: "document",
  redis: "keyvalue",
};

export function engineFamily(engine: Engine | undefined): EngineFamily {
  return (engine && FAMILY[engine]) ?? "sql";
}
