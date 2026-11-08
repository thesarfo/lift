//! After a run: is every document from the dump in its table, and what's in the table that
//! didn't come from the dump?

use anyhow::{Result, bail};
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
    pub docs: usize,
    pub present: i64,
    pub missing: Vec<String>,
    /// rows in the table whose key isn't in the dump (seed data, rows created after the dump)
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

        let mut keys = Vec::new();
        for doc in dump::open(dump_dir, &tm.collection)? {
            let doc = doc?;
            if let Some(Ok(k)) = convert::lookup(&doc, pk_path).map(|v| convert::to_pg(v, udt, &ids)) {
                keys.push(k);
            }
        }

        let target = format!("{}.{}", ident(&m.schema), ident(&tm.table));
        let col = ident(pk);
        let present: i64 =
            db.query_one(&format!("select count(*) from {target} where {col}::text = any($1)"), &[&keys])?.get(0);
        let extra: i64 =
            db.query_one(&format!("select count(*) from {target} where not ({col}::text = any($1))"), &[&keys])?.get(0);
        let missing = db
            .query(
                &format!("select k from unnest($1::text[]) k where not exists (select 1 from {target} where {col}::text = k) limit 10"),
                &[&keys],
            )?
            .iter()
            .map(|r| r.get(0))
            .collect();

        checks.push(Check { table: tm.table.clone(), docs: keys.len(), present, missing, extra });
    }
    Ok(checks)
}
