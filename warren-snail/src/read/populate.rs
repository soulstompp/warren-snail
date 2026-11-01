//! jsonb_populate_record and its kin: a row of a row type, a table's or a composite's, made from a
//! JSON object as jsonfuncs.c's populate_record makes it. Each field takes the object's key of its
//! name: a JSON null NULL; a string read by the field's type's input, as is any scalar's text; an
//! object into a composite field and an array into an array field made in turn; a json or jsonb
//! field the value as a document. A key the object lacks keeps the base row's field, NULL where no
//! base row is given.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::textual::Textual;
use crate::{Error, Result};

const JSON: ColumnType = ColumnType::Textual(Textual::Json);
const JSONB: ColumnType = ColumnType::Textual(Textual::Jsonb);

/// The functions giving one row of a row type a call.
pub(super) const ROWS: [&str; 2] = ["jsonb_populate_record", "json_populate_record"];

/// The functions giving a set of rows of a row type.
pub(super) const SETS: [&str; 2] = ["jsonb_populate_recordset", "json_populate_recordset"];

/// Whether a name's document is json rather than jsonb.
fn of_json(name: &str) -> bool {
    name.starts_with("json_")
}

/// The type a call returns, as PostgreSQL resolves it; none where the name is none of these. The
/// first argument's type is the row type, a row type it must be.
pub(super) fn typed(
    name: &str,
    args: &[Option<ColumnType>],
    release: u8,
) -> Result<Option<Option<ColumnType>>> {
    let valid = name == "jsonb_populate_record_valid";
    if !(ROWS.contains(&name) || SETS.contains(&name) || valid) {
        return Ok(None);
    }
    let document = if of_json(name) { JSON } else { JSONB };
    let arity = match of_json(name) {
        true => args.len() == 2 || args.len() == 3,
        false => args.len() == 2,
    };
    let taken = arity
        && args[1].is_none_or(|t| t == document)
        && args.get(2).is_none_or(|t| t.is_none_or(|t| t == ColumnType::Boolean))
        && (!valid || release >= 17);
    if !taken {
        return Err(super::typed::no_form(name, args));
    }
    match args[0] {
        None => Err(Error::new(
            "could not determine polymorphic type because input has type unknown",
        )),
        Some(t) if fields_of(t).is_some() => Ok(Some(Some(if valid {
            ColumnType::Boolean
        } else {
            t
        }))),
        Some(_) => Err(Error::new(format!(
            "first argument of {name} must be a row type"
        ))),
    }
}

/// The fields of a row type.
fn fields_of(ty: ColumnType) -> Option<&'static [Field]> {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            Composed::Record { fields, .. } => Some(fields),
            _ => None,
        },
        _ => None,
    }
}

/// A call's value; none where the name is none of the functions giving one row. Its row type is
/// its first argument's, as the typing resolved it.
pub(super) fn call(
    name: &str,
    args: &[(Value, bool)],
    types: &[Option<ColumnType>],
) -> Result<Option<Value>> {
    let valid = name == "jsonb_populate_record_valid";
    if !(ROWS.contains(&name) || valid) {
        return Ok(None);
    }
    let ty = types
        .first()
        .copied()
        .flatten()
        .or_else(|| args.first().and_then(|(v, _)| row_type(v)))
        .ok_or_else(|| Error::new(format!("{name}: its row type is not known")))?;
    Ok(Some(match valid {
        // Whether the row is made, its fields' inputs refusing none.
        true => Value::Bool(made(name, ty, args).is_ok()),
        false => made(name, ty, args)?,
    }))
}

/// The type a base row argument is of, where the typing left it to its value.
fn row_type(v: &Value) -> Option<ColumnType> {
    match v {
        Value::Composed(t, _) if fields_of(*t).is_some() => Some(*t),
        _ => None,
    }
}

/// The row a call makes: the base row where the document is NULL.
fn made(name: &str, ty: ColumnType, args: &[(Value, bool)]) -> Result<Value> {
    let base = base_of(&args[0].0);
    let doc = &args[1];
    if doc.0.is_null() {
        return Ok(match base {
            Some(_) => args[0].0.clone(),
            None => Value::Null,
        });
    }
    let doc = document(doc, of_json(name))?;
    let json = Jv::of(doc, of_json(name));
    let item = composite(ty, &json, base.as_deref())?;
    Ok(super::pipeline::composed(ty, item))
}

/// A base row's fields, where it is given and not NULL.
fn base_of(v: &Value) -> Option<Vec<Item>> {
    match v {
        Value::Composed(_, c) => match &c.item {
            Item::Record(items) => Some(items.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// A document argument's text: a json's or a jsonb's; a literal read by the type's input.
fn document(v: &(Value, bool), json: bool) -> Result<String> {
    match v {
        (Value::Typed(ColumnType::Textual(Textual::Json | Textual::Jsonb), s), _) => Ok(s.clone()),
        (Value::Text(s), _) if json => crate::jsonb::json_check(s).map(|_| s.clone()),
        (Value::Text(s), _) => crate::jsonb::jsonb_in(s),
        (other, _) => Err(Error::new(format!(
            "{} is no {}",
            super::pipeline::kind_name(other),
            if json { "json" } else { "jsonb" }
        ))),
    }
}

/// The rows of a call of a set of rows; none where the name is none of these.
pub(super) fn sets(
    name: &str,
    args: &[(Value, bool)],
    ty: Option<ColumnType>,
) -> Result<Option<Vec<Value>>> {
    if !SETS.contains(&name) {
        return Ok(None);
    }
    let ty = ty.ok_or_else(|| Error::new(format!("{name}: its row type is not known")))?;
    if args[1].0.is_null() {
        return Ok(Some(Vec::new()));
    }
    let json = of_json(name);
    let doc = Jv::of(document(&args[1], json)?, json);
    match doc.kind() {
        Kind::Array => {}
        Kind::Object if json => {
            return Err(Error::new(format!("cannot call {name} on an object")))
        }
        _ if json => return Err(Error::new(format!("cannot call {name} on a scalar"))),
        _ => return Err(Error::new(format!("cannot call {name} on a non-array"))),
    }
    let base = base_of(&args[0].0);
    doc.elements()?
        .iter()
        .map(|e| {
            if e.kind() != Kind::Object {
                return Err(Error::new(format!(
                    "argument of {name} must be an array of objects"
                )));
            }
            Ok(super::pipeline::composed(ty, composite(ty, e, base.as_deref())?))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// What a JSON value is at its top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Null,
    String,
    Object,
    Array,
    Other,
}

/// A JSON value: its text as its document writes it, and whether that document is json, written
/// as it came, rather than jsonb.
struct Jv {
    raw: String,
    json: bool,
}

impl Jv {
    fn of(raw: String, json: bool) -> Jv {
        Jv { raw, json }
    }

    fn kind(&self) -> Kind {
        match self.raw.trim_start().as_bytes().first() {
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            Some(b'"') => Kind::String,
            Some(b'n') => Kind::Null,
            _ => Kind::Other,
        }
    }

    /// An object's fields, each key once, its last value where the document writes it twice.
    fn fields(&self) -> Result<Vec<(String, Jv)>> {
        let pairs = match self.json {
            true => crate::jsonb::json_each(&self.raw)?,
            false => crate::jsonb::jsonb_each(&self.raw)?,
        };
        let mut out: Vec<(String, Jv)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let v = Jv::of(v, self.json);
            match out.iter_mut().find(|(n, _)| *n == k) {
                Some(at) => at.1 = v,
                None => out.push((k, v)),
            }
        }
        Ok(out)
    }

    /// An array's elements.
    fn elements(&self) -> Result<Vec<Jv>> {
        let all = match self.json {
            true => crate::jsonb::json_array_elements(&self.raw)?,
            false => crate::jsonb::jsonb_array_elements(&self.raw)?,
        };
        Ok(all.into_iter().map(|v| Jv::of(v, self.json)).collect())
    }

    /// The text a type's input reads it by: a string's characters, unquoted; any other value as
    /// its document writes it.
    fn text(&self) -> Result<String> {
        if self.kind() != Kind::String {
            return Ok(self.raw.trim().to_string());
        }
        let doc = format!("[{}]", self.raw);
        let text = match self.json {
            true => crate::jsonb::json_array_elements_text(&doc)?,
            false => crate::jsonb::jsonb_array_elements_text(&doc)?,
        };
        Ok(text.into_iter().next().flatten().unwrap_or_default())
    }
}

/// A row of the row type `ty` from a JSON object, a base row's fields kept where it lacks a key.
fn composite(ty: ColumnType, v: &Jv, base: Option<&[Item]>) -> Result<Item> {
    match v.kind() {
        Kind::Object => {}
        Kind::Array => return Err(Error::new("cannot call populate_composite on an array")),
        _ => return Err(Error::new("cannot call populate_composite on a scalar")),
    }
    let fields = fields_of(ty).expect("a row type");
    if let Some(f) = fields.iter().find(|f| f.domain.is_some()) {
        return Err(Error::new(format!(
            "{}: a row of a field of a domain ({}) is not made",
            ty.pg_name(),
            f.name
        )));
    }
    let given = v.fields()?;
    let mut items = Vec::with_capacity(fields.len());
    for (i, f) in fields.iter().enumerate() {
        items.push(match given.iter().find(|(k, _)| *k == f.name) {
            Some((_, v)) => field(f.ty, v)?,
            None => base.map_or(Item::Null, |b| b[i].clone()),
        });
    }
    Ok(Item::Record(items))
}

/// A field of type `ty` from a JSON value.
fn field(ty: ColumnType, v: &Jv) -> Result<Item> {
    let kind = v.kind();
    if kind == Kind::Null {
        return Ok(Item::Null);
    }
    let composed = match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    };
    // A string into an array or a composite is read by its type's input, as any scalar is.
    match composed {
        Some(Composed::Record { .. }) if kind != Kind::String => composite(ty, v, None),
        Some(Composed::Array { element, .. }) if kind != Kind::String => {
            if kind != Kind::Array {
                return Err(Error::new("expected JSON array"));
            }
            let elements = v.elements()?;
            if elements.iter().any(|e| e.kind() == Kind::Array) {
                return Err(Error::new(format!(
                    "{}: an array of more than one dimension made from JSON is not read",
                    ty.pg_name()
                )));
            }
            let n = elements.len() as i32;
            Ok(Item::Array {
                elements: elements
                    .iter()
                    .map(|e| field(*element, e))
                    .collect::<Result<Vec<_>>>()?,
                dims: if n == 0 { Vec::new() } else { vec![n] },
                lower: if n == 0 { Vec::new() } else { vec![1] },
            })
        }
        // A document field takes the value as a document: from json as written, a string written
        // again as a JSON string.
        _ if ty == JSON || ty == JSONB => {
            let text = match (v.json, kind) {
                (true, Kind::String) => {
                    let mut out = String::new();
                    crate::jsonb::escape_json(&mut out, &v.text()?);
                    out
                }
                _ => v.raw.trim().to_string(),
            };
            super::pipeline::part_of(ty, Value::Text(text), true)
        }
        _ => super::pipeline::part_of(ty, Value::Text(v.text()?), true),
    }
}
