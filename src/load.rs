//! Load a dump into the target schema: COPY each collection into a temp table, drop rows whose
//! references point at nothing, then write into the real table. Re-running gives the same result.
//!
//! Embedded arrays mapped as children load right after their parent, one row per element. They
//! usually have database-generated keys, so instead of upserting, lift replaces the children of
//! every parent in the dump.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Write,
    path::Path,
};

use anyhow::{Context, Result, bail};
use bson::{Bson, Document};
use postgres::{Client, Transaction};

use crate::{
    convert, dump,
    ids::{NIL, Suffix},
    mapping::{Mapping, Source},
    schema::{self, Column, Table},
};

pub struct Options<'a> {
    pub dump: &'a Path,
    pub rejects: &'a Path,
    /// false = do all the work, then roll back (`lift check`)
    pub commit: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    pub table: String,
    /// documents, or array elements for a child table
    pub docs: usize,
    pub loaded: u64,
    pub rejects: Vec<Reject>,
    pub counts: Counts,
    /// the collection isn't in the dump (mongodump skips empty collections)
    pub missing: bool,
    /// rows that loaded, but a deferred reference pointed at nothing and was left NULL
    pub nulled_refs: Vec<Reject>,
    pub elapsed: std::time::Duration,
}

#[derive(Debug, Default)]
pub struct Counts {
    /// column -> rows that got Go's zero value because the field was missing
    pub zero_filled: BTreeMap<String, usize>,
    /// column -> rows where Go's zero ObjectId (unset) went into a nullable column as NULL
    pub zero_ids: BTreeMap<String, usize>,
}

#[derive(Debug, serde::Serialize)]
pub struct Reject {
    pub id: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<serde_json::Value>,
}

pub(crate) enum Fill {
    Path(String),
    ParentPath(String),
    Const(Bson),
    Zero(&'static str),
    ParentKey,
}

pub(crate) struct Plan<'a> {
    pub table_name: &'a str,
    pub table: &'a Table,
    pub columns: Vec<(&'a Column, Fill)>,
    /// go_zero_values: a mapped required field missing on one document gets Go's zero value
    zero_missing: bool,
    /// nullable foreign-key columns written after every table is loaded, to break FK cycles
    pub deferred: BTreeSet<String>,
}

pub(crate) struct Child<'a> {
    pub array: &'a str,
    pub parent_key: &'a str,
    pub plan: Plan<'a>,
}

/// A collection: its own table plus any embedded arrays loaded into child tables.
pub(crate) struct Unit<'a> {
    pub collection: &'a str,
    pub pk: String,
    pub plan: Plan<'a>,
    pub children: Vec<Child<'a>>,
}

/// Where a row's values come from: the document (or array element), its parent, the parent's key.
struct Ctx<'a> {
    node: &'a Bson,
    parent: Option<&'a Document>,
    parent_key: Option<&'a str>,
}

pub fn run(db: &mut Client, m: &Mapping, opts: &Options) -> Result<Vec<Report>> {
    let ids = Suffix::parse(&m.id_suffix)?;
    let tables = schema::read(db, &m.schema)?;
    let mut units = plan(m, &tables)?;
    let order = order_units(&mut units)?;

    fs::create_dir_all(opts.rejects).with_context(|| format!("create {}", opts.rejects.display()))?;
    let mut tx = db.transaction()?;
    for sql in &m.before {
        tx.batch_execute(sql).with_context(|| format!("before: {sql}"))?;
    }

    let mut reports: Vec<(String, Report)> = Vec::new();
    for (i, name) in order.iter().enumerate() {
        let unit = &units[name];
        let t0 = std::time::Instant::now();
        let mut r = load_table(&mut tx, &m.schema, unit, i, &ids, opts).with_context(|| format!("loading {name}"))?;
        r.elapsed = t0.elapsed();
        reports.push((unit.collection.to_string(), r));
        for child in &unit.children {
            let t0 = std::time::Instant::now();
            let mut r = load_children(&mut tx, &m.schema, unit, child, &ids, opts).with_context(|| {
                format!("loading {} from {}.{}", child.plan.table_name, unit.collection, child.array)
            })?;
            r.elapsed = t0.elapsed();
            reports.push((unit.collection.to_string(), r));
        }
    }

    // references held back to break cycles, now that every table is in
    for (i, name) in order.iter().enumerate() {
        let unit = &units[name];
        if unit.plan.deferred.is_empty() {
            continue;
        }
        let nulled = fill_deferred(&mut tx, &m.schema, unit, i).with_context(|| format!("deferred refs of {name}"))?;
        if let Some((_, r)) = reports.iter_mut().find(|(_, r)| r.table == unit.plan.table_name) {
            r.nulled_refs = nulled;
        }
    }

    for sql in &m.after {
        tx.batch_execute(sql).with_context(|| format!("after: {sql}"))?;
    }
    if opts.commit {
        tx.commit()?;
    } else {
        tx.rollback()?;
    }

    let mut lines: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (collection, r) in &reports {
        let entry = lines.entry(collection.as_str()).or_default();
        entry.extend(r.rejects.iter().chain(&r.nulled_refs).map(|x| reject_line(&r.table, x)));
    }
    for (collection, l) in &lines {
        write_rejects(opts.rejects, collection, l)?;
    }
    Ok(reports.into_iter().map(|(_, r)| r).collect())
}

/// Load order from foreign keys between mapped columns. If they form a cycle, order by the
/// required ones only, and defer each nullable reference that points at a table loaded at the
/// same point or later: it's written as NULL first and filled in at the end.
pub(crate) fn order_units(units: &mut BTreeMap<String, Unit>) -> Result<Vec<String>> {
    let names: Vec<String> = units.keys().cloned().collect();
    let owner: HashMap<String, String> = units
        .iter()
        .flat_map(|(n, u)| {
            std::iter::once(u.plan.table_name)
                .chain(u.children.iter().map(|c| c.plan.table_name))
                .map(move |t| (t.to_string(), n.clone()))
        })
        .collect();
    // `relaxed` units ignore their nullable references to each other
    let refs = |units: &BTreeMap<String, Unit>, name: &str, relaxed: &BTreeSet<String>| -> Vec<String> {
        let u = &units[name];
        std::iter::once(&u.plan)
            .chain(u.children.iter().map(|c| &c.plan))
            .flat_map(mapped_fks)
            .filter_map(|(col, fk)| {
                let target = owner.get(&fk.ref_table)?;
                let skip = col.nullable && relaxed.contains(name) && relaxed.contains(target);
                (!skip).then(|| target.clone())
            })
            .collect()
    };

    // relax only the units actually stuck in a cycle, widening until an order exists
    let mut relaxed = BTreeSet::new();
    let order = loop {
        match schema::load_order(&names, |n| refs(units, n, &relaxed)) {
            Ok(order) => break order,
            Err(stuck) => {
                // the stuck set also holds tables that merely depend on a cycle; peel off any
                // that no other stuck table references, until only cycle members are left
                let mut cycle: BTreeSet<String> = stuck.iter().cloned().collect();
                loop {
                    let referenced: BTreeSet<String> =
                        cycle.iter().flat_map(|n| refs(units, n, &relaxed)).filter(|t| cycle.contains(t)).collect();
                    let leaves: Vec<String> = cycle.iter().filter(|n| !referenced.contains(*n)).cloned().collect();
                    if leaves.is_empty() {
                        break;
                    }
                    for l in leaves {
                        cycle.remove(&l);
                    }
                }
                let before = relaxed.len();
                relaxed.extend(cycle);
                if relaxed.len() == before {
                    bail!(
                        "foreign keys form a cycle through NOT NULL columns, so no load order works: {}",
                        stuck.join(", ")
                    );
                }
            }
        }
    };
    let pos: HashMap<&str, usize> = order.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();

    for (i, name) in order.iter().enumerate() {
        let u = units.get_mut(name).expect("unit");
        let later = |t: &str| owner.get(t).is_some_and(|o| pos[o.as_str()] >= i);
        let defer: Vec<String> = mapped_fks(&u.plan)
            .filter(|(col, fk)| col.nullable && fk.ref_table != u.plan.table_name && later(&fk.ref_table))
            .map(|(col, _)| col.name.clone())
            .collect();
        u.plan.deferred.extend(defer);
        for c in &u.children {
            if let Some((col, fk)) =
                mapped_fks(&c.plan).find(|(_, fk)| owner.get(&fk.ref_table).is_some_and(|o| pos[o.as_str()] > i))
            {
                bail!(
                    "{}.{} references {}, which loads later; lift can't defer child-table columns",
                    c.plan.table_name,
                    col.name,
                    fk.ref_table
                );
            }
        }
    }
    Ok(order)
}

pub(crate) fn mapped_fks<'p>(p: &'p Plan) -> impl Iterator<Item = (&'p Column, &'p schema::ForeignKey)> {
    p.table
        .foreign_keys
        .iter()
        .filter_map(|fk| p.columns.iter().find(|(c, _)| c.name == fk.column).map(|(c, _)| (*c, fk)))
}

/// Write deferred references: valid ones are filled in, ones pointing at nothing stay NULL.
fn fill_deferred(tx: &mut Transaction, schema: &str, u: &Unit, i: usize) -> Result<Vec<Reject>> {
    let p = &u.plan;
    let (target, pk) = (target(schema, p), ident(&u.pk));
    let mut nulled = Vec::new();
    for (col, fk) in mapped_fks(p).filter(|(c, _)| p.deferred.contains(&c.name)) {
        let c = ident(&col.name);
        let exists = format!(
            "exists (select 1 from {}.{} r where r.{} = d.{c})",
            ident(&fk.ref_schema),
            ident(&fk.ref_table),
            ident(&fk.ref_column)
        );
        for r in tx.query(
            &format!("select d.{pk}::text, d.{c}::text from lift_def_{i} d where d.{c} is not null and not {exists}"),
            &[],
        )? {
            let value: String = r.get(1);
            nulled.push(Reject {
                id: r.get(0),
                reason: format!("{} -> {}: {value} doesn't exist, left NULL", col.name, fk.ref_table),
                document: None,
            });
        }
        tx.execute(
            &format!("update {target} t set {c} = d.{c} from lift_def_{i} d where t.{pk} = d.{pk} and d.{c} is not null and {exists}"),
            &[],
        )?;
    }
    Ok(nulled)
}

/// Check the mapping against the real schema before touching any data.
pub(crate) fn plan<'a>(m: &'a Mapping, tables: &'a HashMap<String, Table>) -> Result<BTreeMap<String, Unit<'a>>> {
    let mut units = BTreeMap::new();
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();

    for tm in &m.tables {
        let Some(table) = tables.get(&tm.table) else {
            problems.push(format!("{}: no table {:?} in schema {:?}", tm.collection, tm.table, m.schema));
            continue;
        };
        let [pk] = table.primary_key.as_slice() else {
            problems.push(format!("{}: lift needs a single-column primary key", tm.table));
            continue;
        };
        if !matches!(tm.columns.get(pk), Some(Source::Path(_))) {
            problems.push(format!("{}: map the primary key {pk:?} to a path (usually \"_id\")", tm.table));
        }
        let plan = plan_columns(&tm.table, table, &tm.columns, None, m.go_zero_values, &mut problems);

        let mut children = Vec::new();
        for cm in &tm.children {
            let Some(ctable) = tables.get(&cm.table) else {
                problems.push(format!("{}.{}: no table {:?}", tm.collection, cm.array, cm.table));
                continue;
            };
            if ctable.column(&cm.parent_key).is_none() {
                problems.push(format!("{}: parent_key {:?} is not a column", cm.table, cm.parent_key));
            }
            let plan =
                plan_columns(&cm.table, ctable, &cm.columns, Some(&cm.parent_key), m.go_zero_values, &mut problems);
            children.push(Child { array: &cm.array, parent_key: &cm.parent_key, plan });
        }

        for t in std::iter::once(&tm.table).chain(tm.children.iter().map(|c| &c.table)) {
            if !seen.insert(t.clone()) {
                problems.push(format!("{t} is mapped twice"));
            }
        }
        units.insert(tm.table.clone(), Unit { collection: &tm.collection, pk: pk.clone(), plan, children });
    }

    if !problems.is_empty() {
        bail!("mapping doesn't fit the schema:\n  {}", problems.join("\n  "));
    }
    Ok(units)
}

fn plan_columns<'a>(
    name: &'a str,
    table: &'a Table,
    mapped: &'a BTreeMap<String, Source>,
    parent_key: Option<&str>,
    go_zero_values: bool,
    problems: &mut Vec<String>,
) -> Plan<'a> {
    for col in mapped.keys() {
        if table.column(col).is_none() {
            problems.push(format!("{name}: no column {col:?}"));
        }
        if Some(col.as_str()) == parent_key {
            problems.push(format!("{name}: {col:?} is the parent_key, lift fills it; remove it from columns"));
        }
    }
    let mut columns = Vec::new();
    for col in &table.columns {
        let fill = match mapped.get(&col.name) {
            _ if Some(col.name.as_str()) == parent_key => Fill::ParentKey,
            Some(Source::Path(p)) => match p.strip_prefix("$parent.") {
                Some(rest) => Fill::ParentPath(rest.to_string()),
                None => Fill::Path(p.clone()),
            },
            Some(Source::Value { value }) => Fill::Const(convert::from_toml(value)),
            None if col.nullable || col.has_default => continue, // NULL / the column default
            None => match (go_zero_values, convert::zero(&col.udt)) {
                (true, Some(z)) => Fill::Zero(z),
                _ => {
                    problems.push(format!("{name}: required column {:?} ({}) is not mapped", col.name, col.udt));
                    continue;
                }
            },
        };
        columns.push((col, fill));
    }
    Plan { table_name: name, table, columns, zero_missing: go_zero_values, deferred: BTreeSet::new() }
}

fn load_table(tx: &mut Transaction, schema: &str, u: &Unit, i: usize, ids: &Suffix, opts: &Options) -> Result<Report> {
    let p = &u.plan;
    let mut report = stage_table(tx, schema, u, ids, opts.dump)?;
    if report.missing {
        return Ok(report);
    }

    // deferred references are kept aside and written once every table is loaded
    let pk = ident(&u.pk);
    if !p.deferred.is_empty() {
        let keep: Vec<String> = p.deferred.iter().map(|c| ident(c)).collect();
        tx.batch_execute(&format!(
            "create temp table lift_def_{i} on commit drop as select {pk}, {} from lift_tmp",
            keep.join(", ")
        ))?;
    }

    // upsert, so running twice changes nothing
    let cols = insert_cols(p);
    let updates: Vec<String> = p
        .columns
        .iter()
        .filter(|(c, _)| !p.deferred.contains(&c.name))
        .map(|(c, _)| ident(&c.name))
        .filter(|c| *c != pk)
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    // rows that are already identical are left alone, so a re-run writes only what changed
    let t = target(schema, p);
    let on_conflict = if updates.is_empty() {
        "do nothing".to_string()
    } else {
        let set_cols: Vec<String> = p
            .columns
            .iter()
            .filter(|(c, _)| !p.deferred.contains(&c.name))
            .map(|(c, _)| ident(&c.name))
            .filter(|c| *c != pk)
            .collect();
        let old = set_cols.iter().map(|c| format!("{t}.{c}")).collect::<Vec<_>>().join(", ");
        let new = set_cols.iter().map(|c| format!("excluded.{c}")).collect::<Vec<_>>().join(", ");
        format!("do update set {} where row({old}) is distinct from row({new})", updates.join(", "))
    };
    report.loaded = insert_without_fks(
        tx,
        &t,
        &format!("insert into {t} ({cols}) select {cols} from lift_tmp on conflict ({pk}) {on_conflict}"),
    )?;
    Ok(report)
}

/// Postgres checks foreign keys row by row on insert, which is most of a bulk load's time. lift
/// has already dropped every row whose references point at nothing, so for the insert it drops
/// the table's foreign keys and adds them back afterwards, which re-validates them in one pass.
/// Without permission to alter the table it just inserts with the per-row checks.
fn insert_without_fks(tx: &mut Transaction, target: &str, insert: &str) -> Result<u64> {
    let fks: Vec<(String, String)> = tx
        .query(
            "select conname::text, pg_get_constraintdef(oid) from pg_constraint
             where conrelid = $1::text::regclass and contype = 'f'",
            &[&target],
        )?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let dropped = !fks.is_empty() && {
        let mut sp = tx.transaction()?;
        let ok = fks.iter().all(|(name, _)| {
            sp.batch_execute(&format!("alter table {target} drop constraint {}", ident(name))).is_ok()
        });
        if ok {
            sp.commit()?
        } else {
            sp.rollback()?
        }
        ok
    };
    let t0 = std::time::Instant::now();
    let n = tx.execute(insert, &[])?;
    trace(target, "insert", t0);
    if dropped {
        let t0 = std::time::Instant::now();
        for (name, def) in &fks {
            tx.batch_execute(&format!("alter table {target} add constraint {} {def}", ident(name)))
                .with_context(|| format!("re-adding foreign key {name} on {target}"))?;
        }
        trace(target, "re-check foreign keys", t0);
    }
    Ok(n)
}

/// Convert every document into `lift_tmp` (typed like the target) and drop rows whose references
/// point at nothing. What's left is exactly what the table should hold. Used by `run` and `verify`.
pub(crate) fn stage_table(
    tx: &mut Transaction,
    schema: &str,
    u: &Unit,
    ids: &Suffix,
    dump_dir: &Path,
) -> Result<Report> {
    let p = &u.plan;
    let mut report = Report { table: p.table_name.into(), ..Default::default() };
    let Some(docs) = dump::open(dump_dir, u.collection)? else {
        report.missing = true;
        return Ok(report);
    };
    let t0 = std::time::Instant::now();
    let mut copy = std::io::BufWriter::with_capacity(1 << 20, start_copy(tx, schema, p)?);
    for doc in docs {
        let node = Bson::Document(doc?);
        report.docs += 1;
        let ctx = Ctx { node: &node, parent: None, parent_key: None };
        let label = doc_id(&node);
        match row(&ctx, p, ids, &label, &mut report.counts) {
            Ok(line) => copy.write_all(line.as_bytes())?,
            Err(reason) => {
                report.rejects.push(Reject { id: label, reason, document: Some(node.into_relaxed_extjson()) })
            }
        }
    }
    copy.into_inner().map_err(|e| e.into_error())?.finish()?;
    trace(p.table_name, "read + copy", t0);
    let t0 = std::time::Instant::now();
    tx.batch_execute("analyze lift_tmp")?;
    trace(p.table_name, "analyze", t0);

    // every parent key, kept for the child tables (indexed, so children match by join)
    if !u.children.is_empty() {
        tx.batch_execute(&format!(
            "drop table if exists lift_keys;
             create temp table lift_keys on commit drop as select {pk} k from lift_tmp;
             create index on lift_keys (k);
             analyze lift_keys",
            pk = ident(&u.pk)
        ))?;
    }

    let t0 = std::time::Instant::now();
    drop_orphans(tx, schema, p, &mut report)?;
    trace(p.table_name, "orphans", t0);
    Ok(report)
}

fn load_children(
    tx: &mut Transaction,
    schema: &str,
    u: &Unit,
    c: &Child,
    ids: &Suffix,
    opts: &Options,
) -> Result<Report> {
    let p = &c.plan;
    let mut report = stage_children(tx, schema, u, c, ids, opts.dump)?;
    if report.missing {
        return Ok(report);
    }

    // replace the children of every parent in the dump, so re-runs don't pile up rows
    let cols = insert_cols(p);
    let t0 = std::time::Instant::now();
    tx.execute(
        &format!("delete from {} where {} in (select k from lift_keys)", target(schema, p), ident(c.parent_key)),
        &[],
    )?;
    trace(p.table_name, "delete old children", t0);
    let t = target(schema, p);
    report.loaded = insert_without_fks(tx, &t, &format!("insert into {t} ({cols}) select {cols} from lift_tmp"))?;
    Ok(report)
}

/// Child rows for every parent into `lift_tmp`, orphans dropped. Expects `lift_keys` (every parent
/// key) from staging the parent table just before.
pub(crate) fn stage_children(
    tx: &mut Transaction,
    schema: &str,
    u: &Unit,
    c: &Child,
    ids: &Suffix,
    dump_dir: &Path,
) -> Result<Report> {
    let p = &c.plan;
    let mut report = Report { table: p.table_name.into(), ..Default::default() };
    let Some(docs) = dump::open(dump_dir, u.collection)? else {
        report.missing = true;
        return Ok(report);
    };
    let t0 = std::time::Instant::now();
    let mut copy = std::io::BufWriter::with_capacity(1 << 20, start_copy(tx, schema, p)?);
    for doc in docs {
        let doc = doc?;
        // the parent's key, converted the same way as in its own table
        let Some(key) = parent_key(&doc, u, ids) else { continue };
        let Some(Bson::Array(items)) = convert::lookup(&doc, c.array) else { continue };
        let parent_label = id_label(doc.get("_id"));
        for (i, item) in items.iter().enumerate() {
            report.docs += 1;
            let label = format!("{parent_label}.{}[{i}]", c.array);
            let ctx = Ctx { node: item, parent: Some(&doc), parent_key: Some(&key) };
            match row(&ctx, p, ids, &label, &mut report.counts) {
                Ok(line) => copy.write_all(line.as_bytes())?,
                Err(reason) => report.rejects.push(Reject {
                    id: label,
                    reason,
                    document: Some(item.clone().into_relaxed_extjson()),
                }),
            }
        }
    }
    copy.into_inner().map_err(|e| e.into_error())?.finish()?;
    trace(p.table_name, "read + copy", t0);
    let t0 = std::time::Instant::now();
    tx.batch_execute("analyze lift_tmp")?;
    drop_orphans(tx, schema, p, &mut report)?;
    trace(p.table_name, "analyze + orphans", t0);
    Ok(report)
}

/// A temp table shaped like the target, plus `lift_ref` naming each row's source for rejects.
fn start_copy<'t>(tx: &'t mut Transaction, schema: &str, p: &Plan) -> Result<postgres::CopyInWriter<'t>> {
    tx.batch_execute(&format!(
        "drop table if exists lift_tmp;
         create temp table lift_tmp (like {} including defaults including identity including generated) on commit drop;
         alter table lift_tmp add column lift_ref text",
        target(schema, p)
    ))?;
    Ok(tx.copy_in(&format!("copy lift_tmp ({}, lift_ref) from stdin", col_list(p)))?)
}

/// Remove rows whose foreign keys point at rows that don't exist (deleted parents, Go's zero
/// id on a required reference, a parent that was itself rejected).
fn drop_orphans(tx: &mut Transaction, schema: &str, p: &Plan, report: &mut Report) -> Result<()> {
    for fk in &p.table.foreign_keys {
        if !p.columns.iter().any(|(c, _)| c.name == fk.column) || p.deferred.contains(&fk.column) {
            continue;
        }
        let (col, rc) = (ident(&fk.column), ident(&fk.ref_column));
        let self_ref = if fk.ref_schema == schema && fk.ref_table == p.table_name {
            format!(" and not exists (select 1 from lift_tmp t2 where t2.{rc} = t.{col})")
        } else {
            String::new()
        };
        let sql = format!(
            "delete from lift_tmp t where t.{col} is not null
             and not exists (select 1 from {}.{} r where r.{rc} = t.{col}){self_ref}
             returning t.lift_ref, t.{col}::text",
            ident(&fk.ref_schema),
            ident(&fk.ref_table),
        );
        for r in tx.query(&sql, &[])? {
            let value: String = r.get(1);
            let why =
                if value == NIL { "is Go's zero id (unset)".to_string() } else { format!("{value} doesn't exist") };
            report.rejects.push(Reject {
                id: r.get(0),
                reason: format!("{} -> {}: {why}", fk.column, fk.ref_table),
                document: None,
            });
        }
    }
    Ok(())
}

fn parent_key(doc: &Document, u: &Unit, ids: &Suffix) -> Option<String> {
    let (col, fill) = u.plan.columns.iter().find(|(c, _)| c.name == u.pk)?;
    let Fill::Path(path) = fill else { return None };
    convert::to_pg(convert::lookup(doc, path)?, &col.udt, ids).ok()
}

/// One COPY line, or the reason the value can't be stored.
fn row(ctx: &Ctx, p: &Plan, ids: &Suffix, label: &str, counts: &mut Counts) -> Result<String, String> {
    let mut fields = Vec::with_capacity(p.columns.len() + 1);
    for (col, fill) in &p.columns {
        let found = match fill {
            Fill::Const(v) => Some(convert::to_pg(v, &col.udt, ids)),
            Fill::Zero(z) => {
                *counts.zero_filled.entry(col.name.clone()).or_default() += 1;
                Some(Ok(z.to_string()))
            }
            Fill::ParentKey => ctx.parent_key.map(|k| Ok(k.to_string())),
            Fill::Path(path) => match convert::lookup_in(ctx.node, path) {
                Some(v) if col.nullable && convert::is_zero_oid(v) => {
                    *counts.zero_ids.entry(col.name.clone()).or_default() += 1;
                    None
                }
                v => v.map(|v| convert::to_pg(v, &col.udt, ids)),
            },
            Fill::ParentPath(path) => match ctx.parent.and_then(|d| convert::lookup(d, path)) {
                Some(v) if col.nullable && convert::is_zero_oid(v) => {
                    *counts.zero_ids.entry(col.name.clone()).or_default() += 1;
                    None
                }
                v => v.map(|v| convert::to_pg(v, &col.udt, ids)),
            },
        };
        let value = match found {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => return Err(format!("{}: {e}", col.name)),
            None if col.nullable => None,
            // a mapped required field missing on this document: Go would have sent its zero value
            None => match convert::zero(&col.udt) {
                Some(z) if p.zero_missing && !matches!(fill, Fill::ParentKey) => {
                    *counts.zero_filled.entry(col.name.clone()).or_default() += 1;
                    Some(z.to_string())
                }
                _ => return Err(format!("{} is required but missing", col.name)),
            },
        };
        fields.push(convert::copy_field(value.as_deref()));
    }
    fields.push(convert::copy_field(Some(label)));
    Ok(fields.join("\t") + "\n")
}

/// Columns written by the insert: everything except deferred references.
fn insert_cols(p: &Plan) -> String {
    p.columns
        .iter()
        .filter(|(c, _)| !p.deferred.contains(&c.name))
        .map(|(c, _)| ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn col_list(p: &Plan) -> String {
    p.columns.iter().map(|(c, _)| ident(&c.name)).collect::<Vec<_>>().join(", ")
}

pub(crate) fn target(schema: &str, p: &Plan) -> String {
    format!("{}.{}", ident(schema), ident(p.table_name))
}

fn reject_line(table: &str, r: &Reject) -> String {
    let mut v = serde_json::to_value(r).unwrap_or_default();
    v["table"] = table.into();
    v.to_string()
}

fn write_rejects(dir: &Path, collection: &str, lines: &[String]) -> Result<()> {
    let path = dir.join(format!("{collection}.jsonl"));
    if lines.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    fs::write(&path, lines.join("\n") + "\n").with_context(|| format!("write {}", path.display()))
}

fn doc_id(node: &Bson) -> String {
    id_label(node.as_document().and_then(|d| d.get("_id")))
}

fn id_label(id: Option<&Bson>) -> String {
    match id {
        Some(Bson::ObjectId(o)) => o.to_hex(),
        Some(other) => other.clone().into_relaxed_extjson().to_string(),
        None => "(no _id)".into(),
    }
}

/// With LIFT_TRACE set, print how long each phase took, to see where a slow load spends its time.
pub(crate) fn trace(table: &str, phase: &str, since: std::time::Instant) {
    if std::env::var_os("LIFT_TRACE").is_some() {
        eprintln!("trace {table:<28} {phase:<22} {:>8.2}s", since.elapsed().as_secs_f64());
    }
}

pub fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use bson::{bson, doc};

    use super::*;

    fn col(name: &str, udt: &str, nullable: bool) -> Column {
        Column { name: name.into(), udt: udt.into(), nullable, has_default: false }
    }

    fn ids() -> Suffix {
        Suffix::parse("6c696674").unwrap()
    }

    fn plan_for(table: &Table, columns: Vec<(&str, Fill)>) -> Plan<'static> {
        let table: &'static Table = Box::leak(Box::new(Table { columns: table.columns.clone(), ..Default::default() }));
        let columns = columns.into_iter().map(|(n, f)| (table.column(n).unwrap(), f)).collect();
        Plan { table_name: "T", table, columns, zero_missing: true, deferred: BTreeSet::new() }
    }

    #[test]
    fn child_row_takes_element_fields_parent_key_and_parent_fields() {
        let t = Table {
            columns: vec![col("Year", "int4", false), col("ProgramId", "uuid", false), col("OrgName", "text", true)],
            ..Default::default()
        };
        let p = plan_for(
            &t,
            vec![
                ("Year", Fill::Path("year".into())),
                ("ProgramId", Fill::ParentKey),
                ("OrgName", Fill::ParentPath("org.name".into())),
            ],
        );
        let parent = doc! { "org": { "name": "Acme" } };
        let item = bson!({ "year": 2026 });
        let ctx = Ctx { node: &item, parent: Some(&parent), parent_key: Some("6a3a73d0-68e5-ff12-5878-a53f6c696674") };

        let line = row(&ctx, &p, &ids(), "p.yearly_targets[0]", &mut Counts::default()).unwrap();
        assert_eq!(line, "2026\t6a3a73d0-68e5-ff12-5878-a53f6c696674\tAcme\tp.yearly_targets[0]\n");
    }

    #[test]
    fn plain_value_arrays_use_dollar_value() {
        let t = Table { columns: vec![col("Tag", "text", false)], ..Default::default() };
        let p = plan_for(&t, vec![("Tag", Fill::Path("$value".into()))]);
        let item = Bson::String("AN-0001".into());
        let ctx = Ctx { node: &item, parent: None, parent_key: None };
        assert_eq!(row(&ctx, &p, &ids(), "x", &mut Counts::default()).unwrap(), "AN-0001\tx\n");
    }

    #[test]
    fn missing_required_element_field_gets_zero_and_is_counted() {
        let t = Table { columns: vec![col("Count", "int4", false)], ..Default::default() };
        let p = plan_for(&t, vec![("Count", Fill::Path("count".into()))]);
        let item = bson!({});
        let ctx = Ctx { node: &item, parent: None, parent_key: None };
        let mut counts = Counts::default();
        assert_eq!(row(&ctx, &p, &ids(), "x", &mut counts).unwrap(), "0\tx\n");
        assert_eq!(counts.zero_filled["Count"], 1);
    }

    #[test]
    fn bad_value_names_the_column() {
        let t = Table { columns: vec![col("Year", "int4", false)], ..Default::default() };
        let p = plan_for(&t, vec![("Year", Fill::Path("year".into()))]);
        let item = bson!({ "year": "soon" });
        let ctx = Ctx { node: &item, parent: None, parent_key: None };
        let err = row(&ctx, &p, &ids(), "x", &mut Counts::default()).unwrap_err();
        assert!(err.starts_with("Year: can't store a string"), "{err}");
    }
}

/// End to end against a real Postgres. Runs only when LIFT_TEST_DATABASE_URL is set; builds its
/// own throwaway schema, so any scratch database works.
#[cfg(test)]
mod db_tests {
    use bson::{doc, oid::ObjectId};
    use postgres::NoTls;

    use super::*;
    use crate::verify;

    const SCHEMA: &str = "lift_it";

    fn db() -> Option<Client> {
        let url = std::env::var("LIFT_TEST_DATABASE_URL").ok()?;
        let mut db = Client::connect(&url, NoTls).expect("connect LIFT_TEST_DATABASE_URL");
        db.batch_execute(&format!(
            "drop schema if exists {SCHEMA} cascade; create schema {SCHEMA};
             create table {SCHEMA}.\"Programs\" (\"Id\" uuid primary key, \"Name\" text not null);
             create table {SCHEMA}.\"Targets\" (
               \"Id\" int generated by default as identity primary key,
               \"ProgramId\" uuid not null references {SCHEMA}.\"Programs\"(\"Id\") on delete cascade,
               \"Year\" int not null,
               \"Percent\" int not null);"
        ))
        .unwrap();
        Some(db)
    }

    fn dump(dir: &Path, docs: &[Document]) {
        let mut bytes = Vec::new();
        for d in docs {
            d.to_writer(&mut bytes).unwrap();
        }
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("programs.bson"), bytes).unwrap();
    }

    fn mapping() -> Mapping {
        toml::from_str(&format!(
            r#"
            schema = "{SCHEMA}"
            id_suffix = "6c696674"
            go_zero_values = false
            [[table]]
            collection = "programs"
            table = "Programs"
            [table.columns]
            Id = "_id"
            Name = "name"
            [[table.children]]
            array = "yearly_targets"
            table = "Targets"
            parent_key = "ProgramId"
            [table.children.columns]
            Year = "year"
            Percent = "target_percentage"
            "#
        ))
        .unwrap()
    }

    #[test]
    fn fk_cycle_is_broken_by_deferring_the_nullable_side() {
        let Ok(url) = std::env::var("LIFT_TEST_DATABASE_URL") else {
            eprintln!("skipped: set LIFT_TEST_DATABASE_URL to run");
            return;
        };
        let mut db = Client::connect(&url, NoTls).unwrap();
        // a classic cycle: a user's zone is optional, a zone's owner (a user) is required
        db.batch_execute(
            "drop schema if exists lift_cycle cascade; create schema lift_cycle;
             create table lift_cycle.\"Users\" (\"Id\" text primary key, \"ZoneId\" uuid);
             create table lift_cycle.\"Zones\" (\"Id\" uuid primary key,
               \"ProviderId\" text not null references lift_cycle.\"Users\"(\"Id\"));
             alter table lift_cycle.\"Users\" add foreign key (\"ZoneId\") references lift_cycle.\"Zones\"(\"Id\");",
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("lift-cycle-{}", std::process::id()));
        let (u1, u2, u3, z1) = (ObjectId::new(), ObjectId::new(), ObjectId::new(), ObjectId::new());
        let write = |name: &str, docs: &[Document]| {
            let mut bytes = Vec::new();
            for d in docs {
                d.to_writer(&mut bytes).unwrap();
            }
            fs::create_dir_all(dir.join("db")).unwrap();
            fs::write(dir.join("db").join(format!("{name}.bson")), bytes).unwrap();
        };
        write(
            "users",
            &[
                doc! { "_id": u1, "zone_id": z1 },
                doc! { "_id": u2, "zone_id": ObjectId::new() }, // zone was deleted
                doc! { "_id": u3, "zone_id": ObjectId::from_bytes([0; 12]) }, // Go's "unset"
            ],
        );
        write("zones", &[doc! { "_id": z1, "provider": u1 }]);
        let m: Mapping = toml::from_str(
            r#"
            schema = "lift_cycle"
            id_suffix = "6c696674"
            [[table]]
            collection = "users"
            table = "Users"
            [table.columns]
            Id = "_id"
            ZoneId = "zone_id"
            [[table]]
            collection = "zones"
            table = "Zones"
            [table.columns]
            Id = "_id"
            ProviderId = "provider"
            "#,
        )
        .unwrap();
        let rejects = dir.join("rejects");
        let reports = run(&mut db, &m, &Options { dump: &dir, rejects: &rejects, commit: true }).unwrap();

        let users = reports.iter().find(|r| r.table == "Users").unwrap();
        assert_eq!(users.loaded, 3, "every user loads; the bad zone ref is nulled, not the row");
        assert_eq!(users.nulled_refs.len(), 1);
        assert_eq!(users.counts.zero_ids["ZoneId"], 1);
        assert_eq!(reports.iter().find(|r| r.table == "Zones").unwrap().loaded, 1);

        let zones: Vec<Option<String>> = db
            .query("select \"ZoneId\"::text from lift_cycle.\"Users\" order by \"ZoneId\" nulls last", &[])
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(zones, [Some(crate::ids::Suffix::parse("6c696674").unwrap().uuid(&z1)), None, None]);

        db.batch_execute("drop schema lift_cycle cascade").unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn children_load_replace_on_rerun_and_follow_rejected_parents() {
        let Some(mut db) = db() else {
            eprintln!("skipped: set LIFT_TEST_DATABASE_URL to run");
            return;
        };
        let dir = std::env::temp_dir().join(format!("lift-it-{}", std::process::id()));
        let good = ObjectId::new();
        dump(
            &dir.join("db"),
            &[
                doc! { "_id": good, "name": "Spring", "yearly_targets": [
                    { "year": 2025, "target_percentage": 50 },
                    { "year": 2026, "target_percentage": 70 },
                ]},
                // no name and zero values off: the parent is rejected, so its target must be too
                doc! { "_id": ObjectId::new(), "yearly_targets": [ { "year": 2026, "target_percentage": 10 } ] },
                doc! { "_id": ObjectId::new(), "name": "No targets" },
            ],
        );
        let m = mapping();
        let rejects = dir.join("rejects");
        let opts = Options { dump: &dir, rejects: &rejects, commit: true };

        let reports = run(&mut db, &m, &opts).unwrap();
        let by = |t: &str| reports.iter().find(|r| r.table == t).unwrap();
        assert_eq!((by("Programs").docs, by("Programs").loaded, by("Programs").rejects.len()), (3, 2, 1));
        assert_eq!((by("Targets").docs, by("Targets").loaded, by("Targets").rejects.len()), (3, 2, 1));
        assert!(by("Targets").rejects[0].reason.starts_with("ProgramId -> Programs:"), "{:?}", by("Targets").rejects);

        // a second run replaces children instead of piling them up
        run(&mut db, &m, &opts).unwrap();
        let n: i64 = db.query_one(&format!("select count(*) from {SCHEMA}.\"Targets\""), &[]).unwrap().get(0);
        assert_eq!(n, 2);
        let years: Vec<i32> = db
            .query(&format!("select \"Year\" from {SCHEMA}.\"Targets\" order by 1"), &[])
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(years, [2025, 2026]);

        let checks = verify::run(&mut db, &m, &dir).unwrap();
        let targets = checks.iter().find(|c| c.table == "Targets").unwrap();
        // the rejected parent's target is rejected too, and is not expected in the table
        assert_eq!((targets.docs, targets.rejected, targets.expected), (3, 1, 2));
        assert!(checks.iter().all(|c| c.ok()), "{checks:?}");

        // a value changed behind lift's back is caught, with the column named
        db.batch_execute(&format!("update {SCHEMA}.\"Programs\" set \"Name\" = 'Edited'")).unwrap();
        db.batch_execute(&format!("update {SCHEMA}.\"Targets\" set \"Percent\" = 99 where \"Year\" = 2026")).unwrap();
        let checks = verify::run(&mut db, &m, &dir).unwrap();
        let programs = checks.iter().find(|c| c.table == "Programs").unwrap();
        assert_eq!(programs.changed, 2);
        assert_eq!(programs.changed_columns, [("Name".to_string(), 2)]);
        let targets = checks.iter().find(|c| c.table == "Targets").unwrap();
        assert_eq!(targets.changed, 1, "one parent's targets differ");

        // and so is a row deleted behind lift's back
        db.batch_execute(&format!("delete from {SCHEMA}.\"Targets\" where \"Year\" = 2025")).unwrap();
        let checks = verify::run(&mut db, &m, &dir).unwrap();
        assert_eq!(checks.iter().find(|c| c.table == "Targets").unwrap().missing, 1);

        let lines = fs::read_to_string(rejects.join("programs.jsonl")).unwrap();
        assert_eq!(lines.lines().count(), 2);
        db.batch_execute(&format!("drop schema {SCHEMA} cascade")).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
