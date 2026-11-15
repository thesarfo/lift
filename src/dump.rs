//! Stream documents out of a mongodump `<collection>.bson` file, one at a time.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bson::Document;

pub struct Collection {
    reader: BufReader<File>,
    path: PathBuf,
}

/// `dir` may be the database folder (`dump/shop`) or the dump root (`dump`) holding one.
/// None when the dump has no such collection: mongodump writes nothing for an empty or
/// never-created collection, so that's treated as zero documents.
pub fn open(dir: &Path, collection: &str) -> Result<Option<Collection>> {
    let Some(path) = find(dir, collection)? else { return Ok(None) };
    let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    Ok(Some(Collection { reader: BufReader::with_capacity(1 << 20, file), path }))
}

/// Collection names available under `dir` (the database folder or the dump root holding one).
pub fn list(dir: &Path) -> Result<Vec<String>> {
    let direct = bson_stems(dir)?;
    if !direct.is_empty() {
        return Ok(sorted(direct));
    }
    let mut hits: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?.flatten() {
        if entry.path().is_dir() {
            let names = bson_stems(&entry.path())?;
            if !names.is_empty() {
                hits.push((entry.path(), names));
            }
        }
    }
    match hits.len() {
        1 => Ok(sorted(hits.remove(0).1)),
        0 => Ok(Vec::new()),
        _ => bail!("collections found under several databases in {}; point DUMP at one", dir.display()),
    }
}

fn bson_stems(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("bson")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_string());
        }
    }
    Ok(names)
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn find(dir: &Path, collection: &str) -> Result<Option<PathBuf>> {
    let name = format!("{collection}.bson");
    let direct = dir.join(&name);
    if direct.is_file() {
        return Ok(Some(direct));
    }
    if dir.join(format!("{name}.gz")).is_file() {
        bail!("{collection} is gzipped; run mongodump without --gzip");
    }
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?.flatten() {
        let candidate = entry.path().join(&name);
        if candidate.is_file() {
            hits.push(candidate);
        }
    }
    match hits.len() {
        1 => Ok(Some(hits.remove(0))),
        0 => Ok(None),
        _ => bail!("{name} found in several databases under {}; point --dump at one", dir.display()),
    }
}

impl Iterator for Collection {
    type Item = Result<Document>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.reader.fill_buf() {
            Ok([]) => None,
            Ok(_) => Some(
                Document::from_reader(&mut self.reader).with_context(|| format!("bad bson in {}", self.path.display())),
            ),
            Err(e) => Some(Err(e.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lift-dump-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lists_collections_in_the_db_folder_directly() {
        let dir = scratch("direct");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("orders.bson"), b"").unwrap();
        std::fs::write(dir.join("customers.bson"), b"").unwrap();
        assert_eq!(list(&dir).unwrap(), ["customers", "orders"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_collections_from_the_one_db_folder_under_the_dump_root() {
        let dir = scratch("root");
        std::fs::create_dir_all(dir.join("shop")).unwrap();
        std::fs::write(dir.join("shop").join("orders.bson"), b"").unwrap();
        assert_eq!(list(&dir).unwrap(), ["orders"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ambiguous_root_with_several_db_folders_is_an_error() {
        let dir = scratch("ambiguous");
        std::fs::create_dir_all(dir.join("shop")).unwrap();
        std::fs::write(dir.join("shop").join("orders.bson"), b"").unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("other").join("orders.bson"), b"").unwrap();
        assert!(list(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
