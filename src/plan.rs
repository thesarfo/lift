//! Draft a `mapping.toml` by matching the dump's collections to the target schema's tables.
//!
//! Matching is name-based and best-effort: a table and a collection match if their names agree
//! up to case and a trailing "s"; a column matches a field seen in a sample of documents, by
//! exact name or by the last segment of a dotted path. A leftover table with exactly one foreign
//! key into a matched table, paired with exactly one array-of-documents field found in that
//! table's sample, becomes a child table. Anything left unmatched is a comment, not a guess.

use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use bson::{Bson, Document};

use crate::{
    dump,
    schema::{Column, Table},
};

const SAMPLE: usize = 50;

pub struct Plan {
    pub schema: String,
    pub id_suffix: String,
    pub tables: Vec<TablePlan>,
    pub unmatched_tables: Vec<String>,
    pub unmatched_collections: Vec<String>,
}

pub struct TablePlan {
    pub collection: String,
    pub table: String,
    /// column -> matched path, `None` when nothing in the sample looked right
    pub columns: Vec<(String, Option<String>)>,
    pub children: Vec<ChildPlan>,
}

pub struct ChildPlan {
    pub array: String,
    pub table: String,
    pub parent_key: String,
    pub columns: Vec<(String, Option<String>)>,
}

pub fn build(schema: &str, dump_dir: &Path, tables: &HashMap<String, Table>) -> Result<Plan> {
    let collections = dump::list(dump_dir)?;
    let mut table_names: Vec<String> = tables.keys().cloned().collect();
    table_names.sort();

    let mut claimed_collections: BTreeSet<String> = BTreeSet::new();
    let mut claimed_tables: BTreeSet<String> = BTreeSet::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for table in &table_names {
        let hit = collections.iter().find(|c| !claimed_collections.contains(c.as_str()) && names_match(table, c));
        if let Some(collection) = hit {
            pairs.push((table.clone(), collection.clone()));
            claimed_tables.insert(table.clone());
            claimed_collections.insert(collection.clone());
        }
    }

    let mut table_plans = Vec::new();
    for (table, collection) in &pairs {
        let t = &tables[table];
        let docs = sample(dump_dir, collection)?;
        let (scalars, arrays) = flatten(&docs);

        let columns = t
            .columns
            .iter()
            .filter(|c| !c.has_default)
            .map(|c| (c.name.clone(), match_source(c, t, &scalars)))
            .collect();

        let children = child_plans(table, &table_names, &mut claimed_tables, tables, &arrays, &docs, &scalars);
        table_plans.push(TablePlan { collection: collection.clone(), table: table.clone(), columns, children });
    }

    let unmatched_tables = table_names.into_iter().filter(|t| !claimed_tables.contains(t)).collect();
    let unmatched_collections = collections.into_iter().filter(|c| !claimed_collections.contains(c)).collect();

    Ok(Plan {
        schema: schema.to_string(),
        id_suffix: random_suffix(),
        tables: table_plans,
        unmatched_tables,
        unmatched_collections,
    })
}

fn sample(dump_dir: &Path, collection: &str) -> Result<Vec<Document>> {
    let Some(coll) = dump::open(dump_dir, collection)? else { return Ok(Vec::new()) };
    coll.take(SAMPLE).collect()
}

/// Top-level and one-level-nested scalar field paths, and top-level fields that hold an array
/// of documents (candidate child tables).
fn flatten(docs: &[Document]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut scalars = BTreeSet::new();
    let mut arrays = BTreeSet::new();
    for doc in docs {
        for (k, v) in doc {
            match v {
                Bson::Document(sub) => {
                    for (k2, v2) in sub {
                        if !matches!(v2, Bson::Document(_) | Bson::Array(_)) {
                            scalars.insert(format!("{k}.{k2}"));
                        }
                    }
                }
                Bson::Array(items) if items.first().is_some_and(|i| matches!(i, Bson::Document(_))) => {
                    arrays.insert(k.clone());
                }
                Bson::Array(_) => {}
                _ => {
                    scalars.insert(k.clone());
                }
            }
        }
    }
    (scalars, arrays)
}

fn match_source(column: &Column, table: &Table, scalars: &BTreeSet<String>) -> Option<String> {
    let is_the_primary_key = table.primary_key.len() == 1 && table.primary_key[0] == column.name;
    if is_the_primary_key && column.name.eq_ignore_ascii_case("id") {
        return Some("_id".to_string());
    }
    match_path(&column.name, scalars)
}

/// Case and separator don't carry meaning across the two sides: Mongo's `full_name` or
/// `farmer_address_location.residential_address` is the same name as Postgres's `FullName` or
/// an EF owned type flattened to `FarmerAddressLocation_ResidentialAddress`.
fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn match_path(name: &str, scalars: &BTreeSet<String>) -> Option<String> {
    let target = normalize(name);
    unique_match(&target, scalars, normalize)
        .or_else(|| unique_match(&target, scalars, |p| normalize(p.rsplit('.').next().unwrap_or(p))))
}

fn unique_match(target: &str, scalars: &BTreeSet<String>, key: impl Fn(&str) -> String) -> Option<String> {
    let hits: Vec<&String> = scalars.iter().filter(|p| key(p) == target).collect();
    (hits.len() == 1).then(|| hits[0].clone())
}

fn names_match(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    a == b || a.strip_suffix('s').unwrap_or(&a) == b.strip_suffix('s').unwrap_or(&b)
}

/// A leftover table with exactly one foreign key into `parent_table`, paired with the one
/// array-of-documents field found in the parent's sample, when both sides are unambiguous.
fn child_plans(
    parent_table: &str,
    all_tables: &[String],
    claimed: &mut BTreeSet<String>,
    tables: &HashMap<String, Table>,
    arrays: &BTreeSet<String>,
    parent_docs: &[Document],
    parent_scalars: &BTreeSet<String>,
) -> Vec<ChildPlan> {
    if arrays.len() != 1 {
        return Vec::new();
    }
    let array = arrays.iter().next().unwrap().clone();

    let candidates: Vec<(String, String)> = all_tables
        .iter()
        .filter(|t| !claimed.contains(t.as_str()))
        .filter_map(|t| {
            let fks: Vec<_> =
                tables[t.as_str()].foreign_keys.iter().filter(|fk| fk.ref_table == parent_table).collect();
            (fks.len() == 1).then(|| (t.clone(), fks[0].column.clone()))
        })
        .collect();
    if candidates.len() != 1 {
        return Vec::new();
    }
    let (child_table, parent_key) = candidates[0].clone();

    let elements: Vec<Document> = parent_docs
        .iter()
        .filter_map(|d| d.get_array(&array).ok())
        .flat_map(|a| a.iter())
        .filter_map(|v| v.as_document().cloned())
        .collect();
    let (elem_scalars, _) = flatten(&elements);

    let t = &tables[&child_table];
    let columns = t
        .columns
        .iter()
        .filter(|c| !c.has_default && c.name != parent_key)
        .map(|c| {
            let source = match_path(&c.name, &elem_scalars)
                .or_else(|| match_path(&c.name, parent_scalars).map(|p| format!("$parent.{p}")));
            (c.name.clone(), source)
        })
        .collect();

    claimed.insert(child_table.clone());
    vec![ChildPlan { array, table: child_table, parent_key, columns }]
}

fn random_suffix() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u32;
    format!("{:08x}", nanos ^ std::process::id())
}

pub fn render(p: &Plan) -> String {
    let mut out = String::new();
    writeln!(out, "# Drafted by `lift plan`. Review every column, especially the commented ones,").unwrap();
    writeln!(out, "# before running `lift check`.").unwrap();
    if !p.unmatched_tables.is_empty() {
        writeln!(out, "#\n# Tables with no matching collection: {}", p.unmatched_tables.join(", ")).unwrap();
    }
    if !p.unmatched_collections.is_empty() {
        writeln!(out, "# Collections with no matching table: {}", p.unmatched_collections.join(", ")).unwrap();
    }
    writeln!(out, "\nschema = {:?}", p.schema).unwrap();
    writeln!(out, "id_suffix = {:?}  # any 8 hex chars; keep it fixed once ids are in use", p.id_suffix).unwrap();

    for t in &p.tables {
        writeln!(out, "\n[[table]]").unwrap();
        writeln!(out, "collection = {:?}", t.collection).unwrap();
        writeln!(out, "table = {:?}", t.table).unwrap();
        writeln!(out, "[table.columns]").unwrap();
        render_columns(&mut out, &t.columns);

        for c in &t.children {
            writeln!(out, "\n[[table.children]]").unwrap();
            writeln!(out, "array = {:?}", c.array).unwrap();
            writeln!(out, "table = {:?}", c.table).unwrap();
            writeln!(out, "parent_key = {:?}", c.parent_key).unwrap();
            writeln!(out, "[table.children.columns]").unwrap();
            render_columns(&mut out, &c.columns);
        }
    }
    out
}

fn render_columns(out: &mut String, columns: &[(String, Option<String>)]) {
    for (name, source) in columns {
        match source {
            Some(path) => writeln!(out, "{name} = {path:?}").unwrap(),
            None => writeln!(out, "# {name}: no matching field found in the sample; map it by hand").unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;
    use crate::schema::ForeignKey;

    fn col(name: &str, has_default: bool) -> Column {
        Column { name: name.into(), udt: "text".into(), nullable: false, has_default }
    }

    #[test]
    fn flatten_finds_nested_scalars_and_document_arrays() {
        let docs =
            vec![doc! { "name": "a", "meta": { "created_at": "x" }, "tags": ["a", "b"], "items": [{ "qty": 1 }] }];
        let (scalars, arrays) = flatten(&docs);
        assert!(scalars.contains("name"));
        assert!(scalars.contains("meta.created_at"));
        assert!(!scalars.iter().any(|s| s.starts_with("tags")));
        assert_eq!(arrays, BTreeSet::from(["items".to_string()]));
    }

    #[test]
    fn column_matches_exact_name_then_unique_path_suffix() {
        let scalars = BTreeSet::from(["name".to_string(), "meta.created_at".to_string()]);
        assert_eq!(match_path("name", &scalars), Some("name".to_string()));
        assert_eq!(match_path("created_at", &scalars), Some("meta.created_at".to_string()));
        assert_eq!(match_path("missing", &scalars), None);
    }

    #[test]
    fn column_matches_across_case_and_separator_style() {
        // PascalCase Postgres columns against snake_case Mongo fields, including an EF owned
        // type flattened as Parent_Child against Mongo's nested parent.child.
        let scalars = BTreeSet::from(["full_name".to_string(), "farmer_address.residential_address".to_string()]);
        assert_eq!(match_path("FullName", &scalars), Some("full_name".to_string()));
        assert_eq!(
            match_path("FarmerAddress_ResidentialAddress", &scalars),
            Some("farmer_address.residential_address".to_string())
        );
    }

    #[test]
    fn ambiguous_path_suffix_is_left_unmatched() {
        let scalars = BTreeSet::from(["a.created_at".to_string(), "b.created_at".to_string()]);
        assert_eq!(match_path("created_at", &scalars), None);
    }

    #[test]
    fn table_and_collection_names_match_case_and_plural_insensitively() {
        assert!(names_match("Orders", "orders"));
        assert!(names_match("order", "orders"));
        assert!(!names_match("orders", "customers"));
    }

    #[test]
    fn builds_a_plan_from_schema_and_dump() {
        let dir = std::env::temp_dir().join(format!("lift-plan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, docs: &[Document]| {
            let mut bytes = Vec::new();
            for d in docs {
                d.to_writer(&mut bytes).unwrap();
            }
            std::fs::write(dir.join(format!("{name}.bson")), bytes).unwrap();
        };
        write("orders", &[doc! { "_id": "x", "status": "paid", "items": [{ "product_id": "p1", "qty": 2 }] }]);

        let mut tables = HashMap::new();
        tables.insert(
            "orders".to_string(),
            Table {
                columns: vec![col("id", false), col("status", false)],
                primary_key: vec!["id".into()],
                foreign_keys: vec![],
            },
        );
        tables.insert(
            "order_items".to_string(),
            Table {
                columns: vec![col("id", true), col("order_id", false), col("product_id", false)],
                primary_key: vec!["id".into()],
                foreign_keys: vec![ForeignKey {
                    column: "order_id".into(),
                    ref_schema: "shop".into(),
                    ref_table: "orders".into(),
                    ref_column: "id".into(),
                }],
            },
        );

        let plan = build("shop", &dir, &tables).unwrap();
        assert_eq!(plan.unmatched_tables, Vec::<String>::new());
        let orders = plan.tables.iter().find(|t| t.table == "orders").unwrap();
        assert_eq!(
            orders.columns,
            vec![("id".to_string(), Some("_id".to_string())), ("status".to_string(), Some("status".to_string()))]
        );
        let child = &orders.children[0];
        assert_eq!(child.table, "order_items");
        assert_eq!(child.parent_key, "order_id");
        assert!(child.columns.contains(&("product_id".to_string(), Some("product_id".to_string()))));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
