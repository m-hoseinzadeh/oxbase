// oxbase_test fixture data (MongoDB). Mirrors the SQL engines' schema as closely as MongoDB allows:
//   customers (validator, unique email)  ~ table customers
//   orders (customer_id ref + index)     ~ table orders + fk_orders_customer + idx_orders_customer_id
//   v_customer_order_totals (view)       ~ view v_customer_order_totals
//   counters {_id: "order_no_seq"}       ~ sequence order_no_seq (next value 1005)
// MongoDB has no triggers. fixture_meta {_id: "seed"} is written last; the compose
// healthcheck waits for it.
// Runs once via the image's /docker-entrypoint-initdb.d hook, on an empty data volume.

const t = db.getSiblingDB("oxbase_test");

t.createCollection("customers", {
  validator: {
    $jsonSchema: {
      bsonType: "object",
      required: ["_id", "email", "name", "created_at"],
      properties: {
        _id: { bsonType: "int" },
        email: { bsonType: "string" },
        name: { bsonType: "string", maxLength: 100 },
        created_at: { bsonType: "date" },
        updated_at: { bsonType: ["date", "null"] },
      },
    },
  },
});
t.customers.createIndex({ email: 1 }, { unique: true, name: "uq_customers_email" });

t.createCollection("orders", {
  validator: {
    $jsonSchema: {
      bsonType: "object",
      required: ["_id", "order_no", "customer_id", "status", "total", "ordered_at"],
      properties: {
        _id: { bsonType: "int" },
        order_no: { bsonType: "int" },
        customer_id: { bsonType: "int" },
        status: { bsonType: "string", maxLength: 20 },
        total: { bsonType: "decimal", minimum: NumberDecimal("0") },
        ordered_at: { bsonType: "date" },
      },
    },
  },
});
t.orders.createIndex({ order_no: 1 }, { unique: true, name: "uq_orders_order_no" });
t.orders.createIndex({ customer_id: 1 }, { name: "idx_orders_customer_id" });

t.customers.insertMany([
  { _id: NumberInt(1), email: "alice@example.com", name: "Alice Anders", created_at: ISODate("2026-01-15T10:00:00Z"), updated_at: null },
  { _id: NumberInt(2), email: "bob@example.com", name: "Bob Brown", created_at: ISODate("2026-01-16T11:30:00Z"), updated_at: null },
  { _id: NumberInt(3), email: "carol@example.com", name: "Carol Chen", created_at: ISODate("2026-01-17T09:15:00Z"), updated_at: null },
]);

t.orders.insertMany([
  { _id: NumberInt(1), order_no: NumberInt(1000), customer_id: NumberInt(1), status: "paid", total: NumberDecimal("120.50"), ordered_at: ISODate("2026-02-01T08:00:00Z") },
  { _id: NumberInt(2), order_no: NumberInt(1001), customer_id: NumberInt(1), status: "shipped", total: NumberDecimal("35.00"), ordered_at: ISODate("2026-02-03T14:20:00Z") },
  { _id: NumberInt(3), order_no: NumberInt(1002), customer_id: NumberInt(2), status: "new", total: NumberDecimal("999.99"), ordered_at: ISODate("2026-02-05T16:45:00Z") },
  { _id: NumberInt(4), order_no: NumberInt(1003), customer_id: NumberInt(2), status: "cancelled", total: NumberDecimal("0.00"), ordered_at: ISODate("2026-02-06T10:10:00Z") },
  { _id: NumberInt(5), order_no: NumberInt(1004), customer_id: NumberInt(3), status: "paid", total: NumberDecimal("15.25"), ordered_at: ISODate("2026-02-07T12:00:00Z") },
]);

// Next order_no to hand out, matching the SQL sequences after the seed.
t.counters.insertOne({ _id: "order_no_seq", next: NumberInt(1005) });

t.createView("v_customer_order_totals", "customers", [
  { $lookup: { from: "orders", localField: "_id", foreignField: "customer_id", as: "o" } },
  // $replaceWith (not $project with `name: 1`) so the output field order matches the SQL views:
  // customer_id, name, order_count, total_spent.
  {
    $replaceWith: {
      customer_id: "$_id",
      name: "$name",
      order_count: { $size: "$o" },
      total_spent: { $ifNull: [{ $sum: "$o.total" }, NumberDecimal("0")] },
    },
  },
  { $sort: { customer_id: 1 } },
]);

t.fixture_meta.insertOne({ _id: "seed", schema_version: NumberInt(1), seeded_at: new Date() });
