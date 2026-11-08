//! The mapping file: which collection goes to which table, and where each column comes from.
//!
//! ```toml
//! schema = "shepherd"
//! id_suffix = "53484550"
//! go_zero_values = true
//!
//! [[table]]
//! collection = "species"
//! table = "Species"
//! [table.columns]
//! Id = "_id"
//! Name = "name"
//! CreatedAt = "meta.created_at"
//! Archived = { value = false }
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
    #[serde(rename = "table")]
    pub tables: Vec<TableMap>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableMap {
    pub collection: String,
    pub table: String,
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
