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

`verify` converts every document again and has Postgres compare it with what's stored, value by value. It reports rows that are missing, rows whose values differ (and which columns), and rows that didn't come from the dump. Each table gets a fingerprint of its migrated rows; record it at sign-off to show nothing changed since. It exits non-zero if anything is missing or different. Columns rewritten by an `after` hook will show as different.

Add `--report report.html` to also write a single-file HTML report for whoever signs off the migration: the verdict, every table's counts and fingerprint, and per table which columns differ, why documents were rejected, and what was filled in. Passwords in the connection string are left out.

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

## Speed

On a laptop (8 cores, stock Postgres 16 settings), with [examples/gen_shop.rs](examples/gen_shop.rs) generating a 353 MB dump of 1M orders and 3M line items: `run` takes 86 s and `verify` 103 s, both using about 12 MB of memory. Time grows linearly, so 5 GB is roughly 20 minutes to load and 25 to verify.

For the insert, lift drops a table's foreign keys and adds them back afterwards, which re-checks them in one pass instead of row by row. That needs permission to alter the table; without it lift falls back to the slower per-row checks. Set `LIFT_TRACE=1` to see how long each phase takes.

To reproduce:

```sh
cargo run --release --example gen_shop -- bench 1000000
psql -f examples/shop.sql
cargo run --release -- run examples/shop.toml --dump bench
```

## Build

```sh
cargo build --release
```

The end-to-end tests need a scratch Postgres: `LIFT_TEST_DATABASE_URL=... cargo test`. Without it, they're skipped.
