//! A question in PostgreSQL's SQL, as the snail reads it: the tables it reads and the conditions on
//! each, the keys two tables meet on, and what it returns.

use crate::catalog::{ident, object_name, shape, Catalog, ColumnType, Shape, Table};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    JoinConstraint, JoinOperator, OrderByKind, OrderBySort, SelectItem, SetExpr, Statement,
    TableFactor, UnaryOperator, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::ops::Bound::{self, Excluded, Included, Unbounded};

/// Values from `lo` to `hi`, both included, as their keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub lo: i128,
    pub hi: i128,
}

impl Interval {
    /// Every value.
    pub const ALL: Interval = Interval {
        lo: i128::MIN,
        hi: i128::MAX,
    };

    pub fn at(v: i128) -> Interval {
        Interval { lo: v, hi: v }
    }

    pub fn holds(&self, v: i128) -> bool {
        self.lo <= v && v <= self.hi
    }
}

/// A condition on one column, or on its month: the values `kept` holds less those `except` names
/// and those in its `gaps`, and the column's NULLs too where `nulls`. `<>` keeps every value but
/// one; NOT BETWEEN every value but a run; IS NOT TRUE keeps false and NULL.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cond {
    pub shape: Shape,
    pub kept: Interval,
    pub except: Vec<i128>,
    pub nulls: bool,
    pub gaps: Vec<Interval>,
}

impl Cond {
    pub fn new(shape: Shape, kept: Interval) -> Cond {
        Cond {
            shape,
            kept,
            except: Vec::new(),
            nulls: false,
            gaps: Vec::new(),
        }
    }

    /// Whether it keeps a run of values and nothing else, as a range on a level does.
    pub fn is_range(&self) -> bool {
        self.except.is_empty() && !self.nulls && self.gaps.is_empty()
    }

    /// Whether it keeps a row whose value is `v`, none for a NULL.
    pub fn holds(&self, v: Option<i128>) -> bool {
        match v {
            None => self.nulls,
            Some(v) => {
                self.kept.holds(v)
                    && !self.except.contains(&v)
                    && !self.gaps.iter().any(|g| g.holds(v))
            }
        }
    }

    /// Whether one of its gaps holds every value from `lo` to `hi`.
    pub fn gap_holds(&self, lo: i128, hi: i128) -> bool {
        self.gaps.iter().any(|g| g.lo <= lo && hi <= g.hi)
    }

    /// Whether none of its gaps holds a value from `lo` to `hi`.
    pub fn clear_of_gaps(&self, lo: i128, hi: i128) -> bool {
        self.gaps.iter().all(|g| g.hi < lo || hi < g.lo)
    }

    /// Whether it keeps no value, and no NULL.
    fn is_empty(&self) -> bool {
        !self.nulls
            && (self.kept.lo > self.kept.hi
                || (self.kept.lo == self.kept.hi && self.except.contains(&self.kept.lo)))
    }
}

/// A column of one of the tables a question reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Col {
    pub side: usize,
    pub column: usize,
}

/// A value a question returns or groups by: a column as it is, or the month of an instant on the UTC
/// clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Key {
    Column(Col),
    Month(Col),
}

impl Key {
    pub fn col(self) -> Col {
        match self {
            Key::Column(c) | Key::Month(c) => c,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Item {
    Key(Key),
    Count,
    Sum(Col),
    /// min of a column.
    Least(Col),
    /// max of a column.
    Greatest(Col),
    /// avg of a float column.
    Avg(Col),
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Select {
    /// Rows, as they are.
    Rows(Vec<(String, Key)>),
    /// One row a group of the keys, or one row in all where there are none.
    Groups {
        keys: Vec<Key>,
        items: Vec<(String, Item)>,
    },
}

/// A question the counts can answer: a count in all, or a count by the month of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Countable {
    Total,
    ByMonth(usize),
}

impl Select {
    /// What the counts of a table read alone can answer.
    pub fn countable(&self) -> Option<Countable> {
        let (keys, items) = match self {
            Select::Groups { keys, items } => (keys, items),
            Select::Rows(_) => return None,
        };
        if !items
            .iter()
            .all(|(_, i)| matches!(i, Item::Count | Item::Key(_)))
        {
            return None;
        }
        match keys.as_slice() {
            [] => Some(Countable::Total),
            [Key::Month(c)] => Some(Countable::ByMonth(c.column)),
            _ => None,
        }
    }

    /// The columns the answer reads, the keys first, then the columns summed.
    pub fn inputs(&self) -> Vec<Key> {
        match self {
            Select::Rows(keys) => keys.iter().map(|(_, k)| *k).collect(),
            Select::Groups { keys, items } => {
                let mut out = keys.clone();
                for (_, i) in items {
                    if let Item::Sum(c) | Item::Least(c) | Item::Greatest(c) | Item::Avg(c) = i {
                        if !out[keys.len()..].contains(&Key::Column(*c)) {
                            out.push(Key::Column(*c));
                        }
                    }
                }
                out
            }
        }
    }
}

/// A condition on a column of a string type, bytea or a bit string: its values between two bounds,
/// compared by the bytes they are keyed by (`strings::key_bytes`), less those `except` names and
/// those in its `gaps`, each a run between two bounds.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextCond {
    pub column: usize,
    pub lo: Bound<Vec<u8>>,
    pub hi: Bound<Vec<u8>>,
    pub except: Vec<Vec<u8>>,
    pub gaps: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)>,
}

/// Whether `v` lies from `lo` to `hi`.
pub fn bytes_within(v: &[u8], lo: &Bound<Vec<u8>>, hi: &Bound<Vec<u8>>) -> bool {
    let above = match lo {
        Included(l) => v >= l.as_slice(),
        Excluded(l) => v > l.as_slice(),
        Unbounded => true,
    };
    let below = match hi {
        Included(h) => v <= h.as_slice(),
        Excluded(h) => v < h.as_slice(),
        Unbounded => true,
    };
    above && below
}

impl TextCond {
    /// The values from `lo` to `hi`.
    pub fn new(column: usize, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) -> TextCond {
        TextCond {
            column,
            lo,
            hi,
            except: Vec::new(),
            gaps: Vec::new(),
        }
    }

    /// Whether it keeps a value keyed by `v`.
    pub fn holds(&self, v: &[u8]) -> bool {
        !self.except.iter().any(|x| x == v)
            && bytes_within(v, &self.lo, &self.hi)
            && !self.gaps.iter().any(|(l, h)| bytes_within(v, l, h))
    }

    /// Whether it keeps a run of values and nothing else, as a range on a level does.
    pub fn is_range(&self) -> bool {
        self.except.is_empty() && self.gaps.is_empty()
    }

    fn is_empty(&self) -> bool {
        match (&self.lo, &self.hi) {
            (Unbounded, _) | (_, Unbounded) => false,
            (Included(l), Included(h)) => l > h || (l == h && self.except.contains(l)),
            (Included(l) | Excluded(l), Included(h) | Excluded(h)) => l >= h,
        }
    }
}

/// The tighter of two bounds on one side: the greater of two lower bounds, the lesser of two upper
/// ones, and at one value the one that excludes it.
fn tighter(a: Bound<Vec<u8>>, b: Bound<Vec<u8>>, lower: bool) -> Bound<Vec<u8>> {
    let take_a = match (&a, &b) {
        (Unbounded, _) => false,
        (_, Unbounded) => true,
        (Included(x) | Excluded(x), Included(y) | Excluded(y)) => {
            if x == y {
                matches!(a, Excluded(_))
            } else {
                (x > y) == lower
            }
        }
    };
    if take_a {
        a
    } else {
        b
    }
}

/// One column the answer is ordered by: its place, whether descending, and whether NULLs come first.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Sort {
    pub at: usize,
    pub desc: bool,
    pub nulls_first: bool,
}

/// One table a question reads, and the conditions on it alone.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Side {
    pub table: usize,
    /// The name it goes by in the question: its alias, or its own without its schema.
    pub(crate) goes_by: String,
    pub conds: Vec<Cond>,
    pub texts: Vec<TextCond>,
    /// Each column a condition keeps to its NULLs, IS NULL (true), or to its values, IS NOT NULL
    /// (false).
    pub nulls: Vec<(usize, bool)>,
    /// Some condition can hold for no value.
    pub empty: bool,
    /// The conditions by an operator a GiST or SP-GiST strand answers, and by a pattern.
    pub ops: Vec<OpCond>,
}

/// A condition held a row at a time as a pipeline holds it: a pattern on a column of text, or a
/// condition on a range, a multirange or an inet column by an operator a GiST or SP-GiST strand
/// answers: a range's or a multirange's `&&`, `@>`, `<@`, `<<`, `>>`, `&<`, `&>` and `-|-`, an
/// inet's `&&`, `<<`, `<<=`, `>>` and `>>=`, with a value. A row is held to it as a pipeline holds
/// one; a block of a strand that keeps the column's span is ruled out where `probe` lies outside it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OpCond {
    pub column: usize,
    /// The condition, the column written as its name.
    pub expr: Expr,
    /// The column's name.
    pub name: String,
    /// The operator as the column, on its left, reads it; and its value's span, where the operator
    /// rules a block out by it.
    pub op: String,
    pub probe: Option<crate::spans::Probe>,
    /// The value it compares with, where an element strand may answer it.
    pub value: Option<super::statement::Datum>,
    /// An OR or a NOT over conditions of this table: what its parts ask of the element strands. It
    /// is held to a row with its NULLs, and reads the other columns `beside` names.
    pub arms: Option<Arm>,
    pub beside: Vec<(usize, String)>,
}

/// What a part of an OR or a NOT over conditions of one table asks of the element strands.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Arm {
    /// A condition an element strand may answer.
    Op(Box<OpCond>),
    /// A condition an order may answer, its rows found by their keys where it rules blocks out.
    Read(Box<Side>),
    /// A condition held a row at a time, asking nothing of a strand.
    Held,
    All(Vec<Arm>),
    Any(Vec<Arm>),
}

impl Side {
    /// Whether it keeps every row of its table: no condition on it.
    pub fn is_whole(&self) -> bool {
        self.conds.is_empty()
            && self.texts.is_empty()
            && self.nulls.is_empty()
            && self.ops.is_empty()
            && !self.empty
    }

    /// A condition on this table alone, kept beside the ones it has.
    fn take(&mut self, piece: Piece, table: &Table) {
        match piece {
            Piece::Cond(c) => merge(&mut self.conds, c),
            Piece::Text(t) => match self.texts.iter_mut().find(|x| x.column == t.column) {
                Some(have) => both_texts(have, t),
                None => self.texts.push(t),
            },
            Piece::Null(c, null) => {
                if table.columns[c].not_null {
                    self.empty |= null;
                } else if !self.nulls.contains(&(c, null)) {
                    self.nulls.push((c, null));
                }
            }
            Piece::Empty => self.empty = true,
            Piece::Op(o) => self.ops.push(o),
        }
    }
}

/// A condition on one column of one table: a range of its keys or of its month's, a range of the
/// bytes it is keyed by, a test for NULL (IS NULL where true), or one no row passes.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Piece {
    Cond(Cond),
    Text(TextCond),
    Null(usize, bool),
    Empty,
    Op(OpCond),
}

impl Piece {
    /// Whether a row whose column is NULL fails it: every comparison, IS NOT NULL, and the truth
    /// tests but IS NOT TRUE and IS NOT FALSE.
    pub fn strict(&self) -> bool {
        match self {
            Piece::Cond(c) => !c.nulls,
            Piece::Null(_, null) => !null,
            Piece::Op(o) => o.arms.is_none(),
            Piece::Text(_) | Piece::Empty => true,
        }
    }

    /// The column it reads, and whether it is read for its month; none where it reads none.
    pub fn column(&self) -> Option<(usize, bool)> {
        match self {
            Piece::Cond(c) => Some(match c.shape {
                Shape::MonthUtc(x) => (x, true),
                Shape::Column(x) | Shape::Clock(x) | Shape::Scaled(x, _) => (x, false),
            }),
            Piece::Text(t) => Some((t.column, false)),
            Piece::Null(c, _) => Some((*c, false)),
            Piece::Op(o) => Some((o.column, false)),
            Piece::Empty => None,
        }
    }
}

/// How two values of two tables' columns are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cmp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl Cmp {
    /// Whether two values that compare so pass it.
    pub fn holds(self, o: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Cmp::Eq => o == Equal,
            Cmp::NotEq => o != Equal,
            Cmp::Lt => o == Less,
            Cmp::LtEq => o != Greater,
            Cmp::Gt => o == Greater,
            Cmp::GtEq => o != Less,
        }
    }

    /// The same comparison, its two sides swapped.
    pub fn flipped(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::LtEq => Cmp::GtEq,
            Cmp::Gt => Cmp::Lt,
            Cmp::GtEq => Cmp::LtEq,
            other => other,
        }
    }
}

/// Two columns of two tables compared: `a op b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Meet {
    pub a: Col,
    pub op: Cmp,
    pub b: Col,
}

impl Meet {
    /// The same comparison written from table `side`'s column, where it holds one.
    pub fn seen_from(self, side: usize) -> Meet {
        if self.a.side == side {
            self
        } else {
            Meet {
                a: self.b,
                op: self.op.flipped(),
                b: self.a,
            }
        }
    }

    pub fn touches(&self, side: usize) -> bool {
        self.a.side == side || self.b.side == side
    }
}

/// A test read on a pair of rows an outer join would join, or on the joined rows where a table may
/// stand NULL: a condition on one table's column, or two tables' columns compared.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Test {
    One(usize, Piece),
    Two(Meet),
}

/// A table an outer join may leave NULL, and its ON: it meets the tables before it on `meets`, each
/// written from its own column, and a pair of rows joins only where `tests` hold too.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Outer {
    pub side: usize,
    pub meets: Vec<Meet>,
    pub tests: Vec<Test>,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Question {
    pub sides: Vec<Side>,
    /// Two tables' columns compared, which every answer row holds: equal on a join's keys, or by
    /// an inequality.
    pub meets: Vec<Meet>,
    /// The tables an outer join may leave NULL, as they are joined, each with its ON.
    pub outer: Vec<Outer>,
    /// The first two tables are joined FULL: `outer` holds the second, and either may stand NULL.
    pub full: bool,
    /// What is read on the joined rows: WHERE's conditions on a table that may stand NULL, each of
    /// which keeps its NULL.
    pub after: Vec<Test>,
    pub select: Select,
    /// How the answer is ordered: by which of its columns, outermost first.
    pub order: Vec<Sort>,
}

impl Question {
    /// Whether table `side` may stand NULL in an answer row.
    pub fn nullable(&self, side: usize) -> bool {
        (self.full && side == 0) || self.outer.iter().any(|o| o.side == side)
    }
}

pub fn number(e: &Expr) -> Option<i64> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) => s.parse().ok(),
            _ => None,
        },
        // A number written with its minus is one constant, as PostgreSQL's grammar folds it: so
        // the least bigint is read whole.
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(v) => match &v.value {
                Value::Number(s, _) => format!("-{s}").parse().ok(),
                _ => None,
            },
            e => number(e).and_then(i64::checked_neg),
        },
        Expr::Nested(e) => number(e),
        _ => None,
    }
}

/// An instant as PostgreSQL's timestamptz input reads it under TimeZone UTC, as microseconds since
/// 1970-01-01 UTC.
pub fn instant(s: &str) -> Result<i64> {
    crate::times::timestamptz_in(s, None, crate::scalars::transaction_clock())
}

/// A truth as PostgreSQL's boolean input reads it: `true`, `yes`, `on` or `1`, `false`, `no`, `off`
/// or `0`, or the start of one that names no other, in either case, the white space around it taken
/// off.
pub fn truth(s: &str) -> Option<bool> {
    let s = s
        .trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
        .to_ascii_lowercase();
    let starts = |word: &str, least: usize| s.len() >= least && word.starts_with(s.as_str());
    match s.as_bytes().first()? {
        b't' if starts("true", 1) => Some(true),
        b'f' if starts("false", 1) => Some(false),
        b'y' if starts("yes", 1) => Some(true),
        b'n' if starts("no", 1) => Some(false),
        b'o' if starts("on", 2) => Some(true),
        b'o' if starts("off", 2) => Some(false),
        b'1' if s.len() == 1 => Some(true),
        b'0' if s.len() == 1 => Some(false),
        _ => None,
    }
}

/// A boolean written out, TRUE or FALSE or a quoted truth, as 1 or 0.
pub(super) fn truth_literal(e: &Expr) -> Result<i64> {
    let wrong = || Error::new(format!("{e} is not a boolean"));
    match e {
        Expr::Value(v) => match &v.value {
            Value::Boolean(b) => Ok(*b as i64),
            Value::SingleQuotedString(s) => truth(s).map(i64::from).ok_or_else(wrong),
            _ => Err(wrong()),
        },
        Expr::Nested(e) => truth_literal(e),
        _ => Err(wrong()),
    }
}

/// A literal compared with a column, or its month, as the key its values are compared by.
fn literal(e: &Expr, s: Shape, table: &Table) -> Result<i128> {
    literal_i64(e, s, table)
        .map(i128::from)
        .or_else(|err| match s {
            Shape::Column(c) if table.columns[c].ty == ColumnType::Uuid => {
                crate::scalars::uuid_parse(&text_literal(e)?)
            }
            Shape::Column(c) => match table.columns[c].ty {
                // An interval is compared by its total, a literal read as the column's type reads it.
                // Read as an interval of every field, whatever fields the column keeps.
                ColumnType::Interval { .. } => Ok(crate::times::interval_in(
                    &text_literal(uncast_to(e, "interval")).map_err(|_| err)?,
                    crate::times::INTERVAL_FULL_RANGE,
                    None,
                )?
                .total()),
                _ => Err(err),
            },
            _ => Err(err),
        })
}

/// A literal as written, its cast to the type named taken off.
fn uncast_to<'e>(e: &'e Expr, name: &str) -> &'e Expr {
    match e {
        Expr::Cast {
            expr, data_type, ..
        } if data_type.to_string().to_lowercase().starts_with(name) => expr,
        Expr::Nested(x) => uncast_to(x, name),
        e => e,
    }
}

fn literal_i64(e: &Expr, s: Shape, table: &Table) -> Result<i64> {
    match s {
        Shape::MonthUtc(_) => number(e).ok_or_else(|| Error::new(format!("{e} is not a month"))),
        // A count of a double precision's places is a double precision.
        Shape::Scaled(c, _) => literal_i64(e, Shape::Column(c), table),
        Shape::Column(c) => match table.columns[c].ty {
            ty if crate::scalars::is_scalar(ty) => scalar_literal(e, ty, table.release),
            ty @ (ColumnType::Float4 | ColumnType::Float8) => {
                let wide = ty == ColumnType::Float8;
                let text = match text_literal(e) {
                    Ok(s) => s,
                    Err(_) => super::statement::number_text(e)
                        .ok_or_else(|| Error::new(format!("{e} is not a number")))?,
                };
                let bits = crate::scalars::float_parse(&text, wide)?;
                Ok(if wide {
                    crate::scalars::float8_key(bits)
                } else {
                    crate::scalars::float4_key(bits as u32)
                })
            }
            ColumnType::Boolean => truth_literal(e),
            ColumnType::Uuid => Err(Error::new(format!("{e} is not a uuid"))),
            // An enum: its label's rank.
            ty @ ColumnType::Composed(_) => match crate::composed::literal(ty, e)? {
                super::statement::Datum::Int(k) => Ok(k),
                _ => Err(Error::new(format!(
                    "{e} is not a value of {}",
                    ty.pg_name()
                ))),
            },
            ColumnType::Interval { .. } => Err(Error::new(format!("{e} is not an interval"))),
            ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => {
                instant(&text_literal(e).map_err(|_| Error::new(format!("{e} is not an instant")))?)
            }
            // An integer: a number written out, or a quoted literal its input reads.
            ty @ (ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
                if text_literal(e).is_ok() =>
            {
                crate::composed::int_in(&text_literal(e)?, ty.integer_bits())
            }
            _ => number(e).ok_or_else(|| Error::new(format!("{e} is not an integer"))),
        },
        Shape::Clock(_) => Err(Error::new("compare the column itself, or its month")),
    }
}

/// A literal compared with a numeric, as the number `numeric_in` reads: a number written out, a
/// quoted literal, or either cast to numeric, held to the cast's typmod.
fn numeric_literal(e: &Expr) -> Result<crate::numeric::Numeric> {
    if let Expr::Cast {
        expr, data_type, ..
    } = e
    {
        if let Some(typmod) = crate::catalog::held_type(data_type)
            .ok()
            .and_then(ColumnType::numeric_typmod)
        {
            let n = numeric_literal(expr)?;
            return match typmod {
                Some((p, s)) => n.typmod(p, s),
                None => Ok(n),
            };
        }
    }
    let text = super::statement::numeric_literal_text(e)
        .ok_or_else(|| Error::new(format!("{e} is not a number")))?;
    crate::numeric::Numeric::parse(&text)
}

/// A literal compared with a fixed scalar, as its key: quoted, read by the type's input; a number
/// only beside an oid, cast to one, or an xid, as `xideqint4` reads an integer. A literal cast to
/// the column's type is read by that type's input, and beside an oid or a reg* type a literal cast
/// to any reg* type by that one's, under the file's release, or one cast to an oid.
fn scalar_literal(e: &Expr, ty: ColumnType, release: u8) -> Result<i64> {
    if let Expr::Cast {
        expr, data_type, ..
    } = e
    {
        if let (Ok(s), Ok(to)) = (text_literal(expr), crate::catalog::held_type(data_type)) {
            let to = match to {
                ColumnType::Reg(kind, _) if ty.is_oid() => {
                    crate::regs::check_release(Some(kind), release)?;
                    Some(ColumnType::Reg(kind, release))
                }
                ColumnType::Oid if ty.is_oid() => Some(ColumnType::Oid),
                ColumnType::Money(_) if matches!(ty, ColumnType::Money(_)) => Some(ty),
                to if to == ty => Some(to),
                _ => None,
            };
            if let Some(to) = to {
                return crate::scalars::scalar_parse(to, &s);
            }
        }
    }
    // A quoted literal beside a reg* type is read as an oid, whose comparisons it takes; beside a
    // time type, as that type without the column's precision.
    if let Ok(s) = text_literal(e) {
        let ty = match ty {
            ty if ty.is_oid() => ColumnType::Oid,
            ColumnType::Time(_) => ColumnType::Time(None),
            ColumnType::Timetz(_) => ColumnType::Timetz(None),
            ColumnType::Timestamp(_) => ColumnType::Timestamp(None),
            ty => ty,
        };
        return crate::scalars::scalar_parse(ty, &s);
    }
    let n = match super::statement::number_datum(e) {
        Some((super::statement::Datum::Int(n), ColumnType::Int4 | ColumnType::Int8)) => n,
        Some((_, from)) => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}",
                ty.pg_name(),
                from.pg_name().split('(').next().unwrap_or_default()
            )))
        }
        None => {
            return Err(Error::new(format!(
                "{e} is not a value of {}",
                ty.pg_name()
            )))
        }
    };
    let int4 = i32::try_from(n).is_ok();
    // int8 and oid8 are binary-coercible; an integer comes through int8.
    if ty == ColumnType::Oid8 {
        return Ok(crate::scalars::unsigned_key(n as u64, 64));
    }
    let u = match ty {
        ty if ty.is_oid() && int4 => n as i32 as u32,
        ty if ty.is_oid() => u32::try_from(n).map_err(|_| Error::new("OID out of range"))?,
        ColumnType::Xid if int4 => n as i32 as u32,
        _ => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}",
                ty.pg_name(),
                if int4 { "integer" } else { "bigint" }
            )))
        }
    };
    Ok(crate::scalars::unsigned_key(u64::from(u), 32))
}

/// A time on the UTC clock, as microseconds since 1970-01-01 UTC: what a column at time zone 'UTC' is
/// compared with, read as PostgreSQL's timestamp input reads it.
fn clock_literal(e: &Expr) -> Result<i64> {
    crate::times::timestamp_in(&text_literal(e)?, None, crate::scalars::transaction_clock())
}

/// A literal compared with a column of a string type, bytea or a bit string, read by the type's
/// input as PostgreSQL reads the literal beside one, as the bytes its values are keyed by: text as
/// it is, a bpchar without its trailing blanks, a name cut to 63 bytes, bytea and a bit string as
/// their inputs read them, a bit string of any length.
fn bytes_literal(e: &Expr, ty: ColumnType) -> Result<Vec<u8>> {
    if let Some(s) = bit_literal(e) {
        if !ty.is_bits() {
            return Err(Error::new(format!(
                "operator does not exist: {} = bit",
                ty.pg_name().split('(').next().unwrap_or_default()
            )));
        }
        return crate::strings::bits_parse(&s, None, true);
    }
    let s = match e {
        Expr::Cast {
            expr, data_type, ..
        } if crate::catalog::held_type(data_type).is_ok_and(|t| same_kind(t, ty)) => {
            text_literal(expr)?
        }
        _ => text_literal(e)?,
    };
    Ok(match ty {
        ColumnType::Bpchar(_) => crate::strings::bpchar_key(s.as_bytes()).to_vec(),
        ColumnType::Name => crate::strings::name_in(&s).into_bytes(),
        ColumnType::Bytes => crate::strings::bytea_in(&s)?,
        ColumnType::Bit(_) | ColumnType::Varbit(_) => crate::strings::bits_parse(&s, None, true)?,
        // Its class's key, from the text its input reads; an inet beside a cidr by inet's.
        ColumnType::Textual(t) => {
            let t = t.literal_type();
            crate::textual::key_bytes(t, crate::textual::text_in(t, &s)?.as_bytes()).into_owned()
        }
        _ => s.into_bytes(),
    })
}

/// Whether a literal cast to `to` is read beside a column of `ty` by `ty`'s own input: a cast to its
/// own kind of type, of any modifier.
fn same_kind(to: ColumnType, ty: ColumnType) -> bool {
    std::mem::discriminant(&to) == std::mem::discriminant(&ty) || (to.is_bits() && ty.is_bits())
}

/// A bit-string constant, `B'…'` or `X'…'`, as the input of a bit string reads it: its letter,
/// then its digits.
pub(super) fn bit_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedByteStringLiteral(s) => Some(format!("B{s}")),
            Value::HexStringLiteral(s) => Some(format!("X{s}")),
            _ => None,
        },
        Expr::Nested(e) => bit_literal(e),
        _ => None,
    }
}

pub(super) fn text_literal(e: &Expr) -> Result<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) => Ok(s.clone()),
            _ => Err(Error::new(format!("{e} is not text"))),
        },
        Expr::Nested(e) => text_literal(e),
        _ => Err(Error::new(format!("{e} is not text"))),
    }
}

/// A condition kept to another on the same column as well: the values both keep, and the NULLs
/// both keep.
fn merge(conds: &mut Vec<Cond>, c: Cond) {
    match conds.iter_mut().find(|x| x.shape == c.shape) {
        Some(have) => {
            have.kept = Interval {
                lo: have.kept.lo.max(c.kept.lo),
                hi: have.kept.hi.min(c.kept.hi),
            };
            for v in c.except {
                if !have.except.contains(&v) {
                    have.except.push(v);
                }
            }
            for g in c.gaps {
                if !have.gaps.contains(&g) {
                    have.gaps.push(g);
                }
            }
            have.nulls &= c.nulls;
        }
        None => conds.push(c),
    }
}

/// A condition on text kept to another on the same column as well: the values both keep.
fn both_texts(have: &mut TextCond, t: TextCond) {
    have.lo = tighter(std::mem::replace(&mut have.lo, Unbounded), t.lo, true);
    have.hi = tighter(std::mem::replace(&mut have.hi, Unbounded), t.hi, false);
    for x in t.except {
        if !have.except.contains(&x) {
            have.except.push(x);
        }
    }
    have.gaps.extend(t.gaps);
}

/// The values either of two ranges on one column keeps: the run from the lower's start to the
/// further end, less the gap between them where they neither meet nor overlap. None where either
/// keeps more than a run.
fn either_conds(a: Cond, b: Cond) -> Option<Cond> {
    if !a.is_range() || !b.is_range() {
        return None;
    }
    let empty = |c: &Cond| c.kept.lo > c.kept.hi;
    if empty(&a) {
        return Some(b);
    }
    if empty(&b) {
        return Some(a);
    }
    let (a, b) = if a.kept.lo <= b.kept.lo {
        (a, b)
    } else {
        (b, a)
    };
    let mut c = Cond::new(
        a.shape,
        Interval {
            lo: a.kept.lo,
            hi: a.kept.hi.max(b.kept.hi),
        },
    );
    if a.kept.hi.checked_add(1).is_some_and(|n| n < b.kept.lo) {
        c.gaps.push(Interval {
            lo: a.kept.hi + 1,
            hi: b.kept.lo - 1,
        });
    }
    Some(c)
}

/// Whether a lower bound keeps every value another keeps at its low end.
fn lower_first(a: &Bound<Vec<u8>>, b: &Bound<Vec<u8>>) -> bool {
    match (a, b) {
        (Unbounded, _) => true,
        (_, Unbounded) => false,
        (Included(x), Excluded(y)) => x <= y,
        (Included(x), Included(y)) | (Excluded(x), Excluded(y)) => x <= y,
        (Excluded(x), Included(y)) => x < y,
    }
}

/// The values either of two ranges on one text column keeps, as `either_conds` makes them of a
/// number's.
fn either_texts(a: TextCond, b: TextCond) -> Option<TextCond> {
    if !a.is_range() || !b.is_range() {
        return None;
    }
    if a.is_empty() {
        return Some(b);
    }
    if b.is_empty() {
        return Some(a);
    }
    let (a, b) = if lower_first(&a.lo, &b.lo) {
        (a, b)
    } else {
        (b, a)
    };
    let gap = match (&a.hi, &b.lo) {
        (Included(h) | Excluded(h), Included(l) | Excluded(l)) if l > h => Some((
            if matches!(a.hi, Included(_)) {
                Excluded(h.clone())
            } else {
                Included(h.clone())
            },
            if matches!(b.lo, Included(_)) {
                Excluded(l.clone())
            } else {
                Included(l.clone())
            },
        )),
        (Excluded(h), Excluded(l)) if l == h => Some((Included(h.clone()), Included(h.clone()))),
        _ => None,
    };
    // The further of the two upper bounds.
    let a_further = match (&a.hi, &b.hi) {
        (Unbounded, _) => true,
        (_, Unbounded) => false,
        (Included(x) | Excluded(x), Included(y) | Excluded(y)) => {
            x > y || (x == y && matches!(a.hi, Included(_)))
        }
    };
    let hi = if a_further { a.hi } else { b.hi };
    let mut t = TextCond::new(a.column, a.lo, hi);
    t.gaps.extend(gap);
    Some(t)
}

/// A truth test on a boolean column: the column standing as a condition, alone or under NOT, or
/// under IS [NOT] TRUE or FALSE. Its side and the condition it is: each keeps what `= true` or
/// `= false` keeps, and IS NOT TRUE and IS NOT FALSE keep the column's NULLs too. Refused on a
/// column of any other type, as PostgreSQL refuses it.
fn truth_test(from: &From, e: &Expr) -> Result<Option<(usize, Cond)>> {
    let (x, v, nulls, test) = match e {
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => (expr.as_ref(), 0, false, None),
        Expr::IsTrue(x) => (x.as_ref(), 1, false, Some("IS TRUE")),
        Expr::IsFalse(x) => (x.as_ref(), 0, false, Some("IS FALSE")),
        Expr::IsNotTrue(x) => (x.as_ref(), 0, true, Some("IS NOT TRUE")),
        Expr::IsNotFalse(x) => (x.as_ref(), 1, true, Some("IS NOT FALSE")),
        x => (x, 1, false, None),
    };
    match from.resolve(x) {
        Ok((side, Shape::Column(c))) if from.table(side).columns[c].ty == ColumnType::Boolean => {
            let mut cond = Cond::new(Shape::Column(c), Interval::at(v));
            cond.nulls = nulls;
            Ok(Some((side, cond)))
        }
        Ok((side, _)) if test.is_some() => Err(Error::new(format!(
            "argument of {} must be type boolean, not type {}",
            test.unwrap(),
            match from.resolve(x)? {
                (_, Shape::Column(c)) | (_, Shape::Clock(c)) =>
                    from.table(side).columns[c].ty.pg_name(),
                (_, Shape::MonthUtc(_)) => "smallint".into(),
                (_, Shape::Scaled(..)) => "double precision".into(),
            }
        ))),
        _ => Ok(None),
    }
}

fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        Expr::Nested(inner) => conjuncts(inner, out),
        other => out.push(other),
    }
}

/// The tables a question reads, as it names them.
struct From<'a> {
    catalog: &'a Catalog,
    tables: Vec<(usize, Option<String>)>,
}

impl From<'_> {
    fn table(&self, side: usize) -> &Table {
        &self.catalog.tables[self.tables[side].0]
    }

    /// The name a table goes by in the question: its alias, or its own.
    fn named(&self, side: usize) -> String {
        match &self.tables[side].1 {
            Some(a) => a.clone(),
            None => self.table(side).name.clone(),
        }
    }

    /// What an expression reads, and of which table: the one table whose columns it names.
    fn resolve(&self, e: &Expr) -> Result<(usize, Shape)> {
        if let Some(call) = undeclared(e, &self.catalog.functions) {
            return Err(Error::new(format!("{call} is not read")));
        }
        let mut found = Vec::new();
        let mut first_error = None;
        for (side, (ti, alias)) in self.tables.iter().enumerate() {
            let t = &self.catalog.tables[*ti];
            match shape(
                e,
                &t.name,
                alias.as_deref(),
                &t.columns,
                &self.catalog.functions,
                &[],
            ) {
                Ok(s) => found.push((side, s)),
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        match found.as_slice() {
            [(side, Shape::Column(c))] if self.table(*side).columns[*c].is_virtual() => {
                Err(Error::new(format!(
                    "{e} is a virtual generated column, computed at the read from the columns the \
                     table stores: a scan reads only those"
                )))
            }
            [one] => Ok(*one),
            [] => Err(first_error.unwrap_or_else(|| Error::new(format!("{e} reads no table")))),
            _ => Err(Error::new(format!(
                "{e} could name a column of more than one table: qualify it"
            ))),
        }
    }

    fn key_of(&self, e: &Expr) -> Result<Key> {
        match self.resolve(e)? {
            (side, Shape::Column(column)) => Ok(Key::Column(Col { side, column })),
            (side, Shape::MonthUtc(column)) => Ok(Key::Month(Col { side, column })),
            (_, Shape::Clock(_) | Shape::Scaled(..)) => Err(Error::new(format!(
                "{e}: return the column itself, or its month"
            ))),
        }
    }

    /// An aggregate the snail reads, `count(*)` or `sum` of an integer column; `None` for anything that
    /// is no aggregate.
    fn aggregate(&self, e: &Expr) -> Result<Option<Item>> {
        let f = match e {
            Expr::Function(f) => f,
            _ => return Ok(None),
        };
        let name = object_name(&f.name);
        if !matches!(name.as_str(), "count" | "sum" | "min" | "max" | "avg") {
            return Ok(None);
        }
        let list = match &f.args {
            FunctionArguments::List(l)
                if l.duplicate_treatment.is_none() && l.clauses.is_empty() =>
            {
                &l.args
            }
            _ => return Err(Error::new(format!("{e} is not read"))),
        };
        if f.filter.is_some() || f.over.is_some() || !f.within_group.is_empty() {
            return Err(Error::new(format!("{e} is not read")));
        }
        match (name.as_str(), list.as_slice()) {
            ("count", [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]) => Ok(Some(Item::Count)),
            ("sum", [FunctionArg::Unnamed(FunctionArgExpr::Expr(x))]) => match self.resolve(x)? {
                (side, Shape::Column(column))
                    if matches!(
                        self.table(side).columns[column].ty,
                        ColumnType::Int2
                            | ColumnType::Int4
                            | ColumnType::Int8
                            | ColumnType::Numeric { .. }
                            | ColumnType::NumericText(_)
                            | ColumnType::Money(_)
                            | ColumnType::Float4
                            | ColumnType::Float8
                            | ColumnType::Interval { .. }
                            | ColumnType::Time(_)
                    ) =>
                {
                    Ok(Some(Item::Sum(Col { side, column })))
                }
                _ => Err(Error::new(format!(
                    "{e}: sum is read over an integer, numeric or money column"
                ))),
            },
            ("avg", [FunctionArg::Unnamed(FunctionArgExpr::Expr(x))]) => match self.resolve(x)? {
                // A float's, an integer's or a numeric's; an interval's, and a time's through its cast
                // to one.
                (side, Shape::Column(column))
                    if matches!(self.table(side).columns[column].ty, ty if ty.is_float() || ty.is_integer() || ty.is_numeric())
                        || matches!(
                            self.table(side).columns[column].ty,
                            ColumnType::Interval { .. } | ColumnType::Time(_)
                        ) =>
                {
                    Ok(Some(Item::Avg(Col { side, column })))
                }
                (side, Shape::Column(column)) => Err(Error::new(format!(
                    "function avg({}) does not exist",
                    self.table(side).columns[column].ty.pg_name()
                ))),
                _ => Err(Error::new(format!("{e} is not read"))),
            },
            ("min" | "max", [FunctionArg::Unnamed(FunctionArgExpr::Expr(x))]) => {
                let (side, column) = match self.resolve(x)? {
                    (side, Shape::Column(column)) => (side, column),
                    _ => return Err(Error::new(format!("{e}: min and max read a column itself"))),
                };
                let ty = self.table(side).columns[column].ty;
                let col = Col { side, column };
                match ty {
                    ColumnType::Int2
                    | ColumnType::Int4
                    | ColumnType::Int8
                    | ColumnType::Numeric { .. }
                    | ColumnType::NumericText(_)
                    | ColumnType::Timestamptz
                    | ColumnType::TimestamptzRounded(_)
                    | ColumnType::Oid
                    | ColumnType::Reg(..)
                    | ColumnType::Oid8
                    | ColumnType::Xid8
                    | ColumnType::PgLsn
                    | ColumnType::Char
                    | ColumnType::Money(_)
                    | ColumnType::Float4
                    | ColumnType::Float8
                    | ColumnType::Tid
                    | ColumnType::Text(_)
                    | ColumnType::Bpchar(_)
                    | ColumnType::Name
                    | ColumnType::Date
                    | ColumnType::Time(_)
                    | ColumnType::Timetz(_)
                    | ColumnType::Timestamp(_)
                    | ColumnType::Interval { .. }
                    | ColumnType::Bytes
                    | ColumnType::Internal(crate::textual::Internal::NodeTree) => {
                        Ok(Some(if name == "min" {
                            Item::Least(col)
                        } else {
                            Item::Greatest(col)
                        }))
                    }
                    // Of the types held as their text only inet has min and max, a cidr's taken
                    // as an inet.
                    ColumnType::Textual(t) if t.is_network() => Ok(Some(if name == "min" {
                        Item::Least(col)
                    } else {
                        Item::Greatest(col)
                    })),
                    // An array, a composite or an enum; a range or a multirange has none.
                    ColumnType::Composed(r)
                        if ty.ordered()
                            && !matches!(
                                r.def(),
                                crate::composed::Composed::Range { .. }
                                    | crate::composed::Composed::Multirange { .. }
                            ) =>
                    {
                        Ok(Some(if name == "min" {
                            Item::Least(col)
                        } else {
                            Item::Greatest(col)
                        }))
                    }
                    ColumnType::Textual(_) | ColumnType::Internal(_) => Err(Error::new(format!(
                        "function {name}({}) does not exist",
                        ty.pg_name()
                    ))),
                    ColumnType::Boolean
                    | ColumnType::Uuid
                    | ColumnType::Xid
                    | ColumnType::Cid
                    | ColumnType::Bit(_)
                    | ColumnType::Varbit(_)
                    | ColumnType::Composed(_) => Err(Error::new(format!(
                        "function {name}({}) does not exist",
                        ty.pg_name().split('(').next().unwrap_or_default()
                    ))),
                    _ => Err(Error::new(format!("{e} is not read"))),
                }
            }
            _ => Err(Error::new(format!("{e} is not read"))),
        }
    }

    /// The name PostgreSQL gives a column the question leaves unnamed.
    fn default_name(&self, item: Item) -> String {
        match item {
            Item::Key(Key::Column(c)) => self.table(c.side).columns[c.column].name.clone(),
            Item::Key(Key::Month(_)) => "extract".into(),
            Item::Count => "count".into(),
            Item::Sum(_) => "sum".into(),
            Item::Least(_) => "min".into(),
            Item::Greatest(_) => "max".into(),
            Item::Avg(_) => "avg".into(),
        }
    }
}

/// Whether an expression is a value written out: a literal, signed, cast or in parentheses.
fn valued(e: &Expr) -> bool {
    match e {
        Expr::Value(_) | Expr::TypedString(_) => true,
        Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::Cast { expr: x, .. } => valued(x),
        _ => false,
    }
}

/// The first call in an expression, outermost first, of a function the DDL does not declare: a
/// level reads only the DDL's own.
fn undeclared<'e>(e: &'e Expr, functions: &[crate::catalog::Function]) -> Option<&'e Expr> {
    match e {
        Expr::Function(f) => {
            let name = object_name(&f.name);
            if !functions.iter().any(|g| g.name == name) {
                return Some(e);
            }
            match &f.args {
                FunctionArguments::List(l) => l.args.iter().find_map(|a| match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => undeclared(x, functions),
                    _ => None,
                }),
                _ => None,
            }
        }
        Expr::Nested(x) | Expr::Collate { expr: x, .. } | Expr::Cast { expr: x, .. } => {
            undeclared(x, functions)
        }
        Expr::AtTimeZone { timestamp, .. } => undeclared(timestamp, functions),
        Expr::Extract { expr, .. } => undeclared(expr, functions),
        _ => None,
    }
}

fn relation(catalog: &Catalog, t: &TableFactor) -> Result<(usize, Option<String>)> {
    match t {
        TableFactor::Table { name, alias, .. } => {
            let name = object_name(name);
            let ti = catalog
                .tables
                .iter()
                .position(|t| t.name == name)
                .ok_or_else(|| Error::new(format!("{name} is not in the snail")))?;
            Ok((ti, alias.as_ref().map(|a| ident(&a.name))))
        }
        other => Err(Error::new(format!(
            "{other} is not read: a question reads tables"
        ))),
    }
}

/// A locking clause refused by name, here and in each step and subquery of a FROM: a question takes
/// no lock on the rows it reads.
pub(crate) fn unlocked(q: &sqlparser::ast::Query) -> Result<()> {
    if let Some(lock) = q.locks.first() {
        return Err(Error::new(format!(
            "{lock} is not read: a question takes no lock on the rows it reads"
        )));
    }
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            unlocked(&cte.query)?;
        }
    }
    unlocked_body(&q.body)
}

fn unlocked_body(body: &SetExpr) -> Result<()> {
    match body {
        SetExpr::Query(q) => unlocked(q),
        SetExpr::SetOperation { left, right, .. } => {
            unlocked_body(left)?;
            unlocked_body(right)
        }
        SetExpr::Select(s) => {
            for t in &s.from {
                for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
                    if let TableFactor::Derived { subquery, .. } = f {
                        unlocked(subquery)?;
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The locking clauses sqlparser does not read, `FOR NO KEY UPDATE` and `FOR KEY SHARE`, refused by
/// name before it parses the statement, as `unlocked` refuses the others.
pub(crate) fn unlocked_words(sql: &str) -> Result<()> {
    let lower = sql.to_ascii_lowercase();
    if !(lower.contains("for") && lower.contains("key")) {
        return Ok(());
    }
    let Ok(tokens) = sqlparser::tokenizer::Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize()
    else {
        return Ok(());
    };
    let words: Vec<Option<String>> = tokens
        .iter()
        .filter(|t| !matches!(t, sqlparser::tokenizer::Token::Whitespace(_)))
        .map(|t| match t {
            sqlparser::tokenizer::Token::Word(w) if w.quote_style.is_none() => {
                Some(w.value.to_ascii_uppercase())
            }
            _ => None,
        })
        .collect();
    let at = |i: usize, w: &str| words.get(i).and_then(Option::as_deref) == Some(w);
    for i in 0..words.len() {
        let strength = if at(i, "FOR") && at(i + 1, "NO") && at(i + 2, "KEY") && at(i + 3, "UPDATE")
        {
            "FOR NO KEY UPDATE"
        } else if at(i, "FOR") && at(i + 1, "KEY") && at(i + 2, "SHARE") {
            "FOR KEY SHARE"
        } else {
            continue;
        };
        return Err(Error::new(format!(
            "{strength} is not read: a question takes no lock on the rows it reads"
        )));
    }
    Ok(())
}

pub fn parse(catalog: &Catalog, sql: &str) -> Result<Question> {
    unlocked_words(sql)?;
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)?;
    let q = match statements.as_slice() {
        [Statement::Query(q)] => q,
        _ => return Err(Error::new("a question is one SELECT")),
    };
    unlocked(q)?;
    let s = match q.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Err(Error::new("set operations are not read")),
    };
    // Each clause the question path does not read, by its name.
    let over = match s.from.len() > 1 || s.from.iter().any(|t| !t.joins.is_empty()) {
        true => " over a join",
        false => "",
    };
    let limit = match &q.limit_clause {
        Some(sqlparser::ast::LimitClause::LimitOffset {
            limit: None,
            offset: Some(_),
            ..
        }) => Some("OFFSET"),
        Some(_) => Some("LIMIT"),
        None => None,
    };
    let distinct = match &s.distinct {
        Some(sqlparser::ast::Distinct::Distinct) => Some("DISTINCT"),
        Some(sqlparser::ast::Distinct::On(_)) => Some("DISTINCT ON"),
        Some(sqlparser::ast::Distinct::All) | None => None,
    };
    let clause = [
        q.with.as_ref().map(|_| "WITH"),
        limit,
        q.fetch.as_ref().map(|_| "FETCH"),
        distinct,
        s.having.as_ref().map(|_| "HAVING"),
    ];
    if let Some(clause) = clause.into_iter().flatten().next() {
        return Err(Error::new(format!("{clause} is not read{over}")));
    }
    // Each table after the first, and how it is joined to those before it: its join, the conjuncts
    // of its ON, and the first table of its FROM item, before which its ON may read nothing.
    let mut tables = Vec::new();
    let mut steps: Vec<(Joining, Vec<&Expr>, usize)> = Vec::new();
    if s.from.is_empty() {
        return Err(Error::new("a question reads a table"));
    }
    for t in &s.from {
        let start = tables.len();
        tables.push(relation(catalog, &t.relation)?);
        if start > 0 {
            steps.push((Joining::Inner, Vec::new(), start));
        }
        for j in &t.joins {
            let (joining, constraint) = match &j.join_operator {
                JoinOperator::Join(c) | JoinOperator::Inner(c) => (Joining::Inner, c),
                JoinOperator::CrossJoin(c @ JoinConstraint::None) => (Joining::Inner, c),
                JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (Joining::Left, c),
                JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (Joining::Right, c),
                JoinOperator::FullOuter(c) => (Joining::Full, c),
                other => {
                    return Err(Error::new(format!(
                        "{} is not read",
                        format!("{other:?}").split('(').next().unwrap_or_default()
                    )))
                }
            };
            let mut on = Vec::new();
            match constraint {
                JoinConstraint::On(e) => conjuncts(e, &mut on),
                JoinConstraint::None if matches!(j.join_operator, JoinOperator::CrossJoin(_)) => {}
                JoinConstraint::Using(_) => {
                    return Err(Error::new(
                        "JOIN … USING is not read: name the columns the tables meet on in ON",
                    ))
                }
                _ => return Err(Error::new("a JOIN is read with its ON")),
            }
            tables.push(relation(catalog, &j.relation)?);
            // The table a RIGHT or FULL JOIN keeps is the one before it, so it is read as the
            // first join.
            if matches!(joining, Joining::Right | Joining::Full) && tables.len() != 2 {
                return Err(Error::new(
                    "a RIGHT or FULL JOIN is read only as the first join: write the tables it \
                     keeps first, joined by LEFT JOIN",
                ));
            }
            steps.push((joining, on, start));
        }
    }
    for (i, (ti, alias)) in tables.iter().enumerate() {
        let name = |(t, a): &(usize, Option<String>)| match a {
            Some(a) => a.clone(),
            None => catalog.tables[*t].name.clone(),
        };
        if tables[..i]
            .iter()
            .any(|x| name(x) == name(&(*ti, alias.clone())))
        {
            return Err(Error::new(format!(
                "table name \"{}\" specified more than once",
                name(&(*ti, alias.clone()))
            )));
        }
    }
    let from = From { catalog, tables };

    let mut listed: Vec<(String, Item)> = Vec::new();
    for item in &s.projection {
        let (e, label) = match item {
            SelectItem::UnnamedExpr(e) => (e, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(ident(alias))),
            other => {
                return Err(Error::new(format!(
                    "{other} is not read: name each column a question returns"
                )))
            }
        };
        let it = match from.aggregate(e)? {
            Some(a) => a,
            None => Item::Key(from.key_of(e)?),
        };
        listed.push((label.unwrap_or_else(|| from.default_name(it)), it));
    }
    let grouping = match &s.group_by {
        GroupByExpr::Expressions(e, modifiers) if modifiers.is_empty() => e,
        _ => return Err(Error::new("GROUP BY ALL and its modifiers are not read")),
    };
    let aggregated = listed.iter().any(|(_, i)| !matches!(i, Item::Key(_)));
    let select = if grouping.is_empty() && !aggregated {
        Select::Rows(
            listed
                .iter()
                .map(|(n, i)| match i {
                    Item::Key(k) => (n.clone(), *k),
                    _ => unreachable!("an aggregate makes the question grouped"),
                })
                .collect(),
        )
    } else {
        let mut keys = Vec::new();
        for g in grouping {
            let k = match number(g) {
                Some(n) => match listed.get((n as usize).wrapping_sub(1)) {
                    Some((_, Item::Key(k))) => *k,
                    _ => {
                        return Err(Error::new(format!(
                            "GROUP BY {n} names no key of the select list"
                        )))
                    }
                },
                None => from.key_of(g)?,
            };
            // A type PostgreSQL has no equality for is grouped by none.
            if let Key::Column(c) = k {
                let ty = from.table(c.side).columns[c.column].ty;
                if !ty.grouped() {
                    return Err(Error::new(format!(
                        "could not identify an equality operator for type {}",
                        ty.pg_name()
                    )));
                }
            }
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        for (_, i) in &listed {
            if let Item::Key(k) = i {
                if !keys.contains(k) {
                    // Named as PostgreSQL names it: by its table's alias, or the table's own name.
                    let c = k.col();
                    let table = from.table(c.side);
                    let named = match &from.tables[c.side].1 {
                        Some(a) => a.clone(),
                        None => table
                            .name
                            .rsplit('.')
                            .next()
                            .unwrap_or_default()
                            .to_string(),
                    };
                    return Err(Error::new(format!(
                        "column \"{named}.{}\" must appear in the GROUP BY clause or be used in an \
                         aggregate function",
                        table.columns[c.column].name
                    )));
                }
            }
        }
        Select::Groups {
            keys,
            items: listed.clone(),
        }
    };
    let mut order = Vec::new();
    if let Some(ob) = &q.order_by {
        let exprs = match &ob.kind {
            OrderByKind::Expressions(e) => e,
            _ => return Err(Error::new("ORDER BY ALL is not read")),
        };
        for o in exprs {
            let at = match number(&o.expr) {
                Some(n) if n >= 1 && (n as usize) <= listed.len() => n as usize - 1,
                Some(n) => {
                    return Err(Error::new(format!(
                        "ORDER BY {n} names no column the answer returns"
                    )))
                }
                None => {
                    let named = match &o.expr {
                        Expr::Identifier(i) => {
                            let name = ident(i);
                            listed.iter().position(|(n, _)| *n == name)
                        }
                        _ => None,
                    };
                    match named {
                        Some(at) => at,
                        None => {
                            let k = from.key_of(&o.expr)?;
                            listed
                                .iter()
                                .position(|(_, i)| *i == Item::Key(k))
                                .ok_or_else(|| {
                                    Error::new(format!(
                                        "ORDER BY {}: order by a column the answer returns",
                                        o.expr
                                    ))
                                })?
                        }
                    }
                }
            };
            let desc = match &o.options.sort {
                None | Some(OrderBySort::Asc) => false,
                Some(OrderBySort::Desc) => true,
                Some(_) => {
                    return Err(Error::new(format!(
                        "ORDER BY {}: USING is not read",
                        o.expr
                    )))
                }
            };
            if let (_, Item::Key(Key::Column(c))) = listed[at] {
                let ty = from.table(c.side).columns[c.column].ty;
                if !ty.ordered() {
                    return Err(Error::new(format!(
                        "could not identify an ordering operator for type {}",
                        ty.pg_name()
                    )));
                }
            }
            order.push(Sort {
                at,
                desc,
                nulls_first: o.options.nulls_first.unwrap_or(desc),
            });
        }
    }

    let mut wheres: Vec<&Expr> = Vec::new();
    if let Some(w) = &s.selection {
        conjuncts(w, &mut wheres);
    }
    // WHERE's conjuncts, and each inner join's ON's, hold of every answer row; an outer join's ON
    // holds only of the pairs of rows it joins.
    let n = from.tables.len();
    let mut pool: Vec<Conj> = Vec::new();
    for p in wheres {
        pool.extend(read_conjunct(&from, p)?);
    }
    let mut nullable = vec![false; n];
    let mut held: Vec<Vec<Conj>> = (0..n).map(|_| Vec::new()).collect();
    let mut full = false;
    for (k, (joining, on, start)) in steps.into_iter().enumerate() {
        let side = k + 1;
        let mut read = Vec::new();
        for p in on {
            for c in read_conjunct(&from, p)? {
                // An ON reads only the tables of its FROM item joined by then.
                if let Some(&s) = c.sides().iter().find(|&&s| s < start || s > side) {
                    return Err(Error::new(format!(
                        "invalid reference to FROM-clause entry for table \"{}\"",
                        from.named(s)
                    )));
                }
                read.push(c);
            }
        }
        match joining {
            Joining::Inner => pool.extend(read),
            Joining::Left => {
                nullable[side] = true;
                held[side] = read;
            }
            Joining::Right => {
                nullable[0] = true;
                held[0] = read;
            }
            Joining::Full => {
                nullable[0] = true;
                nullable[1] = true;
                full = true;
                held[1] = read;
            }
        }
    }
    // A conjunct of every answer row that fails on a NULL of a table an outer join may leave NULL
    // leaves no row where it stands NULL: that join is an inner one, and its ON holds of every
    // answer row too. A FULL JOIN that must keep one of its tables keeps the other as a LEFT JOIN.
    while let Some(s) = pool
        .iter()
        .flat_map(Conj::strict_sides)
        .find(|&s| nullable[s])
    {
        nullable[s] = false;
        if full {
            full = false;
            if s == 1 {
                held[0] = std::mem::take(&mut held[1]);
            }
            continue;
        }
        pool.append(&mut held[s]);
    }
    let mut sides: Vec<Side> = from
        .tables
        .iter()
        .map(|(ti, alias)| Side {
            table: *ti,
            goes_by: alias.clone().unwrap_or_else(|| {
                let name = &catalog.tables[*ti].name;
                name.rsplit('.').next().unwrap_or(name).to_string()
            }),
            conds: Vec::new(),
            texts: Vec::new(),
            nulls: Vec::new(),
            empty: false,
            ops: Vec::new(),
        })
        .collect();
    let mut meets: Vec<Meet> = Vec::new();
    let mut after = Vec::new();
    for c in pool {
        match c {
            Conj::One(s, Piece::Op(o)) if nullable[s] => {
                return Err(Error::new(format!(
                    "{}: on a table an outer join may leave NULL, it is not read yet",
                    o.expr
                )))
            }
            Conj::One(s, piece) if nullable[s] => after.push(Test::One(s, piece)),
            Conj::One(s, piece) => sides[s].take(piece, from.table(s)),
            Conj::Two(m) => {
                if !meets.contains(&m) {
                    meets.push(m);
                }
            }
        }
    }
    let mut outer = Vec::new();
    for s in (0..n).filter(|&s| nullable[s] && !(full && s == 0)) {
        let mut o = Outer {
            side: s,
            meets: Vec::new(),
            tests: Vec::new(),
        };
        for c in std::mem::take(&mut held[s]) {
            match c {
                // A condition on the table it may leave NULL narrows that table's rows; in a FULL
                // JOIN, which keeps both, it is read on each pair.
                Conj::One(t, piece) if t == s && !full => sides[s].take(piece, from.table(s)),
                Conj::One(_, Piece::Op(o)) => {
                    return Err(Error::new(format!(
                        "{}: in an outer join's ON, it is not read yet",
                        o.expr
                    )))
                }
                Conj::One(t, piece) => o.tests.push(Test::One(t, piece)),
                Conj::Two(m) if m.touches(s) && m.op == Cmp::Eq => o.meets.push(m.seen_from(s)),
                Conj::Two(m) => o.tests.push(Test::Two(m)),
            }
        }
        if full && o.meets.is_empty() {
            return Err(Error::new(
                "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions",
            ));
        }
        if o.meets.is_empty()
            && !o
                .tests
                .iter()
                .any(|t| matches!(t, Test::Two(m) if m.touches(s)))
        {
            return Err(Error::new(format!(
                "{} is joined on no key: its ON compares none of its columns with another table's",
                from.named(s)
            )));
        }
        outer.push(o);
    }
    // The tables no outer join may leave NULL are joined to each other by what every answer row
    // holds; a FULL JOIN's two by its ON.
    let mut group: Vec<usize> = (0..n).collect();
    let find = |g: &[usize], mut s: usize| {
        while g[s] != s {
            s = g[s];
        }
        s
    };
    let mut joins: Vec<(usize, usize)> = meets.iter().map(|m| (m.a.side, m.b.side)).collect();
    if full {
        joins.push((0, 1));
    }
    for (a, b) in joins {
        let (x, y) = (find(&group, a), find(&group, b));
        group[x] = y;
    }
    let inner: Vec<usize> = (0..n).filter(|&s| !nullable[s] || full && s < 2).collect();
    if let Some(&s) = inner
        .iter()
        .find(|&&s| find(&group, s) != find(&group, inner[0]))
    {
        return Err(Error::new(format!(
            "{} is joined on no key: compare one of its columns with another table's",
            from.named(s)
        )));
    }
    for side in &mut sides {
        // No comparison matches a NULL, so a column kept to its NULLs and compared holds no row.
        let compared = |c: usize| {
            side.conds
                .iter()
                .any(|x| !x.nulls && crate::read::walk::s_column(x.shape) == c)
                || side.texts.iter().any(|t| t.column == c)
                || side.nulls.contains(&(c, false))
        };
        side.empty |= side.conds.iter().any(Cond::is_empty)
            || side.texts.iter().any(TextCond::is_empty)
            || side.nulls.iter().any(|&(c, null)| null && compared(c));
    }
    Ok(Question {
        sides,
        meets,
        outer,
        full,
        after,
        select,
        order,
    })
}

/// How a table is joined to the tables before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joining {
    Inner,
    Left,
    Right,
    Full,
}

/// One conjunct, as a question reads it: a condition on one table, or two tables' columns compared.
#[derive(Debug)]
enum Conj {
    One(usize, Piece),
    Two(Meet),
}

impl Conj {
    fn sides(&self) -> Vec<usize> {
        match self {
            Conj::One(s, _) => vec![*s],
            Conj::Two(m) => vec![m.a.side, m.b.side],
        }
    }

    /// The tables a NULL of which fails it.
    fn strict_sides(&self) -> Vec<usize> {
        match self {
            Conj::One(s, p) if p.strict() => vec![*s],
            Conj::One(..) => Vec::new(),
            Conj::Two(m) => vec![m.a.side, m.b.side],
        }
    }
}

/// The family of keys a type's values are compared by on a join: integers of any width share one,
/// as do a numeric's of one scale, the oid and the reg* types, the bit strings, and each time type
/// of any precision; every other type has its own, whatever length or fields it is declared with, a
/// composed type the one it is; cidr inet's, and pg_node_tree text's. Where PostgreSQL has no
/// `=` that is equality of the values, its refusal: none at all, on point, polygon, json, jsonpath,
/// xml, refcursor, the snapshots and the index summaries; one too many, on the statistics types;
/// one that compares areas, lengths or points' counts or points within a tolerance, on the other
/// geometric types, a path among them; none for an array or a composite one of whose parts has no
/// equality.
fn join_family(ty: ColumnType) -> std::result::Result<String, String> {
    use crate::textual::{Internal, Textual};
    let none = || format!("operator does not exist: {0} = {0}", ty.pg_name());
    Ok(match ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => "integer".into(),
        ColumnType::Numeric { scale, .. } => format!("numeric {scale}"),
        ColumnType::NumericText(_) => "numeric".into(),
        ColumnType::Text(_) => "text".into(),
        ColumnType::Oid | ColumnType::Reg(..) => "oid".into(),
        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => "timestamptz".into(),
        ColumnType::Timestamp(_) => "timestamp".into(),
        ColumnType::Time(_) => "time".into(),
        ColumnType::Interval { .. } => "interval".into(),
        ColumnType::Bpchar(_) => "bpchar".into(),
        ColumnType::Timetz(_) => "timetz".into(),
        ColumnType::Bit(_) | ColumnType::Varbit(_) => "bits".into(),
        ColumnType::Textual(Textual::Inet | Textual::Cidr) => "inet".into(),
        ColumnType::Textual(
            Textual::Json
            | Textual::Jsonpath
            | Textual::Xml
            | Textual::Refcursor
            | Textual::PgSnapshot
            | Textual::TxidSnapshot,
        ) => return Err(none()),
        ColumnType::Textual(t) => format!("{t:?}"),
        ColumnType::Internal(Internal::NodeTree) => "text".into(),
        ColumnType::Internal(Internal::Ndistinct | Internal::Dependencies | Internal::McvList) => {
            return Err(format!("operator is not unique: {0} = {0}", ty.pg_name()))
        }
        ColumnType::Internal(_)
        | ColumnType::Geometric(crate::geo::GeoKind::Point | crate::geo::GeoKind::Polygon) => {
            return Err(none())
        }
        ColumnType::Geometric(_) => {
            return Err(format!(
                "a join on {} is not read: its = compares areas, lengths, points' counts or \
                 points within a tolerance, not the values",
                ty.pg_name()
            ))
        }
        // An array's or a composite's = asks each part's equality first.
        ColumnType::Composed(_) if ty.unequal_part().is_some() => {
            return Err(format!(
                "could not identify an equality operator for type {}",
                ty.unequal_part().unwrap().pg_name()
            ))
        }
        ColumnType::Bytes
        | ColumnType::Tid
        | ColumnType::Name
        | ColumnType::Boolean
        | ColumnType::Uuid
        | ColumnType::Xid
        | ColumnType::Cid
        | ColumnType::Xid8
        | ColumnType::PgLsn
        | ColumnType::Char
        | ColumnType::Money(_)
        | ColumnType::Macaddr
        | ColumnType::Macaddr8
        | ColumnType::Float4
        | ColumnType::Float8
        | ColumnType::Oid8
        | ColumnType::Date
        | ColumnType::Composed(_) => format!("{ty:?}"),
    })
}

/// Whether two tables' columns are compared by `op` on the keys they are held by: two columns of
/// one family, by an operator their type has, and two strings under one collation.
fn comparable(
    p: &Expr,
    ca: &crate::catalog::Column,
    op: Cmp,
    cb: &crate::catalog::Column,
) -> Result<()> {
    let (a, b) = (ca.ty, cb.ty);
    let fa = join_family(a).map_err(|e| Error::new(format!("{p}: {e}")))?;
    let fb = join_family(b).map_err(|e| Error::new(format!("{p}: {e}")))?;
    match (ca.collated(), cb.collated()) {
        (true, true) if ca.collation != cb.collation => {
            return Err(Error::new(format!(
                "{p}: could not determine which collation to use for string comparison"
            )))
        }
        (true, false) | (false, true) => {
            let named = if ca.collated() {
                &ca.collation
            } else {
                &cb.collation
            };
            return Err(Error::new(format!(
                "{p}: a join of a column under the collation {} with one compared by its bytes \
                 is not read",
                named.as_deref().unwrap_or_default()
            )));
        }
        _ => {}
    }
    if fa != fb {
        return Err(Error::new(format!(
            "{p}: {} and {} are compared through a cast, which a join does not read",
            a.pg_name(),
            b.pg_name()
        )));
    }
    let has = match a {
        ColumnType::Cid | ColumnType::Textual(crate::textual::Textual::Aclitem) => op == Cmp::Eq,
        a if !a.ordered() => matches!(op, Cmp::Eq | Cmp::NotEq),
        _ => true,
    };
    if !has {
        let sign = match op {
            Cmp::Eq => "=",
            Cmp::NotEq => "<>",
            Cmp::Lt => "<",
            Cmp::LtEq => "<=",
            Cmp::Gt => ">",
            Cmp::GtEq => ">=",
        };
        return Err(Error::new(format!(
            "operator does not exist: {} {sign} {}",
            a.pg_name(),
            b.pg_name()
        )));
    }
    Ok(())
}

/// A part of an OR or a NOT read on one table: what it asks of the element strands, the condition a
/// row is held to, the columns it reads, and whether an operator a strand may answer stands in it.
struct Arms {
    side: usize,
    arm: Arm,
    expr: Expr,
    columns: Vec<usize>,
    element: bool,
}

/// The names of a condition read as its table's columns, written bare; false where it holds what a
/// row is not held to here.
fn own_names(e: &mut Expr, table: &Table, columns: &mut Vec<usize>) -> bool {
    let mut one = |e: &mut Expr| own_names(e, table, columns);
    match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            let name = match e {
                Expr::Identifier(i) => ident(i),
                Expr::CompoundIdentifier(p) => ident(p.last().expect("a name has a last part")),
                _ => unreachable!("a name is one of these"),
            };
            let Some(c) = table.columns.iter().position(|x| x.name == name) else {
                return false;
            };
            if !columns.contains(&c) {
                columns.push(c);
            }
            *e = Expr::Identifier(sqlparser::ast::Ident::with_quote('"', name));
            true
        }
        Expr::Value(_) => true,
        Expr::Nested(x)
        | Expr::UnaryOp { expr: x, .. }
        | Expr::Cast { expr: x, .. }
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotTrue(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x) => one(x),
        Expr::BinaryOp { left, right, .. }
        | Expr::Like {
            expr: left,
            pattern: right,
            ..
        }
        | Expr::ILike {
            expr: left,
            pattern: right,
            ..
        }
        | Expr::SimilarTo {
            expr: left,
            pattern: right,
            ..
        } => one(left) && one(right),
        Expr::Between {
            expr, low, high, ..
        } => one(expr) && one(low) && one(high),
        Expr::InList { expr, list, .. } => one(expr) && list.iter_mut().all(one),
        _ => false,
    }
}

/// An OR, a NOT or what stands under one, read on one table.
fn arms_of(from: &From, e: &Expr) -> Result<Arms> {
    let not_one = || Error::new(format!("{e} reads more than one table"));
    match e {
        Expr::Nested(x) => {
            let mut a = arms_of(from, x)?;
            a.expr = Expr::Nested(Box::new(a.expr));
            Ok(a)
        }
        Expr::BinaryOp {
            left,
            op: op @ (BinaryOperator::And | BinaryOperator::Or),
            right,
        } => {
            let (l, r) = (arms_of(from, left)?, arms_of(from, right)?);
            if l.side != r.side {
                return Err(not_one());
            }
            let mut columns = l.columns;
            columns.extend(r.columns);
            columns.sort_unstable();
            columns.dedup();
            Ok(Arms {
                side: l.side,
                arm: match op {
                    BinaryOperator::And => Arm::All(vec![l.arm, r.arm]),
                    _ => Arm::Any(vec![l.arm, r.arm]),
                },
                expr: Expr::BinaryOp {
                    left: Box::new(l.expr),
                    op: op.clone(),
                    right: Box::new(r.expr),
                },
                columns,
                element: l.element || r.element,
            })
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => {
            let inner = arms_of(from, expr)?;
            Ok(Arms {
                arm: Arm::Held,
                expr: Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(inner.expr),
                },
                ..inner
            })
        }
        e => {
            let read = read_conjunct(from, e)?;
            let side = match read.as_slice() {
                [Conj::One(s, _), rest @ ..]
                    if rest.iter().all(|c| matches!(c, Conj::One(t, _) if t == s)) =>
                {
                    *s
                }
                _ => return Err(not_one()),
            };
            if let [Conj::One(_, Piece::Op(o))] = read.as_slice() {
                if o.value.is_some() {
                    return Ok(Arms {
                        side,
                        arm: Arm::Op(Box::new(o.clone())),
                        expr: o.expr.clone(),
                        columns: vec![o.column],
                        element: true,
                    });
                }
            }
            let (mut expr, mut columns) = (e.clone(), Vec::new());
            if !own_names(&mut expr, from.table(side), &mut columns) {
                return Err(Error::new(format!("{e} is not held a row at a time here")));
            }
            let mut own = Side {
                table: from.tables[side].0,
                goes_by: String::new(),
                conds: Vec::new(),
                texts: Vec::new(),
                nulls: Vec::new(),
                empty: false,
                ops: Vec::new(),
            };
            for c in read {
                if let Conj::One(_, piece) = c {
                    own.take(piece, from.table(side));
                }
            }
            Ok(Arms {
                side,
                arm: Arm::Read(Box::new(own)),
                expr,
                columns,
                element: false,
            })
        }
    }
}

/// An OR or a NOT over conditions of one table with an operator a strand may answer among them; none
/// where it is any other, or reads what is not held a row at a time here.
fn over_operators(from: &From, p: &Expr) -> Option<Conj> {
    let mut e = p;
    while let Expr::Nested(x) = e {
        e = x;
    }
    if !matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOperator::Or,
            ..
        } | Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        }
    ) {
        return None;
    }
    let a = arms_of(from, e).ok().filter(|a| a.element)?;
    let table = from.table(a.side);
    let name = |c: usize| table.columns[c].name.clone();
    Some(Conj::One(
        a.side,
        Piece::Op(OpCond {
            column: a.columns[0],
            expr: a.expr,
            name: name(a.columns[0]),
            op: String::new(),
            probe: None,
            value: None,
            arms: Some(a.arm),
            beside: a.columns[1..].iter().map(|&c| (c, name(c))).collect(),
        }),
    ))
}

/// What one conjunct says, as the conditions and comparisons PostgreSQL's analyser reads it as: a
/// BETWEEN as its comparisons; a pattern as itself, and the keys its fixed prefix narrows to.
fn read_conjunct(from: &From, p: &Expr) -> Result<Vec<Conj>> {
    if let Some(read) = over_operators(from, p) {
        return Ok(vec![read]);
    }
    if let Some(read) = pattern(from, p)? {
        return Ok(read);
    }
    match p {
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => between(from, p, expr, *negated, low, high),
        p => Ok(vec![conjunct(from, p)?]),
    }
}

/// How a pattern matches: LIKE, ILIKE, SIMILAR TO, or a regular expression, `~*` and `!~*`
/// ignoring case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Matched {
    Like,
    ILike,
    Similar,
    Regex { icase: bool },
}

/// The casing a column's text takes, as a pipeline's `Casings` gives it: its collation's, a name's
/// ASCII's, else the database's default.
fn column_casing(
    column: &crate::catalog::Column,
    declared: &[crate::catalog::Collation],
) -> Result<crate::catalog::Casing> {
    use crate::catalog::Casing;
    Ok(match (&column.collation, column.ty) {
        (Some(n), _) => crate::catalog::collation_casing(n, declared)?,
        (None, ColumnType::Name) => Casing::Ascii,
        _ => Casing::Libc,
    })
}

/// A pattern on a column of a string type, or LIKE on a bytea, against a quoted pattern and
/// escape: held a row at a time as a pipeline holds it, and, where it is no negation, the keys
/// its fixed prefix narrows the read to (`prefix::narrowing`). A NULL pattern or escape keeps no
/// row. A regular expression is compiled as it is read, as PostgreSQL's planner compiles it to
/// read its prefix, so a pattern it refuses is refused over no rows too. None where it is no such
/// pattern.
fn pattern(from: &From, p: &Expr) -> Result<Option<Vec<Conj>>> {
    let (x, pat, escape, negated, how) = match p {
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char,
        } => (expr, pattern, escape_char, *negated, Matched::Like),
        Expr::ILike {
            negated,
            any: false,
            expr,
            pattern,
            escape_char,
        } => (expr, pattern, escape_char, *negated, Matched::ILike),
        Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => (expr, pattern, escape_char, *negated, Matched::Similar),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::PGRegexMatch
            | BinaryOperator::PGRegexIMatch
            | BinaryOperator::PGRegexNotMatch
            | BinaryOperator::PGRegexNotIMatch => (
                left,
                right,
                &None,
                matches!(
                    op,
                    BinaryOperator::PGRegexNotMatch | BinaryOperator::PGRegexNotIMatch
                ),
                Matched::Regex {
                    icase: matches!(
                        op,
                        BinaryOperator::PGRegexIMatch | BinaryOperator::PGRegexNotIMatch
                    ),
                },
            ),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    // The column itself: a COLLATE beside it changes its classes and its case, which a pipeline
    // reads.
    if !matches!(
        x.as_ref(),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_)
    ) {
        return Ok(None);
    }
    let Ok((side, Shape::Column(c))) = from.resolve(x) else {
        return Ok(None);
    };
    let table = from.table(side);
    let column = &table.columns[c];
    if !(column.ty.is_string() || (column.ty == ColumnType::Bytes && how == Matched::Like)) {
        return Ok(None);
    }
    let null = |e: &Expr| matches!(e, Expr::Value(v) if v.value == Value::Null);
    if null(pat) || escape.as_deref().is_some_and(null) {
        return Ok(Some(vec![Conj::One(side, Piece::Empty)]));
    }
    let (Ok(text), Ok(escape)) = (
        text_literal(pat),
        escape.as_deref().map(text_literal).transpose(),
    ) else {
        return Ok(None);
    };
    let ctype = || super::calls::ctype_of(column_casing(column, &from.catalog.collations)?);
    let fixed = match how {
        Matched::Regex { icase } => {
            let fixed = super::prefix::regex_fixed_prefix(&text, icase, ctype()?)?;
            if negated {
                super::prefix::Fixed::None
            } else {
                fixed
            }
        }
        Matched::Similar => {
            let re = crate::regexp::similar_to_escape(&text, escape.as_deref())?;
            let fixed = super::prefix::regex_fixed_prefix(&re, false, ctype()?)?;
            if negated {
                super::prefix::Fixed::None
            } else {
                fixed
            }
        }
        _ if negated => super::prefix::Fixed::None,
        Matched::Like if column.ty == ColumnType::Bytes => match escape {
            None => super::prefix::like_fixed_prefix(&crate::strings::bytea_in(&text)?),
            Some(_) => super::prefix::Fixed::None,
        },
        Matched::Like | Matched::ILike => {
            let escape = match &escape {
                None => Some('\\'),
                Some(e) => crate::strings::like_escape(e)?,
            };
            if how == Matched::Like {
                super::prefix::like_fixed_prefix_chars(&text, escape, |_| false)
            } else {
                match ctype() {
                    // A character of more than one case is a wildcard of its cases.
                    Ok(t) => super::prefix::like_fixed_prefix_chars(&text, escape, |ch| {
                        let c = crate::regex::Chr::from(ch);
                        t.isupper(c) || t.islower(c) || t.toupper(c) != c || t.tolower(c) != c
                    }),
                    Err(_) => super::prefix::Fixed::None,
                }
            }
        }
    };
    let own = Box::new(Expr::Identifier(sqlparser::ast::Ident::with_quote(
        '"',
        column.name.clone(),
    )));
    let expr = match p.clone() {
        Expr::Like {
            negated,
            any,
            pattern,
            escape_char,
            ..
        } => Expr::Like {
            negated,
            any,
            expr: own,
            pattern,
            escape_char,
        },
        Expr::ILike {
            negated,
            any,
            pattern,
            escape_char,
            ..
        } => Expr::ILike {
            negated,
            any,
            expr: own,
            pattern,
            escape_char,
        },
        Expr::SimilarTo {
            negated,
            pattern,
            escape_char,
            ..
        } => Expr::SimilarTo {
            negated,
            expr: own,
            pattern,
            escape_char,
        },
        Expr::BinaryOp { op, right, .. } => Expr::BinaryOp {
            left: own,
            op,
            right,
        },
        _ => unreachable!("a pattern is one of these"),
    };
    let mut read = vec![Conj::One(
        side,
        Piece::Op(OpCond {
            column: c,
            expr,
            name: column.name.clone(),
            op: String::new(),
            probe: None,
            value: None,
            arms: None,
            beside: Vec::new(),
        }),
    )];
    if let Some(t) = super::prefix::narrowing(c, column, fixed)? {
        read.push(Conj::One(side, Piece::Text(t)));
    }
    Ok(Some(read))
}

/// A BETWEEN's lower bound, and whether it is SYMMETRIC: `spelled` writes `BETWEEN SYMMETRIC x`
/// and `BETWEEN ASYMMETRIC x` as a call of the keyword on `(x)`, which no function can be, the
/// keyword being reserved.
pub(super) fn between_low(low: &Expr) -> (&Expr, bool) {
    if let Expr::Function(f) = low {
        let word = match f.name.0.as_slice() {
            [part] => part
                .as_ident()
                .filter(|i| i.quote_style.is_none())
                .map(|i| i.value.to_ascii_uppercase()),
            _ => None,
        };
        if let (Some(word), FunctionArguments::List(l)) = (word, &f.args) {
            if let [FunctionArg::Unnamed(FunctionArgExpr::Expr(x))] = l.args.as_slice() {
                match word.as_str() {
                    "SYMMETRIC" => return (x, true),
                    "ASYMMETRIC" => return (x, false),
                    _ => {}
                }
            }
        }
    }
    (low, false)
}

/// `x BETWEEN a AND b` as `x >= a AND x <= b`; NOT BETWEEN as `x < a OR x > b`; SYMMETRIC as both
/// orders of the bounds or'd, and NOT BETWEEN SYMMETRIC as both orders' NOT BETWEEN and'd, as
/// PostgreSQL's `transformAExprBetween` writes them. A BETWEEN is the two comparisons; the others
/// are read where every comparison is of one column of one table with a value.
fn between(
    from: &From,
    p: &Expr,
    x: &Expr,
    negated: bool,
    low: &Expr,
    high: &Expr,
) -> Result<Vec<Conj>> {
    let (low, symmetric) = between_low(low);
    let compare = |op: BinaryOperator, bound: &Expr| {
        conjunct(
            from,
            &Expr::BinaryOp {
                left: Box::new(x.clone()),
                op,
                right: Box::new(bound.clone()),
            },
        )
    };
    if !negated && !symmetric {
        return Ok(vec![
            compare(BinaryOperator::GtEq, low)?,
            compare(BinaryOperator::LtEq, high)?,
        ]);
    }
    let unread = || {
        Error::new(format!(
            "{p}: NOT BETWEEN and BETWEEN SYMMETRIC are read of one table's column against values"
        ))
    };
    // Each comparison as a condition on its column: its side, and the piece.
    let piece = |op: BinaryOperator, bound: &Expr| -> Result<(usize, Piece)> {
        match compare(op, bound)? {
            Conj::One(side, p @ (Piece::Cond(_) | Piece::Text(_) | Piece::Empty)) => Ok((side, p)),
            _ => Err(unread()),
        }
    };
    let pieces = |a: BinaryOperator, b: BinaryOperator, first: &Expr, second: &Expr| {
        Ok::<_, Error>([piece(a, first)?, piece(b, second)?])
    };
    let both = |[(sa, a), (sb, b)]: [(usize, Piece); 2]| -> Result<(usize, Piece)> {
        if sa != sb {
            return Err(unread());
        }
        Ok((
            sa,
            match (a, b) {
                (Piece::Empty, _) | (_, Piece::Empty) => Piece::Empty,
                (Piece::Cond(c), Piece::Cond(d)) if c.shape == d.shape => {
                    let mut kept = vec![c];
                    merge(&mut kept, d);
                    Piece::Cond(kept.pop().unwrap())
                }
                (Piece::Text(mut t), Piece::Text(u)) if t.column == u.column => {
                    both_texts(&mut t, u);
                    Piece::Text(t)
                }
                _ => return Err(unread()),
            },
        ))
    };
    let either = |(sa, a): (usize, Piece), (sb, b): (usize, Piece)| -> Result<(usize, Piece)> {
        if sa != sb {
            return Err(unread());
        }
        Ok((
            sa,
            match (a, b) {
                (Piece::Empty, p) | (p, Piece::Empty) => p,
                (Piece::Cond(c), Piece::Cond(d)) if c.shape == d.shape => {
                    Piece::Cond(either_conds(c, d).ok_or_else(unread)?)
                }
                (Piece::Text(t), Piece::Text(u)) if t.column == u.column => {
                    Piece::Text(either_texts(t, u).ok_or_else(unread)?)
                }
                _ => return Err(unread()),
            },
        ))
    };
    use BinaryOperator::{Gt, GtEq, Lt, LtEq};
    let outside = |a: &Expr, b: &Expr| -> Result<(usize, Piece)> {
        let [lt, gt] = pieces(Lt, Gt, a, b)?;
        either(lt, gt)
    };
    let read = match (negated, symmetric) {
        (true, false) => outside(low, high)?,
        (false, true) => either(
            both(pieces(GtEq, LtEq, low, high)?)?,
            both(pieces(GtEq, LtEq, high, low)?)?,
        )?,
        _ => both([outside(low, high)?, outside(high, low)?])?,
    };
    Ok(vec![Conj::One(read.0, read.1)])
}

/// What one conjunct says: a condition on one table's column, or two tables' columns compared.
/// `x @@@ y` as the `@@` it is on a tsvector and a tsquery; refused on a column of any other type, as
/// PostgreSQL refuses it.
fn three_ats(from: &From, p: &Expr) -> Result<Option<Expr>> {
    let Expr::BinaryOp {
        left,
        op: op @ BinaryOperator::AtAt,
        right,
    } = p
    else {
        return Ok(None);
    };
    let Some(inner) = super::spelled::right_of_three_ats(right) else {
        return Ok(None);
    };
    for e in [left.as_ref(), inner] {
        if let Ok((side, Shape::Column(c))) = from.resolve(e) {
            let ty = from.table(side).columns[c].ty;
            if !matches!(
                ty,
                ColumnType::Textual(
                    crate::textual::Textual::Tsvector | crate::textual::Textual::Tsquery
                )
            ) {
                return Err(Error::new(format!(
                    "operator does not exist: {} @@@ unknown",
                    ty.pg_name()
                )));
            }
        }
    }
    Ok(Some(Expr::BinaryOp {
        left: left.clone(),
        op: op.clone(),
        right: Box::new(inner.clone()),
    }))
}

fn conjunct(from: &From, p: &Expr) -> Result<Conj> {
    let matched = three_ats(from, p)?;
    let p = matched.as_ref().unwrap_or(p);
    if let Some((side, cond)) = truth_test(from, p)? {
        return Ok(Conj::One(side, Piece::Cond(cond)));
    }
    if let Expr::IsUnknown(x) | Expr::IsNotUnknown(x) = p {
        if let (side, Shape::Column(c)) = from.resolve(x)? {
            let ty = from.table(side).columns[c].ty;
            if ty != ColumnType::Boolean {
                return Err(Error::new(format!(
                    "argument of {} must be type boolean, not type {}",
                    if matches!(p, Expr::IsUnknown(_)) {
                        "IS UNKNOWN"
                    } else {
                        "IS NOT UNKNOWN"
                    },
                    ty.pg_name()
                )));
            }
        }
    }
    if let Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::IsUnknown(x) | Expr::IsNotUnknown(x) = p {
        let null = matches!(p, Expr::IsNull(_) | Expr::IsUnknown(_));
        let (side, c) = match from.resolve(x)? {
            (side, Shape::Column(c)) => (side, c),
            _ => return Err(Error::new(format!("{p}: test a column itself for NULL"))),
        };
        return Ok(Conj::One(side, Piece::Null(c, null)));
    }
    let (left, op, right) = match p {
        Expr::BinaryOp { left, op, right } => (left.as_ref(), op.clone(), right.as_ref()),
        other => {
            return Err(Error::new(format!(
                "{other} is not a comparison the snail reads"
            )))
        }
    };
    if let (Ok((a, Shape::Column(x))), Ok((b, Shape::Column(y)))) =
        (from.resolve(left), from.resolve(right))
    {
        if a == b {
            return Err(Error::new(format!(
                "{p}: two columns of one table compared are not read"
            )));
        }
        let op = match op {
            BinaryOperator::Eq => Cmp::Eq,
            BinaryOperator::NotEq => Cmp::NotEq,
            BinaryOperator::Lt => Cmp::Lt,
            BinaryOperator::LtEq => Cmp::LtEq,
            BinaryOperator::Gt => Cmp::Gt,
            BinaryOperator::GtEq => Cmp::GtEq,
            other => {
                return Err(Error::new(format!(
                    "{p}: two tables are compared by =, <>, <, <=, > or >=, not {other}"
                )))
            }
        };
        comparable(p, &from.table(a).columns[x], op, &from.table(b).columns[y])?;
        return Ok(Conj::Two(Meet {
            a: Col { side: a, column: x },
            op,
            b: Col { side: b, column: y },
        }));
    }
    let lit_left = from.resolve(left).is_err();
    let (side, s, lit, op) = match from.resolve(left) {
        Ok((side, s)) => (side, s, right, op),
        Err(on_left) => {
            // Neither side a level: the one that is no value is named.
            let (side, s) = match from.resolve(right) {
                Ok(found) => found,
                Err(_) if !valued(left) => return Err(on_left),
                Err(on_right) => return Err(on_right),
            };
            let flipped = match op {
                BinaryOperator::Lt => BinaryOperator::Gt,
                BinaryOperator::LtEq => BinaryOperator::GtEq,
                BinaryOperator::Gt => BinaryOperator::Lt,
                BinaryOperator::GtEq => BinaryOperator::LtEq,
                other => other,
            };
            (side, s, left, flipped)
        }
    };
    let table = from.table(side);
    // xid has only `=` and `<>`, an integer on their right; cid only `=`; a type held as its
    // text that PostgreSQL does not order, none, nor do the statistics types, whose `=` is bytea's
    // and text's both. Asked first, as PostgreSQL finds the operator before it reads a value.
    if let Shape::Column(c) = s {
        let ty = table.columns[c].ty;
        let has = match ty {
            ColumnType::Textual(crate::textual::Textual::Aclitem) => {
                matches!(op, BinaryOperator::Eq)
            }
            ColumnType::Textual(t) => t.ordered(),
            ColumnType::Internal(t) => t == crate::textual::Internal::NodeTree,
            ColumnType::Xid => matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq),
            ColumnType::Cid => matches!(op, BinaryOperator::Eq),
            _ => true,
        };
        let number = super::statement::number_datum(lit).is_some();
        if !has || (ty == ColumnType::Xid && lit_left && number) {
            return Err(Error::new(format!(
                "operator does not exist: {} {op} {}",
                if lit_left {
                    "integer".into()
                } else {
                    ty.pg_name()
                },
                if lit_left {
                    ty.pg_name()
                } else {
                    "unknown".into()
                }
            )));
        }
    }
    if let Shape::Column(c) = s {
        if let Some(o) = op_cond(table, c, &op, lit, lit_left)? {
            return Ok(Conj::One(side, Piece::Op(o)));
        }
    }
    // No comparison matches a NULL, so one with NULL keeps no row.
    if matches!(lit, Expr::Value(v) if v.value == Value::Null)
        && matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
        )
    {
        return Ok(Conj::One(side, Piece::Empty));
    }
    if let Shape::Column(c) = s {
        let ty = table.columns[c].ty;
        // A numeric(p, s) by the keys either side of the number, whatever its scale.
        if let ColumnType::Numeric { precision, scale } = ty {
            let n = numeric_literal(lit)?;
            let (at, below, above) = crate::numeric::column_bounds(&n, precision, scale);
            let none = Interval { lo: 1, hi: 0 };
            let cond = match op {
                BinaryOperator::Eq => Cond::new(s, at.map_or(none, Interval::at)),
                BinaryOperator::NotEq => {
                    let mut c = Cond::new(s, Interval::ALL);
                    c.except.extend(at);
                    c
                }
                BinaryOperator::Lt => Cond::new(
                    s,
                    Interval {
                        lo: i128::MIN,
                        hi: below,
                    },
                ),
                BinaryOperator::LtEq => Cond::new(
                    s,
                    Interval {
                        lo: i128::MIN,
                        hi: at.unwrap_or(below),
                    },
                ),
                BinaryOperator::Gt => {
                    Cond::new(s, above.map_or(none, |lo| Interval { lo, hi: i128::MAX }))
                }
                BinaryOperator::GtEq => Cond::new(
                    s,
                    at.or(above)
                        .map_or(none, |lo| Interval { lo, hi: i128::MAX }),
                ),
                other => return Err(Error::new(format!("the comparison {other} is not read"))),
            };
            return Ok(Conj::One(side, Piece::Cond(cond)));
        }
        if ty.keyed_by_text() || ty.keyed_by_bytes() {
            if !ty.ordered() && !matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) {
                return Err(Error::new(format!(
                    "could not identify a comparison function for type {}",
                    ty.pg_name()
                )));
            }
            // An array's or a composite's = asks each part's equality first.
            if let Some(part) = ty.unequal_part().filter(|_| ty.is_nested()) {
                return Err(Error::new(format!(
                    "could not identify an equality operator for type {}",
                    part.pg_name()
                )));
            }
            // A numeric held as its text, by its class's key; a composed value by its key,
            // written as text that sorts as the key does.
            let v = match ty {
                ColumnType::NumericText(_) => numeric_literal(lit)?.class_key().into_bytes(),
                ty if ty.is_nested() => match crate::composed::compared(ty, lit)? {
                    super::statement::Datum::Compound(k) => k.hex.as_bytes().to_vec(),
                    _ => {
                        return Err(Error::new(format!(
                            "{lit} is not a value of {}",
                            ty.pg_name()
                        )))
                    }
                },
                _ => bytes_literal(lit, ty)?,
            };
            // Under a library collation, by the collation's key.
            let v = match table.columns[c].orderer()? {
                Some(k) => k.key(&v),
                None => v,
            };
            let mut except = Vec::new();
            let (lo, hi) = match op {
                BinaryOperator::Eq => (Included(v.clone()), Included(v)),
                BinaryOperator::NotEq => {
                    except.push(v);
                    (Unbounded, Unbounded)
                }
                BinaryOperator::Lt => (Unbounded, Excluded(v)),
                BinaryOperator::LtEq => (Unbounded, Included(v)),
                BinaryOperator::Gt => (Excluded(v), Unbounded),
                BinaryOperator::GtEq => (Included(v), Unbounded),
                other => {
                    return Err(Error::new(format!(
                        "the comparison {other} on {} is not read",
                        ty.pg_name()
                    )))
                }
            };
            return Ok(Conj::One(
                side,
                Piece::Text(TextCond {
                    except,
                    ..TextCond::new(c, lo, hi)
                }),
            ));
        }
    }
    // A real beside a number written out is compared as a double precision: where no real is
    // that number, the reals either side of it bound what is kept.
    if let Shape::Column(c) = s {
        if table.columns[c].ty == ColumnType::Float4 && text_literal(lit).is_err() {
            if let Some(t) = super::statement::number_text(lit) {
                let d = f64::from_bits(crate::scalars::float_parse(&t, true)?);
                if let Some((below, above)) = crate::scalars::float4_between(d) {
                    let (below, above) = (i128::from(below), i128::from(above));
                    let cond = match op {
                        BinaryOperator::Eq => Cond::new(s, Interval { lo: 1, hi: 0 }),
                        BinaryOperator::NotEq => Cond::new(s, Interval::ALL),
                        BinaryOperator::Lt | BinaryOperator::LtEq => Cond::new(
                            s,
                            Interval {
                                lo: i128::MIN,
                                hi: below,
                            },
                        ),
                        BinaryOperator::Gt | BinaryOperator::GtEq => Cond::new(
                            s,
                            Interval {
                                lo: above,
                                hi: i128::MAX,
                            },
                        ),
                        other => {
                            return Err(Error::new(format!("the comparison {other} is not read")))
                        }
                    };
                    return Ok(Conj::One(side, Piece::Cond(cond)));
                }
                let key = crate::scalars::float4_key((d as f32).to_bits());
                let cond = match op {
                    BinaryOperator::Eq => Cond::new(s, Interval::at(key.into())),
                    _ => Cond::new(s, Interval::ALL),
                };
                if op == BinaryOperator::Eq {
                    return Ok(Conj::One(side, Piece::Cond(cond)));
                }
            }
        }
    }
    let (s, v) = match s {
        Shape::Clock(c) => (Shape::Column(c), clock_literal(lit)? as i128),
        _ => (s, literal(lit, s, table)?),
    };
    let cond = match op {
        BinaryOperator::Eq => Cond::new(s, Interval::at(v)),
        BinaryOperator::NotEq => {
            let mut c = Cond::new(s, Interval::ALL);
            c.except.push(v);
            c
        }
        // A strict bound past either end keeps nothing.
        BinaryOperator::Lt => match v.checked_sub(1) {
            Some(hi) => Cond::new(s, Interval { lo: i128::MIN, hi }),
            None => Cond::new(s, Interval { lo: 1, hi: 0 }),
        },
        BinaryOperator::LtEq => Cond::new(
            s,
            Interval {
                lo: i128::MIN,
                hi: v,
            },
        ),
        BinaryOperator::Gt => match v.checked_add(1) {
            Some(lo) => Cond::new(s, Interval { lo, hi: i128::MAX }),
            None => Cond::new(s, Interval { lo: 1, hi: 0 }),
        },
        BinaryOperator::GtEq => Cond::new(
            s,
            Interval {
                lo: v,
                hi: i128::MAX,
            },
        ),
        other => return Err(Error::new(format!("the comparison {other} is not read"))),
    };
    Ok(Conj::One(side, Piece::Cond(cond)))
}

/// A condition by an operator a GiST or SP-GiST strand answers, on column `c` and the value `lit`,
/// the column on the operator's left unless `lit_left`; none where `op` is no such operator on the
/// column's type.
fn op_cond(
    table: &Table,
    c: usize,
    op: &BinaryOperator,
    lit: &Expr,
    lit_left: bool,
) -> Result<Option<OpCond>> {
    use crate::composed::{Composed, Item};
    if let Some(o) = super::elements::element_cond(table, c, op, lit, lit_left)? {
        return Ok(Some(o));
    }
    let ty = table.columns[c].ty;
    if crate::plane::on_plane(ty) {
        return plane_cond(table, c, op, lit, lit_left);
    }
    let name = op.to_string();
    let def = match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    };
    let ranged = matches!(
        def,
        Some(Composed::Range { .. } | Composed::Multirange { .. })
    );
    let inet = crate::spans::is_inet(ty);
    let read = if ranged {
        ["&&", "@>", "<@", "<<", ">>", "&<", "&>", "-|-"].contains(&name.as_str())
    } else {
        inet && ["&&", "<<", "<<=", ">>", ">>="].contains(&name.as_str())
    };
    if !read {
        return Ok(None);
    }
    // A value of a type no form of the operator takes beside the column is refused before any row.
    if ranged {
        let other = super::typed::Typing {
            columns: &[],
            release: table.release,
            types: super::typed::Types::new(),
        }
        .type_of(lit)?;
        let (a, b) = if lit_left {
            (other, Some(ty))
        } else {
            (Some(ty), other)
        };
        super::typed::range_operator(&name, a, b)?;
    }
    let column = table.columns[c].name.clone();
    let own = Box::new(Expr::Identifier(sqlparser::ast::Ident::with_quote(
        '"',
        column.clone(),
    )));
    let (left, right) = if lit_left {
        (Box::new(lit.clone()), own)
    } else {
        (own, Box::new(lit.clone()))
    };
    let expr = Expr::BinaryOp {
        left,
        op: op.clone(),
        right,
    };
    // The operator as the column, on its left, reads it; `&<` and `&>` have no other side.
    let seen = if !lit_left {
        Some(name.as_str())
    } else {
        match name.as_str() {
            "@>" => Some("<@"),
            "<@" => Some("@>"),
            "<<" => Some(">>"),
            ">>" => Some("<<"),
            "<<=" => Some(">>="),
            ">>=" => Some("<<="),
            "&&" | "-|-" => Some(name.as_str()),
            _ => None,
        }
    };
    let quoted = matches!(lit, Expr::Value(v) if matches!(v.value, Value::SingleQuotedString(_)));
    let span = |ty: ColumnType, item: &Item| match crate::composed::span_bounds(ty, item) {
        Some((lo, hi)) => crate::spans::Probe::Span(lo, hi),
        None => crate::spans::Probe::Empty,
    };
    let probe = if inet {
        let text = crate::inet::inet_in(&text_literal(lit)?, false)?;
        let (lo, hi) = crate::inet::inet_span(&text);
        Some(crate::spans::Probe::Span(lo, hi))
    } else {
        let (subtype, _) = crate::composed::range_of(ty).unwrap();
        let range_ty = match def {
            Some(Composed::Multirange { range, .. }) => Some(*range),
            _ => None,
        };
        // A value cast to the multirange's range type is a range; any other its own type's.
        let cast_to_range = range_ty.filter(|r| {
            matches!(lit, Expr::Cast { data_type, .. }
                if crate::catalog::format_type(data_type) == r.pg_name())
        });
        let element = seen == Some("@>") && !quoted && cast_to_range.is_none();
        if element {
            match crate::composed::literal(subtype, lit) {
                Ok(d) => crate::composed::element_bounds(ty, &Item::Scalar(d))
                    .map(|(lo, hi)| crate::spans::Probe::Span(lo, hi)),
                Err(_) => None,
            }
        } else {
            let as_ty = cast_to_range.unwrap_or(ty);
            match crate::composed::compared(as_ty, lit) {
                Ok(super::statement::Datum::Compound(k)) => Some(span(as_ty, &k.item)),
                Ok(_) => None,
                Err(e) if quoted => return Err(e),
                Err(_) => None,
            }
        }
    };
    Ok(Some(OpCond {
        column: c,
        expr,
        name: column,
        op: seen.unwrap_or_default().to_string(),
        probe: seen.and(probe),
        value: None,
        arms: None,
        beside: Vec::new(),
    }))
}

/// A condition on a column of the plane by an operator its operator classes take, with a constant:
/// a value on the left read through the operator's other form, where it has one. A string literal is
/// read as the column's type, but beside a point's `<@`, where it is no one type.
fn plane_cond(
    table: &Table,
    c: usize,
    op: &BinaryOperator,
    lit: &Expr,
    lit_left: bool,
) -> Result<Option<OpCond>> {
    use super::pipeline::{eval_on, Casings, Value as Held};
    let ColumnType::Geometric(kind) = table.columns[c].ty else {
        return Ok(None);
    };
    let name = op.to_string();
    let seen = if lit_left {
        crate::plane::commuted(&name)
    } else {
        Some(name.as_str())
    };
    let quoted = matches!(lit, Expr::Value(v) if matches!(v.value, Value::SingleQuotedString(_)));
    let casings = Casings::under(table.monetary);
    let value = match eval_on(lit, &[], &[], table.release, &casings)? {
        Held::Geometric(g) => g,
        Held::Text(t)
            if quoted && (kind != crate::geo::GeoKind::Point || name != "<@" && name != "@>") =>
        {
            crate::geo::geo_in(kind, &t)?
        }
        _ => return Ok(None),
    };
    // `&<`, `&>`, `&<|` and `|&>` have no other side: read with no rule, as a range's are.
    if !crate::plane::takes(kind, seen.unwrap_or(&name), value.kind()) {
        return Ok(None);
    }
    let column = table.columns[c].name.clone();
    let own = Box::new(Expr::Identifier(sqlparser::ast::Ident::with_quote(
        '"',
        column.clone(),
    )));
    let (left, right) = if lit_left {
        (Box::new(lit.clone()), own)
    } else {
        (own, Box::new(lit.clone()))
    };
    Ok(Some(OpCond {
        column: c,
        expr: Expr::BinaryOp {
            left,
            op: op.clone(),
            right,
        },
        name: column,
        op: seen.unwrap_or_default().to_string(),
        probe: seen.map(|_| crate::spans::Probe::Plane(kind, value)),
        value: None,
    }))
}

/// Whether a condition on one table reads a column of the plane by an operator its operator classes
/// take, with a constant, so that the question path rules blocks out by it.
pub(super) fn plane_condition(table: &Table, e: &Expr) -> bool {
    let Expr::BinaryOp { left, op, right } = e else {
        return false;
    };
    let column = |x: &Expr| {
        let name = match x {
            Expr::Identifier(i) => crate::catalog::ident(i),
            Expr::CompoundIdentifier(parts) => crate::catalog::ident(parts.last()?),
            _ => return None,
        };
        table.columns.iter().position(|c| c.name == name)
    };
    let cond = match (column(left), column(right)) {
        (Some(c), None) => plane_cond(table, c, op, right, false),
        (None, Some(c)) => plane_cond(table, c, op, left, true),
        _ => return false,
    };
    matches!(cond, Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::truth;

    #[test]
    fn a_truth_is_read_as_postgresql_reads_a_boolean() {
        for (s, want) in [
            ("true", true),
            ("t", true),
            ("TrU", true),
            (" TRUE ", true),
            ("yes", true),
            ("y", true),
            ("on", true),
            ("1", true),
            ("\ttrue\n", true),
            ("false", false),
            ("F", false),
            ("no", false),
            ("n", false),
            ("off", false),
            ("of", false),
            ("0", false),
        ] {
            assert_eq!(truth(s), Some(want), "{s:?}");
        }
        for s in [
            "", " ", "o", "truex", "yess", "onn", "2", "01", "tr ue", "nope", "ja",
        ] {
            assert_eq!(truth(s), None, "{s:?}");
        }
    }
}
