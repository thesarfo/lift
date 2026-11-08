# lift

Move a MongoDB dump into a Postgres schema you already designed, and check nothing was lost.

You write a mapping file from collections to tables. lift reads the target schema from Postgres itself, loads in foreign-key order, and tells you about every document that didn't fit.

## Use

```sh
mongodump --db shepherd --out dump
export LIFT_DATABASE_URL="host=localhost user=postgres dbname=shepherd"

lift check  mapping.toml --dump dump    # load, report, roll back
lift run    mapping.toml --dump dump    # load for real
lift verify mapping.toml --dump dump    # every document in its table?
```

`run` is safe to repeat: rows are upserted on their primary key.

## Mapping

```toml
schema = "shepherd"
id_suffix = "53484550"
go_zero_values = true

[[table]]
collection = "species"
table = "Species"
[table.columns]
Id = "_id"
OrganizationId = "organization_id"
Name = "name"
CreatedAt = "meta.created_at"
Archived = { value = false }
```

Columns map to dotted paths into the document, or to a fixed `value`. See [examples/shepherd.toml](examples/shepherd.toml) for a full one.

## Notes

- ObjectIds become UUIDs made of the 12 ObjectId bytes plus `id_suffix`, so the original id can always be recovered. A zero ObjectId becomes the nil UUID.
- `go_zero_values` fills required columns the document doesn't have with Go's zero value (`0`, `false`, `{}`, `0001-01-01`). The report counts every fill.
- Documents that can't be stored, or that reference rows that don't exist, go to `lift-rejects/<collection>.jsonl` with the reason.
- Reads `mongodump` output only, never a live Mongo. Dump without `--gzip`.

## Build

```sh
cargo build --release
```
