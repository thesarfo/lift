//! After a run: is every document from the dump in its table, and what's in the table that
//! didn't come from the dump? Child tables are checked by count per parent.

use anyhow::{Result, bail};
use bson::Bson;
use postgres::Client;

use crate::{
    convert, dump,
    ids::Suffix,
    load::ident,
    mapping::{Mapping, Source},
    schema,
};

pub struct Check {
    pub table: String,
    /// documents, or array elements for a child table
    pub docs: usize,
    pub present: i64,
    /// keys of missing rows (top-level tables only; child rows have no key of their own)
    pub missing: Vec<String>,
    /// rows not from the dump (seed data, rows created after the dump)
    pub extra: i64,
}

pub fn run(db: &mut Client, m: &Mapping, dump_dir: &std::path::Path) -> Result<Vec<Check>> {
    let ids = Suffix::parse(&m.id_suffix)?;
    let tables = schema::read(db, &m.schema)?;
    let mut checks = Vec::new();

    for tm in &m.tables {
        let Some(table) = tables.get(&tm.table) else { bail!("no table {}", tm.table) };
        let [pk] = table.primary_key.as_slice() else { bail!("{}: needs a single-column primary key", tm.table) };
        let Some(Source::Path(pk_path)) = tm.columns.get(pk) else { bail!("{}: primary key must map to a path", tm.table) };
        let udt = &table.column(pk).expect("pk column").udt;

        // parent keys, and how many elements each child array has
        let mut keys = Vec::new();
        let mut child_counts = vec![0usize; tm.children.len()];
        for doc in dump::open(dump_dir, &tm.collection)? {
            let doc = doc?;
            let Some(Ok(k)) = convert::lookup(&doc, pk_path).map(|v| convert::to_pg(v, udt, &ids)) else { continue };
            keys.push(k);
            for (n, c) in child_counts.iter_mut().zip(&tm.children) {
                if let Some(Bson::Array(a)) = convert::lookup(&doc, &c.array) {
                    *n += a.len();
                }
            }
        }

        let target = format!("{}.{}", ident(&m.schema), ident(&tm.table));
        let col = ident(pk);
        let count = |db: &mut Client, sql: String| -> Result<i64> { Ok(db.query_one(&sql, &[&keys])?.get(0)) };
        let present = count(db, format!("select count(*) from {target} where {col}::text = any($1)"))?;
        let extra = count(db, format!("select count(*) from {target} where not ({col}::text = any($1))"))?;
        let missing = db
            .query(
                &format!("select k from unnest($1::text[]) k where not exists (select 1 from {target} where {col}::text = k) limit 10"),
                &[&keys],
            )?
            .iter()
            .map(|r| r.get(0))
            .collect();
        checks.push(Check { table: tm.table.clone(), docs: keys.len(), present, missing, extra });

        for (c, expected) in tm.children.iter().zip(child_counts) {
            let ctarget = format!("{}.{}", ident(&m.schema), ident(&c.table));
            let pkey = ident(&c.parent_key);
            let present = count(db, format!("select count(*) from {ctarget} where {pkey}::text = any($1)"))?;
            let extra = count(db, format!("select count(*) from {ctarget} where not ({pkey}::text = any($1))"))?;
            checks.push(Check { table: c.table.clone(), docs: expected, present, missing: vec![], extra });
        }
    }
    Ok(checks)
}
