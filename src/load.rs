//! Load a dump into the target schema: COPY each collection into a temp table, drop rows whose
//! references point at nothing, then upsert into the real table. Re-running gives the same result.

use std::{
    collections::{BTreeMap, HashMap},
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
    mapping::{Mapping, Source, TableMap},
    schema::{self, Table},
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
    Const(Bson),
    Zero(&'static str),
}

struct Plan<'a> {
    map: &'a TableMap,
    table: &'a Table,
    pk: String,
    columns: Vec<(&'a schema::Column, Fill)>,
    /// go_zero_values: a mapped required field missing on one document gets Go's zero value
    zero_missing: bool,
}

pub fn run(db: &mut Client, m: &Mapping, opts: &Options) -> Result<Vec<Report>> {
    let ids = Suffix::parse(&m.id_suffix)?;
    let tables = schema::read(db, &m.schema)?;
    let plans = plan(m, &tables)?;

    let names: Vec<String> = plans.keys().cloned().collect();
    let order = schema::load_order(&names, &tables)?;

    fs::create_dir_all(opts.rejects).with_context(|| format!("create {}", opts.rejects.display()))?;
    let mut tx = db.transaction()?;
    let mut reports = Vec::new();
    for name in &order {
        let report = load_table(&mut tx, &m.schema, &plans[name], &ids, opts).with_context(|| format!("loading {name}"))?;
        write_rejects(opts.rejects, &plans[name].map.collection, &report.rejects)?;
        reports.push(report);
    }
    if opts.commit {
        tx.commit()?;
    } else {
        tx.rollback()?;
    }
    Ok(reports)
}

/// Check the mapping against the real schema before touching any data.
fn plan<'a>(m: &'a Mapping, tables: &'a HashMap<String, Table>) -> Result<BTreeMap<String, Plan<'a>>> {
    let mut plans = BTreeMap::new();
    let mut problems = Vec::new();

    for tm in &m.tables {
        let Some(table) = tables.get(&tm.table) else {
            problems.push(format!("{}: no table {:?} in schema {:?}", tm.collection, tm.table, m.schema));
            continue;
        };
        let [pk] = table.primary_key.as_slice() else {
            problems.push(format!("{}: lift needs a single-column primary key", tm.table));
            continue;
        };
        if !tm.columns.contains_key(pk) {
            problems.push(format!("{}: map the primary key {pk:?} (usually to \"_id\")", tm.table));
        }
        for col in tm.columns.keys() {
            if table.column(col).is_none() {
                problems.push(format!("{}: no column {col:?}", tm.table));
            }
        }

        let mut columns = Vec::new();
        for col in &table.columns {
            match tm.columns.get(&col.name) {
                Some(Source::Path(p)) => columns.push((col, Fill::Path(p.clone()))),
                Some(Source::Value { value }) => columns.push((col, Fill::Const(convert::from_toml(value)))),
                None if col.nullable || col.has_default => {} // left to NULL / the column default
                None => match (m.go_zero_values, convert::zero(&col.udt)) {
                    (true, Some(z)) => columns.push((col, Fill::Zero(z))),
                    _ => problems.push(format!("{}: required column {:?} ({}) is not mapped", tm.table, col.name, col.udt)),
                },
            }
        }

        if plans.insert(tm.table.clone(), Plan { map: tm, table, pk: pk.clone(), columns, zero_missing: m.go_zero_values }).is_some() {
            problems.push(format!("{} is mapped twice", tm.table));
        }
    }

    if !problems.is_empty() {
        bail!("mapping doesn't fit the schema:\n  {}", problems.join("\n  "));
    }
    Ok(plans)
}

fn load_table(tx: &mut Transaction, schema: &str, p: &Plan, ids: &Suffix, opts: &Options) -> Result<Report> {
    let target = format!("{}.{}", ident(schema), ident(&p.map.table));
    let cols: Vec<String> = p.columns.iter().map(|(c, _)| ident(&c.name)).collect();
    let col_list = cols.join(", ");
    let mut report = Report { table: p.map.table.clone(), ..Default::default() };

    tx.batch_execute(&format!(
        "drop table if exists lift_tmp; create temp table lift_tmp (like {target} including defaults) on commit drop"
    ))?;

    // 1. COPY every document that converts cleanly; the rest become rejects
    let mut writer = tx.copy_in(&format!("copy lift_tmp ({col_list}) from stdin"))?;
    for doc in dump::open(opts.dump, &p.map.collection)? {
        let doc = doc?;
        report.docs += 1;
        match row(&doc, p, ids, |col| *report.zero_filled.entry(col.to_string()).or_default() += 1) {
            Ok(line) => writer.write_all(line.as_bytes())?,
            Err(reason) => report.rejects.push(Reject {
                id: doc_id(&doc),
                reason,
                document: Some(Bson::Document(doc).into_relaxed_extjson()),
            }),
        }
    }
    writer.finish()?;

    // 2. references to rows that don't exist (deleted parents, Go's zero id on a required ref)
    let pk = ident(&p.pk);
    for fk in &p.table.foreign_keys {
        if !p.columns.iter().any(|(c, _)| c.name == fk.column) {
            continue;
        }
        let (col, rc) = (ident(&fk.column), ident(&fk.ref_column));
        let self_ref = if fk.ref_schema == schema && fk.ref_table == p.map.table {
            format!(" and not exists (select 1 from lift_tmp t2 where t2.{rc} = t.{col})")
        } else {
            String::new()
        };
        let sql = format!(
            "delete from lift_tmp t where t.{col} is not null
             and not exists (select 1 from {}.{} r where r.{rc} = t.{col}){self_ref}
             returning t.{pk}::text, t.{col}::text",
            ident(&fk.ref_schema),
            ident(&fk.ref_table),
        );
        for r in tx.query(&sql, &[])? {
            let value: String = r.get(1);
            let why = if value == NIL { "is Go's zero id (unset)".to_string() } else { format!("{value} doesn't exist") };
            report.rejects.push(Reject {
                id: r.get(0),
                reason: format!("{} -> {}: {why}", fk.column, fk.ref_table),
                document: None,
            });
        }
    }

    // 3. upsert, so running twice changes nothing
    let updates: Vec<String> =
        cols.iter().filter(|c| **c != pk).map(|c| format!("{c} = excluded.{c}")).collect();
    let on_conflict =
        if updates.is_empty() { "do nothing".to_string() } else { format!("do update set {}", updates.join(", ")) };
    report.loaded = tx.execute(
        &format!("insert into {target} ({col_list}) select {col_list} from lift_tmp on conflict ({pk}) {on_conflict}"),
        &[],
    )?;
    Ok(report)
}

/// One COPY line for a document, or the reason it can't be stored.
fn row(doc: &Document, p: &Plan, ids: &Suffix, mut zero_filled: impl FnMut(&str)) -> Result<String, String> {
    let mut fields = Vec::with_capacity(p.columns.len());
    for (col, fill) in &p.columns {
        let value = match fill {
            Fill::Const(v) => Some(convert::to_pg(v, &col.udt, ids)),
            Fill::Zero(z) => {
                zero_filled(&col.name);
                Some(Ok(z.to_string()))
            }
            Fill::Path(path) => convert::lookup(doc, path).map(|v| convert::to_pg(v, &col.udt, ids)),
        };
        let value = match value {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => return Err(format!("{}: {e}", col.name)),
            None if col.nullable => None,
            None => match (matches!(fill, Fill::Path(_)), convert::zero(&col.udt)) {
                // a mapped required field missing on this document: Go would have sent its zero value
                (true, Some(z)) if p.zero_missing => {
                    zero_filled(&col.name);
                    Some(z.to_string())
                }
                _ => return Err(format!("{} is required but missing", col.name)),
            },
        };
        fields.push(convert::copy_field(value.as_deref()));
    }
    Ok(fields.join("\t") + "\n")
}

fn write_rejects(dir: &Path, collection: &str, rejects: &[Reject]) -> Result<()> {
    let path = dir.join(format!("{collection}.jsonl"));
    if rejects.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let mut out = String::new();
    for r in rejects {
        out += &serde_json::to_string(r)?;
        out.push('\n');
    }
    fs::write(&path, out).with_context(|| format!("write {}", path.display()))
}

fn doc_id(doc: &Document) -> String {
    match doc.get("_id") {
        Some(Bson::ObjectId(o)) => o.to_hex(),
        Some(other) => other.clone().into_relaxed_extjson().to_string(),
        None => "(no _id)".into(),
    }
}

pub fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
