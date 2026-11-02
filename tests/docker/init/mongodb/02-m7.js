// M7 fixture data (MongoDB, docs/m7-contracts.md §9.2; names in tests/fixtures.toml [m7.mongodb]).
// Runs once after 01-seed.js via the image's /docker-entrypoint-initdb.d hook, on an empty data
// volume. fixture_meta {_id: "m7"} is written last; the compose healthcheck waits for it, so a
// data volume from before M7 reports unhealthy (run scripts/fixtures-down.sh to rebuild it).
//
// mongosh numbers are doubles: every int32 / int64 value is written with NumberInt / NumberLong.

const t = db.getSiblingDB("oxbase_test");

const pad2 = (n) => String(n).padStart(2, "0");
const day = 24 * 60 * 60 * 1000;
const epoch = Date.UTC(2026, 0, 1);
const prices = ["12.50", "0.10", "3.00", "999.99", "0.01", "42.00", "7.25", "100.00", "0.00", "19.99", "5.50", "1234.56"];
const cities = ["Berlin", "Lagos", "Lima", "Oslo", "Pune", "Quito", "Riga", "Seoul", "Tunis", "Turin", "Vaduz", "Zagreb"];
// `mixed`: a string, an int, a double, null, an object, an array, then missing (docs 7-12).
const mixed = ["text", NumberInt(2), 3.5, null, { k: "v" }, [1, "two"]];

// m7_docs: 12 documents covering every BSON type the grid maps (oxbase_dialect::document).
const docs = [];
for (let i = 1; i <= 12; i++) {
  const nn = pad2(i);
  const d = {
    _id: ObjectId("65a0000000000000000000" + nn),
    name: "doc-" + nn,
    // int32 in docs 1-6, int64 in 7-12: one Int64 column.
    n: i <= 6 ? NumberInt(i) : NumberLong(i),
    price: NumberDecimal(prices[i - 1]),
    at: new Date(epoch + i * day),
    bin: BinData(0, Buffer.from([0x00, 0xff, i]).toString("base64")),
    uuid: UUID("65a00000-0000-4000-8000-0000000000" + nn),
    tags: i % 3 === 0 ? ["t" + nn] : ["t" + nn, "common"],
    meta: { city: cities[i - 1], zip: "Z" + nn, geo: { lat: 10 + i / 4, lon: 20 - i / 8 } },
    flag: i % 2 === 1,
  };
  if (i <= 6) {
    d.mixed = mixed[i - 1];
  }
  // A field only doc 12 has (outside a 10-document sample).
  if (i === 12) {
    d.only_late = "late";
  }
  docs.push(d);
}
t.m7_docs.insertMany(docs);

// m7_big: 200 000 documents {_id: int32 n, g: int32 (n % 100), s: "row-<n>"}, in batches.
const bigCount = 200000;
const batch = 10000;
for (let start = 1; start <= bigCount; start += batch) {
  const rows = [];
  for (let n = start; n < start + batch && n <= bigCount; n++) {
    rows.push({ _id: NumberInt(n), g: NumberInt(n % 100), s: "row-" + n });
  }
  t.m7_big.insertMany(rows, { ordered: false });
}
if (t.m7_big.countDocuments({}) !== bigCount) {
  throw new Error("m7_big: expected " + bigCount + " documents");
}

// m7_edit: 3 seed documents; the edit tests insert their own (_id "t<token>-<n>").
t.m7_edit.insertMany([1, 2, 3].map((n) => ({ _id: NumberInt(n), name: "seed-" + n, v: NumberInt(n) })));

// m7_view: flagged documents, name and n only.
t.createView("m7_view", "m7_docs", [{ $match: { flag: true } }, { $project: { name: 1, n: 1 } }]);

// Indexes: plain, multikey, TTL, unique partial, and one on m7_big. The TTL is the largest
// expireAfterSeconds the server accepts (int32 max, about 68 years), so nothing expires.
t.m7_docs.createIndex({ name: 1 }, { name: "m7_docs_name_idx" });
t.m7_docs.createIndex({ tags: 1 }, { name: "m7_docs_tags_idx" });
t.m7_docs.createIndex({ at: 1 }, { name: "m7_docs_at_ttl", expireAfterSeconds: 2147483647 });
t.m7_docs.createIndex(
  { "meta.city": 1 },
  { name: "m7_docs_meta_city_uq", unique: true, partialFilterExpression: { "meta.city": { $exists: true } } },
);
t.m7_big.createIndex({ g: 1 }, { name: "m7_big_g_idx" });

// Role m7_role (in oxbase_test: find on m7_docs) and user m7_reader (in admin, read on
// oxbase_test).
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

t.fixture_meta.insertOne({ _id: "m7", schema_version: NumberInt(1), seeded_at: new Date() });
