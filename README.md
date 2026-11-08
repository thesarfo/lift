# lift

Move a MongoDB dump into a Postgres schema you already designed, and check nothing was lost.

You write a mapping file from collections to tables. lift reads the target schema from Postgres itself, loads in foreign-key order, and tells you about every document that didn't fit.

## Use

```sh
mongodump --db shop --out dump
export LIFT_DATABASE_URL="host=localhost user=postgres dbname=shop"

lift check  mapping.toml --dump dump    # load, report, roll back
lift run    mapping.toml --dump dump    # load for real
lift verify mapping.toml --dump dump    # every document in its table?
```

`run` is safe to repeat: rows are upserted on their primary key.

## Mapping

```toml
schema = "shop"
id_suffix = "6c696674"

[[table]]
collection = "orders"
table = "orders"
[table.columns]
id = "_id"
customer_id = "customer_id"
status = "status"
placed_at = "meta.created_at"
source = { value = "mongo" }
```

Columns map to dotted paths into the document, or to a fixed `value`. See [examples/shop.toml](examples/shop.toml) for a full one.

An embedded array becomes rows in its own table, one per element:

```toml
[[table.children]]
array = "items"
table = "order_items"
parent_key = "order_id"
[table.children.columns]
product_id = "product_id"
quantity = "qty"
```

lift fills `parent_key` with the parent's id. Paths are relative to the element; `$value` is the element itself and `$parent.x` reads the parent. On a re-run, each parent's children are replaced rather than added again.

## Notes

- ObjectIds become UUIDs made of the 12 ObjectId bytes plus `id_suffix`, so the original id can always be recovered. A zero ObjectId becomes the nil UUID. In a `text` column an ObjectId is kept as its 24-char hex.
- A zero ObjectId going into a nullable column is stored as NULL. In a required column it's rejected.
- `go_zero_values` is for data written by Go services, which leave unset fields out: it fills required columns the document doesn't have with Go's zero value (`0`, `false`, `{}`, `0001-01-01`). The report counts every fill.
- Documents that can't be stored, or that reference rows that don't exist, go to `lift-rejects/<collection>.jsonl` with the reason.
- When foreign keys form a cycle (users → teams → users), lift writes the nullable side as NULL first and fills it in once both tables are loaded. A reference that points at nothing stays NULL and is reported.
- `before` and `after` take SQL to run inside the load transaction, e.g. clearing seed rows that clash with the dump, or filling columns lift can't derive.
- A collection that isn't in the dump loads as empty. mongodump skips empty collections.
- Reads `mongodump` output only, never a live Mongo. Dump without `--gzip`.

## Build

```sh
cargo build --release
```

The end-to-end tests need a scratch Postgres: `LIFT_TEST_DATABASE_URL=... cargo test`. Without it, they're skipped.
