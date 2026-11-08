//! The mapping file: which collection goes to which table, and where each column comes from.
//!
//! ```toml
//! schema = "shop"
//! id_suffix = "6c696674"
//!
//! [[table]]
//! collection = "orders"
//! table = "orders"
//! [table.columns]
//! id = "_id"
//! status = "status"
//! placed_at = "meta.created_at"
//! source = { value = "mongo" }
//!
//! [[table.children]]
//! array = "items"
//! table = "order_items"
//! parent_key = "order_id"
//! [table.children.columns]
//! quantity = "qty"
//! ```

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub schema: String,
    pub id_suffix: String,
    /// Fill NOT NULL columns the document doesn't have with Go's zero value for the type
    /// (0, false, "", empty array, 0001-01-01). That's what the Go API returned for them anyway.
    #[serde(default)]
    pub go_zero_values: bool,
    /// SQL run inside the load transaction before any table loads, e.g. clearing seed rows
    /// that clash with the dump.
    #[serde(default)]
    pub before: Vec<String>,
    /// SQL run after every table loads, before commit, e.g. filling derived columns.
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(rename = "table")]
    pub tables: Vec<TableMap>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableMap {
    pub collection: String,
    pub table: String,
    pub columns: BTreeMap<String, Source>,
    #[serde(default)]
    pub children: Vec<ChildMap>,
}

/// An embedded array that becomes rows in its own table, one row per element.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildMap {
    /// Path to the array in the parent document.
    pub array: String,
    pub table: String,
    /// Column in the child table that gets the parent row's primary key.
    pub parent_key: String,
    /// Paths are relative to the array element. "$value" is the element itself,
    /// "$parent.x" reads from the parent document.
    pub columns: BTreeMap<String, Source>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Source {
    /// A dotted path into the document, e.g. "meta.created_at".
    Path(String),
    /// The same value for every row.
    Value { value: toml::Value },
}

pub fn load(path: &Path) -> Result<Mapping> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_mapping_parses() {
        let m: Mapping = toml::from_str(include_str!("../examples/shop.toml")).unwrap();
        assert_eq!(m.tables.len(), 3);
        assert_eq!(m.tables[2].children[0].parent_key, "order_id");
        assert_eq!((m.before.len(), m.after.len()), (1, 1));
    }
}
