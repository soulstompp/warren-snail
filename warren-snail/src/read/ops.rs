//! The operators of the numbers and the strings by their own names: prefix `+`, `@`, `|/`, `||/` and
//! `~`, `^`, `^@`, the integers' `&`, `|`, `#`, `<<` and `>>`, and the pattern comparisons `~<~`,
//! `~<=~`, `~>=~` and `~>~`, each typed as PostgreSQL resolves it, a refusal in its words before any
//! row, and answered as the function PostgreSQL's catalog names for it.

use super::pipeline::{quoted, Value};
use super::typed::NUMERIC;
use crate::catalog::ColumnType;
use crate::composed::Item;
use crate::{Error, Result};
use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator};
use std::cmp::Ordering;

const TSQUERY: ColumnType = ColumnType::Textual(crate::textual::Textual::Tsquery);

/// Whether an expression is a pattern match against a pattern written out.
pub(super) fn pattern_written(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) => pattern_written(x),
        Expr::Like { pattern, .. }
        | Expr::ILike { pattern, .. }
        | Expr::SimilarTo { pattern, .. } => quoted(pattern),
        Expr::BinaryOp {
            op:
                BinaryOperator::PGRegexMatch
                | BinaryOperator::PGRegexIMatch
                | BinaryOperator::PGRegexNotMatch
                | BinaryOperator::PGRegexNotIMatch,
            right,
            ..
        } => quoted(right),
        _ => false,
    }
}

/// A pattern comparison by its name: how an order of bytes reads it.
pub(crate) fn pattern_comparison(op: &BinaryOperator) -> Option<BinaryOperator> {
    let BinaryOperator::Custom(name) = op else {
        return None;
    };
    Some(match name.as_str() {
        "~<~" => BinaryOperator::Lt,
        "~<=~" => BinaryOperator::LtEq,
        "~>=~" => BinaryOperator::GtEq,
        "~>~" => BinaryOperator::Gt,
        _ => return None,
    })
}

/// What an operand is to an operator's forms: an integer of its width, a number of another kind, a
/// string, a quoted literal or a NULL of no type yet, one no type was resolved for, or another type.
#[derive(Clone, Copy, PartialEq)]
enum Operand {
    Integer(ColumnType),
    Number,
    Text,
    Untyped,
    Unresolved,
    Other,
}

fn operand(t: Option<ColumnType>, e: &Expr) -> Operand {
    use ColumnType as C;
    match t {
        Some(t) if t.is_integer() => Operand::Integer(t),
        Some(t) if t.is_numeric() || t.is_float() => Operand::Number,
        Some(C::Text(_) | C::Bpchar(_) | C::Name | C::Char) => Operand::Text,
        Some(_) => Operand::Other,
        None if super::typed::untyped(e).is_some() => Operand::Untyped,
        None => Operand::Unresolved,
    }
}

/// The name a type is written under in PostgreSQL's refusal of an operator.
fn named(t: Option<ColumnType>) -> String {
    match t {
        None => "unknown".into(),
        Some(t) if t.is_numeric() => "numeric".into(),
        Some(t) => t
            .pg_name()
            .split('(')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

fn prefix_refused(why: &str, op: &UnaryOperator, t: Option<ColumnType>) -> Error {
    Error::new(format!("operator {why}: {op} {}", named(t)))
}

/// The type a prefix operator returns, or its refusal where it has no form for its operand; none
/// where the operator is none of these, or the operand's type is not resolved.
pub(super) fn prefix_type(
    op: &UnaryOperator,
    t: Option<ColumnType>,
    e: &Expr,
) -> Result<Option<ColumnType>> {
    use ColumnType as C;
    let what = operand(t, e);
    match op {
        UnaryOperator::Plus | UnaryOperator::PGAbs => Ok(match what {
            Operand::Integer(t) => Some(t),
            Operand::Number => t.map(|t| if t.is_numeric() { NUMERIC } else { t }),
            Operand::Untyped => Some(C::Float8),
            Operand::Unresolved => None,
            _ => return Err(prefix_refused("does not exist", op, t)),
        }),
        UnaryOperator::PGSquareRoot | UnaryOperator::PGCubeRoot => match what {
            Operand::Integer(_) | Operand::Number | Operand::Untyped | Operand::Unresolved => {
                Ok(Some(C::Float8))
            }
            _ => Err(prefix_refused("does not exist", op, t)),
        },
        UnaryOperator::BitwiseNot => match what {
            Operand::Integer(t) => Ok(Some(t)),
            Operand::Untyped => Err(prefix_refused("is not unique", op, None)),
            Operand::Unresolved => Ok(None),
            Operand::Other
                if t.is_some_and(|t| {
                    t.is_bits() || matches!(t, C::Macaddr | C::Macaddr8) || crate::spans::is_inet(t)
                }) =>
            {
                Ok(None)
            }
            _ => Err(prefix_refused("does not exist", op, t)),
        },
        _ => Ok(None),
    }
}

/// A prefix operator's value: `ret` the type it resolved to, `t` its operand's. None where the
/// operator is none of these.
pub(super) fn prefix(
    op: &UnaryOperator,
    v: Value,
    t: Option<ColumnType>,
    ret: Option<ColumnType>,
) -> Result<Option<Value>> {
    let number = |name: &str, v: Value, ret: Option<ColumnType>| {
        Ok::<_, Error>(super::calls::numbers(name, &[v], &[t], ret)?.unwrap_or(Value::Null))
    };
    Ok(Some(match op {
        UnaryOperator::Plus => match v {
            Value::Text(_) => super::pipeline::float_cast(v, true)?,
            v => v,
        },
        UnaryOperator::PGAbs => number("abs", v, ret)?,
        UnaryOperator::PGSquareRoot => number("sqrt", v, Some(ColumnType::Float8))?,
        UnaryOperator::PGCubeRoot => number("cbrt", v, Some(ColumnType::Float8))?,
        _ => return Ok(None),
    }))
}

fn refused(why: &str, op: &BinaryOperator, a: Option<ColumnType>, b: Option<ColumnType>) -> Error {
    Error::new(format!("operator {why}: {} {op} {}", named(a), named(b)))
}

/// The type an operator of these returns, or its refusal where it has no form for its operands;
/// none where the operator is none of these, or leaves them to another type's form.
pub(super) fn binary_type(
    op: &BinaryOperator,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
    left: &Expr,
    right: &Expr,
) -> Result<Option<ColumnType>> {
    use ColumnType as C;
    use Operand as O;
    // A bit string written as one is a bit.
    let (a, b) = (
        a.or_else(|| super::question::bit_literal(left).map(|_| C::Bit(0))),
        b.or_else(|| super::question::bit_literal(right).map(|_| C::Bit(0))),
    );
    let (x, y) = (operand(a, left), operand(b, right));
    if let Some(t) = document_type(op, a, b, x, y)? {
        return Ok(Some(t));
    }
    if *op == BinaryOperator::AtAt {
        return match (x, y) {
            (O::Text | O::Untyped, O::Text | O::Untyped) => Ok(Some(C::Boolean)),
            (O::Text | O::Untyped, O::Other) if b == Some(TSQUERY) => Ok(Some(C::Boolean)),
            (O::Text | O::Integer(_) | O::Number, _)
            | (O::Other, O::Text | O::Integer(_) | O::Number) => {
                Err(refused("does not exist", op, a, b))
            }
            _ => Ok(None),
        };
    }
    if pattern_comparison(op).is_some() || *op == BinaryOperator::PGStartsWith {
        return match (x, y) {
            (O::Text | O::Untyped | O::Unresolved, O::Text | O::Untyped | O::Unresolved) => {
                Ok(Some(C::Boolean))
            }
            _ => Err(refused("does not exist", op, a, b)),
        };
    }
    match op {
        BinaryOperator::PGExp => match (x, y) {
            (O::Integer(_) | O::Number | O::Untyped, O::Integer(_) | O::Number | O::Untyped) => {
                let numeric = [a, b].iter().flatten().any(|t| t.is_numeric())
                    && ![a, b].iter().flatten().any(|t| t.is_float());
                Ok(Some(if numeric { NUMERIC } else { C::Float8 }))
            }
            (O::Unresolved, _) | (_, O::Unresolved) => Ok(None),
            _ => Err(refused("does not exist", op, a, b)),
        },
        BinaryOperator::BitwiseAnd | BinaryOperator::BitwiseOr | BinaryOperator::PGBitwiseXor => {
            match (x, y) {
                (O::Integer(s), O::Integer(t)) => Ok(Some(super::typed::wider(s, t))),
                (O::Integer(t), O::Untyped) | (O::Untyped, O::Integer(t)) => Ok(Some(t)),
                (O::Untyped, O::Untyped) => Err(refused("is not unique", op, a, b)),
                (O::Unresolved, _) | (_, O::Unresolved) => Ok(None),
                (O::Other, O::Other | O::Untyped) | (O::Untyped, O::Other)
                    if [a, b].iter().flatten().all(|t| bitwise(*t)) =>
                {
                    Ok(None)
                }
                _ => Err(refused("does not exist", op, a, b)),
            }
        }
        BinaryOperator::PGBitwiseShiftLeft | BinaryOperator::PGBitwiseShiftRight => match (x, y) {
            (O::Integer(s), O::Integer(C::Int2 | C::Int4) | O::Untyped) => Ok(Some(s)),
            (O::Untyped, O::Integer(C::Int4)) => Ok(Some(C::Int4)),
            (O::Untyped, O::Untyped | O::Integer(C::Int2)) => {
                Err(refused("is not unique", op, a, b))
            }
            (O::Unresolved, _) | (_, O::Unresolved) => Ok(None),
            (O::Other, O::Integer(C::Int2 | C::Int4)) if a.is_some_and(|t| t.is_bits()) => Ok(None),
            (O::Other, O::Other | O::Untyped) | (O::Untyped, O::Other) => Ok(None),
            _ => Err(refused("does not exist", op, a, b)),
        },
        _ => Ok(None),
    }
}

/// The type a document's operator with keys or a path on its right returns, or its refusal: json's
/// and jsonb's `#>` and `#>>`, jsonb's `#-`, `?|`, `?&` and `-` beside a text[]. None where the
/// operator is another, or leaves its operands to another type's form.
fn document_type(
    op: &BinaryOperator,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
    x: Operand,
    y: Operand,
) -> Result<Option<ColumnType>> {
    use crate::textual::Textual::{Json, Jsonb};
    use BinaryOperator as B;
    use ColumnType as C;
    use Operand as O;
    const JSONB: ColumnType = C::Textual(Jsonb);
    let geometric = |t: Option<ColumnType>| matches!(t, Some(C::Geometric(_)));
    let keys = b.is_some_and(of_strings);
    let (of_json, returns): (bool, fn(ColumnType) -> ColumnType) = match op {
        B::HashArrow => (true, |t| t),
        B::HashLongArrow => (true, |_| C::Text(None)),
        B::HashMinus => (false, |_| JSONB),
        B::QuestionAnd | B::QuestionPipe if !geometric(a) && !geometric(b) => {
            (false, |_| C::Boolean)
        }
        B::Minus if keys => (false, |_| JSONB),
        _ => return Ok(None),
    };
    let right = keys || y == O::Untyped;
    match (a, x) {
        _ if y == O::Unresolved => Ok(None),
        (Some(t @ C::Textual(Jsonb)), _) if right => Ok(Some(returns(t))),
        (Some(t @ C::Textual(Json)), _) if right && of_json => Ok(Some(returns(t))),
        (None, O::Untyped) if !right => Err(refused("does not exist", op, a, b)),
        // A form of one type alone takes a literal as that type; of two, none.
        (None, O::Untyped) if of_json || matches!(op, B::QuestionPipe) && !keys => {
            Err(refused("is not unique", op, a, b))
        }
        (None, O::Untyped) => Ok(Some(returns(JSONB))),
        (None, _) => Ok(None),
        _ => Err(refused("does not exist", op, a, b)),
    }
}

/// Whether a type is an array of a string type, which an array of text takes unasked.
fn of_strings(t: ColumnType) -> bool {
    super::typed::element_of(t).is_some_and(|e| e.is_string())
}

/// A text[]'s elements as text, a NULL's as none, and how many dimensions it has; none where a value
/// is no array of strings.
pub(super) fn text_items(v: &Value) -> Option<(Vec<Option<String>>, usize)> {
    let Value::Composed(ty, c) = v else {
        return None;
    };
    let element = super::typed::element_of(*ty).filter(|e| e.is_string())?;
    let crate::composed::Item::Array { elements, dims, .. } = &c.item else {
        return None;
    };
    Some((
        elements
            .iter()
            .map(|e| super::pipeline::part_value(element, e).text())
            .collect(),
        dims.len(),
    ))
}

/// A quoted literal read as a text[], as `text_items` gives its elements.
pub(super) fn text_array_literal(t: &str) -> Result<(Vec<Option<String>>, usize)> {
    let ty = super::typed::array_of(ColumnType::Text(None));
    let Item::Array { elements, dims, .. } = crate::composed::text_in(ty, t)? else {
        return Ok((Vec::new(), 0));
    };
    Ok((
        elements
            .iter()
            .map(|e| super::pipeline::part_value(ColumnType::Text(None), e).text())
            .collect(),
        dims.len(),
    ))
}

/// Whether `-` deletes a text[]'s keys from a jsonb or a NULL, which no array's own `-` reads.
pub(super) fn deletes_keys(op: &BinaryOperator, x: &Value, y: &Value) -> bool {
    use crate::textual::Textual::Jsonb;
    *op == BinaryOperator::Minus
        && matches!(y, Value::Composed(ty, _) if of_strings(*ty))
        && (x.is_null() || matches!(x, Value::Typed(ColumnType::Textual(Jsonb), _)))
}

/// A quoted literal on the left of an operator that resolved to jsonb's form, read as a jsonb.
pub(super) fn document_literal(
    op: &BinaryOperator,
    x: Value,
    left: &Expr,
    ret: Option<ColumnType>,
) -> Result<Value> {
    use crate::textual::Textual::Jsonb;
    use BinaryOperator as B;
    let form = matches!(
        op,
        B::HashMinus | B::QuestionAnd | B::QuestionPipe | B::Minus
    ) && matches!(ret, Some(ColumnType::Boolean | ColumnType::Textual(Jsonb)));
    Ok(match x {
        Value::Text(t) if form && quoted(left) => Value::Typed(
            ColumnType::Textual(Jsonb),
            crate::textual::text_in(Jsonb, &t)?,
        ),
        x => x,
    })
}

/// A quoted literal on the right of a bit string's shift, read as the integer it takes.
pub(super) fn shift_count(op: &BinaryOperator, x: &Value, y: Value, right: &Expr) -> Result<Value> {
    use BinaryOperator as B;
    Ok(match y {
        Value::Text(t)
            if matches!(op, B::PGBitwiseShiftLeft | B::PGBitwiseShiftRight)
                && matches!(x, Value::Bits(..))
                && quoted(right) =>
        {
            Value::Int(crate::composed::int_in(&t, 32)?)
        }
        y => y,
    })
}

/// Whether a type other than an integer has `&`, `|` or `#`.
fn bitwise(t: ColumnType) -> bool {
    t.is_bits()
        || matches!(
            t,
            ColumnType::Macaddr | ColumnType::Macaddr8 | ColumnType::Geometric(_)
        )
        || crate::spans::is_inet(t)
}

/// The forms of LIKE and ILIKE, as `~~`, `!~~`, `~~*` and `!~~*`: a string's, each side's type one
/// a string casts to, and bytea's, but for ILIKE; refused otherwise, as PostgreSQL finds no operator.
pub(super) fn like_form(
    op: &str,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
    left: &Expr,
    right: &Expr,
) -> Result<()> {
    use Operand as O;
    let bytes = |t: Option<ColumnType>| t == Some(ColumnType::Bytes);
    let of_text = matches!(
        (operand(a, left), operand(b, right)),
        (
            O::Text | O::Untyped | O::Unresolved,
            O::Text | O::Untyped | O::Unresolved
        )
    );
    let of_bytes = !op.ends_with('*')
        && (bytes(a) && matches!(operand(b, right), O::Untyped | O::Unresolved)
            || bytes(b) && matches!(operand(a, left), O::Untyped | O::Unresolved)
            || bytes(a) && bytes(b));
    let internal = |t: Option<ColumnType>| matches!(t, Some(ColumnType::Internal(_)));
    if of_text || of_bytes || internal(a) || internal(b) {
        return Ok(());
    }
    Err(Error::new(format!(
        "operator does not exist: {} {op} {}",
        named(a),
        named(b)
    )))
}

/// An integer's width in bits.
fn bits_of(t: ColumnType) -> u32 {
    match t {
        ColumnType::Int2 => 16,
        ColumnType::Int4 => 32,
        _ => 64,
    }
}

/// An operand of an integer's operator, a quoted literal read by the input of the type it takes.
fn integer_of(v: &Value, e: &Expr, width: u32) -> Result<Option<i64>> {
    Ok(match v {
        Value::Null => None,
        Value::Int(x) => Some(*x),
        Value::Text(t) if quoted(e) => Some(crate::composed::int_in(t, width)?),
        v => return Err(Error::new(format!("{v:?} is no integer"))),
    })
}

/// The integers' `&`, `|`, `#`, `<<` and `>>`, as `int2and` and its kin compute them: over a NULL,
/// NULL; a shift by what the machine's own shift of its width takes of its count, an integer's and
/// a smallint's in 32 bits. None where neither side is an integer.
pub(super) fn of_integers(
    op: &BinaryOperator,
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
    typed: [Option<ColumnType>; 2],
) -> Result<Option<Value>> {
    use BinaryOperator as B;
    if !matches!(
        op,
        B::BitwiseAnd
            | B::BitwiseOr
            | B::PGBitwiseXor
            | B::PGBitwiseShiftLeft
            | B::PGBitwiseShiftRight
    ) {
        return Ok(None);
    }
    let integral = |v: &Value, t: Option<ColumnType>| {
        matches!(v, Value::Int(_)) || t.is_some_and(|t| t.is_integer())
    };
    let shift = matches!(op, B::PGBitwiseShiftLeft | B::PGBitwiseShiftRight);
    // A shift of a bit string takes an integer on its right alone.
    if !integral(x, typed[0]) && !(integral(y, typed[1]) && !matches!(x, Value::Bits(..) if shift))
    {
        return Ok(None);
    }
    let other = |v: &Value, e: &Expr| {
        !integral(v, None) && !v.is_null() && !(matches!(v, Value::Text(_)) && quoted(e))
    };
    if other(x, left) || other(y, right) {
        return Err(Error::new(format!(
            "operator does not exist: {} {op} {}",
            super::pipeline::kind_name(x),
            super::pipeline::kind_name(y)
        )));
    }
    let by_value = |v: &Value, t: Option<ColumnType>| match (v, t) {
        (_, Some(t)) => Some(t),
        (Value::Int(x), None) if i32::try_from(*x).is_ok() => Some(ColumnType::Int4),
        (Value::Int(_), None) => Some(ColumnType::Int8),
        _ => None,
    };
    let (s, t) = (by_value(x, typed[0]), by_value(y, typed[1]));
    let width = match shift {
        true => s.map_or(32, bits_of),
        false => s.into_iter().chain(t).map(bits_of).max().unwrap_or(32),
    };
    let (a, b) = (
        integer_of(x, left, width)?,
        integer_of(y, right, if shift { 32 } else { width })?,
    );
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(Some(Value::Null));
    };
    Ok(Some(Value::Int(match op {
        B::BitwiseAnd => a & b,
        B::BitwiseOr => a | b,
        B::PGBitwiseXor => a ^ b,
        // The count in 32 bits, as the machine takes it.
        B::PGBitwiseShiftLeft => match width {
            16 => i64::from((a as i32).wrapping_shl(b as u32) as i16),
            32 => i64::from((a as i32).wrapping_shl(b as u32)),
            _ => a.wrapping_shl(b as u32),
        },
        _ => match width {
            16 => i64::from((a as i32).wrapping_shr(b as u32) as i16),
            32 => i64::from((a as i32).wrapping_shr(b as u32)),
            _ => a.wrapping_shr(b as u32),
        },
    })))
}

/// A `"char"` as the text it is cast to where a string's operator meets it; any other value as it is.
pub(super) fn char_as_text(v: Value) -> Value {
    match v {
        Value::Scalar(ColumnType::Char, _) => Value::Text(v.text().unwrap_or_default()),
        v => v,
    }
}

/// `^` as `dpow` or `numeric_power`, by the type it resolved to.
pub(super) fn power(
    x: Value,
    y: Value,
    typed: [Option<ColumnType>; 2],
    ret: Option<ColumnType>,
) -> Result<Value> {
    Ok(super::calls::numbers("power", &[x, y], &typed, ret)?.unwrap_or(Value::Null))
}

/// `@@` of text with text or a tsquery, as `ts_match_tt` and `ts_match_tq` read them under the file's
/// configuration: the text a vector of it, and a text on the right a query as `plainto_tsquery`
/// makes one. None where the operator is another, or its left side a vector or a query.
pub(super) fn text_matched(
    op: &BinaryOperator,
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
    casings: &super::pipeline::Casings,
) -> Result<Option<Value>> {
    if *op != BinaryOperator::AtAt {
        return Ok(None);
    }
    let text = |v: &Value| match char_as_text(v.clone()) {
        Value::Text(t) => Some(t),
        Value::Str(ty, s) => Some(crate::strings::text_of(ty, &s).to_string()),
        _ => None,
    };
    let tsquery = |v: &Value| matches!(v, Value::Typed(t, _) if *t == TSQUERY);
    let beside_text = text(y).is_some() || y.is_null() || tsquery(y);
    let of_text = text(x).is_some() && beside_text;
    // A NULL of text, or a literal NULL beside one.
    if !of_text && !(x.is_null() && (text(y).is_some() || tsquery(y) || y.is_null())) {
        return Ok(None);
    }
    let _ = (left, right);
    let (Some(a), false) = (text(x), y.is_null()) else {
        return Ok(Some(Value::Null));
    };
    let config = casings.text_search()?.config();
    let vector = crate::tsearch::to_tsvector(config, &a)?;
    let query = match y {
        Value::Typed(_, q) => q.clone(),
        y => crate::tsearch::plainto_tsquery(config, &text(y).unwrap_or_default())?,
    };
    Ok(Some(Value::Bool(crate::tsearch::ts_match(&vector, &query))))
}

/// A string's bytes as a pattern comparison reads them: a bpchar's, and a quoted literal's or a
/// varchar's beside one, without their trailing blanks, as `bpchar_pattern_lt` reads each and a
/// bpchar's cast to text leaves it.
fn pattern_bytes(
    v: &Value,
    e: &Expr,
    t: Option<ColumnType>,
    other: &Value,
) -> Result<Option<Vec<u8>>> {
    let bpchar = |v: &Value| matches!(v, Value::Str(ColumnType::Bpchar(_), _));
    let varchar = matches!(t, Some(ColumnType::Text(Some(_))));
    Ok(match v {
        Value::Null => None,
        Value::Text(t) if (quoted(e) || varchar) && bpchar(other) => {
            Some(crate::strings::bpchar_key(t.as_bytes()).to_vec())
        }
        Value::Text(t) => Some(t.clone().into_bytes()),
        Value::Str(ColumnType::Bpchar(_), s) => {
            Some(crate::strings::bpchar_key(s.as_bytes()).to_vec())
        }
        Value::Str(_, s) => Some(s.clone().into_bytes()),
        _ => {
            return Err(Error::new(format!(
                "{v:?} is not a string, beside a pattern comparison"
            )))
        }
    })
}

/// `~<~` and its kin, as `text_pattern_lt` and `bpchar_pattern_lt` and their kin compare: the bytes
/// in their order, under no collation, a string before what it begins. None where the operator is
/// none of them.
pub(super) fn pattern_compared(
    op: &BinaryOperator,
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
    typed: [Option<ColumnType>; 2],
) -> Result<Option<Value>> {
    let Some(cmp) = pattern_comparison(op) else {
        return Ok(None);
    };
    let (x, y) = (&char_as_text(x.clone()), &char_as_text(y.clone()));
    let (a, b) = (
        pattern_bytes(x, left, typed[0], y)?,
        pattern_bytes(y, right, typed[1], x)?,
    );
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(Some(Value::Null));
    };
    let o = a.cmp(&b);
    Ok(Some(Value::Bool(match cmp {
        BinaryOperator::Lt => o == Ordering::Less,
        BinaryOperator::LtEq => o != Ordering::Greater,
        BinaryOperator::GtEq => o != Ordering::Less,
        _ => o == Ordering::Greater,
    })))
}
