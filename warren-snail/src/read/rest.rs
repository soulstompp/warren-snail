//! The functions a pipeline answers by name that are no other family's, each as PostgreSQL 19
//! computes it: the geometric types' (geo_ops.c), the network addresses' (network.c, mac.c,
//! mac8.c), the bit strings' (varbit.c), the enums' (enum.c), the uuids' (uuid.c), the settings'
//! (guc_funcs.c) of the fixed settings the snail answers under, and the rest: a value's type and a
//! type's name, an aclitem made, a snapshot's and a tid's parts, and the keywords.
//!
//! A call is resolved to one of the function's forms as PostgreSQL resolves it: an argument of a
//! type cast to a parameter's implicitly meets it, a quoted literal or a NULL of no type meets any,
//! the form with the most arguments of the parameter's own type is taken, then an inet over a cidr
//! for a literal; where more than one is left, the call is not unique.

use super::pipeline::{composed, kind_name, part_of, part_value, type_of_value, Value};
use crate::catalog::ColumnType;
use crate::composed::Item;
use crate::geo::{GeoArg, GeoKind, GeoValue};
use crate::read::statement::Datum;
use crate::regs::RegKind;
use crate::textual::Textual;
use crate::{Error, Result};

/// A parameter of a form: a type; a bit string, bit or bit varying; any enum, one type across the
/// form; any value, of no type too; any value of a type.
#[derive(Clone, Copy, Debug, PartialEq)]
enum P {
    T(ColumnType),
    Bits,
    Enum,
    Any,
    Element,
}

/// What a form returns: a type; a bit string of its own length; its enum's or its value's type; an
/// array of its enum; rows of the keywords.
#[derive(Clone, Copy, Debug, PartialEq)]
enum R {
    T(ColumnType),
    Bit,
    First,
    ArrayOfFirst,
    Rows,
}

/// A form of a function: its name, its parameters and what it returns.
type Form = (&'static str, &'static [P], R);

const POINT: P = P::T(ColumnType::Geometric(GeoKind::Point));
const LINE: P = P::T(ColumnType::Geometric(GeoKind::Line));
const LSEG: P = P::T(ColumnType::Geometric(GeoKind::Lseg));
const BOX: P = P::T(ColumnType::Geometric(GeoKind::Box));
const PATH: P = P::T(ColumnType::Geometric(GeoKind::Path));
const POLYGON: P = P::T(ColumnType::Geometric(GeoKind::Polygon));
const CIRCLE: P = P::T(ColumnType::Geometric(GeoKind::Circle));
const F8: P = P::T(ColumnType::Float8);
const I4: P = P::T(ColumnType::Int4);
const I8: P = P::T(ColumnType::Int8);
const TEXT: P = P::T(ColumnType::Text(None));
const BOOL: P = P::T(ColumnType::Boolean);
const OID: P = P::T(ColumnType::Oid);
const INET: P = P::T(ColumnType::Textual(Textual::Inet));
const CIDR: P = P::T(ColumnType::Textual(Textual::Cidr));
const MAC: P = P::T(ColumnType::Macaddr);
const MAC8: P = P::T(ColumnType::Macaddr8);
const UUID: P = P::T(ColumnType::Uuid);
const INTERVAL: P = P::T(ColumnType::Interval {
    range: crate::times::INTERVAL_FULL_RANGE,
    precision: None,
});
const REGTYPE: P = P::T(ColumnType::Reg(RegKind::Type, crate::regs::LATEST));
const SNAPSHOT: P = P::T(ColumnType::Textual(Textual::PgSnapshot));
const TXID_SNAPSHOT: P = P::T(ColumnType::Textual(Textual::TxidSnapshot));
const XID8: P = P::T(ColumnType::Xid8);
const TID: P = P::T(ColumnType::Tid);

/// The type a parameter names.
const fn ty(p: P) -> ColumnType {
    match p {
        P::T(t) => t,
        _ => ColumnType::Boolean,
    }
}

/// Every form the snail answers, by PostgreSQL's pg_proc on 19.
const FORMS: &[Form] = &[
    ("area", &[BOX], R::T(ColumnType::Float8)),
    ("area", &[CIRCLE], R::T(ColumnType::Float8)),
    ("area", &[PATH], R::T(ColumnType::Float8)),
    ("box", &[POINT, POINT], R::T(ty(BOX))),
    ("center", &[BOX], R::T(ty(POINT))),
    ("center", &[CIRCLE], R::T(ty(POINT))),
    ("circle", &[POINT, F8], R::T(ty(CIRCLE))),
    ("diagonal", &[BOX], R::T(ty(LSEG))),
    ("diameter", &[CIRCLE], R::T(ColumnType::Float8)),
    ("height", &[BOX], R::T(ColumnType::Float8)),
    ("isclosed", &[PATH], R::T(ColumnType::Boolean)),
    ("ishorizontal", &[LINE], R::T(ColumnType::Boolean)),
    ("ishorizontal", &[LSEG], R::T(ColumnType::Boolean)),
    ("ishorizontal", &[POINT, POINT], R::T(ColumnType::Boolean)),
    ("isopen", &[PATH], R::T(ColumnType::Boolean)),
    ("isparallel", &[LINE, LINE], R::T(ColumnType::Boolean)),
    ("isparallel", &[LSEG, LSEG], R::T(ColumnType::Boolean)),
    ("isperp", &[LINE, LINE], R::T(ColumnType::Boolean)),
    ("isperp", &[LSEG, LSEG], R::T(ColumnType::Boolean)),
    ("isvertical", &[LINE], R::T(ColumnType::Boolean)),
    ("isvertical", &[LSEG], R::T(ColumnType::Boolean)),
    ("isvertical", &[POINT, POINT], R::T(ColumnType::Boolean)),
    ("length", &[LSEG], R::T(ColumnType::Float8)),
    ("length", &[PATH], R::T(ColumnType::Float8)),
    ("line", &[POINT, POINT], R::T(ty(LINE))),
    ("lseg", &[POINT, POINT], R::T(ty(LSEG))),
    ("npoints", &[PATH], R::T(ColumnType::Int4)),
    ("npoints", &[POLYGON], R::T(ColumnType::Int4)),
    ("pclose", &[PATH], R::T(ty(PATH))),
    ("point", &[F8, F8], R::T(ty(POINT))),
    ("polygon", &[I4, CIRCLE], R::T(ty(POLYGON))),
    ("popen", &[PATH], R::T(ty(PATH))),
    ("radius", &[CIRCLE], R::T(ColumnType::Float8)),
    ("slope", &[POINT, POINT], R::T(ColumnType::Float8)),
    ("width", &[BOX], R::T(ColumnType::Float8)),
    ("abbrev", &[CIDR], R::T(ColumnType::Text(None))),
    ("abbrev", &[INET], R::T(ColumnType::Text(None))),
    ("broadcast", &[INET], R::T(ty(INET))),
    ("family", &[INET], R::T(ColumnType::Int4)),
    ("host", &[INET], R::T(ColumnType::Text(None))),
    ("hostmask", &[INET], R::T(ty(INET))),
    ("macaddr8_set7bit", &[MAC8], R::T(ColumnType::Macaddr8)),
    ("masklen", &[INET], R::T(ColumnType::Int4)),
    ("netmask", &[INET], R::T(ty(INET))),
    ("network", &[INET], R::T(ty(CIDR))),
    ("set_masklen", &[CIDR, I4], R::T(ty(CIDR))),
    ("set_masklen", &[INET, I4], R::T(ty(INET))),
    ("trunc", &[MAC], R::T(ColumnType::Macaddr)),
    ("trunc", &[MAC8], R::T(ColumnType::Macaddr8)),
    ("bit_count", &[P::Bits], R::T(ColumnType::Int8)),
    ("bit_length", &[P::Bits], R::T(ColumnType::Int4)),
    ("get_bit", &[P::Bits, I4], R::T(ColumnType::Int4)),
    ("length", &[P::Bits], R::T(ColumnType::Int4)),
    ("octet_length", &[P::Bits], R::T(ColumnType::Int4)),
    ("overlay", &[P::Bits, P::Bits, I4], R::Bit),
    ("overlay", &[P::Bits, P::Bits, I4, I4], R::Bit),
    ("position", &[P::Bits, P::Bits], R::T(ColumnType::Int4)),
    ("set_bit", &[P::Bits, I4, I4], R::Bit),
    ("substring", &[P::Bits, I4], R::Bit),
    ("substring", &[P::Bits, I4, I4], R::Bit),
    ("enum_first", &[P::Enum], R::First),
    ("enum_last", &[P::Enum], R::First),
    ("enum_range", &[P::Enum], R::ArrayOfFirst),
    ("enum_range", &[P::Enum, P::Enum], R::ArrayOfFirst),
    ("enum_send", &[P::Enum], R::T(ColumnType::Bytes)),
    ("gen_random_uuid", &[], R::T(ColumnType::Uuid)),
    (
        "uuid_extract_timestamp",
        &[UUID],
        R::T(ColumnType::Timestamptz),
    ),
    ("uuid_extract_version", &[UUID], R::T(ColumnType::Int2)),
    ("uuidv4", &[], R::T(ColumnType::Uuid)),
    ("uuidv7", &[], R::T(ColumnType::Uuid)),
    ("uuidv7", &[INTERVAL], R::T(ColumnType::Uuid)),
    ("current_setting", &[TEXT], R::T(ColumnType::Text(None))),
    (
        "current_setting",
        &[TEXT, BOOL],
        R::T(ColumnType::Text(None)),
    ),
    (
        "set_config",
        &[TEXT, TEXT, BOOL],
        R::T(ColumnType::Text(None)),
    ),
    ("error_on_null", &[P::Element], R::First),
    ("format_type", &[OID, I4], R::T(ColumnType::Text(None))),
    ("nameconcatoid", &[P::T(ColumnType::Name), OID], R::T(ColumnType::Name)),
    (
        "makeaclitem",
        &[OID, OID, TEXT, BOOL],
        R::T(ColumnType::Textual(Textual::Aclitem)),
    ),
    ("pg_basetype", &[REGTYPE], R::T(ty(REGTYPE))),
    ("pg_collation_for", &[P::Any], R::T(ColumnType::Text(None))),
    ("pg_get_keywords", &[], R::Rows),
    ("pg_snapshot_xip", &[SNAPSHOT], R::T(ColumnType::Xid8)),
    ("pg_snapshot_xmax", &[SNAPSHOT], R::T(ColumnType::Xid8)),
    ("pg_snapshot_xmin", &[SNAPSHOT], R::T(ColumnType::Xid8)),
    ("pg_typeof", &[P::Any], R::T(ty(REGTYPE))),
    (
        "pg_visible_in_snapshot",
        &[XID8, SNAPSHOT],
        R::T(ColumnType::Boolean),
    ),
    ("tid_block", &[TID], R::T(ColumnType::Int8)),
    ("tid_offset", &[TID], R::T(ColumnType::Int4)),
    (
        "txid_snapshot_xip",
        &[TXID_SNAPSHOT],
        R::T(ColumnType::Int8),
    ),
    (
        "txid_snapshot_xmax",
        &[TXID_SNAPSHOT],
        R::T(ColumnType::Int8),
    ),
    (
        "txid_snapshot_xmin",
        &[TXID_SNAPSHOT],
        R::T(ColumnType::Int8),
    ),
    (
        "txid_visible_in_snapshot",
        &[I8, TXID_SNAPSHOT],
        R::T(ColumnType::Boolean),
    ),
];

/// The names PostgreSQL also gives functions of strings and bytea and of the numbers, of the same
/// number of arguments: a call of one is this module's only where an argument is of a type its
/// forms here name.
fn shared(name: &str) -> bool {
    matches!(
        name,
        "length"
            | "octet_length"
            | "bit_length"
            | "position"
            | "substring"
            | "overlay"
            | "get_bit"
            | "set_bit"
            | "bit_count"
            | "trunc"
    )
}

/// Whether PostgreSQL gives a function of this name and number of arguments to other types: the
/// casts among the geometric types, of one argument; trunc of a numeric to its places, of two.
fn other_arities(name: &str, n: usize) -> bool {
    match name {
        "point" | "box" | "circle" | "line" | "lseg" | "polygon" => n == 1,
        "trunc" => n == 2,
        _ => false,
    }
}

/// The functions that return a set: the builder reads their rows from `set` and `rows`.
fn returns_set(name: &str) -> bool {
    matches!(
        name,
        "pg_snapshot_xip" | "txid_snapshot_xip" | "pg_get_keywords"
    )
}

/// The functions that take a NULL argument as a value rather than answering NULL.
fn takes_null(name: &str) -> bool {
    matches!(
        name,
        "enum_first"
            | "enum_last"
            | "enum_range"
            | "pg_typeof"
            | "error_on_null"
            | "format_type"
            | "set_config"
    )
}

/// Whether an argument of type `t` meets a parameter of type `p`: Some(true) where `t` is `p`,
/// Some(false) where PostgreSQL casts it to `p` implicitly, none where it does not.
fn meets(t: ColumnType, p: ColumnType) -> Option<bool> {
    use ColumnType as C;
    Some(match (t, p) {
        (C::Text(None), C::Text(None)) => true,
        (C::Text(Some(_)) | C::Bpchar(_) | C::Name | C::Char, C::Text(None)) => false,
        (C::Int2, C::Int4) | (C::Int2 | C::Int4, C::Int8) => false,
        (C::Int2 | C::Int4 | C::Int8 | C::Float4, C::Float8) => false,
        (t, C::Float8) if t.is_numeric() => false,
        (C::Int2 | C::Int4 | C::Int8 | C::Reg(..), C::Oid) => false,
        (C::Reg(a, _), C::Reg(b, _)) => {
            if a == b {
                true
            } else {
                return None;
            }
        }
        (C::Int2 | C::Int4 | C::Int8 | C::Oid, C::Reg(RegKind::Type, _)) => false,
        (C::Textual(Textual::Cidr), C::Textual(Textual::Inet)) => false,
        (C::Macaddr, C::Macaddr8) | (C::Macaddr8, C::Macaddr) => false,
        (C::Interval { .. }, C::Interval { .. }) => true,
        (C::Time(_), C::Interval { .. }) => false,
        (t, p) if t == p => true,
        _ => return None,
    })
}

/// Whether a type is an enum.
fn is_enum(t: ColumnType) -> bool {
    crate::composed::labels(t).is_some()
}

/// How a form takes its arguments: none where one does not meet its parameter; else how many are
/// of the parameter's own type, and whether one meets it only as a NULL or literal of no type.
fn fit(form: &Form, types: &[Option<ColumnType>], open: bool) -> Option<usize> {
    let (_, params, _) = form;
    // The enum a form's enum parameters take, from the arguments that name one.
    let enums: Vec<ColumnType> = params
        .iter()
        .zip(types)
        .filter(|(p, _)| **p == P::Enum)
        .filter_map(|(_, t)| *t)
        .collect();
    if enums.iter().any(|t| *t != enums[0]) {
        return None;
    }
    let mut exact = 0;
    for (p, t) in params.iter().zip(types) {
        match (p, t) {
            (P::Any | P::Element, _) => exact += 1,
            (P::Enum, Some(t)) if is_enum(*t) => exact += 1,
            (P::Enum, None) if open || !enums.is_empty() => {}
            (P::Bits, Some(ColumnType::Bit(_) | ColumnType::Varbit(_))) => exact += 1,
            (P::Bits, None) => {}
            (P::T(p), Some(t)) => exact += usize::from(meets(*t, *p)?),
            (P::T(_), None) => {}
            _ => return None,
        }
    }
    Some(exact)
}

/// How a call resolves.
enum Found {
    /// One form.
    Form(&'static Form),
    /// No function of this module's, by that name and those arguments.
    NotMine,
    /// Some of the arguments' types are not known yet, and they decide the form.
    Open,
}

/// The name an argument's type is written under in PostgreSQL's errors.
fn arg_name(t: Option<ColumnType>) -> String {
    match t {
        None => "unknown".into(),
        Some(ColumnType::NumericText(_) | ColumnType::Numeric { .. }) => "numeric".into(),
        Some(t) => t
            .pg_name()
            .split('(')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// The form a call takes, its arguments of the types given, a type not known yet none: where
/// `open`, one that is still to be resolved, else a literal or a NULL of no type.
fn resolve(name: &str, types: &[Option<ColumnType>], open: bool) -> Result<Found> {
    let named: Vec<&'static Form> = FORMS.iter().filter(|f| f.0 == name).collect();
    if named.is_empty() {
        return Ok(Found::NotMine);
    }
    let args = || {
        types
            .iter()
            .map(|t| arg_name(*t))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let no_form = || {
        Err(Error::new(format!(
            "function {name}({}) does not exist",
            args()
        )))
    };
    if !named.iter().any(|f| f.1.len() == types.len()) && other_arities(name, types.len()) {
        return Ok(Found::NotMine);
    }
    let fits: Vec<(&'static Form, usize)> = named
        .iter()
        .filter(|f| f.1.len() == types.len())
        .filter_map(|f| fit(f, types, open).map(|e| (*f, e)))
        .collect();
    if shared(name) {
        // Its own only where an argument of a known type meets a parameter of a type of its own.
        let own = named.iter().filter(|f| f.1.len() == types.len()).any(|f| {
            f.1.iter().zip(types).any(|(p, t)| match (p, t) {
                (P::Bits, Some(t)) => matches!(t, ColumnType::Bit(_) | ColumnType::Varbit(_)),
                (P::T(p), Some(t)) if *p != ty(I4) && *p != ty(F8) => meets(*t, *p).is_some(),
                _ => false,
            })
        });
        if !own {
            return Ok(Found::NotMine);
        }
    }
    if fits.is_empty() {
        return no_form();
    }
    let most = fits.iter().map(|(_, e)| *e).max().unwrap_or(0);
    let mut left: Vec<&'static Form> = fits
        .iter()
        .filter(|(_, e)| *e == most)
        .map(|(f, _)| *f)
        .collect();
    if left.len() > 1 {
        // A literal of no type where the network addresses' forms differ is an inet, their
        // category's preferred type.
        let inet: Vec<&'static Form> = left
            .iter()
            .copied()
            .filter(|f| {
                f.1.iter()
                    .zip(types)
                    .all(|(p, t)| t.is_some() || *p != CIDR)
            })
            .collect();
        if !inet.is_empty() && inet.len() < left.len() {
            left = inet;
        }
    }
    match left.as_slice() {
        [f] => Ok(Found::Form(f)),
        _ if open => Ok(Found::Open),
        _ => Err(Error::new(format!(
            "function {name}({}) is not unique",
            args()
        ))),
    }
}

/// The type a form returns over its arguments' types; none where it is not known yet.
fn returns(form: &Form, types: &[Option<ColumnType>]) -> Option<ColumnType> {
    let first = || {
        form.1
            .iter()
            .zip(types)
            .find(|(p, t)| matches!(p, P::Enum | P::Element) && t.is_some())
            .and_then(|(_, t)| *t)
    };
    match form.2 {
        R::T(t) => Some(t),
        R::Bit => Some(ColumnType::Bit(0)),
        R::First => first(),
        R::ArrayOfFirst => first().map(super::typed::array_of),
        R::Rows => Some(keyword_record()),
    }
}

/// The record of a row of `pg_get_keywords()`, its OUT parameters' names and types.
pub(super) fn keyword_record() -> ColumnType {
    crate::composed::type_of(crate::composed::Composed::Record {
        name: "record".into(),
        fields: KEYWORD_COLUMNS
            .iter()
            .map(|(n, t)| crate::composed::Field::new(n, *t))
            .collect(),
    })
}

/// The type a function of this module's returns over its arguments' types, or PostgreSQL's
/// refusal where it has no form for them; none where the name is no function of this module's, or
/// the step leaves the type open.
pub(super) fn typed(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    Ok(match resolve(name, args, true)? {
        Found::NotMine => None,
        Found::Open => Some(None),
        Found::Form(f) => Some(returns(f, args)),
    })
}

/// An argument's type: the step's, else its value's; none for a quoted literal or a NULL of no
/// type.
fn arg_type(a: &(Value, bool, Option<ColumnType>)) -> Option<ColumnType> {
    if a.1 {
        return None;
    }
    a.2.or_else(|| type_of_value(&a.0))
}

/// An argument taken as its parameter takes it: a literal read by the parameter type's input,
/// another value cast to it; a bit string's literal as a bit varying; an enum's by its enum.
fn coerce(
    p: P,
    a: &(Value, bool, Option<ColumnType>),
    enum_ty: Option<ColumnType>,
) -> Result<Value> {
    let (v, quoted, _) = a;
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(match p {
        // An integer's bits, as int4's cast to an oid takes them.
        P::T(t @ (ColumnType::Oid | ColumnType::Reg(..)))
            if matches!(arg_type(a), Some(ColumnType::Int2 | ColumnType::Int4)) =>
        {
            match v {
                Value::Int(n) => {
                    Value::Scalar(t, crate::scalars::unsigned_key(u64::from(*n as u32), 32))
                }
                v => part_value(t, &part_of(t, v.clone(), *quoted)?),
            }
        }
        P::T(t) => part_value(t, &part_of(t, v.clone(), *quoted)?),
        P::Bits => match v {
            Value::Text(s) if *quoted => Value::Bits(
                ColumnType::Varbit(None),
                crate::strings::bits_parse(s, None, true)?,
            ),
            v => v.clone(),
        },
        P::Enum => match (v, enum_ty) {
            (Value::Text(_), Some(t)) if *quoted => composed(t, part_of(t, v.clone(), true)?),
            (v, _) => v.clone(),
        },
        P::Any | P::Element => v.clone(),
    })
}

/// The value of a function of this module's over its arguments, each its value, whether it is a
/// quoted literal and the type the step resolved it to; none where the snail answers no function
/// of this module's by that name and those arguments, or the function returns a set.
pub(super) fn call(
    name: &str,
    args: &[(Value, bool, Option<ColumnType>)],
) -> Result<Option<Value>> {
    let types: Vec<Option<ColumnType>> = args.iter().map(arg_type).collect();
    let form = match resolve(name, &types, false)? {
        Found::Form(f) => f,
        Found::NotMine | Found::Open => return Ok(None),
    };
    if returns_set(name) {
        return Ok(None);
    }
    let enum_ty = form
        .1
        .iter()
        .zip(&types)
        .find(|(p, t)| **p == P::Enum && t.is_some())
        .and_then(|(_, t)| *t);
    if form.1.contains(&P::Element) && types[0].is_none() {
        return Err(Error::new(
            "could not determine polymorphic type because input has type unknown",
        ));
    }
    let values = form
        .1
        .iter()
        .zip(args)
        .map(|(p, a)| coerce(*p, a, enum_ty))
        .collect::<Result<Vec<_>>>()?;
    if !takes_null(name) && values.iter().any(Value::is_null) {
        return Ok(Some(Value::Null));
    }
    if let Some(v) = geometric(name, &values)? {
        return Ok(Some(v));
    }
    if let Some(v) = network(name, form, &values)? {
        return Ok(Some(v));
    }
    if let Some(v) = bit_strings(name, &values)? {
        return Ok(Some(v));
    }
    if let Some(t) = enum_ty {
        return enums(name, t, &values).map(Some);
    }
    if let Some(v) = uuids(name, &values)? {
        return Ok(Some(v));
    }
    if let Some(v) = settings(name, &values)? {
        return Ok(Some(v));
    }
    the_rest(name, &types, &values).map(Some)
}

/// The values of a set-returning function of this module's over its arguments, as `call` takes
/// them: a snapshot's transactions in progress; none where it is no such function.
pub(super) fn set(
    name: &str,
    args: &[(Value, bool, Option<ColumnType>)],
) -> Result<Option<Vec<Value>>> {
    if !matches!(name, "pg_snapshot_xip" | "txid_snapshot_xip") {
        return Ok(None);
    }
    let types: Vec<Option<ColumnType>> = args.iter().map(arg_type).collect();
    let form = match resolve(name, &types, false)? {
        Found::Form(f) => f,
        Found::NotMine | Found::Open => return Ok(None),
    };
    let v = coerce(form.1[0], &args[0], None)?;
    if v.is_null() {
        return Ok(Some(Vec::new()));
    }
    let (_, _, xip) = snapshot(&v)?;
    Ok(Some(
        xip.into_iter()
            .map(|x| xid(name == "pg_snapshot_xip", x))
            .collect(),
    ))
}

/// The columns of `pg_get_keywords()`: each one's name and type.
pub(super) const KEYWORD_COLUMNS: [(&str, ColumnType); 5] = [
    ("word", ColumnType::Text(None)),
    ("catcode", ColumnType::Char),
    ("barelabel", ColumnType::Boolean),
    ("catdesc", ColumnType::Text(None)),
    ("baredesc", ColumnType::Text(None)),
];

/// The rows of `pg_get_keywords()`, a keyword a row in the order of PostgreSQL 19's list, each its
/// values in `KEYWORD_COLUMNS`' order; none where `name` is no function of rows of this module's.
pub(super) fn rows(
    name: &str,
    args: &[(Value, bool, Option<ColumnType>)],
) -> Result<Option<Vec<Vec<Value>>>> {
    if name != "pg_get_keywords" {
        return Ok(None);
    }
    if !args.is_empty() {
        let types: Vec<Option<ColumnType>> = args.iter().map(arg_type).collect();
        resolve(name, &types, false)?;
        return Ok(None);
    }
    let mut out = Vec::with_capacity(500);
    for line in include_str!("../kwlist.txt").lines() {
        let mut f = line.split('\t');
        let (word, cat, bare) = (
            f.next().unwrap_or_default(),
            f.next().unwrap_or_default(),
            f.next() == Some("t"),
        );
        let desc = match cat {
            "U" => "unreserved",
            "C" => "unreserved (cannot be function or type name)",
            "T" => "reserved (can be function or type name)",
            _ => "reserved",
        };
        out.push(vec![
            Value::Text(word.into()),
            Value::Scalar(
                ColumnType::Char,
                crate::scalars::unsigned_key(u64::from(cat.as_bytes()[0]), 8),
            ),
            Value::Bool(bare),
            Value::Text(desc.into()),
            Value::Text(
                if bare {
                    "can be bare label"
                } else {
                    "requires AS"
                }
                .into(),
            ),
        ]);
    }
    Ok(Some(out))
}

// The geometric types.

/// A geometric function's value over its arguments, none NULL; none where it is no geometric one.
fn geometric(name: &str, values: &[Value]) -> Result<Option<Value>> {
    let mut args = Vec::with_capacity(values.len());
    for v in values {
        args.push(match v {
            Value::Geometric(g) => GeoArg::Geo(g),
            Value::Double(f) => GeoArg::Float(*f),
            Value::Int(n) => GeoArg::Int(*n as i32),
            _ => return Ok(None),
        });
    }
    let Some(v) = crate::geo::geo_function(name, &args) else {
        return Ok(None);
    };
    Ok(Some(match v? {
        GeoValue::Geo(g) => Value::Geometric(g),
        GeoValue::Bool(b) => Value::Bool(b),
        GeoValue::Float(f) => Value::Double(f),
        GeoValue::Int(n) => Value::Int(i64::from(n)),
        _ => Value::Null,
    }))
}

// The network addresses.

/// A function of the network addresses or the MAC addresses over its arguments, none NULL; none
/// where it is no such function.
fn network(name: &str, form: &Form, values: &[Value]) -> Result<Option<Value>> {
    use crate::inet::NetValue;
    match values {
        [Value::Scalar(t @ (ColumnType::Macaddr | ColumnType::Macaddr8), k)] => {
            let mut b = crate::scalars::fixed_of_key(*t, *k);
            match name {
                "trunc" => b[3..].fill(0),
                "macaddr8_set7bit" => b[0] |= 0x02,
                _ => return Ok(None),
            }
            Ok(Some(Value::Scalar(
                *t,
                crate::scalars::key_of_fixed(*t, &b),
            )))
        }
        [Value::Typed(ColumnType::Textual(Textual::Inet | Textual::Cidr), text), rest @ ..] => {
            let bits = match rest {
                [Value::Int(n)] => Some(*n as i32),
                _ => None,
            };
            let cidr = form.1[0] == CIDR;
            let Some(v) = crate::inet::inet_function(name, text, cidr, bits) else {
                return Ok(None);
            };
            Ok(Some(match v? {
                NetValue::Inet(s) => Value::Typed(ty(INET), s),
                NetValue::Cidr(s) => Value::Typed(ty(CIDR), s),
                NetValue::Text(s) => Value::Text(s),
                NetValue::Int(n) => Value::Int(i64::from(n)),
            }))
        }
        _ => Ok(None),
    }
}

// The bit strings.

/// A bit string's length and its bytes, the last padded with zeros.
fn bits_of(v: &Value) -> Option<(u32, Vec<u8>)> {
    match v {
        Value::Bits(_, k) => Some(crate::strings::bits_of_key(k)),
        _ => None,
    }
}

/// A bit string of `len` bits as a value: a bit of its own length.
fn bit_value(len: u32, bytes: &[u8]) -> Value {
    Value::Bits(ColumnType::Bit(len), crate::strings::bits_key(len, bytes))
}

/// Bits `from` to before `to` of a bit string, counted from 0.
fn bits_between(bytes: &[u8], from: usize, to: usize) -> Vec<u8> {
    let mut out = vec![0u8; (to - from).div_ceil(8)];
    for (i, r) in (from..to).enumerate() {
        if bytes[r / 8] & (0x80 >> (r % 8)) != 0 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

/// Two bit strings one after the other.
fn bits_cat(a: (u32, Vec<u8>), b: (u32, Vec<u8>)) -> (u32, Vec<u8>) {
    let len = a.0 + b.0;
    let mut out = vec![0u8; (len as usize).div_ceil(8)];
    let bit = |x: &(u32, Vec<u8>), r: usize| x.1[r / 8] & (0x80 >> (r % 8)) != 0;
    for r in 0..len as usize {
        let set = if r < a.0 as usize {
            bit(&a, r)
        } else {
            bit(&b, r - a.0 as usize)
        };
        if set {
            out[r / 8] |= 0x80 >> (r % 8);
        }
    }
    (len, out)
}

/// `bitsubstring`: the bits from position `s`, counted from 1, `l` of them, or to the end where
/// none is named.
fn bit_substring(len: u32, bytes: &[u8], s: i32, l: Option<i32>) -> Result<(u32, Vec<u8>)> {
    let bitlen = len as i32;
    let s1 = s.max(1);
    let e1 = match l {
        None => bitlen + 1,
        Some(l) if l < 0 => return Err(Error::new("negative substring length not allowed")),
        Some(l) => match s.checked_add(l) {
            None => bitlen + 1,
            Some(e) => e.min(bitlen + 1),
        },
    };
    if s1 > bitlen || e1 <= s1 {
        return Ok((0, Vec::new()));
    }
    let (from, to) = ((s1 - 1) as usize, (e1 - 1) as usize);
    Ok(((to - from) as u32, bits_between(bytes, from, to)))
}

/// `bitposition`: where `sub` first starts in `s`, counted from 1, 0 where it does not; compared a
/// byte at a time, each shifted by 0 to 7 bits, the last bytes under their padding's masks.
fn bit_position(s: (u32, &[u8]), sub: (u32, &[u8])) -> i64 {
    let ((str_len, st), (sub_len, sb)) = (s, sub);
    if str_len == 0 || sub_len > str_len {
        return 0;
    }
    if sub_len == 0 {
        return 1;
    }
    let pad = |len: u32, n: usize| (n as u32) * 8 - len;
    let end_mask = (0xffu32 << pad(sub_len, sb.len())) as u8;
    let str_mask = (0xffu32 << pad(str_len, st.len())) as u8;
    for i in 0..st.len() - sb.len() + 1 {
        for is in 0..8u32 {
            let mut is_match = true;
            let mut p = i;
            let mut mask1 = (0xffu32 >> is) as u8;
            let mut mask2 = !mask1;
            let mut s = 0;
            while is_match && s < sb.len() {
                let mut cmp = (u32::from(sb[s]) >> is) as u8;
                if s == sb.len() - 1 {
                    mask1 &= (u32::from(end_mask) >> is) as u8;
                    if p == st.len() - 1 {
                        if mask1 & !str_mask != 0 {
                            is_match = false;
                            break;
                        }
                        mask1 &= str_mask;
                    }
                }
                is_match = (cmp ^ st[p]) & mask1 == 0;
                if !is_match {
                    break;
                }
                p += 1;
                if p == st.len() {
                    mask2 = (u32::from(end_mask) << (8 - is)) as u8;
                    is_match = mask2 == 0;
                    break;
                }
                cmp = (u32::from(sb[s]) << (8 - is)) as u8;
                if s == sb.len() - 1 {
                    mask2 &= (u32::from(end_mask) << (8 - is)) as u8;
                    if p == st.len() - 1 {
                        if mask2 & !str_mask != 0 {
                            is_match = false;
                            break;
                        }
                        mask2 &= str_mask;
                    }
                }
                is_match = (cmp ^ st[p]) & mask2 == 0;
                s += 1;
            }
            if is_match {
                return (i * 8) as i64 + i64::from(is) + 1;
            }
        }
    }
    0
}

/// A function of the bit strings over its arguments, none NULL; none where it is no such function.
fn bit_strings(name: &str, values: &[Value]) -> Result<Option<Value>> {
    let Some((len, bytes)) = values.first().and_then(bits_of) else {
        return Ok(None);
    };
    let int = |i: usize| match values.get(i) {
        Some(Value::Int(n)) => Some(*n as i32),
        _ => None,
    };
    let index = |n: i32| {
        if n < 0 || n >= len as i32 {
            Err(Error::new(format!(
                "bit index {n} out of valid range (0..{})",
                len as i32 - 1
            )))
        } else {
            Ok(n as usize)
        }
    };
    Ok(Some(match name {
        "bit_count" => Value::Int(bytes.iter().map(|b| i64::from(b.count_ones())).sum()),
        "bit_length" | "length" => Value::Int(i64::from(len)),
        "octet_length" => Value::Int(bytes.len() as i64),
        "get_bit" => {
            let n = index(int(1).unwrap_or(-1))?;
            Value::Int(i64::from(bytes[n / 8] & (0x80 >> (n % 8)) != 0))
        }
        "set_bit" => {
            let n = index(int(1).unwrap_or(-1))?;
            let mut out = bytes.clone();
            match int(2) {
                Some(0) => out[n / 8] &= !(0x80 >> (n % 8)),
                Some(1) => out[n / 8] |= 0x80 >> (n % 8),
                _ => return Err(Error::new("new bit must be 0 or 1")),
            }
            bit_value(len, &out)
        }
        "substring" => {
            let (l, b) = bit_substring(len, &bytes, int(1).unwrap_or(1), int(2))?;
            bit_value(l, &b)
        }
        "position" => {
            let Some((sl, sb)) = values.get(1).and_then(bits_of) else {
                return Ok(None);
            };
            Value::Int(bit_position((len, &bytes), (sl, &sb)))
        }
        "overlay" => {
            let Some(placed) = values.get(1).and_then(bits_of) else {
                return Ok(None);
            };
            let sp = int(2).unwrap_or(1);
            let sl = int(3).unwrap_or(placed.0 as i32);
            if sp <= 0 {
                return Err(Error::new("negative substring length not allowed"));
            }
            let end = sp
                .checked_add(sl)
                .ok_or_else(|| Error::new("integer out of range"))?;
            let head = bit_substring(len, &bytes, 1, Some(sp - 1))?;
            let tail = bit_substring(len, &bytes, end, None)?;
            let (l, b) = bits_cat(bits_cat(head, placed), tail);
            bit_value(l, &b)
        }
        _ => return Ok(None),
    }))
}

// The enums.

/// An enum's value of rank `k`.
fn enum_value(ty: ColumnType, k: usize) -> Value {
    composed(ty, Item::Scalar(Datum::Int(k as i64)))
}

/// An enum's rank, where the value is one.
fn rank(v: &Value) -> Option<usize> {
    match v {
        Value::Composed(_, c) => match &c.item {
            Item::Scalar(Datum::Int(k)) => Some(*k as usize),
            _ => None,
        },
        _ => None,
    }
}

/// A function of the enums over its arguments, the enum `ty` theirs.
fn enums(name: &str, ty: ColumnType, values: &[Value]) -> Result<Value> {
    let labels = crate::composed::labels(ty).unwrap_or_default();
    let empty = || Error::new(format!("enum {} contains no values", ty.pg_name()));
    Ok(match name {
        "enum_first" if labels.is_empty() => return Err(empty()),
        "enum_first" => enum_value(ty, 0),
        "enum_last" if labels.is_empty() => return Err(empty()),
        "enum_last" => enum_value(ty, labels.len() - 1),
        "enum_send" => match values.first().and_then(rank) {
            Some(k) => Value::Bytes(labels[k].as_bytes().to_vec()),
            None => Value::Null,
        },
        _ => {
            // From the lower bound, or the first; through the upper, or the last; none where the
            // upper comes first.
            let last = labels.len().saturating_sub(1);
            let (lower, upper) = match values {
                [a, b] => (rank(a).unwrap_or(0), rank(b).unwrap_or(last)),
                _ => (0, last),
            };
            let ranks: Vec<usize> = if labels.is_empty() || upper < lower {
                Vec::new()
            } else {
                (lower..=upper).collect()
            };
            let n = ranks.len() as i32;
            let elements: Vec<Item> = ranks
                .into_iter()
                .map(|k| Item::Scalar(Datum::Int(k as i64)))
                .collect();
            let (dims, lower) = if n == 0 {
                (Vec::new(), Vec::new())
            } else {
                (vec![n], vec![1])
            };
            composed(
                super::typed::array_of(ty),
                Item::Array {
                    elements,
                    dims,
                    lower,
                },
            )
        }
    })
}

// The uuids.

/// The microseconds from 1582-10-15, a version 1 uuid's epoch, to 1970-01-01.
const GREGORIAN_TO_UNIX_US: i64 = 12_219_292_800 * 1_000_000;

/// A function of the uuids over its arguments, none NULL; none where it is no such function.
fn uuids(name: &str, values: &[Value]) -> Result<Option<Value>> {
    use crate::scalars::{uuid_bytes, uuid_v4, uuid_v7, uuid_v7_shifted};
    Ok(Some(match (name, values) {
        ("gen_random_uuid" | "uuidv4", []) => Value::Uuid(uuid_v4()?),
        ("uuidv7", []) => Value::Uuid(uuid_v7()?),
        ("uuidv7", [Value::Interval(shift)]) => {
            if !shift.is_finite() {
                return Err(Error::new(
                    "interval out of range for UUID version 7\nDETAIL:  UUID version 7 does not support infinite intervals.",
                ));
            }
            Value::Uuid(uuid_v7_shifted(|us| {
                crate::times::timestamptz_pl_interval(us, *shift)
            })?)
        }
        ("uuid_extract_version" | "uuid_extract_timestamp", [Value::Uuid(k)]) => {
            let d = uuid_bytes(*k);
            if d[8] & 0xc0 != 0x80 {
                return Ok(Some(Value::Null));
            }
            let version = d[6] >> 4;
            if name == "uuid_extract_version" {
                return Ok(Some(Value::Int(i64::from(version))));
            }
            let b = |i: usize| u64::from(d[i]);
            let us = match version {
                1 => {
                    let tms = (b(0) << 24)
                        + (b(1) << 16)
                        + (b(2) << 8)
                        + b(3)
                        + (b(4) << 40)
                        + (b(5) << 32)
                        + ((b(6) & 0xf) << 56)
                        + (b(7) << 48);
                    (tms / 10) as i64 - GREGORIAN_TO_UNIX_US
                }
                7 => {
                    let ms = (0..6).fold(0u64, |acc, i| (acc << 8) | b(i));
                    ms as i64 * 1000
                }
                _ => return Ok(Some(Value::Null)),
            };
            Value::Scalar(ColumnType::Timestamptz, us)
        }
        _ => return Ok(None),
    }))
}

// The settings.

/// The settings the snail answers under, each its name as PostgreSQL reads it and its value as SHOW
/// writes it.
const FIXED: [(&str, &str); 6] = [
    ("timezone", "UTC"),
    ("datestyle", "ISO, MDY"),
    ("intervalstyle", "postgres"),
    ("extra_float_digits", "1"),
    ("bytea_output", "hex"),
    ("standard_conforming_strings", "on"),
];

/// The fixed setting `name` names, by PostgreSQL's case-insensitive comparison.
fn fixed(name: &str) -> Option<(&'static str, &'static str)> {
    FIXED
        .iter()
        .copied()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// The refusal of a setting the snail does not hold fixed.
fn not_fixed(name: &str) -> Error {
    Error::new(format!(
        "setting \"{name}\": the snail answers under its fixed settings only (TimeZone, DateStyle, \
         IntervalStyle, extra_float_digits, bytea_output, standard_conforming_strings), and this is \
         none of them"
    ))
}

/// Whether `value` sets the fixed setting `name` to the value it holds, as its check reads it:
/// TimeZone's UTC in any case; DateStyle's ISO and MDY, or either alone; an enum's in any case;
/// extra_float_digits' 1 as an integer setting reads it; a boolean's true by its spellings.
fn sets_fixed(name: &str, value: &str) -> bool {
    match name {
        "timezone" => value.eq_ignore_ascii_case("utc"),
        "datestyle" => {
            let toks: Vec<&str> = value
                .split(',')
                .map(|t| t.trim_matches(is_c_space))
                .collect();
            !toks.is_empty()
                && toks.iter().all(|t| {
                    let t = t.to_ascii_lowercase();
                    t == "iso" || t == "mdy" || t == "us" || t.starts_with("noneuro")
                })
        }
        "intervalstyle" => value.eq_ignore_ascii_case("postgres"),
        "bytea_output" => value.eq_ignore_ascii_case("hex"),
        "standard_conforming_strings" => {
            value.trim() == value && crate::read::truth(value) == Some(true)
        }
        "extra_float_digits" => {
            let t = value.trim_matches(is_c_space);
            let t = t.strip_prefix('+').unwrap_or(t);
            let digits = t.trim_start_matches('0');
            t.bytes().all(|b| b.is_ascii_digit()) && digits == "1"
        }
        _ => false,
    }
}

/// White space as C's `isspace` takes it.
fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// A text argument's string.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        v if v.is_null() => None,
        v => v.text(),
    }
}

/// current_setting and set_config of the fixed settings; none where it is neither.
fn settings(name: &str, values: &[Value]) -> Result<Option<Value>> {
    // A writer's session hands its own settings to the statement.
    match name {
        "current_setting" => {
            let n = text_of(&values[0]).unwrap_or_default();
            let missing_ok = matches!(values.get(1), Some(Value::Bool(true)));
            if let Some(got) = super::settings::current_setting(&n, missing_ok) {
                return Ok(Some(got?.map_or(Value::Null, Value::Text)));
            }
        }
        "set_config" => {
            let Some(n) = text_of(&values[0]) else {
                return Err(Error::new("SET requires parameter name"));
            };
            let local = matches!(values.get(2), Some(Value::Bool(true)));
            if let Some(got) = super::settings::set_config(&n, text_of(&values[1]).as_deref(), local)
            {
                return Ok(Some(Value::Text(got?)));
            }
        }
        _ => {}
    }
    Ok(Some(match name {
        "current_setting" => {
            let n = text_of(&values[0]).unwrap_or_default();
            match fixed(&n) {
                Some((_, v)) => Value::Text(v.into()),
                None => return Err(not_fixed(&n)),
            }
        }
        "set_config" => {
            let Some(n) = text_of(&values[0]) else {
                return Err(Error::new("SET requires parameter name"));
            };
            let Some((key, v)) = fixed(&n) else {
                return Err(not_fixed(&n));
            };
            match text_of(&values[1]) {
                Some(value) if sets_fixed(key, &value) => Value::Text(v.into()),
                Some(value) => {
                    return Err(Error::new(format!(
                    "set_config: setting \"{n}\" to \"{value}\": the snail answers under {n} {v} \
                         only"
                )))
                }
                None => {
                    return Err(Error::new(format!(
                        "set_config: resetting \"{n}\" takes the server's default, and the snail \
                         answers under {n} {v} only"
                    )))
                }
            }
        }
        _ => return Ok(None),
    }))
}

// The rest.

/// The release whose catalog names a type the functions here return.
const RELEASE: u8 = crate::regs::LATEST;

/// The OID of `unknown`, a literal's type before it is resolved.
const UNKNOWN_OID: u32 = 705;

/// A type's name as PostgreSQL's type names read it, its modifiers left off; none for a type the
/// database made.
fn type_name_of(t: ColumnType) -> Option<String> {
    use ColumnType as C;
    Some(match t {
        C::Bit(_) => "bit".into(),
        C::Varbit(_) => "varbit".into(),
        C::Bpchar(_) => "bpchar".into(),
        C::Text(Some(_)) => "varchar".into(),
        C::Numeric { .. } | C::NumericText(_) => "numeric".into(),
        C::Time(_) => "time".into(),
        C::Timetz(_) => "timetz".into(),
        C::Timestamp(_) => "timestamp".into(),
        C::TimestamptzRounded(_) => "timestamptz".into(),
        C::Interval { .. } => "interval".into(),
        C::Composed(r) => match r.def() {
            crate::composed::Composed::Array {
                element,
                domain: None,
            } => format!("{}[]", type_name_of(*element)?),
            crate::composed::Composed::Array { .. } => return None,
            _ => t.pg_name(),
        },
        t => t.pg_name(),
    })
}

/// A type's OID in the catalog the snail carries; none for a type the database made.
fn type_oid_of(t: ColumnType) -> Option<u32> {
    crate::regs::type_oid(&type_name_of(t)?, RELEASE).ok()
}

/// An OID of an argument: an oid's or a reg* type's, an integer's bits.
fn oid_of(v: &Value) -> Option<u32> {
    match v {
        Value::Scalar(_, k) => Some(crate::scalars::unsigned_of(*k, 32) as u32),
        Value::Int(n) => Some(*n as u32),
        _ => None,
    }
}

/// A regtype of OID `oid`.
fn regtype(oid: u32) -> Value {
    Value::Scalar(
        ColumnType::Reg(RegKind::Type, RELEASE),
        crate::scalars::unsigned_key(u64::from(oid), 32),
    )
}

/// Whether the carried catalog holds a type of OID `oid`; refused for an OID a database hands out,
/// which the snail does not hold.
fn carried_type(oid: u32, name: &str) -> Result<bool> {
    let text = crate::regs::reg_text(RegKind::Type, oid, RELEASE);
    if text != oid.to_string() {
        return Ok(true);
    }
    if oid >= crate::regs::FIXED_BELOW {
        return Err(Error::new(format!(
            "{name}({oid}): a type of an OID from {} on is the database's own, which the snail \
             does not hold",
            crate::regs::FIXED_BELOW
        )));
    }
    Ok(false)
}

/// A bpchar's or a varchar's modifier as `anychar_typmodout` writes it.
fn char_typmod(t: i32) -> String {
    if t > 4 {
        format!("({})", t - 4)
    } else {
        String::new()
    }
}

/// An interval's modifier as `intervaltypmodout` writes it: its fields, then its precision.
fn interval_typmod(t: i32) -> Result<String> {
    const MONTH: i32 = 1 << 1;
    const YEAR: i32 = 1 << 2;
    const DAY: i32 = 1 << 3;
    const HOUR: i32 = 1 << 10;
    const MINUTE: i32 = 1 << 11;
    const SECOND: i32 = 1 << 12;
    let (fields, precision) = ((t >> 16) & 0x7fff, t & 0xffff);
    let f = match fields {
        YEAR => " year",
        MONTH => " month",
        DAY => " day",
        HOUR => " hour",
        MINUTE => " minute",
        SECOND => " second",
        _ if fields == YEAR | MONTH => " year to month",
        _ if fields == DAY | HOUR => " day to hour",
        _ if fields == DAY | HOUR | MINUTE => " day to minute",
        _ if fields == DAY | HOUR | MINUTE | SECOND => " day to second",
        _ if fields == HOUR | MINUTE => " hour to minute",
        _ if fields == HOUR | MINUTE | SECOND => " hour to second",
        _ if fields == MINUTE | SECOND => " minute to second",
        0x7fff => "",
        _ => return Err(Error::new(format!("invalid INTERVAL typmod: 0x{t:x}"))),
    };
    Ok(if precision != 0xffff {
        format!("{f}({precision})")
    } else {
        f.into()
    })
}

/// A type's name as `format_type_extended` writes it under a modifier given or not: `-` for 0,
/// `???` for an OID no type has, an array's element's with `[]`, the SQL standard's names of the
/// types it names, a modifier as the type's own typmodout writes it, or in parentheses.
fn format_type(oid: u32, typmod: Option<i32>) -> Result<String> {
    if oid == 0 {
        return Ok("-".into());
    }
    if !carried_type(oid, "format_type")? {
        return Ok("???".into());
    }
    let name = crate::regs::reg_text(RegKind::Type, oid, RELEASE);
    if let Some(element) = name.strip_suffix("[]") {
        if let Ok(e) = crate::regs::type_oid(element, RELEASE) {
            return Ok(format!("{}[]", format_type(e, typmod)?));
        }
    }
    let given = typmod.is_some();
    let with = typmod.filter(|t| *t >= 0);
    let timed = |base: &str, tz: &str| match with {
        Some(t) => format!("{base}({t}) {tz}"),
        None => format!("{base} {tz}"),
    };
    Ok(match oid {
        1560 => match with {
            Some(t) => format!("bit({t})"),
            None if given => "\"bit\"".into(),
            None => "bit".into(),
        },
        1042 => match with {
            Some(t) => format!("character{}", char_typmod(t)),
            None if given => "bpchar".into(),
            None => "character".into(),
        },
        1043 => format!(
            "character varying{}",
            with.map(char_typmod).unwrap_or_default()
        ),
        1562 => match with {
            Some(t) => format!("bit varying({t})"),
            None => "bit varying".into(),
        },
        1700 => match with {
            Some(t) => format!(
                "numeric({},{})",
                ((t - 4) >> 16) & 0xffff,
                (((t - 4) & 0x7ff) ^ 1024) - 1024
            ),
            None => "numeric".into(),
        },
        1186 => match with {
            Some(t) => format!("interval{}", interval_typmod(t)?),
            None => "interval".into(),
        },
        1083 => timed("time", "without time zone"),
        1266 => timed("time", "with time zone"),
        1114 => timed("timestamp", "without time zone"),
        1184 => timed("timestamp", "with time zone"),
        16 | 700 | 701 | 21 | 23 | 20 | 114 => name,
        _ => match with {
            Some(t) => format!("{name}({t})"),
            None => name,
        },
    })
}

/// The privileges `makeaclitem` reads, each its name and its bit, as `any_priv_map` holds them.
const PRIVILEGES: [(&str, u64); 16] = [
    ("SELECT", 1 << 1),
    ("INSERT", 1 << 0),
    ("UPDATE", 1 << 2),
    ("DELETE", 1 << 3),
    ("TRUNCATE", 1 << 4),
    ("REFERENCES", 1 << 5),
    ("TRIGGER", 1 << 6),
    ("EXECUTE", 1 << 7),
    ("USAGE", 1 << 8),
    ("CREATE", 1 << 9),
    ("TEMP", 1 << 10),
    ("TEMPORARY", 1 << 10),
    ("CONNECT", 1 << 11),
    ("SET", 1 << 12),
    ("ALTER SYSTEM", 1 << 13),
    ("MAINTAIN", 1 << 14),
];

/// Privileges named as `convert_any_priv_string` reads them: split on commas, each with white
/// space around it passed over, matched in any case.
fn privileges(s: &str) -> Result<u64> {
    let mut out = 0;
    for chunk in s.split(',') {
        let chunk = chunk.trim_matches(is_c_space);
        let (_, bit) = PRIVILEGES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(chunk))
            .ok_or_else(|| Error::new(format!("unrecognized privilege type: \"{chunk}\"")))?;
        out |= bit;
    }
    Ok(out)
}

/// A snapshot's xmin, its xmax and its transactions in progress.
fn snapshot(v: &Value) -> Result<(u64, u64, Vec<u64>)> {
    match v {
        Value::Typed(_, s) => crate::textual::snapshot_parse(s),
        other => Err(Error::new(format!("{} is no snapshot", kind_name(other)))),
    }
}

/// A transaction number as an xid8, or as a txid function's bigint.
fn xid(as_xid8: bool, x: u64) -> Value {
    if as_xid8 {
        Value::Scalar(ColumnType::Xid8, crate::scalars::unsigned_key(x, 64))
    } else {
        Value::Int(x as i64)
    }
}

/// The rest's functions over their arguments.
fn the_rest(name: &str, types: &[Option<ColumnType>], values: &[Value]) -> Result<Value> {
    Ok(match name {
        "pg_typeof" => match types[0] {
            None => regtype(UNKNOWN_OID),
            Some(t) => regtype(type_oid_of(t).ok_or_else(|| {
                Error::new(format!(
                    "pg_typeof: {} is a type the database made, whose OID is the server's own, which \
                     the snail does not hold",
                    t.pg_name()
                ))
            })?),
        },
        "error_on_null" if values[0].is_null() => {
            return Err(Error::new("null value not allowed"))
        }
        "error_on_null" => values[0].clone(),
        // The name, cut at a character so that it and `_` and the OID fit in a name's 63 bytes.
        "nameconcatoid" => match (&values[0], oid_of(&values[1])) {
            (Value::Null, _) | (_, None) => Value::Null,
            (v, Some(oid)) => {
                let name = v.text().unwrap_or_default();
                let suffix = format!("_{oid}");
                let mut cut = name.len().min(63 - suffix.len());
                while !name.is_char_boundary(cut) {
                    cut -= 1;
                }
                Value::Str(ColumnType::Name, format!("{}{suffix}", &name[..cut]))
            }
        },
        "format_type" => match oid_of(&values[0]) {
            None => Value::Null,
            Some(oid) => {
                let typmod = match &values[1] {
                    Value::Int(t) => Some(*t as i32),
                    _ => None,
                };
                Value::Text(format_type(oid, typmod)?)
            }
        },
        "pg_basetype" => {
            let oid = oid_of(&values[0]).unwrap_or(0);
            if oid != 0 && carried_type(oid, name)? {
                regtype(oid)
            } else {
                Value::Null
            }
        }
        "makeaclitem" => {
            let (grantee, grantor) = (
                oid_of(&values[0]).unwrap_or(0),
                oid_of(&values[1]).unwrap_or(0),
            );
            let privs = privileges(&text_of(&values[2]).unwrap_or_default())?;
            let goption = matches!(values[3], Value::Bool(true));
            Value::Typed(
                ColumnType::Textual(Textual::Aclitem),
                crate::textual::aclitem_text(
                    grantee,
                    grantor,
                    privs | if goption { privs << 32 } else { 0 },
                ),
            )
        }
        "pg_snapshot_xmin" | "pg_snapshot_xmax" | "txid_snapshot_xmin" | "txid_snapshot_xmax" => {
            let (xmin, xmax, _) = snapshot(&values[0])?;
            xid(
                name.starts_with("pg_"),
                if name.ends_with("xmin") { xmin } else { xmax },
            )
        }
        "pg_visible_in_snapshot" | "txid_visible_in_snapshot" => {
            let x = match &values[0] {
                Value::Scalar(_, k) => crate::scalars::unsigned_of(*k, 64),
                Value::Int(n) => *n as u64,
                _ => 0,
            };
            let (xmin, xmax, xip) = snapshot(&values[1])?;
            Value::Bool(x < xmin || (x < xmax && !xip.contains(&x)))
        }
        "tid_block" | "tid_offset" => {
            let (block, offset) = match &values[0] {
                Value::Scalar(_, k) => crate::scalars::tid_parts(*k),
                _ => (0, 0),
            };
            Value::Int(if name == "tid_block" {
                i64::from(block)
            } else {
                i64::from(offset)
            })
        }
        _ => {
            return Err(Error::new(format!(
                "{name}: the snail does not compute it yet"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot argument as a step hands it over.
    fn snapshot_arg(t: Textual, s: &str) -> (Value, bool, Option<ColumnType>) {
        (Value::Typed(ColumnType::Textual(t), s.into()), false, None)
    }

    #[test]
    fn a_snapshot_s_transactions_in_progress_are_its_set() {
        // PostgreSQL 19: `SELECT pg_snapshot_xip('10:20:11,12,15')` gives 11, 12 and 15; of
        // '1:1:' none; txid_snapshot_xip a bigint each.
        let texts = |name: &str, t: Textual, s: &str| -> Vec<String> {
            set(name, &[snapshot_arg(t, s)])
                .unwrap()
                .unwrap()
                .iter()
                .map(|v| v.text().unwrap())
                .collect()
        };
        assert_eq!(
            texts("pg_snapshot_xip", Textual::PgSnapshot, "10:20:11,12,15"),
            ["11", "12", "15"]
        );
        assert!(texts("pg_snapshot_xip", Textual::PgSnapshot, "1:1:").is_empty());
        let wide = set(
            "txid_snapshot_xip",
            &[snapshot_arg(
                Textual::TxidSnapshot,
                "4294967297:4294967301:4294967298",
            )],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(wide.as_slice(), [Value::Int(4294967298)]));
        let none = (
            Value::Null,
            false,
            Some(ColumnType::Textual(Textual::PgSnapshot)),
        );
        assert!(set("pg_snapshot_xip", &[none]).unwrap().unwrap().is_empty());
        assert!(call(
            "pg_snapshot_xip",
            &[snapshot_arg(Textual::PgSnapshot, "1:1:")]
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn the_keywords_are_postgresql_19_s_in_its_order() {
        // PostgreSQL 19: 499 rows; abort U t, array R f, authorization T t, between C t.
        let all = rows("pg_get_keywords", &[]).unwrap().unwrap();
        assert_eq!(all.len(), 499);
        let text = |r: &[Value]| {
            r.iter()
                .map(|v| v.text().unwrap())
                .collect::<Vec<_>>()
                .join("|")
        };
        let find = |w: &str| {
            all.iter()
                .find(|r| matches!(&r[0], Value::Text(t) if t == w))
                .map(|r| text(r))
                .unwrap()
        };
        assert_eq!(text(&all[0]), "abort|U|true|unreserved|can be bare label");
        assert_eq!(find("array"), "array|R|false|reserved|requires AS");
        assert_eq!(
            find("authorization"),
            "authorization|T|true|reserved (can be function or type name)|can be bare label"
        );
        assert_eq!(
            find("between"),
            "between|C|true|unreserved (cannot be function or type name)|can be bare label"
        );
        assert!(rows("pg_get_keywords", &[(Value::Int(1), false, None)]).is_err());
    }
}
