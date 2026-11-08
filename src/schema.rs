//! The target schema, read from Postgres itself so lift never needs the app's ORM models.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Result, bail};
use postgres::GenericClient;

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    /// Postgres udt name: uuid, text, int4, timestamptz, _text (array of text), ...
    pub udt: String,
    pub nullable: bool,
    /// a default, an identity, or a generated column: the database fills it
    pub has_default: bool,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub column: String,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_column: String,
}

#[derive(Debug, Default)]
pub struct Table {
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

pub fn read(db: &mut impl GenericClient, schema: &str) -> Result<HashMap<String, Table>> {
    let mut tables: HashMap<String, Table> = HashMap::new();

    for row in db.query(
        "select table_name::text, column_name::text, udt_name::text, is_nullable = 'YES',
                column_default is not null or is_identity = 'YES' or is_generated = 'ALWAYS'
         from information_schema.columns where table_schema = $1 order by table_name, ordinal_position",
        &[&schema],
    )? {
        tables.entry(row.get(0)).or_default().columns.push(Column {
            name: row.get(1),
            udt: row.get(2),
            nullable: row.get(3),
            has_default: row.get(4),
        });
    }
    if tables.is_empty() {
        bail!("schema {schema:?} has no tables");
    }

    for row in db.query(
        "select tc.table_name::text, kcu.column_name::text
         from information_schema.table_constraints tc
         join information_schema.key_column_usage kcu
           on kcu.constraint_name = tc.constraint_name and kcu.table_schema = tc.table_schema
         where tc.table_schema = $1 and tc.constraint_type = 'PRIMARY KEY'
         order by kcu.ordinal_position",
        &[&schema],
    )? {
        if let Some(t) = tables.get_mut(&row.get::<_, String>(0)) {
            t.primary_key.push(row.get(1));
        }
    }

    // single-column foreign keys only; composite ones are rare in ORM schemas
    for row in db.query(
        "select cl.relname::text, a.attname::text, rn.nspname::text, rcl.relname::text, ra.attname::text
         from pg_constraint co
         join pg_class cl on cl.oid = co.conrelid
         join pg_namespace n on n.oid = cl.relnamespace
         join pg_class rcl on rcl.oid = co.confrelid
         join pg_namespace rn on rn.oid = rcl.relnamespace
         join pg_attribute a on a.attrelid = co.conrelid and a.attnum = co.conkey[1]
         join pg_attribute ra on ra.attrelid = co.confrelid and ra.attnum = co.confkey[1]
         where co.contype = 'f' and n.nspname = $1 and array_length(co.conkey, 1) = 1",
        &[&schema],
    )? {
        if let Some(t) = tables.get_mut(&row.get::<_, String>(0)) {
            t.foreign_keys.push(ForeignKey {
                column: row.get(1),
                ref_schema: row.get(2),
                ref_table: row.get(3),
                ref_column: row.get(4),
            });
        }
    }
    Ok(tables)
}

/// Order units so each comes after the tables it references. `refs` lists the tables a unit
/// references; references to anything outside `names` don't constrain the order.
pub fn load_order(names: &[String], refs: impl Fn(&str) -> Vec<String>) -> Result<Vec<String>> {
    let wanted: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    let mut deps: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for name in names {
        let r = refs(name).into_iter().filter(|r| wanted.contains(r.as_str()) && r != name).collect();
        deps.insert(name, r);
    }

    let mut order = Vec::new();
    while !deps.is_empty() {
        let ready: Vec<&str> = deps.iter().filter(|(_, d)| d.is_empty()).map(|(n, _)| *n).collect();
        if ready.is_empty() {
            bail!("foreign keys form a cycle between: {}", deps.keys().copied().collect::<Vec<_>>().join(", "));
        }
        for n in ready {
            deps.remove(n);
            for d in deps.values_mut() {
                d.remove(n);
            }
            order.push(n.to_string());
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(pairs: &[(&str, &[&str])]) -> impl Fn(&str) -> Vec<String> {
        let map: HashMap<String, Vec<String>> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect();
        move |n| map.get(n).cloned().unwrap_or_default()
    }

    #[test]
    fn parents_load_first() {
        let r = refs(&[
            ("Species", &["Organizations"]),
            ("Organizations", &["Countries"]),
            ("Users", &["Users"]), // self-reference is fine
        ]);
        let names: Vec<String> = ["Species", "Users", "Organizations", "Countries"].map(String::from).to_vec();
        let order = load_order(&names, r).unwrap();
        let pos = |n: &str| order.iter().position(|o| o == n).unwrap();
        assert!(pos("Countries") < pos("Organizations"));
        assert!(pos("Organizations") < pos("Species"));
    }

    #[test]
    fn cycles_are_reported() {
        let r = refs(&[("A", &["B"]), ("B", &["A"])]);
        assert!(load_order(&["A".into(), "B".into()], r).is_err());
    }
}
