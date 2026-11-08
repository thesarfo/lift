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
    Ok(Some(Collection { reader: BufReader::new(file), path }))
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
            Ok(_) => Some(Document::from_reader(&mut self.reader).with_context(|| format!("bad bson in {}", self.path.display()))),
            Err(e) => Some(Err(e.into())),
        }
    }
}
