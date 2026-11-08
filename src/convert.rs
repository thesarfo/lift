//! BSON values to Postgres COPY text, driven by the target column's type.

use bson::{Bson, Document, spec::BinarySubtype};

use crate::ids::{NIL, Suffix};

/// Follow a dotted path ("meta.created_at"). Missing and null both come back as None.
pub fn lookup<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut parts = path.split('.');
    let mut cur = doc.get(parts.next()?)?;
    for part in parts {
        cur = match cur {
            Bson::Document(d) => d.get(part)?,
            Bson::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    (!matches!(cur, Bson::Null | Bson::Undefined)).then_some(cur)
}

/// Like `lookup`, from any value: "$value" is the value itself, other paths need a document.
pub fn lookup_in<'a>(root: &'a Bson, path: &str) -> Option<&'a Bson> {
    match (path, root) {
        ("$value", Bson::Null | Bson::Undefined) => None,
        ("$value", v) => Some(v),
        (_, Bson::Document(d)) => lookup(d, path),
        _ => None,
    }
}

/// The value as Postgres text input for a column of type `udt`. `_foo` is an array of `foo`.
pub fn to_pg(v: &Bson, udt: &str, ids: &Suffix) -> Result<String, String> {
    if let Some(base) = udt.strip_prefix('_') {
        let Bson::Array(items) = v else {
            return Err(format!("expected an array for {udt}, got {}", kind(v)));
        };
        let elems = items
            .iter()
            .map(|item| match item {
                Bson::Null => Ok("NULL".to_string()),
                _ => to_pg(item, base, ids).map(|s| quote_array_elem(&s)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(format!("{{{}}}", elems.join(",")));
    }

    let bad = || Err(format!("can't store {} in a {udt} column", kind(v)));
    match udt {
        "uuid" => match v {
            Bson::ObjectId(oid) => Ok(ids.uuid(oid)),
            Bson::String(s) => ids.uuid_from_str(s).ok_or_else(|| format!("{s:?} is not an ObjectId or UUID")),
            Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => {
                let h: String = b.bytes.iter().map(|x| format!("{x:02x}")).collect();
                Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
            }
            _ => bad(),
        },
        "text" | "varchar" | "bpchar" | "citext" | "name" => match v {
            Bson::String(s) => Ok(s.clone()),
            Bson::ObjectId(oid) => Ok(oid.to_hex()),
            Bson::Int32(n) => Ok(n.to_string()),
            Bson::Int64(n) => Ok(n.to_string()),
            Bson::Double(n) => Ok(n.to_string()),
            Bson::Boolean(b) => Ok(b.to_string()),
            Bson::DateTime(d) => d.try_to_rfc3339_string().map_err(|e| e.to_string()),
            Bson::Document(_) | Bson::Array(_) => Ok(v.clone().into_relaxed_extjson().to_string()),
            _ => bad(),
        },
        "int2" | "int4" | "int8" => match v {
            Bson::Int32(n) => Ok(n.to_string()),
            Bson::Int64(n) => Ok(n.to_string()),
            Bson::Double(n) if n.fract() == 0.0 => Ok(format!("{n:.0}")),
            Bson::String(s) if s.trim().parse::<i64>().is_ok() => Ok(s.trim().to_string()),
            _ => bad(),
        },
        "float4" | "float8" | "numeric" => match v {
            Bson::Int32(n) => Ok(n.to_string()),
            Bson::Int64(n) => Ok(n.to_string()),
            Bson::Double(n) if n.is_finite() => Ok(n.to_string()),
            Bson::Decimal128(d) => Ok(d.to_string()),
            Bson::String(s) if s.trim().parse::<f64>().is_ok() => Ok(s.trim().to_string()),
            _ => bad(),
        },
        "bool" => match v {
            Bson::Boolean(b) => Ok(b.to_string()),
            Bson::String(s) if s == "true" || s == "false" => Ok(s.clone()),
            _ => bad(),
        },
        "timestamptz" | "timestamp" => match v {
            Bson::DateTime(d) => d.try_to_rfc3339_string().map_err(|e| e.to_string()),
            Bson::String(s) => Ok(s.clone()),
            _ => bad(),
        },
        "date" => match v {
            Bson::DateTime(d) => d.try_to_rfc3339_string().map(|s| s[..10].to_string()).map_err(|e| e.to_string()),
            Bson::String(s) => Ok(s.clone()),
            _ => bad(),
        },
        "json" | "jsonb" => Ok(v.clone().into_relaxed_extjson().to_string()),
        _ => Err(format!("lift doesn't handle {udt} columns yet")),
    }
}

/// Go's "unset" id. In a nullable column it's stored as NULL, which is what the C# side means.
pub fn is_zero_oid(v: &Bson) -> bool {
    matches!(v, Bson::ObjectId(o) if o.bytes() == [0; 12])
}

/// Go's zero value for a column type, used when go_zero_values is on and the field is missing.
pub fn zero(udt: &str) -> Option<&'static str> {
    if udt.starts_with('_') {
        return Some("{}");
    }
    Some(match udt {
        "uuid" => NIL,
        "text" | "varchar" | "bpchar" | "citext" => "",
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" => "0",
        "bool" => "false",
        "timestamptz" | "timestamp" => "0001-01-01 00:00:00+00",
        "date" => "0001-01-01",
        "json" | "jsonb" => "null",
        _ => return None,
    })
}

pub fn from_toml(v: &toml::Value) -> Bson {
    match v {
        toml::Value::String(s) => Bson::String(s.clone()),
        toml::Value::Integer(n) => Bson::Int64(*n),
        toml::Value::Float(n) => Bson::Double(*n),
        toml::Value::Boolean(b) => Bson::Boolean(*b),
        toml::Value::Datetime(d) => Bson::String(d.to_string()),
        toml::Value::Array(a) => Bson::Array(a.iter().map(from_toml).collect()),
        toml::Value::Table(t) => Bson::Document(t.iter().map(|(k, v)| (k.clone(), from_toml(v))).collect()),
    }
}

/// Escape one field for COPY's text format. None is SQL NULL.
pub fn copy_field(v: Option<&str>) -> String {
    match v {
        None => "\\N".into(),
        Some(s) => s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t"),
    }
}

fn quote_array_elem(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn kind(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "a double",
        Bson::String(_) => "a string",
        Bson::Document(_) => "a sub-document",
        Bson::Array(_) => "an array",
        Bson::Boolean(_) => "a bool",
        Bson::Int32(_) | Bson::Int64(_) => "an integer",
        Bson::ObjectId(_) => "an ObjectId",
        Bson::DateTime(_) => "a date",
        Bson::Binary(_) => "binary",
        Bson::Decimal128(_) => "a decimal",
        _ => "an unsupported bson value",
    }
}

#[cfg(test)]
mod tests {
    use bson::{doc, oid::ObjectId};

    use super::*;

    fn ids() -> Suffix {
        Suffix::parse("53484550").unwrap()
    }

    #[test]
    fn dotted_lookup() {
        let d = doc! { "meta": { "created_at": "x" }, "tags": ["a", "b"], "gone": null };
        assert_eq!(lookup(&d, "meta.created_at"), Some(&Bson::String("x".into())));
        assert_eq!(lookup(&d, "tags.1"), Some(&Bson::String("b".into())));
        assert_eq!(lookup(&d, "gone"), None);
        assert_eq!(lookup(&d, "meta.nope"), None);
    }

    #[test]
    fn object_ids_into_uuid_and_text() {
        let oid = Bson::ObjectId(ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap());
        assert_eq!(to_pg(&oid, "uuid", &ids()).unwrap(), "507f1f77-bcf8-6cd7-9943-901153484550");
        // text columns that reference string user ids keep the hex, like the API now mints
        assert_eq!(to_pg(&oid, "text", &ids()).unwrap(), "507f1f77bcf86cd799439011");
    }

    #[test]
    fn numbers_drift_but_still_fit() {
        assert_eq!(to_pg(&Bson::String(" 12 ".into()), "int4", &ids()).unwrap(), "12");
        assert_eq!(to_pg(&Bson::Double(3.0), "int4", &ids()).unwrap(), "3");
        assert!(to_pg(&Bson::Double(3.5), "int4", &ids()).is_err());
        assert!(to_pg(&Bson::String("twelve".into()), "int4", &ids()).is_err());
    }

    #[test]
    fn arrays_become_pg_literals() {
        let v = Bson::Array(vec!["cattle".into(), "say \"hi\"".into(), Bson::Null]);
        assert_eq!(to_pg(&v, "_text", &ids()).unwrap(), r#"{"cattle","say \"hi\"",NULL}"#);
    }

    #[test]
    fn copy_escaping() {
        assert_eq!(copy_field(Some("a\tb\nc\\d")), "a\\tb\\nc\\\\d");
        assert_eq!(copy_field(None), "\\N");
    }

    #[test]
    fn zero_values() {
        assert_eq!(zero("_text"), Some("{}"));
        assert_eq!(zero("int4"), Some("0"));
        assert_eq!(zero("tsvector"), None);
    }
}
