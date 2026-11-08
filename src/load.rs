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
    /// column -> how many rows got Go's zero value
    pub zero_filled: BTreeMap<String, usize>,
}

#[derive(Debug, serde::Serialize)]
pub struct Reject {
    pub id: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<serde_json::Value>,
}

enum Fill {
    Path(String),
    ParentPath(String),
    Const(Bson),
    Zero(&'static str),
    ParentKey,
}

struct Plan<'a> {
    table_name: &'a str,
    table: &'a Table,
    columns: Vec<(&'a Column, Fill)>,
    /// go_zero_values: a mapped required field missing on one document gets Go's zero value
    zero_missing: bool,
}

struct Child<'a> {
    array: &'a str,
    parent_key: &'a str,
    plan: Plan<'a>,
}

/// A collection: its own table plus any embedded arrays loaded into child tables.
struct Unit<'a> {
    collection: &'a str,
    pk: String,
    plan: Plan<'a>,
    children: Vec<Child<'a>>,
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
    let units = plan(m, &tables)?;

    let names: Vec<String> = units.keys().cloned().collect();
    let order = schema::load_order(&names, |name| {
        let u = &units[name];
        std::iter::once(&u.plan)
            .chain(u.children.iter().map(|c| &c.plan))
            .flat_map(|p| p.table.foreign_keys.iter().map(|fk| fk.ref_table.clone()))
            .collect()
    })?;

    fs::create_dir_all(opts.rejects).with_context(|| format!("create {}", opts.rejects.display()))?;
    let mut tx = db.transaction()?;
    let mut reports = Vec::new();
    for name in &order {
        let unit = &units[name];
        let mut rejects = Vec::new();

        let r = load_table(&mut tx, &m.schema, unit, &ids, opts).with_context(|| format!("loading {name}"))?;
        rejects.extend(r.rejects.iter().map(|x| reject_line(&r.table, x)));
        reports.push(r);

        for child in &unit.children {
            let r = load_children(&mut tx, &m.schema, unit, child, &ids, opts)
                .with_context(|| format!("loading {} from {}.{}", child.plan.table_name, unit.collection, child.array))?;
            rejects.extend(r.rejects.iter().map(|x| reject_line(&r.table, x)));
            reports.push(r);
        }
        write_rejects(opts.rejects, unit.collection, &rejects)?;
    }
    if opts.commit {
        tx.commit()?;
    } else {
        tx.rollback()?;
    }
    Ok(reports)
}

/// Check the mapping against the real schema before touching any data.
fn plan<'a>(m: &'a Mapping, tables: &'a HashMap<String, Table>) -> Result<BTreeMap<String, Unit<'a>>> {
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
            let plan = plan_columns(&cm.table, ctable, &cm.columns, Some(&cm.parent_key), m.go_zero_values, &mut problems);
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
    Plan { table_name: name, table, columns, zero_missing: go_zero_values }
}

fn load_table(tx: &mut Transaction, schema: &str, u: &Unit, ids: &Suffix, opts: &Options) -> Result<Report> {
    let p = &u.plan;
    let mut report = Report { table: p.table_name.into(), ..Default::default() };
    let mut copy = start_copy(tx, schema, p)?;
    for doc in dump::open(opts.dump, u.collection)? {
        let node = Bson::Document(doc?);
        report.docs += 1;
        let ctx = Ctx { node: &node, parent: None, parent_key: None };
        let label = doc_id(&node);
        match row(&ctx, p, ids, &label, &mut report.zero_filled) {
            Ok(line) => copy.write_all(line.as_bytes())?,
            Err(reason) => report.rejects.push(Reject { id: label, reason, document: Some(node.into_relaxed_extjson()) }),
        }
    }
    copy.finish()?;

    drop_orphans(tx, schema, p, &mut report)?;

    // upsert, so running twice changes nothing
    let cols = col_list(p);
    let pk = ident(&u.pk);
    let updates: Vec<String> = p
        .columns
        .iter()
        .map(|(c, _)| ident(&c.name))
        .filter(|c| *c != pk)
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    let on_conflict =
        if updates.is_empty() { "do nothing".to_string() } else { format!("do update set {}", updates.join(", ")) };
    report.loaded = tx.execute(
        &format!("insert into {} ({cols}) select {cols} from lift_tmp on conflict ({pk}) {on_conflict}", target(schema, p)),
        &[],
    )?;
    Ok(report)
}

fn load_children(tx: &mut Transaction, schema: &str, u: &Unit, c: &Child, ids: &Suffix, opts: &Options) -> Result<Report> {
    let p = &c.plan;
    let mut report = Report { table: p.table_name.into(), ..Default::default() };
    let mut parent_keys = Vec::new();
    let mut copy = start_copy(tx, schema, p)?;
    for doc in dump::open(opts.dump, u.collection)? {
        let doc = doc?;
        // the parent's key, converted the same way as in its own table
        let Some(key) = parent_key(&doc, u, ids) else { continue };
        parent_keys.push(key.clone());
        let Some(Bson::Array(items)) = convert::lookup(&doc, c.array) else { continue };
        let parent_label = doc_id(&Bson::Document(doc.clone()));
        for (i, item) in items.iter().enumerate() {
            report.docs += 1;
            let label = format!("{parent_label}.{}[{i}]", c.array);
            let ctx = Ctx { node: item, parent: Some(&doc), parent_key: Some(&key) };
            match row(&ctx, p, ids, &label, &mut report.zero_filled) {
                Ok(line) => copy.write_all(line.as_bytes())?,
                Err(reason) => {
                    report.rejects.push(Reject { id: label, reason, document: Some(item.clone().into_relaxed_extjson()) })
                }
            }
        }
    }
    copy.finish()?;

    drop_orphans(tx, schema, p, &mut report)?;

    // replace the children of every parent in the dump, so re-runs don't pile up rows
    let cols = col_list(p);
    tx.execute(
        &format!("delete from {} where {}::text = any($1)", target(schema, p), ident(c.parent_key)),
        &[&parent_keys],
    )?;
    report.loaded = tx.execute(&format!("insert into {} ({cols}) select {cols} from lift_tmp", target(schema, p)), &[])?;
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
        if !p.columns.iter().any(|(c, _)| c.name == fk.column) {
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
            let why = if value == NIL { "is Go's zero id (unset)".to_string() } else { format!("{value} doesn't exist") };
            report.rejects.push(Reject { id: r.get(0), reason: format!("{} -> {}: {why}", fk.column, fk.ref_table), document: None });
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
fn row(ctx: &Ctx, p: &Plan, ids: &Suffix, label: &str, zero_filled: &mut BTreeMap<String, usize>) -> Result<String, String> {
    let mut fields = Vec::with_capacity(p.columns.len() + 1);
    for (col, fill) in &p.columns {
        let found = match fill {
            Fill::Const(v) => Some(convert::to_pg(v, &col.udt, ids)),
            Fill::Zero(z) => {
                *zero_filled.entry(col.name.clone()).or_default() += 1;
                Some(Ok(z.to_string()))
            }
            Fill::ParentKey => ctx.parent_key.map(|k| Ok(k.to_string())),
            Fill::Path(path) => convert::lookup_in(ctx.node, path).map(|v| convert::to_pg(v, &col.udt, ids)),
            Fill::ParentPath(path) => {
                ctx.parent.and_then(|d| convert::lookup(d, path)).map(|v| convert::to_pg(v, &col.udt, ids))
            }
        };
        let value = match found {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => return Err(format!("{}: {e}", col.name)),
            None if col.nullable => None,
            // a mapped required field missing on this document: Go would have sent its zero value
            None => match convert::zero(&col.udt) {
                Some(z) if p.zero_missing && !matches!(fill, Fill::ParentKey) => {
                    *zero_filled.entry(col.name.clone()).or_default() += 1;
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

fn col_list(p: &Plan) -> String {
    p.columns.iter().map(|(c, _)| ident(&c.name)).collect::<Vec<_>>().join(", ")
}

fn target(schema: &str, p: &Plan) -> String {
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
    match node.as_document().and_then(|d| d.get("_id")) {
        Some(Bson::ObjectId(o)) => o.to_hex(),
        Some(other) => other.clone().into_relaxed_extjson().to_string(),
        None => "(no _id)".into(),
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
        Suffix::parse("53484550").unwrap()
    }

    fn plan_for(table: &Table, columns: Vec<(&str, Fill)>) -> Plan<'static> {
        let table: &'static Table = Box::leak(Box::new(Table { columns: table.columns.clone(), ..Default::default() }));
        let columns = columns.into_iter().map(|(n, f)| (table.column(n).unwrap(), f)).collect();
        Plan { table_name: "T", table, columns, zero_missing: true }
    }

    #[test]
    fn child_row_takes_element_fields_parent_key_and_parent_fields() {
        let t = Table {
            columns: vec![
                col("Year", "int4", false),
                col("ProgramId", "uuid", false),
                col("OrgName", "text", true),
            ],
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
        let parent = doc! { "org": { "name": "Cowtribe" } };
        let item = bson!({ "year": 2026 });
        let ctx = Ctx { node: &item, parent: Some(&parent), parent_key: Some("6a3a73d0-68e5-ff12-5878-a53f53484550") };

        let line = row(&ctx, &p, &ids(), "p.yearly_targets[0]", &mut BTreeMap::new()).unwrap();
        assert_eq!(line, "2026\t6a3a73d0-68e5-ff12-5878-a53f53484550\tCowtribe\tp.yearly_targets[0]\n");
    }

    #[test]
    fn plain_value_arrays_use_dollar_value() {
        let t = Table { columns: vec![col("Tag", "text", false)], ..Default::default() };
        let p = plan_for(&t, vec![("Tag", Fill::Path("$value".into()))]);
        let item = Bson::String("AN-0001".into());
        let ctx = Ctx { node: &item, parent: None, parent_key: None };
        assert_eq!(row(&ctx, &p, &ids(), "x", &mut BTreeMap::new()).unwrap(), "AN-0001\tx\n");
    }

    #[test]
    fn missing_required_element_field_gets_zero_and_is_counted() {
        let t = Table { columns: vec![col("Count", "int4", false)], ..Default::default() };
        let p = plan_for(&t, vec![("Count", Fill::Path("count".into()))]);
        let item = bson!({});
        let ctx = Ctx { node: &item, parent: None, parent_key: None };
        let mut zero = BTreeMap::new();
        assert_eq!(row(&ctx, &p, &ids(), "x", &mut zero).unwrap(), "0\tx\n");
        assert_eq!(zero["Count"], 1);
    }

    #[test]
    fn bad_value_names_the_column() {
        let t = Table { columns: vec![col("Year", "int4", false)], ..Default::default() };
        let p = plan_for(&t, vec![("Year", Fill::Path("year".into()))]);
        let item = bson!({ "year": "soon" });
        let ctx = Ctx { node: &item, parent: None, parent_key: None };
        let err = row(&ctx, &p, &ids(), "x", &mut BTreeMap::new()).unwrap_err();
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
            id_suffix = "53484550"
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
                doc! { "_id": good, "name": "FMD", "yearly_targets": [
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
        assert_eq!((targets.docs, targets.present), (3, 2)); // the rejected parent's target is the gap

        let lines = fs::read_to_string(rejects.join("programs.jsonl")).unwrap();
        assert_eq!(lines.lines().count(), 2);
        db.batch_execute(&format!("drop schema {SCHEMA} cascade")).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
