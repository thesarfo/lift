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
lift run    mapping.toml --dump dump            # load for real, safe to repeat
lift verify mapping.toml --dump dump            # every document in its table?
```

`plan` matches collections to tables and fields to columns by name, drafting child tables for embedded arrays. Anything it isn't sure about is left as a comment, not a guess. Review the draft before using it.

`verify` converts every document again and compares it with what's stored, column by column, and gives each table a fingerprint to record at sign-off. Add `--report report.html` for a single-file HTML report to hand whoever signs off the migration.

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

Columns map to dotted paths into the document, or to a fixed `value`. Embedded arrays become child tables, one row per element. See [examples/shop.toml](examples/shop.toml) for a full mapping, children included.

## Build

```sh
cargo build --release
```

The end-to-end tests need a scratch Postgres: `LIFT_TEST_DATABASE_URL=... cargo test`. Without it, they're skipped.
