//! An ORDER BY item's direction: ASC, DESC, or `USING` an operator, as PostgreSQL's
//! `addTargetToSortList` reads it: the operator of its type's own two operands, a B-tree class's `<`
//! or `>`, and what it orders by. `~<~` and `~>~` order strings by their bytes.

use crate::catalog::ColumnType;
use crate::composed::Composed;
use crate::error::{NO_OPERATOR, NOT_ORDERING};
use crate::{Error, Result};
use sqlparser::ast::{OrderByOptions, OrderBySort, ObjectNamePart};

/// The operator an item names: its schema where it is written with one, and its name.
fn operator(o: &OrderByOptions) -> Option<(Option<String>, String)> {
    let Some(OrderBySort::Using(name)) = &o.sort else {
        return None;
    };
    let mut parts = name.0.iter().map(|p| match p {
        ObjectNamePart::Identifier(i) => i.value.clone(),
        other => other.to_string(),
    });
    let last = parts.next_back()?;
    let qualifier = parts.collect::<Vec<_>>();
    Some((
        (!qualifier.is_empty()).then(|| qualifier.join(".")),
        last,
    ))
}

/// Whether an item sorts descending, so that its NULLs come first unless it says otherwise.
pub(super) fn descends(o: &OrderByOptions) -> bool {
    match &o.sort {
        Some(OrderBySort::Desc) => true,
        Some(OrderBySort::Using(_)) => {
            matches!(operator(o), Some((_, op)) if op == ">" || op == "~>~")
        }
        _ => false,
    }
}

/// Whether an item orders strings by their bytes, whatever their collation.
pub(super) fn by_bytes(o: &OrderByOptions) -> bool {
    matches!(operator(o), Some((_, op)) if op == "~<~" || op == "~>~")
}

/// Whether an item sorts as a key's order does ascending under its own collation, NULLs last.
pub(super) fn ascending(o: &OrderByOptions) -> bool {
    let up = match &o.sort {
        None | Some(OrderBySort::Asc) => true,
        Some(OrderBySort::Desc) => false,
        Some(OrderBySort::Using(_)) => matches!(operator(o), Some((_, op)) if op == "<"),
    };
    up && o.nulls_first != Some(true)
}

/// The direction of an item a key's order can serve: whether it descends; none where it names an
/// operator that orders otherwise.
pub(super) fn along_a_key(o: &OrderByOptions) -> Option<bool> {
    match operator(o) {
        None => Some(descends(o)),
        Some((_, op)) if op == "<" || op == ">" => Some(op == ">"),
        Some(_) => None,
    }
}

/// A type's name in PostgreSQL's errors: no modifier.
fn bare(ty: ColumnType) -> String {
    let name = ty.pg_name();
    match (name.find('('), name.find(')')) {
        (Some(a), Some(b)) => format!("{}{}", &name[..a], &name[b + 1..]),
        _ => name,
    }
}

/// The name an operator's list writes a type's operands under.
fn operands_of(ty: ColumnType) -> String {
    if let ColumnType::Composed(r) = ty {
        return match r.def() {
            Composed::Array { .. } | Composed::Vector { .. } => "anyarray".into(),
            Composed::Enum { .. } => "anyenum".into(),
            Composed::Range { .. } => "anyrange".into(),
            Composed::Multirange { .. } => "anymultirange".into(),
            Composed::Record { .. } => "record".into(),
            _ => bare(ty),
        };
    }
    match bare(ty).as_str() {
        "character varying" => "text".into(),
        other => other.into(),
    }
}

/// Whether the list holds an operator of `name` on two operands of a type so named.
fn listed(name: &str, operands: &str) -> bool {
    crate::lists::all(crate::lists::Kind::Operator)
        .iter()
        .any(|l| {
            let (op, left, right) = l.operands();
            op == name && left == Some(operands) && right == operands
        })
}

/// An item refused in PostgreSQL's words where it names an operator its type does not take, or one
/// no B-tree class orders by. `ty` is the type it sorts, none for an untyped literal's text.
pub(super) fn checked(o: &OrderByOptions, ty: Option<ColumnType>) -> Result<()> {
    let Some((schema, op)) = operator(o) else {
        return Ok(());
    };
    let ty = ty.unwrap_or(ColumnType::Text(None));
    let written = match &schema {
        Some(s) => format!("{s}.{op}"),
        None => op.clone(),
    };
    let missing = || {
        Error::new(format!(
            "operator does not exist: {} {written} {}{NO_OPERATOR}",
            bare(ty),
            bare(ty)
        ))
    };
    if schema.as_deref().is_some_and(|s| s != "pg_catalog") {
        return Err(missing());
    }
    let invalid = || {
        Error::new(format!(
            "operator {op} is not a valid ordering operator{NOT_ORDERING}"
        ))
    };
    let operands = operands_of(ty);
    match op.as_str() {
        "<" | ">" if ty.ordered() => Ok(()),
        "~<~" | "~>~" if ty.is_string() && ty != ColumnType::Name => Ok(()),
        _ if listed(&op, &operands) => Err(invalid()),
        _ if ty == ColumnType::Name && listed(&op, "text") => Err(Error::new(format!(
            "operator requires run-time type coercion: name {written} name"
        ))),
        _ => Err(missing()),
    }
}
