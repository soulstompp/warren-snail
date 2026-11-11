//! The functions of text search a pipeline answers by name, over `crate::tsearch`: those of
//! tsvector and tsquery, and those that read a configuration, named beside the text or the file's
//! `default_text_search_config`, under `simple` and `english`.

use super::pipeline::{composed, Value};
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::read::statement::Datum;
use crate::textual::Textual;
use crate::tsearch as ts;
use crate::{Error, Result};

const TSVECTOR: ColumnType = ColumnType::Textual(Textual::Tsvector);
const TSQUERY: ColumnType = ColumnType::Textual(Textual::Tsquery);
const TEXT: ColumnType = ColumnType::Text(None);

/// The set-returning functions of text search.
pub(super) const SETS: [&str; 3] = ["ts_parse", "ts_token_type", "ts_debug"];

/// The functions of text search that read what the snail does not: a query run by its text.
fn unread(name: &str, args: &[Option<ColumnType>]) -> Option<Error> {
    let json = args
        .iter()
        .any(|t| matches!(t, Some(ColumnType::Textual(Textual::Json | Textual::Jsonb))));
    match name {
        "ts_stat" => Some(Error::new(
            "ts_stat runs a query by its text: it is not read",
        )),
        "ts_rewrite" if args.len() == 2 => Some(Error::new(
            "ts_rewrite of a query's text runs it: it is not read",
        )),
        "json_to_tsvector" | "jsonb_to_tsvector" => {
            Some(Error::new(format!("{name} is not read yet")))
        }
        "to_tsvector" | "ts_headline" if json => {
            Some(Error::new(format!("{name} of a json is not read yet")))
        }
        _ => None,
    }
}

/// Whether a call names no configuration, so that it reads the file's: `ts_headline`'s forms without
/// one take the query second.
fn names_none(name: &str, types: &[Option<ColumnType>]) -> bool {
    match name {
        "to_tsvector" | "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery"
        | "websearch_to_tsquery" | "ts_debug" => types.len() == 1,
        "ts_headline" => types.len() == 2 || types.get(1) == Some(&Some(TSQUERY)),
        _ => false,
    }
}

/// A call's arguments, the file's configuration before them where it names none.
pub(super) fn named_setting(
    name: &str,
    given: Vec<(Value, bool)>,
    casings: &super::pipeline::Casings,
) -> Result<Vec<(Value, bool)>> {
    let types: Vec<Option<ColumnType>> = given
        .iter()
        .map(|(v, _)| super::pipeline::type_of_value(v))
        .collect();
    if !names_none(name, &types) {
        return Ok(given);
    }
    let config = Value::Text(casings.text_search()?.config().to_string());
    Ok(std::iter::once((config, true)).chain(given).collect())
}

/// `get_current_ts_config`, and `current_setting` and `set_config` of `default_text_search_config`:
/// the file's, a value set to another refused by name. None where a call is none of them.
pub(super) fn setting(
    name: &str,
    given: &[(Value, bool)],
    casings: &super::pipeline::Casings,
) -> Result<Option<Value>> {
    let named = |i: usize| given.get(i).and_then(|(v, _)| v.text());
    let of_setting = || {
        named(0).is_some_and(|n| n.eq_ignore_ascii_case("default_text_search_config"))
    };
    Ok(Some(match name {
        "get_current_ts_config" if given.is_empty() => {
            Value::Text(casings.text_search()?.config().to_string())
        }
        "current_setting" if of_setting() => Value::Text(casings.text_search()?.name().to_string()),
        "set_config" if of_setting() => {
            let file = casings.text_search()?;
            match named(1) {
                Some(value) if ts::TextSearch::open(&value).is_ok_and(|t| t == file) => {
                    Value::Text(file.name().to_string())
                }
                Some(value) => {
                    return Err(Error::new(format!(
                        "set_config: setting \"default_text_search_config\" to \"{value}\": the \
                         snail answers under default_text_search_config {} only",
                        file.name()
                    )))
                }
                None => {
                    return Err(Error::new(format!(
                        "set_config: resetting \"default_text_search_config\" takes the server's \
                         default, and the snail answers under default_text_search_config {} only",
                        file.name()
                    )))
                }
            }
        }
        _ => return Ok(None),
    }))
}

/// The record type of a set's rows.
fn record(name: &str) -> ColumnType {
    let f = |names: &[(&str, ColumnType)]| {
        names
            .iter()
            .map(|(n, t)| Field::new(n, *t))
            .collect::<Vec<_>>()
    };
    let text_array = super::typed::array_of(TEXT);
    let fields = match name {
        "unnest" => f(&[
            ("lexeme", TEXT),
            ("positions", super::typed::array_of(ColumnType::Int2)),
            ("weights", text_array),
        ]),
        "ts_parse" => f(&[("tokid", ColumnType::Int4), ("token", TEXT)]),
        "ts_token_type" => f(&[
            ("tokid", ColumnType::Int4),
            ("alias", TEXT),
            ("description", TEXT),
        ]),
        _ => f(&[
            ("alias", TEXT),
            ("description", TEXT),
            ("token", TEXT),
            ("dictionaries", text_array),
            ("dictionary", TEXT),
            ("lexemes", text_array),
        ]),
    };
    crate::composed::type_of(Composed::Record {
        name: "record".into(),
        fields,
    })
}

/// The type a function of text search returns over its arguments' types, or the refusal of one
/// the snail does not read; none where the name is not one of them.
pub(super) fn typed(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    let vector = matches!(args.first(), Some(Some(TSVECTOR)));
    if let Some(e) = unread(name, args) {
        return Err(e);
    }
    if names_none(name, args) && !args[0].is_none_or(|t| t.is_string()) {
        return Err(super::typed::no_form(name, args));
    }
    Ok(Some(Some(match name {
        "get_current_ts_config" => TEXT,
        "length" if vector => ColumnType::Int4,
        "unnest" if vector => record("unnest"),
        "numnode" => ColumnType::Int4,
        "strip" | "setweight" | "ts_delete" | "ts_filter" | "array_to_tsvector" | "to_tsvector" => {
            TSVECTOR
        }
        "tsvector_to_array" | "ts_lexize" => super::typed::array_of(TEXT),
        "querytree" | "ts_headline" => TEXT,
        "ts_rewrite"
        | "tsquery_phrase"
        | "to_tsquery"
        | "plainto_tsquery"
        | "phraseto_tsquery"
        | "websearch_to_tsquery" => TSQUERY,
        "ts_rank" | "ts_rank_cd" => ColumnType::Float4,
        "ts_parse" | "ts_token_type" | "ts_debug" => record(name),
        _ => return Ok(None),
    })))
}

/// A tsvector or tsquery argument's held text, a quoted literal read by the type's input.
fn held(v: &Value, quoted: bool, kind: Textual) -> Result<String> {
    match v {
        Value::Typed(ColumnType::Textual(k), s) if *k == kind => Ok(s.clone()),
        Value::Text(s) if quoted => crate::textual::text_in(kind, s),
        other => Err(Error::new(format!(
            "{} is no {}",
            super::pipeline::kind_name(other),
            ColumnType::Textual(kind).pg_name()
        ))),
    }
}

/// An array argument's elements as text, a quoted literal read as the array type `element[]`.
fn elements(v: &Value, element: ColumnType) -> Result<Vec<Option<String>>> {
    let item = match v {
        Value::Composed(_, c) => c.item.clone(),
        Value::Text(s) => crate::composed::text_in(super::typed::array_of(element), s)?,
        other => {
            return Err(Error::new(format!(
                "{} is no array",
                super::pipeline::kind_name(other)
            )))
        }
    };
    let Item::Array { elements, dims, .. } = item else {
        return Ok(Vec::new());
    };
    if dims.len() > 1 {
        return Err(Error::new("array must be one-dimensional"));
    }
    Ok(elements
        .iter()
        .map(|e| super::pipeline::part_value(element, e).text())
        .collect())
}

fn int(v: &Value) -> Result<i32> {
    match v {
        Value::Int(x) => i32::try_from(*x).map_err(|_| Error::new("integer out of range")),
        Value::Text(s) => s
            .trim()
            .parse()
            .map_err(|_| Error::new(format!("invalid input syntax for type integer: \"{s}\""))),
        other => Err(Error::new(format!("{other:?} is no integer"))),
    }
}

/// A `"char"` argument's byte.
fn weight(v: &Value) -> u8 {
    v.text().and_then(|t| t.bytes().next()).unwrap_or(0)
}

/// A name argument: a regconfig's, a regdictionary's, or a text's.
fn named(v: &Value) -> String {
    v.text().unwrap_or_default()
}

fn text_array(xs: Vec<String>) -> Item {
    let n = xs.len() as i32;
    Item::Array {
        elements: xs
            .into_iter()
            .map(|x| Item::Scalar(Datum::Text(x)))
            .collect(),
        dims: if n == 0 { Vec::new() } else { vec![n] },
        lower: if n == 0 { Vec::new() } else { vec![1] },
    }
}

/// The value of a function of text search over its arguments' values, each with whether it is a
/// quoted literal; none where the snail answers no function of text search by that name.
pub(super) fn call(name: &str, args: &[(Value, bool)]) -> Result<Option<Value>> {
    let types: Vec<Option<ColumnType>> = args
        .iter()
        .map(|(v, _)| super::pipeline::type_of_value(v))
        .collect();
    let vector_first = matches!(
        args.first(),
        Some((Value::Typed(ColumnType::Textual(Textual::Tsvector), _), _))
    );
    let ours = match name {
        "length" | "unnest" => vector_first,
        n => typed(n, &[]).ok().flatten().is_some() || unread(n, &types).is_some(),
    };
    if !ours || SETS.contains(&name) || name == "unnest" {
        return Ok(None);
    }
    if let Some(e) = unread(name, &types) {
        return Err(e);
    }
    // Every one is strict.
    if args.iter().any(|(v, _)| v.is_null()) {
        return Ok(Some(Value::Null));
    }
    let vec_at = |i: usize| held(&args[i].0, args[i].1, Textual::Tsvector);
    let query_at = |i: usize| held(&args[i].0, args[i].1, Textual::Tsquery);
    let vector = |s: String| Value::Typed(TSVECTOR, s);
    let query = |s: String| Value::Typed(TSQUERY, s);
    let text_at = |i: usize| args[i].0.text().unwrap_or_default();
    // A rank's weights first, where they are given.
    let ranked = |cd: bool| -> Result<Value> {
        let (weights, at) = match &args[0].0 {
            Value::Composed(..) => (Some(elements(&args[0].0, ColumnType::Float4)?), 1),
            Value::Text(_)
                if args[0].1 && args.len() >= 3 && !matches!(args[1].0, Value::Int(_)) =>
            {
                (Some(elements(&args[0].0, ColumnType::Float4)?), 1)
            }
            _ => (None, 0),
        };
        let weights: Option<Vec<Option<f32>>> = weights.map(|w| {
            w.into_iter()
                .map(|x| x.and_then(|x| x.parse().ok()))
                .collect()
        });
        let normalization = match args.get(at + 2) {
            Some((v, _)) => int(v)?,
            None => 0,
        };
        let (v, q) = (vec_at(at)?, query_at(at + 1)?);
        let r = if cd {
            ts::ts_rank_cd(weights.as_deref(), &v, &q, normalization)?
        } else {
            ts::ts_rank(weights.as_deref(), &v, &q, normalization)?
        };
        Ok(Value::Real(r))
    };
    Ok(Some(match (name, args.len()) {
        ("length", 1) => Value::Int(i64::from(ts::ts_length(&vec_at(0)?))),
        ("strip", 1) => vector(ts::strip(&vec_at(0)?)),
        ("setweight", 2) => vector(ts::setweight(&vec_at(0)?, weight(&args[1].0))?),
        ("setweight", 3) => vector(ts::setweight_lexemes(
            &vec_at(0)?,
            weight(&args[1].0),
            &elements(&args[2].0, TEXT)?,
        )?),
        ("ts_delete", 2) if matches!(args[1].0, Value::Composed(..)) => vector(
            ts::ts_delete_lexemes(&vec_at(0)?, &elements(&args[1].0, TEXT)?),
        ),
        ("ts_delete", 2) => vector(ts::ts_delete(&vec_at(0)?, &text_at(1))),
        ("ts_filter", 2) => {
            let w: Vec<Option<u8>> = elements(&args[1].0, ColumnType::Char)?
                .into_iter()
                .map(|x| x.and_then(|x| x.bytes().next()))
                .collect();
            vector(ts::ts_filter(&vec_at(0)?, &w)?)
        }
        ("tsvector_to_array", 1) => composed(
            super::typed::array_of(TEXT),
            text_array(ts::tsvector_to_array(&vec_at(0)?)),
        ),
        ("array_to_tsvector", 1) => vector(ts::array_to_tsvector(&elements(&args[0].0, TEXT)?)?),
        ("numnode", 1) => Value::Int(i64::from(ts::numnode(&query_at(0)?))),
        ("querytree", 1) => Value::Text(ts::querytree(&query_at(0)?)),
        ("ts_rewrite", 3) => query(ts::ts_rewrite(&query_at(0)?, &query_at(1)?, &query_at(2)?)?),
        ("tsquery_phrase", 2) => query(ts::tsquery_phrase(&query_at(0)?, &query_at(1)?)?),
        ("tsquery_phrase", 3) => query(ts::tsquery_phrase_distance(
            &query_at(0)?,
            &query_at(1)?,
            int(&args[2].0)?,
        )?),
        ("ts_rank", _) => ranked(false)?,
        ("ts_rank_cd", _) => ranked(true)?,
        ("to_tsvector", 2) => vector(ts::to_tsvector(&named(&args[0].0), &text_at(1))?),
        ("to_tsquery", 2) => query(ts::to_tsquery(&named(&args[0].0), &text_at(1))?),
        ("plainto_tsquery", 2) => query(ts::plainto_tsquery(&named(&args[0].0), &text_at(1))?),
        ("phraseto_tsquery", 2) => query(ts::phraseto_tsquery(&named(&args[0].0), &text_at(1))?),
        ("websearch_to_tsquery", 2) => {
            query(ts::websearch_to_tsquery(&named(&args[0].0), &text_at(1))?)
        }
        ("ts_lexize", 2) => match ts::ts_lexize(&named(&args[0].0), &text_at(1))? {
            Some(xs) => composed(super::typed::array_of(TEXT), text_array(xs)),
            None => Value::Null,
        },
        ("ts_headline", 3 | 4) => Value::Text(ts::ts_headline(
            &named(&args[0].0),
            &text_at(1),
            &query_at(2)?,
            args.get(3)
                .map(|(v, _)| v.text().unwrap_or_default())
                .as_deref(),
        )?),
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

/// The rows of a set-returning function of text search; none where the name is not one of them.
pub(super) fn sets(name: &str, args: &[(Value, bool)]) -> Result<Option<Vec<Value>>> {
    let vector_first = matches!(
        args.first(),
        Some((Value::Typed(ColumnType::Textual(Textual::Tsvector), _), _))
    );
    if !(SETS.contains(&name) || name == "unnest" && vector_first) {
        return Ok(None);
    }
    if args.iter().any(|(v, _)| v.is_null()) {
        return Ok(Some(Vec::new()));
    }
    let text = |s: String| Item::Scalar(Datum::Text(s));
    let texts = |xs: Option<Vec<String>>| xs.map_or(Item::Null, text_array);
    let int = |x: i32| Item::Scalar(Datum::Int(i64::from(x)));
    let row = |items: Vec<Item>| composed(record(name), Item::Record(items));
    let named = |i: usize| args[i].0.text().unwrap_or_default();
    // A parser by its OID, an integer or an oid: the one parser, `default`, is 3722.
    let parser = || -> Result<String> {
        let oid = match &args[0].0 {
            Value::Int(n) => Some(*n as u32),
            Value::Scalar(ColumnType::Oid, k) => Some(crate::scalars::unsigned_of(*k, 32) as u32),
            _ => None,
        };
        match oid {
            Some(3722) => Ok("default".into()),
            Some(o) => Err(Error::coded(
                "XX000",
                format!("cache lookup failed for text search parser {o}"),
            )),
            None => Ok(named(0)),
        }
    };
    Ok(Some(match (name, args.len()) {
        ("unnest", 1) => {
            let v = held(&args[0].0, args[0].1, Textual::Tsvector)?;
            ts::unnest(&v)
                .into_iter()
                .map(|u| {
                    let positions = u.positions.map_or(Item::Null, |p| {
                        let n = p.len() as i32;
                        Item::Array {
                            elements: p
                                .into_iter()
                                .map(|x| Item::Scalar(Datum::Int(i64::from(x))))
                                .collect(),
                            dims: if n == 0 { Vec::new() } else { vec![n] },
                            lower: if n == 0 { Vec::new() } else { vec![1] },
                        }
                    });
                    row(vec![text(u.lexeme), positions, texts(u.weights)])
                })
                .collect()
        }
        ("ts_parse", 2) => ts::ts_parse(&parser()?, &named(1))?
            .into_iter()
            .map(|(id, token)| row(vec![int(id), text(token)]))
            .collect(),
        ("ts_token_type", 1) => ts::ts_token_type(&parser()?)?
            .into_iter()
            .map(|(id, alias, description)| row(vec![int(id), text(alias), text(description)]))
            .collect(),
        ("ts_debug", 2) => ts::ts_debug(&named(0), &named(1))?
            .into_iter()
            .map(|d| {
                row(vec![
                    text(d.alias),
                    text(d.description),
                    text(d.token),
                    text_array(d.dictionaries),
                    d.dictionary.map_or(Item::Null, text),
                    texts(d.lexemes),
                ])
            })
            .collect(),
        _ => {
            return Err(Error::new(format!(
                "function {name}({} arguments) does not exist",
                args.len()
            )))
        }
    }))
}
