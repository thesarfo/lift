//! After a run: does the database hold exactly what the dump says it should?
//!
//! Every document is converted again with the loader's own code and staged in a temp table typed
//! like the target, then Postgres compares the two, so timestamps, numbers, arrays and json are
//! compared as values, not as text. Parent tables are compared row by row on the primary key.
//! Child tables have generated keys, so they're compared as a set of rows per parent. Nothing is
//! written: the whole check runs in a transaction that is rolled back.

use anyhow::Result;
use postgres::{Client, Transaction};

use crate::{
    ids::Suffix,
    load::{self, Child, Report, Unit, ident, mapped_fks, target},
    mapping::Mapping,
    schema,
};

#[derive(Debug, Default)]
pub struct Check {
    pub table: String,
    /// documents, or array elements for a child table
    pub docs: usize,
    /// documents that can't be stored (the reasons are in lift-rejects from the last run)
    pub rejected: usize,
    /// rows the table should hold
    pub expected: i64,
    pub missing: i64,
    /// rows present but with different values (parents differing, for a child table)
    pub changed: i64,
    pub changed_columns: Vec<(String, i64)>,
    /// up to 10 examples: source ids of missing or changed rows
    pub samples: Vec<String>,
    /// rows in the table that didn't come from the dump (seed data, rows created since)
    pub extra: i64,
    /// md5 over the migrated rows as stored; record it at sign-off to prove nothing moved since
    pub fingerprint: String,
    pub child: bool,
    pub not_in_dump: bool,
    /// reject reasons, most common first
    pub reasons: Vec<(String, usize)>,
    /// column -> rows filled with Go's zero value / rows where a zero ObjectId became NULL
    pub zero_filled: Vec<(String, usize)>,
    pub zero_ids: Vec<(String, usize)>,
}

impl Check {
    pub fn ok(&self) -> bool {
        self.missing == 0 && self.changed == 0
    }
}

pub fn run(db: &mut Client, m: &Mapping, dump_dir: &std::path::Path) -> Result<Vec<Check>> {
    let ids = Suffix::parse(&m.id_suffix)?;
    let tables = schema::read(db, &m.schema)?;
    let mut units = load::plan(m, &tables)?;
    let order = load::order_units(&mut units)?;

    let mut tx = db.transaction()?;
    let mut checks = Vec::new();
    for name in &order {
        let u = &units[name];
        let staged = load::stage_table(&mut tx, &m.schema, u, &ids, dump_dir)?;
        checks.push(check_table(&mut tx, &m.schema, u, &staged)?);
        for c in &u.children {
            let (staged, keys) = load::stage_children(&mut tx, &m.schema, u, c, &ids, dump_dir)?;
            checks.push(check_children(&mut tx, &m.schema, c, &staged, &keys)?);
        }
    }
    tx.rollback()?;
    Ok(checks)
}

fn check_table(tx: &mut Transaction, schema: &str, u: &Unit, staged: &Report) -> Result<Check> {
    let p = &u.plan;
    let mut check = base(staged, false);
    if staged.missing {
        return Ok(check);
    }
    let t = target(schema, p);
    let pk = ident(&u.pk);

    // deferred references that point at nothing were left NULL by the load; expect the same
    for (col, fk) in mapped_fks(p).filter(|(c, _)| p.deferred.contains(&c.name)) {
        let c = ident(&col.name);
        tx.execute(
            &format!(
                "update lift_tmp e set {c} = null where {c} is not null
                 and not exists (select 1 from {}.{} r where r.{} = e.{c})",
                ident(&fk.ref_schema),
                ident(&fk.ref_table),
                ident(&fk.ref_column)
            ),
            &[],
        )?;
    }

    let cols: Vec<String> = p.columns.iter().map(|(c, _)| ident(&c.name)).collect();
    let row_e = format!("row({})", cols.iter().map(|c| format!("e.{c}")).collect::<Vec<_>>().join(", "));
    let row_t = format!("row({})", cols.iter().map(|c| format!("t.{c}")).collect::<Vec<_>>().join(", "));
    let join = format!("lift_tmp e join {t} t on t.{pk} = e.{pk}");
    let absent = format!("not exists (select 1 from {t} t where t.{pk} = e.{pk})");

    check.expected = tx.query_one("select count(*) from lift_tmp", &[])?.get(0);
    check.missing = tx.query_one(&format!("select count(*) from lift_tmp e where {absent}"), &[])?.get(0);
    check.extra = tx
        .query_one(&format!("select count(*) from {t} t where not exists (select 1 from lift_tmp e where e.{pk} = t.{pk})"), &[])?
        .get(0);

    // per-column differences on rows that are present
    let per_col: Vec<String> = cols.iter().map(|c| format!("count(*) filter (where e.{c} is distinct from t.{c})")).collect();
    let row = tx.query_one(
        &format!("select count(*) filter (where {row_e} is distinct from {row_t}), {} from {join}", per_col.join(", ")),
        &[],
    )?;
    check.changed = row.get(0);
    for (i, (col, _)) in p.columns.iter().enumerate() {
        let n: i64 = row.get(i + 1);
        if n > 0 {
            check.changed_columns.push((col.name.clone(), n));
        }
    }

    let samples = tx.query(
        &format!(
            "(select 'missing ' || e.lift_ref from lift_tmp e where {absent} limit 5)
             union all
             (select 'changed ' || e.lift_ref from {join} where {row_e} is distinct from {row_t} limit 5)"
        ),
        &[],
    )?;
    check.samples = samples.iter().map(|r| r.get(0)).collect();

    check.fingerprint = tx
        .query_one(
            &format!("select coalesce(md5(string_agg(r, '|' order by r)), '') from (select {row_t}::text r from {join}) x"),
            &[],
        )?
        .get(0);
    Ok(check)
}

fn check_children(tx: &mut Transaction, schema: &str, c: &Child, staged: &Report, keys: &[String]) -> Result<Check> {
    let p = &c.plan;
    let mut check = base(staged, true);
    if staged.missing {
        return Ok(check);
    }
    let t = target(schema, p);
    let pkey = ident(c.parent_key);
    let cols = p.columns.iter().map(|(c, _)| ident(&c.name)).collect::<Vec<_>>().join(", ");
    let per_parent = |from: &str| {
        format!(
            "select {pkey} k, count(*) n, md5(string_agg(r, '|' order by r)) h
             from (select {pkey}, row({cols})::text r from {from}) x group by 1"
        )
    };
    let stored = format!("{t} where {pkey}::text = any($1)");

    check.expected = tx.query_one("select count(*) from lift_tmp", &[])?.get(0);
    let present: i64 = tx.query_one(&format!("select count(*) from {stored}"), &[&keys])?.get(0);
    check.extra = tx.query_one(&format!("select count(*) from {t} where not ({pkey}::text = any($1))"), &[&keys])?.get(0);

    let differing = tx.query(
        &format!(
            "with e as ({}), t as ({})
             select coalesce(e.k, t.k)::text, coalesce(e.n, 0), coalesce(t.n, 0)
             from e full join t on e.k = t.k where e.h is distinct from t.h",
            per_parent("lift_tmp"),
            per_parent(&stored)
        ),
        &[&keys],
    )?;
    check.changed = differing.len() as i64;
    check.missing = (check.expected - present).max(0);
    check.samples = differing
        .iter()
        .take(10)
        .map(|r| format!("parent {}: expected {} rows, found {}", r.get::<_, String>(0), r.get::<_, i64>(1), r.get::<_, i64>(2)))
        .collect();
    check.fingerprint = tx
        .query_one(
            &format!("select coalesce(md5(string_agg(r, '|' order by r)), '') from (select row({cols})::text r from {stored}) x"),
            &[&keys],
        )?
        .get(0);
    Ok(check)
}

fn base(staged: &Report, child: bool) -> Check {
    let mut reasons: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in &staged.rejects {
        *reasons.entry(r.reason.as_str()).or_default() += 1;
    }
    let mut reasons: Vec<(String, usize)> = reasons.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let list = |m: &std::collections::BTreeMap<String, usize>| m.iter().map(|(k, v)| (k.clone(), *v)).collect();
    Check {
        table: staged.table.clone(),
        docs: staged.docs,
        rejected: staged.rejects.len(),
        child,
        not_in_dump: staged.missing,
        reasons,
        zero_filled: list(&staged.counts.zero_filled),
        zero_ids: list(&staged.counts.zero_ids),
        ..Default::default()
    }
}
