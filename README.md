# lift

I built this to migrate a production MongoDB database into Postgres: load a mongodump into a schema I'd already designed, and check nothing was lost.

You write a mapping file from collections to tables. lift reads the target schema from Postgres itself, loads in foreign-key order, and tells you about every document that didn't fit.

## Install

```sh
git clone git@github.com:thesarfo/lift.git
cd lift
./install.sh
```

Or unpack a release tarball and run `./install.sh` inside it. This puts `lift` in `~/.local/bin`; remove it with `./install.sh uninstall`.

## Use

```sh
mongodump --db shop --out dump
export LIFT_DATABASE_URL="host=localhost user=postgres dbname=shop"

lift plan   dump --schema shop > mapping.toml   # draft a mapping, then fix what didn't line up
lift check  mapping.toml --dump dump            # load, report, roll back
lift run    mapping.toml --dump dump            # load for real
lift verify mapping.toml --dump dump            # every document in its table?
```

`plan` reads the target schema and matches each table to a collection of the same name (case and a trailing "s" are ignored), then matches columns to fields it finds in a sample of that collection's documents. A table with one foreign key into a matched table, paired with the one array-of-documents field on that side, is drafted as a child table. Anything it isn't sure about is left as a comment in the output, not a guess. Review the draft before using it.

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

## Build

```sh
cargo build --release
```

The end-to-end tests need a scratch Postgres: `LIFT_TEST_DATABASE_URL=... cargo test`. Without it, they're skipped.
