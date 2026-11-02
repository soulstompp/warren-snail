//! The functions of json and jsonb a pipeline answers by name, over `crate::jsonb`: a value given
//! to the builders sorted by its type as `json_categorize_type` sorts it, a strict function NULL
//! where an argument is, and the aggregates folded a row at a time.

use super::pipeline::{composed, part_value, Value};
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::jsonb::{self as j, JsonValue};
use crate::read::statement::Datum;
use crate::textual::Textual;
use crate::{Error, Result};

const JSON: ColumnType = ColumnType::Textual(Textual::Json);
const JSONB: ColumnType = ColumnType::Textual(Textual::Jsonb);
const TEXT: ColumnType = ColumnType::Text(None);

/// The aggregates of json and jsonb.
pub(super) const AGGREGATES: [&str; 12] = [
    "json_agg",
    "json_agg_strict",
    "jsonb_agg",
    "jsonb_agg_strict",
    "json_object_agg",
    "json_object_agg_strict",
    "json_object_agg_unique",
    "json_object_agg_unique_strict",
    "jsonb_object_agg",
    "jsonb_object_agg_strict",
    "jsonb_object_agg_unique",
    "jsonb_object_agg_unique_strict",
];

/// The set-returning functions of json and jsonb.
pub(super) const SETS: [&str; 12] = [
    "jsonb_path_query",
    "jsonb_path_query_tz",
    "json_object_keys",
    "jsonb_object_keys",
    "json_each",
    "json_each_text",
    "jsonb_each",
    "jsonb_each_text",
    "json_array_elements",
    "json_array_elements_text",
    "jsonb_array_elements",
    "jsonb_array_elements_text",
];

/// The record type of each row json_each and its kind give.
fn pair(value: ColumnType) -> ColumnType {
    crate::composed::type_of(Composed::Record {
        name: "record".into(),
        fields: vec![Field::new("key", TEXT), Field::new("value", value)],
    })
}

/// The type a function of json returns over its arguments' types, or its refusal where it has no
/// form for them; none where the name is not one of them.
pub(super) fn typed(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    let ty = match name {
        "to_json"
        | "array_to_json"
        | "row_to_json"
        | "json_build_array"
        | "json_build_object"
        | "json_object"
        | "json_strip_nulls"
        | "json_array_elements" => JSON,
        "to_jsonb"
        | "jsonb_build_array"
        | "jsonb_build_object"
        | "jsonb_object"
        | "jsonb_set"
        | "jsonb_set_lax"
        | "jsonb_insert"
        | "jsonb_strip_nulls"
        | "jsonb_array_elements" => JSONB,
        "json_typeof"
        | "jsonb_typeof"
        | "jsonb_pretty"
        | "json_object_keys"
        | "jsonb_object_keys"
        | "json_array_elements_text"
        | "jsonb_array_elements_text" => TEXT,
        "json_array_length" | "jsonb_array_length" => ColumnType::Int4,
        "jsonb_path_exists"
        | "jsonb_path_exists_tz"
        | "jsonb_path_match"
        | "jsonb_path_match_tz" => ColumnType::Boolean,
        "jsonb_path_query"
        | "jsonb_path_query_tz"
        | "jsonb_path_query_array"
        | "jsonb_path_query_array_tz"
        | "jsonb_path_query_first"
        | "jsonb_path_query_first_tz" => JSONB,
        "json_each" => pair(JSON),
        "jsonb_each" => pair(JSONB),
        "json_each_text" | "jsonb_each_text" => pair(TEXT),
        n if AGGREGATES.contains(&n) => {
            if n.starts_with("jsonb") {
                JSONB
            } else {
                JSON
            }
        }
        _ => return Ok(None),
    };
    // A document argument of another type has no form.
    let document = match name {
        n if n.starts_with("jsonb_")
            && !n.starts_with("jsonb_build")
            && !n.contains("_agg")
            && n != "jsonb_object" =>
        {
            Some(JSONB)
        }
        "json_typeof"
        | "json_array_length"
        | "json_strip_nulls"
        | "json_object_keys"
        | "json_each"
        | "json_each_text"
        | "json_array_elements"
        | "json_array_elements_text" => Some(JSON),
        _ => None,
    };
    if let (Some(d), Some(Some(t))) = (document, args.first()) {
        if *t != d {
            return Err(super::typed::no_form(name, args));
        }
    }
    Ok(Some(Some(ty)))
}

/// A value as the builders take it, sorted by its type as `json_categorize_type` sorts it.
pub(super) fn json_value(v: &Value) -> JsonValue {
    match v {
        v if v.is_null() => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Int(_) | Value::Num(..) | Value::Numeric(_) | Value::Real(_) | Value::Double(_) => {
            JsonValue::Number(v.text().unwrap_or_default())
        }
        Value::Scalar(ColumnType::Date, k) => JsonValue::Date(*k as i32),
        Value::Scalar(ColumnType::Timestamp(_), k) => JsonValue::Timestamp(*k),
        Value::Scalar(ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_), k) => {
            JsonValue::TimestampTz(*k)
        }
        Value::Typed(ColumnType::Textual(Textual::Json), s) => JsonValue::Json(s.clone()),
        Value::Typed(ColumnType::Textual(Textual::Jsonb), s) => JsonValue::Jsonb(s.clone()),
        Value::Composed(ty, c) => match (ty, &c.item) {
            (ColumnType::Composed(r), Item::Array { elements, dims, .. }) => {
                let (Composed::Array { element, .. } | Composed::Vector { element }) = r.def() else {
                    return JsonValue::Other(v.text().unwrap_or_default());
                };
                JsonValue::Array {
                    dims: dims.iter().map(|d| *d as usize).collect(),
                    elements: elements
                        .iter()
                        .map(|e| json_value(&part_value(*element, e)))
                        .collect(),
                }
            }
            (ColumnType::Composed(r), Item::Record(items)) => match r.def() {
                Composed::Record { fields, .. } => JsonValue::Composite(
                    fields
                        .iter()
                        .zip(items)
                        .map(|(f, i)| (f.name.clone(), json_value(&part_value(f.ty, i))))
                        .collect(),
                ),
                _ => JsonValue::Other(v.text().unwrap_or_default()),
            },
            _ => JsonValue::Other(v.text().unwrap_or_default()),
        },
        v => JsonValue::Other(v.text().unwrap_or_default()),
    }
}

/// A document argument's text: a json's or a jsonb's, a quoted literal read by the type's input.
fn document(v: &Value, quoted: bool, jsonb: bool) -> Result<String> {
    match v {
        Value::Typed(ColumnType::Textual(Textual::Json | Textual::Jsonb), s) => Ok(s.clone()),
        Value::Text(s) if quoted => {
            if jsonb {
                j::jsonb_in(s)
            } else {
                j::json_check(s).map(|_| s.clone())
            }
        }
        other => Err(Error::new(format!(
            "{} is no {}",
            super::pipeline::kind_name(other),
            if jsonb { "jsonb" } else { "json" }
        ))),
    }
}

/// A text array argument's elements and dimensions.
fn text_array(v: &Value) -> Result<(Vec<Option<String>>, Vec<usize>)> {
    let item = match v {
        Value::Composed(_, c) => c.item.clone(),
        Value::Text(s) => crate::composed::text_in(super::typed::array_of(TEXT), s)?,
        other => {
            return Err(Error::new(format!(
                "{} is no text[]",
                super::pipeline::kind_name(other)
            )))
        }
    };
    let Item::Array { elements, dims, .. } = item else {
        return Ok((Vec::new(), Vec::new()));
    };
    Ok((
        elements
            .iter()
            .map(|e| part_value(TEXT, e).text())
            .collect(),
        dims.iter().map(|d| *d as usize).collect(),
    ))
}

/// A path argument: a text array of one dimension at most.
fn path(v: &Value) -> Result<Vec<Option<String>>> {
    let (elements, dims) = text_array(v)?;
    if dims.len() > 1 {
        return Err(Error::new("wrong number of array subscripts"));
    }
    Ok(elements)
}

fn boolean(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Text(s) => super::question::truth(s)
            .ok_or_else(|| Error::new(format!("invalid input syntax for type boolean: \"{s}\""))),
        other => Err(Error::new(format!("{other:?} is no boolean"))),
    }
}

fn json(s: String) -> Value {
    Value::Typed(JSON, s)
}

fn jsonb(s: String) -> Value {
    Value::Typed(JSONB, s)
}

/// A path function's arguments: the target, the path as written, the variables (an empty object
/// where none are given), whether it is silent, and whether it is a `_tz` form.
fn path_args(name: &str, args: &[(Value, bool)]) -> Result<(String, String, String, bool, bool)> {
    let target = document(&args[0].0, args[0].1, true)?;
    let path = match &args[1] {
        (Value::Typed(ColumnType::Textual(Textual::Jsonpath), s), _) => s.clone(),
        (Value::Text(s), true) => {
            crate::jsonpath::jsonpath_in(s)?;
            s.clone()
        }
        (other, _) => {
            return Err(Error::new(format!(
                "{} is no jsonpath",
                super::pipeline::kind_name(other)
            )))
        }
    };
    let vars = match args.get(2) {
        Some((v, q)) => document(v, *q, true)?,
        None => "{}".into(),
    };
    let silent = args.get(3).map_or(Ok(false), |(v, _)| boolean(v))?;
    Ok((target, path, vars, silent, name.ends_with("_tz")))
}

/// The value of a function of json over its arguments' values, each with whether it is a quoted
/// literal; none where the snail answers no function of json by that name.
pub(super) fn call(name: &str, args: &[(Value, bool)]) -> Result<Option<Value>> {
    let strict = !matches!(
        name,
        "jsonb_set_lax"
            | "json_build_array"
            | "json_build_object"
            | "jsonb_build_array"
            | "jsonb_build_object"
    );
    let known = typed(name, &[]).ok().flatten().is_some()
        && !SETS.contains(&name)
        && !AGGREGATES.contains(&name);
    if !known {
        return Ok(None);
    }
    if strict && args.iter().any(|(v, _)| v.is_null()) {
        return Ok(Some(Value::Null));
    }
    let doc = |i: usize, jsonb: bool| document(&args[i].0, args[i].1, jsonb);
    // An argument "any" takes: an untyped literal as text.
    let values: Vec<JsonValue> = args.iter().map(|(v, _)| json_value(v)).collect();
    let polymorphic = |i: usize| -> Result<&JsonValue> {
        match &args[i] {
            (Value::Text(_), true) => Err(Error::new(
                "could not determine polymorphic type because input has type unknown",
            )),
            _ => Ok(&values[i]),
        }
    };
    let pretty = |i: usize| -> Result<bool> { args.get(i).map_or(Ok(false), |(v, _)| boolean(v)) };
    Ok(Some(match (name, args.len()) {
        ("json_typeof", 1) => Value::Text(j::json_typeof(&doc(0, false)?)?.into()),
        ("jsonb_typeof", 1) => Value::Text(j::jsonb_typeof(&doc(0, true)?)?.into()),
        ("json_array_length", 1) => Value::Int(i64::from(j::json_array_length(&doc(0, false)?)?)),
        ("jsonb_array_length", 1) => Value::Int(i64::from(j::jsonb_array_length(&doc(0, true)?)?)),
        ("json_strip_nulls", 1 | 2) => json(j::json_strip_nulls(&doc(0, false)?, pretty(1)?)?),
        ("jsonb_strip_nulls", 1 | 2) => jsonb(j::jsonb_strip_nulls(&doc(0, true)?, pretty(1)?)?),
        ("jsonb_pretty", 1) => Value::Text(j::jsonb_pretty(&doc(0, true)?)?),
        ("jsonb_set", 3 | 4) => jsonb(j::jsonb_set(
            &doc(0, true)?,
            &path(&args[1].0)?,
            &doc(2, true)?,
            args.get(3).map_or(Ok(true), |(v, _)| boolean(v))?,
        )?),
        ("jsonb_insert", 3 | 4) => jsonb(j::jsonb_insert(
            &doc(0, true)?,
            &path(&args[1].0)?,
            &doc(2, true)?,
            pretty(3)?,
        )?),
        ("jsonb_set_lax", 3..=5) => {
            let opt_doc = |i: usize| -> Result<Option<String>> {
                if args[i].0.is_null() {
                    Ok(None)
                } else {
                    doc(i, true).map(Some)
                }
            };
            let target = opt_doc(0)?;
            let p = if args[1].0.is_null() {
                None
            } else {
                Some(text_array(&args[1].0)?)
            };
            let new = opt_doc(2)?;
            let create = match args.get(3) {
                Some((v, _)) if v.is_null() => None,
                Some((v, _)) => Some(boolean(v)?),
                None => Some(true),
            };
            let treatment = match args.get(4) {
                Some((v, _)) => v.text(),
                None => Some("use_json_null".into()),
            };
            // A path of more dimensions is refused only where the value is set or deleted.
            let flat = p.as_ref().map(|(e, _)| e.clone());
            if p.as_ref().is_some_and(|(_, d)| d.len() > 1)
                && target.is_some()
                && create.is_some()
                && !(new.is_none()
                    && matches!(
                        treatment.as_deref(),
                        Some("return_target" | "raise_exception")
                    ))
            {
                return Err(Error::new("wrong number of array subscripts"));
            }
            j::jsonb_set_lax(
                target.as_deref(),
                flat.as_deref(),
                new.as_deref(),
                create,
                treatment.as_deref(),
            )?
            .map_or(Value::Null, jsonb)
        }
        ("json_object", 1) => {
            let (e, d) = text_array(&args[0].0)?;
            json(j::json_object(&e, &d)?)
        }
        ("jsonb_object", 1) => {
            let (e, d) = text_array(&args[0].0)?;
            jsonb(j::jsonb_object(&e, &d)?)
        }
        ("json_object", 2) | ("jsonb_object", 2) => {
            let (k, kd) = text_array(&args[0].0)?;
            let (v, vd) = text_array(&args[1].0)?;
            if name == "json_object" {
                json(j::json_object_pairs(&k, &kd, &v, &vd)?)
            } else {
                jsonb(j::jsonb_object_pairs(&k, &kd, &v, &vd)?)
            }
        }
        (
            "jsonb_path_exists"
            | "jsonb_path_exists_tz"
            | "jsonb_path_match"
            | "jsonb_path_match_tz"
            | "jsonb_path_query_array"
            | "jsonb_path_query_array_tz"
            | "jsonb_path_query_first"
            | "jsonb_path_query_first_tz",
            2..=4,
        ) => {
            let (target, p, vars, silent, tz) = path_args(name, args)?;
            use crate::jsonpath as jp;
            match name.trim_end_matches("_tz") {
                "jsonb_path_exists" => jp::jsonb_path_exists(&target, &p, &vars, silent, tz)?
                    .map_or(Value::Null, Value::Bool),
                "jsonb_path_match" => jp::jsonb_path_match(&target, &p, &vars, silent, tz)?
                    .map_or(Value::Null, Value::Bool),
                "jsonb_path_query_array" => {
                    jsonb(jp::jsonb_path_query_array(&target, &p, &vars, silent, tz)?)
                }
                _ => jp::jsonb_path_query_first(&target, &p, &vars, silent, tz)?
                    .map_or(Value::Null, jsonb),
            }
        }
        ("to_json", 1) => json(j::to_json(polymorphic(0)?)?),
        ("to_jsonb", 1) => jsonb(j::to_jsonb(polymorphic(0)?)?),
        ("array_to_json", 1 | 2) => json(j::array_to_json(&values[0], pretty(1)?)?),
        ("row_to_json", 1 | 2) => json(j::row_to_json(&values[0], pretty(1)?)?),
        ("json_build_array", _) => json(j::json_build_array(&values)?),
        ("json_build_object", _) => json(j::json_build_object(&values)?),
        ("jsonb_build_array", _) => jsonb(j::jsonb_build_array(&values)?),
        ("jsonb_build_object", _) => jsonb(j::jsonb_build_object(&values)?),
        _ => {
            return Err(Error::new(format!(
                "function {name}({}) does not exist",
                args.iter()
                    .map(|(v, _)| super::pipeline::kind_name(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }))
}

/// The values of a set-returning function of json over its arguments' values; none where it is
/// not one of them.
pub(super) fn sets(name: &str, args: &[(Value, bool)]) -> Result<Option<Vec<Value>>> {
    if !SETS.contains(&name) {
        return Ok(None);
    }
    if name.starts_with("jsonb_path_query") {
        if args.iter().any(|(v, _)| v.is_null()) {
            return Ok(Some(Vec::new()));
        }
        let (target, p, vars, silent, tz) = path_args(name, args)?;
        return Ok(Some(
            crate::jsonpath::jsonb_path_query(&target, &p, &vars, silent, tz)?
                .into_iter()
                .map(jsonb)
                .collect(),
        ));
    }
    let [(v, q)] = args else {
        return Err(Error::new(format!("function {name} takes one argument")));
    };
    if v.is_null() {
        return Ok(Some(Vec::new()));
    }
    let binary = name.starts_with("jsonb");
    let d = document(v, *q, binary)?;
    let row = |ty: ColumnType, k: String, value: Item| {
        composed(
            pair(ty),
            Item::Record(vec![Item::Scalar(Datum::Text(k)), value]),
        )
    };
    let text = |s: Option<String>| s.map_or(Item::Null, |s| Item::Scalar(Datum::Text(s)));
    Ok(Some(match name {
        "json_object_keys" => j::json_object_keys(&d)?
            .into_iter()
            .map(Value::Text)
            .collect(),
        "jsonb_object_keys" => j::jsonb_object_keys(&d)?
            .into_iter()
            .map(Value::Text)
            .collect(),
        "json_each" | "jsonb_each" => {
            let (ty, pairs) = if binary {
                (JSONB, j::jsonb_each(&d)?)
            } else {
                (JSON, j::json_each(&d)?)
            };
            pairs
                .into_iter()
                .map(|(k, v)| row(ty, k, Item::Scalar(Datum::Text(v))))
                .collect()
        }
        "json_each_text" | "jsonb_each_text" => {
            let pairs = if binary {
                j::jsonb_each_text(&d)?
            } else {
                j::json_each_text(&d)?
            };
            pairs
                .into_iter()
                .map(|(k, v)| row(TEXT, k, text(v)))
                .collect()
        }
        "json_array_elements" => j::json_array_elements(&d)?.into_iter().map(json).collect(),
        "jsonb_array_elements" => j::jsonb_array_elements(&d)?
            .into_iter()
            .map(jsonb)
            .collect(),
        _ => {
            let elements = if binary {
                j::jsonb_array_elements_text(&d)?
            } else {
                j::json_array_elements_text(&d)?
            };
            elements
                .into_iter()
                .map(|e| e.map_or(Value::Null, Value::Text))
                .collect()
        }
    }))
}

/// An aggregate of json or jsonb as it folds.
#[derive(Clone, Debug)]
pub(super) enum Fold {
    Json(j::JsonAgg),
    Jsonb(j::JsonbAgg),
    JsonObject(j::JsonObjectAgg),
    JsonbObject(j::JsonbObjectAgg),
}

impl Fold {
    /// The aggregate a name makes, where it is one of json's.
    pub(super) fn of(name: &str) -> Option<Fold> {
        if !AGGREGATES.contains(&name) {
            return None;
        }
        let strict = name.ends_with("_strict");
        let unique = name.contains("_unique");
        Some(match (name.starts_with("jsonb"), name.contains("object")) {
            (false, false) => Fold::Json(j::JsonAgg::new(strict)),
            (true, false) => Fold::Jsonb(j::JsonbAgg::new(strict)),
            (false, true) => Fold::JsonObject(j::JsonObjectAgg::new(strict, unique)),
            (true, true) => Fold::JsonbObject(j::JsonbObjectAgg::new(strict, unique)),
        })
    }

    /// Folds in one row's arguments, NULLs among them.
    pub(super) fn add(&mut self, args: &[Value]) -> Result<()> {
        let v = |i: usize| args.get(i).map(json_value).unwrap_or(JsonValue::Null);
        match self {
            Fold::Json(a) => a.add(&v(0)),
            Fold::Jsonb(a) => a.add(&v(0)),
            Fold::JsonObject(a) => a.add(&v(0), &v(1)),
            Fold::JsonbObject(a) => a.add(&v(0), &v(1)),
        }
    }

    /// Its answer: NULL where no row came.
    pub(super) fn value(&self) -> Result<Value> {
        Ok(match self {
            Fold::Json(a) => a.finish().map_or(Value::Null, json),
            Fold::JsonObject(a) => a.finish().map_or(Value::Null, json),
            Fold::Jsonb(a) => a.finish()?.map_or(Value::Null, jsonb),
            Fold::JsonbObject(a) => a.finish()?.map_or(Value::Null, jsonb),
        })
    }
}
