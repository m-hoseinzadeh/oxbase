// M7 fixture data (MongoDB), `[m7.mongodb]` in tests/fixtures.toml (docs/m7-contracts.md §9.2).
// Runs once via the image's /docker-entrypoint-initdb.d hook, after 01-seed.js, on an empty data
// volume. Every init script finishes before the final mongod starts, so the compose healthcheck
// (final server + fixture_meta) also waits for this one.
//
//   m7_docs   12 documents covering every BSON type the grid maps
//   m7_big    200 000 documents {_id: int32 n, g: int32 n % 100, s: "row-<n>"}
//   m7_edit   3 seed documents; edit tests insert and change their own (_id "t<token>-<n>")
//   m7_view   view on m7_docs
//   indexes   four on m7_docs (plus _id_), m7_big_g_idx on m7_big
//   m7_reader user (admin, read on oxbase_test), m7_role role (oxbase_test, find on m7_docs)
//
// Numbers are written with NumberInt / NumberLong: plain numbers in mongosh are doubles.

const t = db.getSiblingDB("oxbase_test");
const DAY_MS = 24 * 60 * 60 * 1000;
const EPOCH_2026 = Date.UTC(2026, 0, 1);

function pad2(k) {
  return (k < 10 ? "0" : "") + k;
}

// ---- m7_docs ----------------------------------------------------------------------------------
const MIXED = {
  1: "text",
  2: NumberInt(2),
  3: 3.5,
  4: null,
  5: { k: "v" },
  6: [NumberInt(1), "two"],
};
const docs = [];
for (let k = 1; k <= 12; k++) {
  const nn = pad2(k);
  const d = {
    _id: ObjectId("65a0000000000000000000" + nn),
    name: "doc-" + nn,
    n: k <= 6 ? NumberInt(k) : NumberLong(k),
    price: NumberDecimal(k === 1 ? "12.50" : k === 2 ? "0.10" : k + ".25"),
    at: new Date(EPOCH_2026 + k * DAY_MS),
    bin: BinData(0, Buffer.from([0x00, 0xff, k]).toString("base64")),
    uuid: UUID("6f1c2d3e-4a5b-4c6d-8e7f-0000000000" + nn),
    tags: ["t" + k, "common"],
    meta: { city: "city-" + nn, zip: "100" + nn, geo: { lat: 10.5 + k, lon: 20.25 + k } },
    flag: k % 2 === 1,
  };
  if (k in MIXED) {
    d.mixed = MIXED[k];
  }
  if (k === 12) {
    // Outside a 10-document sample: only in the document column.
    d.only_late = "late";
  }
  docs.push(d);
}
t.m7_docs.insertMany(docs);
t.m7_docs.createIndex({ name: 1 }, { name: "m7_docs_name_idx" });
t.m7_docs.createIndex({ tags: 1 }, { name: "m7_docs_tags_idx" });
// The largest TTL MongoDB accepts (int32 seconds, about 68 years): nothing expires.
t.m7_docs.createIndex({ at: 1 }, { name: "m7_docs_at_ttl", expireAfterSeconds: 2147483647 });
t.m7_docs.createIndex(
  { "meta.city": 1 },
  {
    name: "m7_docs_meta_city_uq",
    unique: true,
    partialFilterExpression: { "meta.city": { $exists: true } },
  },
);

// ---- m7_big -----------------------------------------------------------------------------------
const BIG = 200000;
const BATCH = 10000;
for (let start = 1; start <= BIG; start += BATCH) {
  const batch = [];
  for (let n = start; n < start + BATCH && n <= BIG; n++) {
    batch.push({ _id: NumberInt(n), g: NumberInt(n % 100), s: "row-" + n });
  }
  t.m7_big.insertMany(batch, { ordered: false });
}
t.m7_big.createIndex({ g: 1 }, { name: "m7_big_g_idx" });

// ---- m7_edit ----------------------------------------------------------------------------------
t.m7_edit.insertMany([
  { _id: NumberInt(1), name: "seed-1", v: NumberInt(1) },
  { _id: NumberInt(2), name: "seed-2", v: NumberInt(2) },
  { _id: NumberInt(3), name: "seed-3", v: NumberInt(3) },
]);

// ---- m7_view ----------------------------------------------------------------------------------
t.createView("m7_view", "m7_docs", [{ $match: { flag: true } }, { $project: { name: 1, n: 1 } }]);

// ---- users and roles --------------------------------------------------------------------------
t.createRole({
  role: "m7_role",
  privileges: [{ resource: { db: "oxbase_test", collection: "m7_docs" }, actions: ["find"] }],
  roles: [],
});
db.getSiblingDB("admin").createUser({
  user: "m7_reader",
  pwd: "Oxbase_Fixture_Pw_2026",
  roles: [{ role: "read", db: "oxbase_test" }],
});

t.fixture_meta.insertOne({ _id: "m7", docs: NumberInt(12), big: NumberInt(BIG), seeded_at: new Date() });
