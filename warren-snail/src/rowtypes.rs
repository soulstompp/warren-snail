//! The row types of PostgreSQL's catalog, each composite in pg_catalog: `pg_class`, `pg_stat_activity`
//! and their kin, held as a composite whose fields are those of the release the file records
//! (`rowtypes.txt`, drawn from each release's catalog).

use crate::catalog::ColumnType;
use crate::composed::{type_of, Composed, Field};
use crate::{Error, Result};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// One definition of a row type: the releases that define it so, a bit a release from 14, and its
/// fields in order, each its name and its type as `format_type` writes it.
struct Definition {
    releases: u8,
    fields: Vec<(&'static str, &'static str)>,
}

/// Every row type's definitions over the releases, by its name.
fn all() -> &'static HashMap<&'static str, Vec<Definition>> {
    static ALL: OnceLock<HashMap<&'static str, Vec<Definition>>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: HashMap<&'static str, Vec<Definition>> = HashMap::new();
        let mut at: Option<&'static str> = None;
        for line in include_str!("rowtypes.txt").lines() {
            if let Some(header) = line.strip_prefix("= ") {
                let (name, releases) = match header.split_once(' ') {
                    Some((name, releases)) => (name, Some(releases)),
                    None => (header, None),
                };
                all.entry(name).or_default().push(Definition {
                    releases: crate::regs::releases_of(releases),
                    fields: Vec::new(),
                });
                at = Some(name);
            } else if let (Some(name), Some((field, ty))) = (at, line.split_once('\t')) {
                if let Some(d) = all.get_mut(name).and_then(|d| d.last_mut()) {
                    d.fields.push((field, ty));
                }
            }
        }
        all
    })
}

fn unqualified(name: &str) -> &str {
    name.strip_prefix("pg_catalog.").unwrap_or(name)
}

/// Whether a type's name is a row type of the catalog, of any release.
pub fn is_row_type(name: &str) -> bool {
    all().contains_key(unqualified(name))
}

/// Whether a release defines a row type.
pub fn in_release(name: &str, release: u8) -> bool {
    let bit = release
        .checked_sub(*crate::regs::RELEASES.start())
        .filter(|b| *b < 8)
        .map(|b| 1u8 << b);
    let (Some(bit), Some(d)) = (bit, all().get(unqualified(name))) else {
        return false;
    };
    d.iter().any(|d| d.releases & bit != 0)
}

/// The newest release that defines a row type.
fn newest(name: &str) -> Option<u8> {
    crate::regs::RELEASES
        .rev()
        .find(|r| in_release(name, *r))
}

/// A field's type under a release, by its name as `format_type` writes it: refused, as PostgreSQL
/// refuses a column of it, where it is a pseudo-type.
fn field_type(field: &str, text: &str, release: u8) -> Result<ColumnType> {
    let pseudo = crate::lists::all(crate::lists::Kind::Type)
        .iter()
        .any(|t| t.name == text && t.family == "pseudo-types");
    if pseudo {
        return Err(Error::new(format!(
            "column \"{field}\" has pseudo-type {text}"
        )));
    }
    if let Some(element) = text.strip_suffix("[]") {
        if is_row_type(element) {
            return Ok(type_of(Composed::Array {
                element: row_type_at(element, release)?,
                domain: None,
            }));
        }
    }
    let dt = crate::ddl::parse_type(text)?;
    crate::catalog::held_type(&dt)
        .ok()
        .or_else(|| crate::composed::resolve(&dt))
        .ok_or_else(|| Error::new(format!("the type {text} is not held")))?
        .at_release(release)
}

/// A row type under a release: its fields those the release defines. Refused where the release
/// defines no such type, and where a field holds a pseudo-type, as PostgreSQL refuses a column of it.
pub fn row_type_at(name: &str, release: u8) -> Result<ColumnType> {
    static MADE: OnceLock<RwLock<HashMap<(&'static str, u8), ColumnType>>> = OnceLock::new();
    let made = MADE.get_or_init(Default::default);
    let name = unqualified(name);
    let Some((&key, definitions)) = all().get_key_value(name) else {
        return Err(Error::new(format!("the type {name} is not held")));
    };
    if let Some(t) = made.read().unwrap().get(&(key, release)) {
        return Ok(*t);
    }
    let bit = release
        .checked_sub(*crate::regs::RELEASES.start())
        .filter(|b| *b < 8)
        .map_or(0, |b| 1u8 << b);
    let Some(d) = definitions.iter().find(|d| d.releases & bit != 0) else {
        return Err(Error::new(format!(
            "the type {name} is not in PostgreSQL {release}'s catalog, and this file's OIDs came from {release}"
        )));
    };
    let fields = d
        .fields
        .iter()
        .map(|(field, text)| Ok(Field::new(field, field_type(field, text, release)?)))
        .collect::<Result<Vec<Field>>>()?;
    let ty = type_of(Composed::Record {
        name: name.to_string(),
        fields,
    });
    made.write().unwrap().insert((key, release), ty);
    Ok(ty)
}

/// A row type as a type's name names it before the file's release is known: under the newest
/// release that defines it. None where the name is no row type.
pub fn row_type(name: &str) -> Result<Option<ColumnType>> {
    match newest(name) {
        Some(r) => row_type_at(name, r).map(Some),
        None => Ok(None),
    }
}
