//! The DDL: each table, its order and its indexes, as PostgreSQL's own statements declare them, and
//! the types, domains, sequences and collations the DDL declares beside them.
//!
//! The order is the index `ALTER TABLE … CLUSTER ON` names, or the primary key where none is named.
//! Every other index is held in an order of its own, and so is a primary key that is not the order,
//! and each UNIQUE.

use crate::{Error, Result};
use sqlparser::ast::{
    CastKind, CharacterLength, DataType, DateTimeField, ExactNumberInfo, Expr, FunctionArg,
    FunctionArgExpr, FunctionArguments, ObjectName, ObjectNamePart, TimezoneInfo, Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnType {
    Int2,
    Int4,
    Int8,
    Timestamptz,
    /// Text, compared by its bytes, with the most characters a value may hold.
    Text(Option<u32>),
    /// Bytes, compared byte by byte.
    Bytes,
    /// A numeric of at most 18 digits, `scale` of them after the point, held as the integer of its
    /// smallest unit.
    Numeric {
        precision: u8,
        scale: u8,
    },
    /// A boolean, false before true, packed a bit a row.
    Boolean,
    /// A uuid: its 16 bytes, compared byte by byte.
    Uuid,
    /// An oid: an unsigned 32-bit number.
    Oid,
    /// A transaction id: an unsigned 32-bit number, compared only by `=` and `<>`.
    Xid,
    /// A command id: an unsigned 32-bit number, compared only by `=`.
    Cid,
    /// A transaction id with its epoch: an unsigned 64-bit number.
    Xid8,
    /// A position in the write-ahead log: an unsigned 64-bit number.
    PgLsn,
    /// `"char"`: one byte, compared unsigned.
    Char,
    /// money: a count of its smallest unit, a cent under the C locale.
    Money,
    /// A MAC address of 6 bytes, compared byte by byte.
    Macaddr,
    /// A MAC address of 8 bytes, compared byte by byte.
    Macaddr8,
    /// real: a float of 32 bits, `-0` equal to `0` and every NaN equal and above infinity.
    Float4,
    /// double precision: a float of 64 bits, compared as a real is.
    Float8,
    /// A reg* type: an oid, printed and read as the name of the object it names in the catalog of
    /// the PostgreSQL release the file's OIDs came from, which it carries.
    Reg(crate::regs::RegKind, u8),
    /// oid8: an unsigned 64-bit object identifier, PostgreSQL 19's.
    Oid8,
    /// A tid: a block number of 32 bits and an offset of 16 in it, both unsigned, compared block
    /// then offset, and held as a group of two leaves.
    Tid,
}

impl ColumnType {
    /// Bytes a value takes, plain, where every value takes the same: what the file writes, so a
    /// smallint, written as Parquet's INT32, takes 4.
    pub fn width(self) -> Option<usize> {
        match self {
            ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Oid
            | ColumnType::Xid
            | ColumnType::Cid
            | ColumnType::Char
            | ColumnType::Reg(..)
            | ColumnType::Float4 => Some(4),
            ColumnType::Int8
            | ColumnType::Timestamptz
            | ColumnType::Numeric { .. }
            | ColumnType::Xid8
            | ColumnType::PgLsn
            | ColumnType::Money
            | ColumnType::Macaddr8
            | ColumnType::Oid8
            | ColumnType::Float8 => Some(8),
            ColumnType::Macaddr => Some(6),
            ColumnType::Uuid => Some(16),
            // Its two leaves, 4 bytes each.
            ColumnType::Tid => Some(8),
            ColumnType::Text(_) | ColumnType::Bytes | ColumnType::Boolean => None,
        }
    }

    /// The bits of its key, for a type keyed as an unsigned number held in the signed range of its
    /// width: 0 for any other.
    pub fn unsigned_bits(self) -> u32 {
        match self {
            ColumnType::Oid | ColumnType::Reg(..) | ColumnType::Xid | ColumnType::Cid => 32,
            ColumnType::Xid8 | ColumnType::PgLsn | ColumnType::Macaddr8 | ColumnType::Oid8 => 64,
            ColumnType::Macaddr | ColumnType::Tid => 48,
            ColumnType::Char => 8,
            _ => 0,
        }
    }

    /// Whether it is compared as an oid: an oid, or a reg* type, which PostgreSQL compares through
    /// oid's operators.
    pub fn is_oid(self) -> bool {
        matches!(self, ColumnType::Oid | ColumnType::Reg(..))
    }

    /// Whether it is a float: a real or a double precision.
    pub fn is_float(self) -> bool {
        matches!(self, ColumnType::Float4 | ColumnType::Float8)
    }

    /// The bytes of a value the file writes as a fixed array of bytes and keys as a number: a
    /// MAC address's.
    pub fn fixed_bytes(self) -> Option<usize> {
        match self {
            ColumnType::Macaddr => Some(6),
            ColumnType::Macaddr8 => Some(8),
            _ => None,
        }
    }

    /// Whether PostgreSQL has a B-tree operator class for it, so that it orders, keys an index and
    /// is compared by `<`: xid and cid have only a hash one.
    pub fn ordered(self) -> bool {
        !matches!(self, ColumnType::Xid | ColumnType::Cid)
    }

    /// Whether its values are keyed in 128 bits.
    pub fn is_wide(self) -> bool {
        matches!(self, ColumnType::Uuid)
    }

    /// Whether 0.1.x held it, so that a snail of format 7 or 8 can.
    pub fn held_by_0_1(self) -> bool {
        matches!(
            self,
            ColumnType::Int2
                | ColumnType::Int4
                | ColumnType::Int8
                | ColumnType::Timestamptz
                | ColumnType::Text(_)
                | ColumnType::Bytes
                | ColumnType::Numeric { .. }
        )
    }

    /// The bits a value takes where the type is narrower than 4 bytes, so that the cut passes over
    /// it.
    pub fn thin_bits(self) -> Option<u64> {
        match self {
            ColumnType::Boolean => Some(1),
            _ => None,
        }
    }

    pub fn is_text(self) -> bool {
        matches!(self, ColumnType::Text(_))
    }

    /// Whether its values are bytes of varying length, held and compared by their bytes: text, or
    /// bytea.
    pub fn is_byte_array(self) -> bool {
        matches!(self, ColumnType::Text(_) | ColumnType::Bytes)
    }

    pub fn is_integer(self) -> bool {
        matches!(self, ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8)
    }

    pub fn name(self) -> String {
        match self {
            ColumnType::Int2 => "smallint".into(),
            ColumnType::Int4 => "integer".into(),
            ColumnType::Int8 => "bigint".into(),
            ColumnType::Timestamptz => "timestamptz".into(),
            ColumnType::Text(None) => "text".into(),
            ColumnType::Text(Some(n)) => format!("varchar({n})"),
            ColumnType::Bytes => "bytea".into(),
            ColumnType::Numeric { precision, scale } => format!("numeric({precision},{scale})"),
            ColumnType::Boolean => "boolean".into(),
            ColumnType::Uuid => "uuid".into(),
            other => other.pg_name(),
        }
    }

    /// The type as PostgreSQL's `format_type` names it.
    pub fn pg_name(self) -> String {
        match self {
            ColumnType::Int2 => "smallint".into(),
            ColumnType::Int4 => "integer".into(),
            ColumnType::Int8 => "bigint".into(),
            ColumnType::Timestamptz => "timestamp with time zone".into(),
            ColumnType::Text(None) => "text".into(),
            ColumnType::Text(Some(n)) => format!("character varying({n})"),
            ColumnType::Bytes => "bytea".into(),
            ColumnType::Numeric { precision, scale } => format!("numeric({precision},{scale})"),
            ColumnType::Boolean => "boolean".into(),
            ColumnType::Uuid => "uuid".into(),
            ColumnType::Oid => "oid".into(),
            ColumnType::Xid => "xid".into(),
            ColumnType::Cid => "cid".into(),
            ColumnType::Xid8 => "xid8".into(),
            ColumnType::PgLsn => "pg_lsn".into(),
            ColumnType::Char => "\"char\"".into(),
            ColumnType::Money => "money".into(),
            ColumnType::Macaddr => "macaddr".into(),
            ColumnType::Macaddr8 => "macaddr8".into(),
            ColumnType::Float4 => "real".into(),
            ColumnType::Float8 => "double precision".into(),
            ColumnType::Reg(kind, _) => kind.type_name().into(),
            ColumnType::Oid8 => "oid8".into(),
            ColumnType::Tid => "tid".into(),
        }
    }

    /// The B-tree operator class PostgreSQL takes for a key of this type where its index names
    /// none; xid's and cid's are hash ones, as they have no other.
    pub fn default_opclass(self) -> &'static str {
        match self {
            ColumnType::Int2 => "int2_ops",
            ColumnType::Int4 => "int4_ops",
            ColumnType::Int8 => "int8_ops",
            ColumnType::Timestamptz => "timestamptz_ops",
            ColumnType::Text(_) => "text_ops",
            ColumnType::Bytes => "bytea_ops",
            ColumnType::Numeric { .. } => "numeric_ops",
            ColumnType::Boolean => "bool_ops",
            ColumnType::Uuid => "uuid_ops",
            ColumnType::Oid => "oid_ops",
            ColumnType::Xid => "xid_ops",
            ColumnType::Cid => "cid_ops",
            ColumnType::Xid8 => "xid8_ops",
            ColumnType::PgLsn => "pg_lsn_ops",
            ColumnType::Char => "char_ops",
            ColumnType::Money => "money_ops",
            ColumnType::Macaddr => "macaddr_ops",
            ColumnType::Macaddr8 => "macaddr8_ops",
            ColumnType::Float4 => "float4_ops",
            ColumnType::Float8 => "float8_ops",
            // A reg* type has none of its own: PostgreSQL takes oid's, to which it is binary-coercible.
            ColumnType::Reg(..) => "oid_ops",
            ColumnType::Oid8 => "oid8_ops",
            ColumnType::Tid => "tid_ops",
        }
    }

    /// The range of an integer type, as PostgreSQL bounds it.
    pub fn integer_range(self) -> Option<(i64, i64)> {
        match self {
            ColumnType::Int2 => Some((i16::MIN as i64, i16::MAX as i64)),
            ColumnType::Int4 => Some((i32::MIN as i64, i32::MAX as i64)),
            ColumnType::Int8 => Some((i64::MIN, i64::MAX)),
            _ => None,
        }
    }

    /// The fields of a value held as a group, each its name, the type it is held as and the bits of
    /// the unsigned number it holds; none for a value held as one leaf.
    fn fields(self) -> &'static [(&'static str, ColumnType, u32)] {
        match self {
            // The block number as an oid is held, the offset as an oid too, 16 bits of it.
            ColumnType::Tid => &[
                ("block", ColumnType::Oid, 32),
                ("offset", ColumnType::Oid, 16),
            ],
            _ => &[],
        }
    }

    /// How many leaves its Parquet type tree has.
    pub fn leaf_count(self) -> usize {
        self.fields().len().max(1)
    }

    /// The leaves of its Parquet type tree: the value itself, or a leaf a field of the group it is.
    pub fn leaves(self) -> Vec<Leaf> {
        match self.fields() {
            [] => vec![Leaf {
                field: None,
                ty: self,
                bits: self.unsigned_bits(),
            }],
            fields => fields
                .iter()
                .map(|&(name, ty, bits)| Leaf {
                    field: Some(name),
                    ty,
                    bits,
                })
                .collect(),
        }
    }
}

/// One leaf of a column's Parquet type tree, the storage's unit: its own descriptor and levels, its
/// own unit and encoding, its own summary and pages in each block, and its own width in the cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Leaf {
    /// Its field of the column's group; none where the column is one leaf, named as the column is.
    pub field: Option<&'static str>,
    /// The type its values are held as: the column's own where it is one leaf, and a field's the
    /// narrowest held type whose values hold the field's.
    pub ty: ColumnType,
    /// The bits of the unsigned number it holds, as the file annotates them: 0 where it holds none.
    pub bits: u32,
}

impl Leaf {
    /// Its path in the file: the column's name, then its field's.
    pub fn path(&self, column: &str) -> Vec<String> {
        std::iter::once(column.to_string())
            .chain(self.field.map(str::to_string))
            .collect()
    }

    /// Its name, as a Parquet reader names a leaf: its path, joined by points.
    pub fn name(&self, column: &str) -> String {
        self.path(column).join(".")
    }
}

/// Whether PostgreSQL assigns a value of type `from` to a column of type `to` without an explicit
/// cast: its implicit and assignment casts among the types held (`pg_cast`, `castcontext` 'i' or
/// 'a'), and the conversion through text output it allows into a string type.
pub fn assignable(from: ColumnType, to: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        (from, to),
        (_, Text(_))
            | (
                Int2 | Int4 | Int8 | Numeric { .. },
                Int2 | Int4 | Int8 | Numeric { .. }
            )
            | (Timestamptz, Timestamptz)
            | (Bytes, Bytes)
            | (Boolean, Boolean)
            | (Uuid, Uuid)
            | (Int2 | Int4 | Int8 | Oid, Oid)
            | (Oid, Int4 | Int8)
            | (Int4 | Int8 | Numeric { .. } | Money, Money)
            | (Money, Numeric { .. })
            | (Text(_) | Char, Char)
            | (Xid, Xid)
            | (Cid, Cid)
            | (Xid8, Xid8)
            | (PgLsn, PgLsn)
            | (Macaddr | Macaddr8, Macaddr | Macaddr8)
            | (Tid, Tid)
            | (
                Int2 | Int4 | Int8 | Numeric { .. } | Float4 | Float8,
                Float4 | Float8
            )
            | (Float4 | Float8, Int2 | Int4 | Int8 | Numeric { .. })
            | (Int2 | Int4 | Int8 | Oid, Reg(..))
            | (Reg(..), Oid | Int4 | Int8)
            | (Int2 | Int4 | Int8 | Oid | Oid8, Oid8)
            | (Oid8, Int8)
    ) || match (from, to) {
        (Reg(a, _), Reg(b, _)) => a == b || crate::regs::casts_between(a, b),
        // text to regclass is regclass(text), an implicit cast; to any other reg* only explicit.
        (Text(_), Reg(crate::regs::RegKind::Class, _)) => true,
        _ => false,
    }
}

/// A column the DDL generates from the row's other columns.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Generated {
    pub expr: Expr,
    /// Computed at the write and kept; otherwise VIRTUAL, computed at the read.
    pub stored: bool,
}

/// A column whose values a sequence of its own hands out.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Identity {
    /// GENERATED ALWAYS: a row may give it no value of its own.
    pub always: bool,
    pub sequence: String,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    /// What fills it where a row gives it none.
    pub default: Option<Expr>,
    pub generated: Option<Generated>,
    pub identity: Option<Identity>,
    /// The collation its DDL names, where it names one.
    pub collation: Option<String>,
}

impl Column {
    /// Whether the column is computed at the read, and so held by no order.
    pub fn is_virtual(&self) -> bool {
        self.generated.as_ref().is_some_and(|g| !g.stored)
    }
}

/// One level of an order: a column, or the month of a timestamptz column on the UTC clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Level {
    Column(usize),
    MonthUtc(usize),
}

impl Level {
    /// The table column the level is read from.
    pub fn column(self) -> usize {
        match self {
            Level::Column(c) | Level::MonthUtc(c) => c,
        }
    }
}

/// The PRIMARY KEY or UNIQUE an order stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyConstraint {
    pub primary: bool,
    /// DEFERRABLE, checked at each statement's end, as PostgreSQL checks one INITIALLY IMMEDIATE.
    pub deferrable: bool,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Order {
    pub name: String,
    pub levels: Vec<Level>,
    /// The table's columns held by position, in the order they are kept.
    pub columns: Vec<usize>,
    pub unique: bool,
    /// The table's own order, rather than one of its indexes.
    pub table_order: bool,
    /// For each level, whether its NULL rows come before its values as they lie ascending: under
    /// NULLS FIRST, or under DESC NULLS LAST, as PostgreSQL's B-tree read backwards holds them.
    pub nulls_first: Vec<bool>,
    /// Whether keys holding a NULL are each their own, so that a unique order takes any number of
    /// them: unless its index says NULLS NOT DISTINCT.
    pub nulls_distinct: bool,
    /// For each level, whether its index writes it DESC: its values are held rising all the same.
    pub desc: Vec<bool>,
    /// Each level as its index writes it: a column's name, or the expression.
    pub keys: Vec<String>,
    /// For each level, its B-tree operator class: the one its index names, or its type's default.
    pub opclasses: Vec<String>,
    /// The columns its INCLUDE names.
    pub include: Vec<usize>,
    /// The storage parameters its index sets, as written: read, and nothing in a snail.
    pub options: Vec<(String, String)>,
    /// The PRIMARY KEY or UNIQUE it stands for, where it stands for one.
    pub constraint: Option<KeyConstraint>,
    /// Whether ALTER TABLE … CLUSTER ON names it.
    pub clustered: bool,
}

impl Order {
    /// Where a table column sits among the columns this order holds.
    pub fn slot(&self, column: usize) -> Option<usize> {
        self.columns.iter().position(|&c| c == column)
    }

    /// The leaves this order holds, by their slots: each column it holds, its leaves in turn, each
    /// with its table column.
    pub fn leaves(&self, table: &Table) -> Vec<(usize, Leaf)> {
        self.columns
            .iter()
            .flat_map(|&c| {
                table.columns[c]
                    .ty
                    .leaves()
                    .into_iter()
                    .map(move |l| (c, l))
            })
            .collect()
    }

    /// The slots of a table column's leaves among those this order holds.
    pub fn slots(&self, table: &Table, column: usize) -> Option<std::ops::Range<usize>> {
        let at = self.slot(column)?;
        let start = self.columns[..at]
            .iter()
            .map(|&c| table.columns[c].ty.leaf_count())
            .sum::<usize>();
        Some(start..start + table.columns[column].ty.leaf_count())
    }

    /// The slot of a column's first leaf among the leaves this order holds: a column of one
    /// leaf's own.
    pub fn leaf_slot(&self, table: &Table, column: usize) -> Option<usize> {
        self.slots(table, column).map(|s| s.start)
    }
}

/// A CHECK on a table's row.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Check {
    pub name: String,
    pub expr: Expr,
    /// NOT ENFORCED checks nothing.
    pub enforced: bool,
    /// Whether every row was checked: false where it was added NOT VALID and not validated since,
    /// so that only the rows written after it are.
    pub valid: bool,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// The table's order first, then its indexes.
    pub orders: Vec<Order>,
    pub checks: Vec<Check>,
    /// The PostgreSQL release whose catalog names its reg* values.
    pub release: u8,
}

impl Table {
    /// The leaves of its columns, in its columns' order, each with its column.
    pub fn leaves(&self) -> Vec<(usize, Leaf)> {
        (0..self.columns.len())
            .flat_map(|c| self.columns[c].ty.leaves().into_iter().map(move |l| (c, l)))
            .collect()
    }

    /// Where a column's first leaf lies among the table's leaves.
    pub fn first_leaf(&self, column: usize) -> usize {
        self.columns[..column]
            .iter()
            .map(|c| c.ty.leaf_count())
            .sum()
    }

    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The columns whose month any order reads: each block keeps the rows on each of their months.
    pub fn month_columns(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for o in &self.orders {
            for l in &o.levels {
                if let Level::MonthUtc(c) = *l {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    pub fn order(&self, name: &str) -> Option<&Order> {
        self.orders.iter().find(|o| o.name == name)
    }

    /// The column whose values are handed out as rows arrive, where the table's order leads with
    /// one: an identity, or a DEFAULT that a sequence, the clock or a uuid's generator fills.
    pub fn arrival_key(&self) -> Option<usize> {
        let order = self.orders.iter().find(|o| o.table_order)?;
        let c = order.levels.first()?.column();
        let col = &self.columns[c];
        (col.identity.is_some() || col.default.as_ref().is_some_and(hands_out)).then_some(c)
    }

    /// What `write` says of a table laid out on a key handed out as rows arrive, where it is: the
    /// DDL names no other order for it.
    pub fn laid_out_on_arrival(&self) -> Option<String> {
        let c = self.arrival_key()?;
        let order = self.orders.iter().find(|o| o.table_order)?;
        let col = &self.columns[c];
        Some(if col.default.as_ref().is_some_and(hands_out_at_random) {
            format!(
                "{}: laid out along {}, on {}, a key handed out at random as rows arrive, since the \
                 DDL names no other order: each insert lands where its key falls, scattered across \
                 the blocks",
                self.name, order.name, col.name
            )
        } else {
            format!(
                "{}: laid out along {}, on {}, a key handed out as rows arrive, since the DDL names \
                 no other order: its blocks follow the rows' arrival, and each insert meets the \
                 newest",
                self.name, order.name, col.name
            )
        })
    }
}

/// Whether a DEFAULT hands out a value as each row arrives: a sequence's next value, the clock, or
/// a uuid.
pub fn hands_out(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) | Expr::Cast { expr: x, .. } => hands_out(x),
        Expr::Function(f) => matches!(
            object_name(&f.name)
                .to_lowercase()
                .trim_start_matches("pg_catalog."),
            "nextval"
                | "now"
                | "current_timestamp"
                | "transaction_timestamp"
                | "statement_timestamp"
                | "clock_timestamp"
                | "gen_random_uuid"
                | "uuidv7"
                | "uuidv4"
        ),
        _ => false,
    }
}

/// Whether a DEFAULT hands out a random value as each row arrives, rising with none: a version 4
/// uuid.
pub fn hands_out_at_random(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) | Expr::Cast { expr: x, .. } => hands_out_at_random(x),
        Expr::Function(f) => matches!(
            object_name(&f.name)
                .to_lowercase()
                .trim_start_matches("pg_catalog."),
            "gen_random_uuid" | "uuidv4"
        ),
        _ => false,
    }
}

/// What a type the DDL declares is.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TypeKind {
    /// Its labels, in their order.
    Enum {
        labels: Vec<String>,
    },
    /// Its fields: each name, its type as PostgreSQL names it, and the collation it names.
    Composite {
        fields: Vec<(String, String, Option<String>)>,
    },
    Range(RangeType),
}

/// A range the DDL declares, and the multirange PostgreSQL makes beside it.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RangeType {
    pub subtype: String,
    pub opclass: Option<String>,
    pub collation: Option<String>,
    pub canonical: Option<String>,
    pub subtype_diff: Option<String>,
    pub multirange: String,
}

/// A type the DDL declares.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Type {
    pub name: String,
    pub kind: TypeKind,
}

/// A domain's CHECK.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DomainCheck {
    pub name: String,
    pub check: Expr,
    /// VALIDATE CONSTRAINT has checked it, or it was never NOT VALID.
    pub valid: bool,
}

/// A domain the DDL declares: a type and what its values must meet.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Domain {
    pub name: String,
    /// Its base type, as PostgreSQL names it.
    pub base: String,
    /// Its base type, where the snail holds it.
    pub base_type: Option<ColumnType>,
    pub collation: Option<String>,
    pub default: Option<Expr>,
    pub not_null: bool,
    /// The name its NOT NULL is given, where it is given one.
    pub not_null_name: Option<String>,
    pub checks: Vec<DomainCheck>,
}

/// The type a sequence counts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SequenceType {
    Int2,
    Int4,
    Int8,
}

impl SequenceType {
    pub fn of(dt: &DataType) -> Option<SequenceType> {
        match held_type(dt).ok()? {
            ColumnType::Int2 => Some(SequenceType::Int2),
            ColumnType::Int4 => Some(SequenceType::Int4),
            ColumnType::Int8 => Some(SequenceType::Int8),
            _ => None,
        }
    }

    pub fn column(self) -> ColumnType {
        match self {
            SequenceType::Int2 => ColumnType::Int2,
            SequenceType::Int4 => ColumnType::Int4,
            SequenceType::Int8 => ColumnType::Int8,
        }
    }

    pub fn min(self) -> i64 {
        self.column().integer_range().unwrap().0
    }

    pub fn max(self) -> i64 {
        self.column().integer_range().unwrap().1
    }
}

/// A sequence the DDL declares, or one a serial or an identity column makes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sequence {
    pub name: String,
    pub ty: SequenceType,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub start: i64,
    pub cache: i64,
    pub cycle: bool,
    /// The table and column it goes with.
    pub owned_by: Option<(String, String)>,
    /// Where it stands once the DDL is read, as PostgreSQL's `last_value` and `is_called`: the value
    /// it hands out next, where it has handed out none since it was made or restarted.
    pub last_value: i64,
    pub is_called: bool,
    /// The identity column it hands values out to.
    pub identity: bool,
}

/// A collation the DDL declares.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Collation {
    pub name: String,
    pub provider: String,
    pub locale: Option<String>,
    pub lc_collate: Option<String>,
    pub lc_ctype: Option<String>,
    pub deterministic: bool,
    pub rules: Option<String>,
    /// The collation it is copied from, where it is.
    pub from: Option<String>,
}

impl Collation {
    /// Whether it orders text by its bytes, as the snail compares text without the feature that
    /// calls a collation library.
    pub fn byte_order(&self) -> bool {
        if let Some(f) = &self.from {
            return BYTE_ORDER.contains(&f.as_str());
        }
        let collate = self.lc_collate.as_ref().or(self.locale.as_ref());
        match self.provider.as_str() {
            "libc" => {
                collate.is_some_and(|c| BYTE_ORDER.contains(&c.as_str()))
                    && self
                        .lc_ctype
                        .as_ref()
                        .or(self.locale.as_ref())
                        .is_some_and(|c| BYTE_ORDER.contains(&c.as_str()))
                    && self.deterministic
            }
            "builtin" => collate.is_some_and(|c| c == "C") && self.deterministic,
            _ => false,
        }
    }
}

/// A function written in SQL whose body is one expression over its arguments.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Function {
    pub name: String,
    pub args: Vec<ColumnType>,
    pub body: Expr,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Catalog {
    /// The DDL as written, kept in the file so the file needs nothing outside itself.
    pub ddl: String,
    pub tables: Vec<Table>,
    pub functions: Vec<Function>,
    /// The types the DDL declares, which a column of one names by its place here.
    pub types: Vec<Type>,
    pub domains: Vec<Domain>,
    pub sequences: Vec<Sequence>,
    pub collations: Vec<Collation>,
    /// The PostgreSQL release the file's OIDs came from, whose catalog names its reg* values.
    pub release: u8,
}

/// A DDL read a statement at a time: the catalog of the statements taken, and each statement taken
/// or refused by name.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Reading {
    pub catalog: Catalog,
    pub statements: Vec<StatementRead>,
}

/// One statement of a DDL as the snail read it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StatementRead {
    /// The statement as written.
    pub text: String,
    /// What it is, as its first words say: CREATE TABLE, ALTER SEQUENCE and so on.
    pub kind: String,
    /// The object it names.
    pub object: String,
    /// Why it was refused, where it was.
    pub refused: Option<String>,
    /// What it declares that is read and does nothing in a snail, or what IF [NOT] EXISTS passed
    /// over.
    pub notices: Vec<String>,
}

/// One fact of a catalog: what kind of object, which, the fact, and its value, as PostgreSQL's own
/// catalogs give the same fact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct Fact {
    pub kind: String,
    pub object: String,
    pub fact: String,
    pub value: String,
}

/// What an expression reads, once the functions it calls are read through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Shape {
    Column(usize),
    /// A timestamptz column at time zone 'UTC'.
    Clock(usize),
    MonthUtc(usize),
}

impl Sequence {
    /// A sequence made by CREATE SEQUENCE, or for a serial or an identity column, from its options
    /// as PostgreSQL's `init_params` reads them.
    pub(crate) fn made(
        name: &str,
        options: &[crate::ddl::SequenceOption],
        what: &str,
    ) -> Result<Sequence> {
        let mut s = Sequence {
            name: name.to_string(),
            ty: SequenceType::Int8,
            increment: 1,
            min: 1,
            max: i64::MAX,
            start: 1,
            cache: 1,
            cycle: false,
            owned_by: None,
            last_value: 1,
            is_called: false,
            identity: false,
        };
        s.set(options, true, what)?;
        Ok(s)
    }

    /// The sequence as ALTER SEQUENCE leaves it.
    pub(crate) fn alter(
        &mut self,
        options: &[crate::ddl::SequenceOption],
        what: &str,
    ) -> Result<()> {
        let mut s = self.clone();
        s.set(options, false, what)?;
        *self = s;
        Ok(())
    }

    fn set(
        &mut self,
        options: &[crate::ddl::SequenceOption],
        init: bool,
        what: &str,
    ) -> Result<()> {
        use crate::ddl::SequenceOption as O;
        let mut seen: Vec<std::mem::Discriminant<O>> = Vec::new();
        for o in options {
            let d = std::mem::discriminant(o);
            if seen.contains(&d) {
                return Err(Error::new(format!(
                    "{what}: conflicting or redundant options"
                )));
            }
            seen.push(d);
        }
        let find = |f: &dyn Fn(&O) -> bool| options.iter().find(|o| f(o));
        let typename = |t: SequenceType| t.column().pg_name();
        let (mut reset_max, mut reset_min) = (false, false);
        if let Some(O::As(t)) = find(&|o| matches!(o, O::As(_))) {
            if !init {
                reset_max = self.max == self.ty.max();
                reset_min = self.min == self.ty.min();
            }
            self.ty = *t;
        } else if init {
            self.ty = SequenceType::Int8;
        }
        if let Some(O::Increment(n)) = find(&|o| matches!(o, O::Increment(_))) {
            if *n == 0 {
                return Err(Error::new(format!("{what}: INCREMENT must not be zero")));
            }
            self.increment = *n;
        } else if init {
            self.increment = 1;
        }
        if let Some(O::Cycle(b)) = find(&|o| matches!(o, O::Cycle(_))) {
            self.cycle = *b;
        } else if init {
            self.cycle = false;
        }
        let max = find(&|o| matches!(o, O::MaxValue(_)));
        match max {
            Some(O::MaxValue(Some(v))) => self.max = *v,
            _ if init || max.is_some() || reset_max => {
                self.max = if self.increment > 0 || reset_max {
                    self.ty.max()
                } else {
                    -1
                }
            }
            _ => {}
        }
        if self.max < self.ty.min() || self.max > self.ty.max() {
            return Err(Error::new(format!(
                "{what}: MAXVALUE ({}) is out of range for sequence data type {}",
                self.max,
                typename(self.ty)
            )));
        }
        let min = find(&|o| matches!(o, O::MinValue(_)));
        match min {
            Some(O::MinValue(Some(v))) => self.min = *v,
            _ if init || min.is_some() || reset_min => {
                self.min = if self.increment < 0 || reset_min {
                    self.ty.min()
                } else {
                    1
                }
            }
            _ => {}
        }
        if self.min < self.ty.min() || self.min > self.ty.max() {
            return Err(Error::new(format!(
                "{what}: MINVALUE ({}) is out of range for sequence data type {}",
                self.min,
                typename(self.ty)
            )));
        }
        if self.min >= self.max {
            return Err(Error::new(format!(
                "{what}: MINVALUE ({}) must be less than MAXVALUE ({})",
                self.min, self.max
            )));
        }
        if let Some(O::Start(n)) = find(&|o| matches!(o, O::Start(_))) {
            self.start = *n;
        } else if init {
            self.start = if self.increment > 0 {
                self.min
            } else {
                self.max
            };
        }
        if self.start < self.min {
            return Err(Error::new(format!(
                "{what}: START value ({}) cannot be less than MINVALUE ({})",
                self.start, self.min
            )));
        }
        if self.start > self.max {
            return Err(Error::new(format!(
                "{what}: START value ({}) cannot be greater than MAXVALUE ({})",
                self.start, self.max
            )));
        }
        if let Some(O::Restart(v)) = find(&|o| matches!(o, O::Restart(_))) {
            self.last_value = v.unwrap_or(self.start);
            self.is_called = false;
        } else if init {
            self.last_value = self.start;
            self.is_called = false;
        }
        if self.last_value < self.min {
            return Err(Error::new(format!(
                "{what}: RESTART value ({}) cannot be less than MINVALUE ({})",
                self.last_value, self.min
            )));
        }
        if self.last_value > self.max {
            return Err(Error::new(format!(
                "{what}: RESTART value ({}) cannot be greater than MAXVALUE ({})",
                self.last_value, self.max
            )));
        }
        if let Some(O::Cache(n)) = find(&|o| matches!(o, O::Cache(_))) {
            if *n <= 0 {
                return Err(Error::new(format!(
                    "{what}: CACHE ({n}) must be greater than zero"
                )));
            }
            self.cache = *n;
        } else if init {
            self.cache = 1;
        }
        Ok(())
    }
}
pub fn object_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => ident(i),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// An identifier as PostgreSQL keeps it: folded to lower case unless quoted.
pub fn ident(i: &sqlparser::ast::Ident) -> String {
    if i.quote_style.is_some() {
        i.value.clone()
    } else {
        i.value.to_lowercase()
    }
}

/// The type a data type names, where the snail holds it.
pub fn held_type(dt: &DataType) -> Result<ColumnType> {
    Ok(match dt {
        DataType::SmallInt(None) | DataType::Int2(None) => ColumnType::Int2,
        DataType::Int(None) | DataType::Integer(None) | DataType::Int4(None) => ColumnType::Int4,
        DataType::BigInt(None) | DataType::Int8(None) => ColumnType::Int8,
        DataType::Timestamp(None, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::Timestamptz
        }
        DataType::Text => ColumnType::Text(None),
        DataType::Varchar(length)
        | DataType::CharacterVarying(length)
        | DataType::CharVarying(length) => ColumnType::Text(match length {
            None => None,
            Some(CharacterLength::IntegerLength { length, unit: None }) => {
                Some(u32::try_from(*length).map_err(|_| {
                    Error::new(format!("{}: the length is out of range", spelled(dt)))
                })?)
            }
            Some(_) => {
                return Err(Error::new(format!(
                    "{}: PostgreSQL counts a length in characters only",
                    spelled(dt)
                )))
            }
        }),
        DataType::Bytea => ColumnType::Bytes,
        DataType::Boolean | DataType::Bool => ColumnType::Boolean,
        DataType::Uuid => ColumnType::Uuid,
        DataType::Regclass => ColumnType::Reg(crate::regs::RegKind::Class, crate::regs::LATEST),
        DataType::Real | DataType::Float4 => ColumnType::Float4,
        DataType::DoublePrecision | DataType::Float8 | DataType::Float(ExactNumberInfo::None) => {
            ColumnType::Float8
        }
        DataType::Float(ExactNumberInfo::Precision(p)) => match p {
            1..=24 => ColumnType::Float4,
            25..=53 => ColumnType::Float8,
            0 => {
                return Err(Error::new(
                    "precision for type float must be at least 1 bit",
                ))
            }
            _ => {
                return Err(Error::new(
                    "precision for type float must be less than 54 bits",
                ))
            }
        },
        DataType::Custom(name, args) if args.is_empty() => {
            let n = object_name(name);
            match n.strip_prefix("pg_catalog.").unwrap_or(&n) {
                "oid" => ColumnType::Oid,
                "xid" => ColumnType::Xid,
                "cid" => ColumnType::Cid,
                "xid8" => ColumnType::Xid8,
                "pg_lsn" => ColumnType::PgLsn,
                // Quoted, as unquoted it is the keyword for character(1).
                "char" => ColumnType::Char,
                "money" => ColumnType::Money,
                "macaddr" => ColumnType::Macaddr,
                "macaddr8" => ColumnType::Macaddr8,
                "oid8" => ColumnType::Oid8,
                "tid" => ColumnType::Tid,
                other => match crate::regs::RegKind::of_type_name(other) {
                    Some(kind) => ColumnType::Reg(kind, crate::regs::LATEST),
                    None => {
                        return Err(Error::new(format!("the type {} is not held", spelled(dt))))
                    }
                },
            }
        }
        DataType::Numeric(info) | DataType::Decimal(info) => {
            let (p, s) = match info {
                ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
                ExactNumberInfo::Precision(p) => (*p, 0),
                ExactNumberInfo::None => {
                    return Err(Error::new(format!(
                        "{}: a numeric is held with its precision, at most 18 digits",
                        spelled(dt)
                    )))
                }
            };
            if !(1..=18).contains(&p) || s < 0 || s as u64 > p {
                return Err(Error::new(format!(
                    "{}: a numeric is held with at most 18 digits, and a scale within them",
                    spelled(dt)
                )));
            }
            ColumnType::Numeric {
                precision: p as u8,
                scale: s as u8,
            }
        }
        other => {
            return Err(Error::new(format!(
                "the type {} is not held",
                spelled(other)
            )))
        }
    })
}

/// A data type as PostgreSQL's `format_type` names it: the held types and the common built-ins by
/// their canonical names, any other as written, its name folded.
pub fn format_type(dt: &DataType) -> String {
    match held_type(dt) {
        Ok(t) => t.pg_name(),
        Err(_) => spelled(dt),
    }
}

/// A data type the snail does not hold, as PostgreSQL names it.
fn spelled(dt: &DataType) -> String {
    let length = |l: &Option<u64>| l.map(|n| format!("({n})")).unwrap_or_default();
    match dt {
        DataType::Array(sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner, _))
        | DataType::Array(sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner)) => {
            format!("{}[]", format_type(inner))
        }
        DataType::Char(l) | DataType::Character(l) => match l {
            Some(CharacterLength::IntegerLength { length, .. }) => format!("character({length})"),
            _ => "character(1)".into(),
        },
        DataType::Varchar(None) | DataType::CharacterVarying(None) => "character varying".into(),
        DataType::Numeric(ExactNumberInfo::None) | DataType::Decimal(ExactNumberInfo::None) => {
            "numeric".into()
        }
        DataType::Real | DataType::Float4 => "real".into(),
        DataType::DoublePrecision | DataType::Float8 => "double precision".into(),
        DataType::Date => "date".into(),
        DataType::Uuid => "uuid".into(),
        DataType::JSON => "json".into(),
        DataType::JSONB => "jsonb".into(),
        DataType::Interval { .. } => "interval".into(),
        DataType::Timestamp(p, tz) => format!(
            "timestamp{} {}",
            length(p),
            match tz {
                TimezoneInfo::Tz | TimezoneInfo::WithTimeZone => "with time zone",
                _ => "without time zone",
            }
        ),
        DataType::Time(p, tz) => format!(
            "time{} {}",
            length(p),
            match tz {
                TimezoneInfo::Tz | TimezoneInfo::WithTimeZone => "with time zone",
                _ => "without time zone",
            }
        ),
        DataType::Custom(name, args) => {
            let n = object_name(name);
            if args.is_empty() {
                n
            } else {
                format!("{n}({})", args.join(","))
            }
        }
        other => other.to_string().to_lowercase(),
    }
}

/// A numeric written in decimal, as the integer of its smallest unit at `scale`, rounded half away
/// from zero as PostgreSQL rounds it; refused where it holds more than `precision` digits.
pub fn numeric_units(text: &str, precision: u8, scale: u8) -> Result<i64> {
    let wrong = || Error::new(format!("'{text}' is not a numeric({precision},{scale})"));
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if (whole.is_empty() && frac.is_empty())
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return Err(wrong());
    }
    let s = scale as usize;
    let kept: String = frac.chars().chain(std::iter::repeat('0')).take(s).collect();
    let mut units: i128 = format!("{whole}{kept}")
        .trim_start_matches('0')
        .parse::<i128>()
        .unwrap_or(0);
    if frac.len() > s && frac.as_bytes()[s] >= b'5' {
        units += 1;
    }
    if units >= 10i128.pow(precision as u32) {
        return Err(Error::new(format!(
            "'{text}' has more digits than numeric({precision},{scale}) holds"
        )));
    }
    let units = if neg { -units } else { units };
    i64::try_from(units).map_err(|_| wrong())
}

/// The collations whose order is the bytes', as the snail compares text.
pub(crate) const BYTE_ORDER: [&str; 2] = ["C", "POSIX"];

/// The B-tree operator classes on text that compare its bytes, as the snail does: every one of
/// them, under a collation of byte order.
pub(crate) const TEXT_OPCLASSES: [&str; 4] = [
    "text_ops",
    "text_pattern_ops",
    "varchar_ops",
    "varchar_pattern_ops",
];

/// The shape of an expression over one table's columns. `alias` is the name the statement gives the
/// table; `env` holds the shapes of a function's arguments while its body is read.
pub fn shape(
    expr: &Expr,
    table: &str,
    alias: Option<&str>,
    columns: &[Column],
    functions: &[Function],
    env: &[Shape],
) -> Result<Shape> {
    let column = |name: &str| -> Result<Shape> {
        columns
            .iter()
            .position(|c| c.name == name)
            .map(Shape::Column)
            .ok_or_else(|| Error::new(format!("{table} has no column {name}")))
    };
    let bare = table.rsplit('.').next().unwrap_or(table);
    match expr {
        Expr::Nested(e) => shape(e, table, alias, columns, functions, env),
        Expr::Identifier(i) => column(&ident(i)),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let qualifier = parts[..parts.len() - 1]
                .iter()
                .map(ident)
                .collect::<Vec<_>>()
                .join(".");
            let name = ident(&parts[parts.len() - 1]);
            if Some(qualifier.as_str()) == alias || qualifier == bare || qualifier == table {
                column(&name)
            } else {
                Err(Error::new(format!(
                    "{qualifier}.{name} names no table the statement reads"
                )))
            }
        }
        Expr::Collate { expr, collation } => {
            let named = object_name(collation);
            match shape(expr, table, alias, columns, functions, env)? {
                Shape::Column(c)
                    if columns[c].ty.is_text() && BYTE_ORDER.contains(&named.as_str()) =>
                {
                    Ok(Shape::Column(c))
                }
                _ => Err(Error::new(format!(
                    "{expr} COLLATE {named}: text is compared by its bytes, so only C is read"
                ))),
            }
        }
        Expr::Value(v) => match &v.value {
            Value::Placeholder(p) => p
                .strip_prefix('$')
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| env.get(n.wrapping_sub(1)).copied())
                .ok_or_else(|| Error::new(format!("{p} is no argument of the function read"))),
            other => Err(Error::new(format!("{other} is a value, not a level"))),
        },
        Expr::Function(f) => {
            let name = object_name(&f.name);
            let function = functions
                .iter()
                .find(|g| g.name == name)
                .ok_or_else(|| Error::new(format!("{name} is no function the DDL declares")))?;
            let args = match &f.args {
                FunctionArguments::List(list) => &list.args,
                _ => return Err(Error::new(format!("{name} is called without arguments"))),
            };
            if args.len() != function.args.len() {
                return Err(Error::new(format!(
                    "{name} takes {} arguments, and {} are given",
                    function.args.len(),
                    args.len()
                )));
            }
            let mut inner = Vec::with_capacity(args.len());
            for a in args {
                match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        inner.push(shape(e, table, alias, columns, functions, env)?)
                    }
                    other => return Err(Error::new(format!("the argument {other} is not read"))),
                }
            }
            shape(&function.body, table, alias, columns, functions, &inner)
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            let zone = match time_zone.as_ref() {
                Expr::Value(v) => match &v.value {
                    Value::SingleQuotedString(z) => z.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            if zone != "UTC" {
                return Err(Error::new(format!(
                    "only the UTC clock is read, not {time_zone}"
                )));
            }
            match shape(timestamp, table, alias, columns, functions, env)? {
                Shape::Column(c) if columns[c].ty == ColumnType::Timestamptz => Ok(Shape::Clock(c)),
                _ => Err(Error::new(format!(
                    "{timestamp} AT TIME ZONE 'UTC' needs a timestamptz column"
                ))),
            }
        }
        Expr::Cast {
            kind: CastKind::DoubleColon | CastKind::Cast,
            expr,
            data_type,
            ..
        } => {
            let month = match expr.as_ref() {
                Expr::Extract {
                    field: DateTimeField::Month,
                    expr: inner,
                    ..
                } => inner,
                Expr::Nested(e) => match e.as_ref() {
                    Expr::Extract {
                        field: DateTimeField::Month,
                        expr: inner,
                        ..
                    } => inner,
                    _ => {
                        return Err(Error::new(format!(
                            "the cast of {expr} is not a level the snail reads"
                        )))
                    }
                },
                _ => {
                    return Err(Error::new(format!(
                        "the cast of {expr} is not a level the snail reads"
                    )))
                }
            };
            if !matches!(data_type, DataType::SmallInt(None) | DataType::Int2(None)) {
                return Err(Error::new(format!(
                    "a month is read as smallint, not {data_type}"
                )));
            }
            match shape(month, table, alias, columns, functions, env)? {
                Shape::Clock(c) => Ok(Shape::MonthUtc(c)),
                Shape::Column(c) if columns[c].ty == ColumnType::Timestamptz => {
                    Err(Error::new(format!(
                        "the month of {} needs a clock: extract on a timestamptz is stable, and \
                         PostgreSQL refuses it as an index expression",
                        columns[c].name
                    )))
                }
                _ => Err(Error::new(format!(
                    "the month of {month} needs a timestamptz on a clock"
                ))),
            }
        }
        Expr::Extract {
            field: DateTimeField::Month,
            ..
        } => Err(Error::new(format!(
            "{expr} is read only as ::smallint, the way the key spells it"
        ))),
        other => Err(Error::new(format!(
            "{other} is not a level the snail reads"
        ))),
    }
}

/// The level a key's expression is. A timestamptz column on the UTC clock keys as the column itself.
pub(crate) fn level(s: Shape) -> Level {
    match s {
        Shape::Column(c) | Shape::Clock(c) => Level::Column(c),
        Shape::MonthUtc(c) => Level::MonthUtc(c),
    }
}

impl Catalog {
    /// The DDL read whole: refused at the first statement the snail does not read, by name.
    pub fn parse(ddl: &str) -> Result<Catalog> {
        let reading = crate::ddl::read_ddl(ddl, true)?;
        match reading.statements.iter().find_map(|s| s.refused.as_ref()) {
            Some(why) => Err(Error::new(why.clone())),
            None => Ok(Catalog {
                ddl: ddl.to_string(),
                ..reading.catalog
            }),
        }
    }

    /// The DDL read a statement at a time: each statement taken or refused by name, and the
    /// catalog of those taken, whose DDL is theirs.
    pub fn read(ddl: &str) -> Result<Reading> {
        crate::ddl::read_ddl(ddl, false)
    }

    /// The statements of the DDL that declare a table, as they are written, with every function,
    /// type, domain and collation the DDL declares, and the sequences the table's columns take
    /// their values from.
    pub fn ddl_of(&self, table: &str) -> Result<String> {
        crate::ddl::ddl_of(self, table)
    }

    /// The statements of the DDL that declare its functions, as they are written, in the order of
    /// `functions`.
    pub fn function_statements(&self) -> Result<Vec<String>> {
        crate::ddl::function_statements(&self.ddl)
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Each reg* type its columns, its functions' arguments and its domains name, named by the
    /// catalog of `release`: refused where the release has no such type.
    pub fn with_release(&mut self, release: u8) -> Result<()> {
        crate::regs::check_release(None, release)?;
        self.release = release;
        let mut types: Vec<&mut ColumnType> = Vec::new();
        for t in &mut self.tables {
            t.release = release;
            types.extend(t.columns.iter_mut().map(|c| &mut c.ty));
        }
        for f in &mut self.functions {
            types.extend(f.args.iter_mut());
        }
        types.extend(self.domains.iter_mut().filter_map(|d| d.base_type.as_mut()));
        for ty in types {
            if let ColumnType::Reg(kind, at) = ty {
                crate::regs::check_release(Some(*kind), release)?;
                *at = release;
            }
        }
        Ok(())
    }

    pub fn sequence(&self, name: &str) -> Option<&Sequence> {
        self.sequences.iter().find(|s| s.name == name)
    }

    /// Every fact the catalog holds, one a row, sorted.
    pub fn facts(&self) -> Vec<Fact> {
        crate::ddl::facts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use sqlparser::tokenizer::Tokenizer;

    fn refused(ddl: &str) -> String {
        Catalog::parse(ddl).unwrap_err().to_string()
    }

    const TABLE: &str =
        "CREATE TABLE s.t (a integer NOT NULL, at timestamptz NOT NULL, PRIMARY KEY (a));";

    #[test]
    fn the_month_without_a_clock_is_refused_as_postgresql_refuses_it() {
        let e = refused(&format!(
            "{TABLE} CREATE INDEX m ON s.t ((extract(month FROM at)::smallint));"
        ));
        assert!(e.contains("extract on a timestamptz is stable"), "{e}");
    }

    #[test]
    fn an_instant_on_the_utc_clock_keys_as_the_instant_itself() {
        let clock =
            "CREATE FUNCTION s.clock(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE \
                     AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;";
        let month = "(extract(month FROM s.clock(at))::smallint)";
        let c = Catalog::parse(&format!(
            "{clock} {TABLE} CREATE INDEX i ON s.t ((s.clock(at))); \
             CREATE INDEX mi ON s.t ({month}, (s.clock(at))) INCLUDE (at, a); \
             CREATE INDEX mc ON s.t ({month}, at) INCLUDE (a);"
        ))
        .unwrap();
        let orders = &c.tables[0].orders;
        assert_eq!(orders[1].levels, [Level::Column(1)]);
        assert_eq!(orders[2].levels, [Level::MonthUtc(1), Level::Column(1)]);
        assert_eq!(orders[2].levels, orders[3].levels);
        assert_eq!(orders[2].columns, orders[3].columns);
    }

    #[test]
    fn the_functions_come_back_as_they_are_written_in_their_order() {
        let clock = "CREATE FUNCTION s.clock(timestamptz) RETURNS timestamp\n    LANGUAGE sql \
                     IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;";
        let day = "CREATE FUNCTION r.day(timestamptz) RETURNS date LANGUAGE sql IMMUTABLE \
                   AS $$ SELECT ($1 AT TIME ZONE 'UTC')::date $$;";
        let c = Catalog::parse(&format!(
            "{clock}\n{TABLE}\nCREATE INDEX i ON s.t (at);\n{day}\nALTER TABLE s.t CLUSTER ON i;"
        ))
        .unwrap();
        let names: Vec<&str> = c.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["s.clock", "r.day"]);
        assert_eq!(c.function_statements().unwrap(), [clock, day]);
    }

    #[test]
    fn a_table_with_no_order_is_refused() {
        let e = refused("CREATE TABLE s.t (a integer NOT NULL);");
        assert!(e.contains("has no order"), "{e}");
    }

    #[test]
    fn an_index_s_nulls_lie_where_its_b_tree_read_ascending_holds_them() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a));
             CREATE INDEX asc_last ON s.t (b, a);
             CREATE INDEX asc_first ON s.t (b ASC NULLS FIRST, a);
             CREATE INDEX desc_first ON s.t (b DESC, a DESC NULLS LAST);
             CREATE INDEX desc_last ON s.t (b DESC NULLS LAST, a NULLS LAST);
             CREATE UNIQUE INDEX distinct_b ON s.t (b);
             CREATE UNIQUE INDEX not_distinct_b ON s.t (b) NULLS NOT DISTINCT;",
        )
        .unwrap();
        let t = &c.tables[0];
        let read = |name: &str| {
            let o = t.order(name).unwrap();
            (o.nulls_first.clone(), o.nulls_distinct)
        };
        assert_eq!(read("t_pkey"), (vec![false], true));
        assert_eq!(read("asc_last"), (vec![false, false], true));
        assert_eq!(read("asc_first"), (vec![true, false], true));
        assert_eq!(read("desc_first"), (vec![false, true], true));
        assert_eq!(read("desc_last"), (vec![true, false], true));
        assert_eq!(read("distinct_b"), (vec![false], true));
        assert_eq!(read("not_distinct_b"), (vec![false], false));
        // Clustered on an index, the table's order takes its NULLs as the index lies.
        let c = Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a));
             CREATE UNIQUE INDEX u ON s.t (b NULLS FIRST) NULLS NOT DISTINCT;
             ALTER TABLE s.t CLUSTER ON u;",
        )
        .unwrap();
        let o = &c.tables[0].orders[0];
        assert!(o.table_order && o.nulls_first == [true] && !o.nulls_distinct);
    }

    #[test]
    fn cluster_on_an_index_the_table_lacks_is_refused() {
        let e = refused(&format!("{TABLE} ALTER TABLE s.t CLUSTER ON nothing;"));
        assert!(e.contains("which is no index of it"), "{e}");
    }

    #[test]
    fn the_index_an_order_is_clustered_on_may_include_columns_it_holds_already() {
        let c = Catalog::parse(&format!(
            "{TABLE} CREATE INDEX o ON s.t (at, a) INCLUDE (a); ALTER TABLE s.t CLUSTER ON o;"
        ))
        .unwrap();
        let order = &c.tables[0].orders[0];
        assert_eq!(order.name, "o");
        assert_eq!(order.levels, [Level::Column(1), Level::Column(0)]);
        let mut held = order.columns.clone();
        held.sort();
        assert_eq!(held, [0, 1]);
        let e = refused(&format!(
            "{TABLE} CREATE INDEX o ON s.t (at) INCLUDE (b); ALTER TABLE s.t CLUSTER ON o;"
        ));
        assert!(e.contains("has no column b"), "{e}");
    }

    #[test]
    fn a_type_not_held_is_refused() {
        let e = refused("CREATE TABLE s.t (a date NOT NULL, PRIMARY KEY (a));");
        assert!(e.contains("is not held"), "{e}");
    }

    #[test]
    fn a_numeric_is_held_to_18_digits_and_bytea_never_as_a_level() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a bigint NOT NULL, k bytea, m numeric(9,2) NOT NULL, PRIMARY KEY (a));",
        )
        .unwrap();
        let types: Vec<ColumnType> = c.tables[0].columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            types,
            [
                ColumnType::Int8,
                ColumnType::Bytes,
                ColumnType::Numeric {
                    precision: 9,
                    scale: 2
                }
            ]
        );
        for (ddl, why) in [
            (
                "CREATE TABLE s.t (a numeric NOT NULL, PRIMARY KEY (a));",
                "with its precision",
            ),
            (
                "CREATE TABLE s.t (a numeric(19,0) NOT NULL, PRIMARY KEY (a));",
                "at most 18 digits",
            ),
            (
                "CREATE TABLE s.t (k bytea NOT NULL, PRIMARY KEY (k));",
                "a bytea level",
            ),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
        assert_eq!(numeric_units("12.345", 9, 2).unwrap(), 1235);
        assert_eq!(numeric_units("-12.344", 9, 2).unwrap(), -1234);
        assert_eq!(numeric_units("7", 9, 2).unwrap(), 700);
        assert!(numeric_units("10000000.00", 9, 2).is_err());
    }

    #[test]
    fn text_is_read_with_its_length_cap() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b varchar(64), c character varying(255) NOT NULL, \
             d text, e varchar COLLATE \"C\", PRIMARY KEY (a));",
        )
        .unwrap();
        let types: Vec<ColumnType> = c.tables[0].columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            types,
            [
                ColumnType::Int4,
                ColumnType::Text(Some(64)),
                ColumnType::Text(Some(255)),
                ColumnType::Text(None),
                ColumnType::Text(None)
            ]
        );
        assert_eq!(types[1].width(), None);
    }

    #[test]
    fn a_collation_not_ordered_by_bytes_is_refused() {
        let e = refused(
            "CREATE TABLE s.t (a integer NOT NULL, b text COLLATE \"en_US\", PRIMARY KEY (a));",
        );
        assert!(e.contains("only C is read"), "{e}");
        let e = refused("CREATE TABLE s.t (a integer NOT NULL COLLATE \"C\", PRIMARY KEY (a));");
        assert!(e.contains("not text"), "{e}");
    }

    #[test]
    fn text_keys_an_order_and_an_index() {
        let c = Catalog::parse("CREATE TABLE s.u (a integer NOT NULL, b text, PRIMARY KEY (a)); CREATE INDEX ub ON s.u (b, a);").unwrap();
        let t = &c.tables[0];
        assert_eq!(t.orders[1].name, "ub");
        assert_eq!(t.orders[1].levels, [Level::Column(1), Level::Column(0)]);
        assert_eq!(t.orders[1].columns, [1, 0]);
        let c = Catalog::parse("CREATE TABLE s.t (a text NOT NULL, PRIMARY KEY (a));").unwrap();
        assert_eq!(c.tables[0].orders[0].levels, [Level::Column(0)]);
    }

    #[test]
    fn a_clock_in_another_zone_is_refused() {
        let e = refused(&format!(
            "CREATE FUNCTION s.c(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'Europe/Lisbon' $$; \
             {TABLE} CREATE INDEX m ON s.t ((extract(month FROM s.c(at))::smallint));"
        ));
        assert!(e.contains("only the UTC clock"), "{e}");
    }

    #[test]
    fn a_ddl_is_read_a_statement_at_a_time_each_taken_or_refused_by_name() {
        let r = Catalog::read(
            "CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));
             CREATE TABLE s.u (a jsonb NOT NULL, PRIMARY KEY (a));
             CREATE INDEX ua ON s.u (a);
             CREATE INDEX tg ON s.t USING gin (a);
             CREATE INDEX tp ON s.t (a) WHERE a > 0;
             CREATE INDEX tb ON s.t USING btree (a DESC) WITH (fillfactor = 90);
             DROP TABLE s.t;",
        )
        .unwrap();
        let read: Vec<(&str, Option<&str>)> = r
            .statements
            .iter()
            .map(|s| (s.object.as_str(), s.refused.as_deref()))
            .collect();
        assert_eq!(read[0], ("s.t", None));
        assert!(read[1].1.unwrap().contains("the type jsonb is not held"));
        assert_eq!(read[2].1, Some("ua is on s.u, which is refused"));
        assert!(read[3].1.unwrap().contains("USING gin is not held yet"));
        assert!(read[4].1.unwrap().contains("a partial index, WHERE a > 0"));
        assert_eq!(read[5].1, None);
        assert_eq!(r.catalog.tables[0].order("tb").unwrap().desc, [true]);
        assert_eq!(
            r.statements[5].notices.len(),
            2,
            "{:?}",
            r.statements[5].notices
        );
        assert!(read[6].1.unwrap().contains("is not read"));
        // The statements taken are a DDL of their own, which reads as the catalog they made.
        let again = Catalog::parse(&r.catalog.ddl).unwrap();
        assert_eq!(again.facts(), r.catalog.facts());
        assert_eq!(again.tables[0].orders.len(), 2);
        let e = refused(
            "CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a)); CREATE INDEX x ON s.t USING hash (a);",
        );
        assert!(e.contains("USING hash"), "{e}");
    }

    #[test]
    fn a_statement_sqlparser_reads_otherwise_than_written_is_refused() {
        let tokens = Tokenizer::new(&PostgreSqlDialect {}, "CREATE TABLE s.t (a int, b int)")
            .tokenize()
            .unwrap();
        let fewer = Parser::parse_sql(&PostgreSqlDialect {}, "CREATE TABLE s.t (a int)").unwrap();
        let e = crate::ddl::read_as_written(&tokens, &fewer[0], "CREATE TABLE s.t")
            .unwrap_err()
            .to_string();
        assert!(e.contains("it drops , b int"), "{e}");
        let same =
            Parser::parse_sql(&PostgreSqlDialect {}, "create table s.t (a INT, b INT)").unwrap();
        crate::ddl::read_as_written(&tokens, &same[0], "CREATE TABLE s.t").unwrap();
    }

    #[test]
    fn keys_checks_and_sequences_are_named_as_postgresql_names_them() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (id serial, a integer UNIQUE, b integer CHECK (b > 0),
                 c integer, d integer GENERATED BY DEFAULT AS IDENTITY,
                 PRIMARY KEY (id), UNIQUE (a, c) INCLUDE (b), UNIQUE (a),
                 CHECK (b < c), CHECK (c > 0), CONSTRAINT named CHECK (c < 9));",
        )
        .unwrap();
        let t = &c.tables[0];
        let orders: Vec<&str> = t.orders.iter().map(|o| o.name.as_str()).collect();
        // UNIQUE (a) written twice is one key; a key's name takes the columns it includes.
        assert_eq!(orders, ["t_pkey", "t_a_key", "t_a_c_b_key"]);
        let checks: Vec<&str> = t.checks.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(checks, ["t_b_check", "t_check", "t_c_check", "named"]);
        let seqs: Vec<&str> = c.sequences.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(seqs, ["s.t_id_seq", "s.t_d_seq"]);
        assert_eq!(
            t.columns[0].default.as_ref().unwrap().to_string(),
            "nextval('s.t_id_seq'::REGCLASS)"
        );
        assert!(t.columns[0].not_null && t.columns[4].not_null);
        assert_eq!(
            t.columns[4].identity,
            Some(Identity {
                always: false,
                sequence: "s.t_d_seq".into()
            })
        );
        // A name taken is numbered, the label first.
        let c = Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, CONSTRAINT t_a_check CHECK (a < 9), CHECK (a > 0), PRIMARY KEY (a));",
        )
        .unwrap();
        let checks: Vec<&str> = c.tables[0].checks.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(checks, ["t_a_check", "t_a_check1"]);
    }

    #[test]
    fn a_sequence_s_options_are_read_as_postgresql_reads_them() {
        let c = Catalog::parse(
            "CREATE SEQUENCE s.up;
             CREATE SEQUENCE s.down INCREMENT BY -1;
             CREATE SEQUENCE s.small AS smallint;
             ALTER SEQUENCE s.small AS integer;
             CREATE SEQUENCE s.moved START WITH 10 MAXVALUE 99 CACHE 5 CYCLE;
             ALTER SEQUENCE s.moved RESTART WITH 50 NO CYCLE;",
        )
        .unwrap();
        let s = |n: &str| c.sequence(n).unwrap().clone();
        let (up, down, small, moved) = (s("s.up"), s("s.down"), s("s.small"), s("s.moved"));
        assert_eq!(
            (up.min, up.max, up.start, up.last_value),
            (1, i64::MAX, 1, 1)
        );
        assert_eq!((down.min, down.max, down.start), (i64::MIN, -1, -1));
        // A type changed takes the new type's MAXVALUE where the old one stood at its type's.
        assert_eq!((small.ty, small.max), (SequenceType::Int4, i32::MAX as i64));
        assert_eq!(
            (
                moved.start,
                moved.max,
                moved.cache,
                moved.cycle,
                moved.last_value,
                moved.is_called
            ),
            (10, 99, 5, false, 50, false)
        );
        for (ddl, why) in [
            (
                "CREATE SEQUENCE s.x INCREMENT 0",
                "INCREMENT must not be zero",
            ),
            (
                "CREATE SEQUENCE s.x MINVALUE 5 MAXVALUE 5",
                "MINVALUE (5) must be less than MAXVALUE (5)",
            ),
            (
                "CREATE SEQUENCE s.x AS smallint MAXVALUE 40000",
                "out of range for sequence data type smallint",
            ),
            (
                "CREATE SEQUENCE s.x START 0",
                "START value (0) cannot be less than MINVALUE (1)",
            ),
            (
                "CREATE SEQUENCE s.x CACHE 0",
                "CACHE (0) must be greater than zero",
            ),
            (
                "CREATE SEQUENCE s.x CACHE 1 CACHE 2",
                "conflicting or redundant options",
            ),
            (
                "CREATE SEQUENCE s.x AS text",
                "must be smallint, integer, or bigint",
            ),
            ("CREATE TEMP SEQUENCE s.x", "kept in its file"),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
    }

    #[test]
    fn types_domains_and_collations_are_kept_as_declared() {
        let c = Catalog::parse(
            "CREATE TYPE s.mood AS ENUM ('sad', 'ok');
             ALTER TYPE s.mood ADD VALUE 'happy';
             ALTER TYPE s.mood ADD VALUE 'meh' BEFORE 'ok';
             ALTER TYPE s.mood ADD VALUE IF NOT EXISTS 'ok';
             ALTER TYPE s.mood RENAME VALUE 'sad' TO 'blue';
             CREATE TYPE s.pair AS (a integer, b text COLLATE \"C\");
             CREATE TYPE s.span AS RANGE (SUBTYPE = bigint);
             CREATE DOMAIN s.pos integer NOT NULL CHECK (VALUE > 0);
             ALTER DOMAIN s.pos ADD CHECK (VALUE < 9) NOT VALID;
             CREATE COLLATION s.bytes (provider = libc, locale = 'C');",
        )
        .unwrap();
        let TypeKind::Enum { labels } = &c.types[0].kind else {
            panic!()
        };
        assert_eq!(labels, &["blue", "meh", "ok", "happy"]);
        let TypeKind::Range(r) = &c.types[2].kind else {
            panic!()
        };
        assert_eq!(
            (r.subtype.as_str(), r.multirange.as_str()),
            ("bigint", "s.span_multirange")
        );
        let d = &c.domains[0];
        let checks: Vec<(&str, bool)> = d
            .checks
            .iter()
            .map(|k| (k.name.as_str(), k.valid))
            .collect();
        assert_eq!(checks, [("pos_check", true), ("pos_check1", false)]);
        assert!(c.collations[0].byte_order());
        for (ddl, why) in [
            (
                "CREATE TYPE s.m AS ENUM ('a'); ALTER TYPE s.m ADD VALUE 'a'",
                "already exists",
            ),
            (
                "CREATE TYPE s.m AS ENUM ('a'); ALTER TYPE s.m ADD VALUE 'b' AFTER 'z'",
                "not an existing enum label",
            ),
            (
                "CREATE TYPE s.m AS ENUM ('a'); CREATE TABLE s.t (m s.m NOT NULL, PRIMARY KEY (m))",
                "an enum the DDL declares",
            ),
            (
                "CREATE COLLATION s.pt (provider = icu, locale = 'pt-PT')",
                "only a collation of byte order",
            ),
            ("CREATE TYPE s.b (INPUT = x, OUTPUT = y)", "written in C"),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
    }

    #[test]
    fn a_column_s_default_generation_and_checks_are_read_and_refused_by_name() {
        let c = Catalog::parse(
            "CREATE SEQUENCE s.n;
             CREATE TABLE s.t (a integer NOT NULL DEFAULT nextval('s.n'), at timestamptz DEFAULT now(),
                 q integer DEFAULT 1 + 1, g integer GENERATED ALWAYS AS (q * 2) STORED,
                 v integer GENERATED ALWAYS AS (q + 1), PRIMARY KEY (a));",
        )
        .unwrap();
        let t = &c.tables[0];
        assert!(t.columns[3].generated.as_ref().is_some_and(|g| g.stored));
        assert!(t.columns[4].is_virtual());
        // A virtual column is held by no order.
        assert_eq!(t.orders[0].columns, [0, 1, 2, 3]);
        assert_eq!(t.arrival_key(), Some(0));
        assert!(t
            .laid_out_on_arrival()
            .unwrap()
            .contains("a key handed out as rows arrive"));
        // A uuid's key: a v7 rises as rows arrive, a v4 scatters them; a CLUSTER ON names another
        // order.
        let u = Catalog::parse(
            "CREATE TABLE s.u (id uuid DEFAULT uuidv7() NOT NULL, PRIMARY KEY (id));
             CREATE TABLE s.r (id uuid DEFAULT gen_random_uuid() NOT NULL, n integer NOT NULL,
                 PRIMARY KEY (id));
             CREATE TABLE s.c (id uuid DEFAULT gen_random_uuid() NOT NULL, n integer NOT NULL,
                 PRIMARY KEY (id));
             CREATE INDEX c_n ON s.c (n);
             ALTER TABLE s.c CLUSTER ON c_n;",
        )
        .unwrap();
        let words: Vec<Option<String>> = u.tables.iter().map(|t| t.laid_out_on_arrival()).collect();
        assert!(words[0]
            .as_deref()
            .unwrap()
            .contains("each insert meets the newest"));
        assert!(words[1]
            .as_deref()
            .unwrap()
            .contains("a key handed out at random"));
        assert_eq!(words[2], None);
        for (ddl, why) in [
            ("CREATE TABLE s.t (a integer NOT NULL DEFAULT nextval('s.none'), PRIMARY KEY (a))", "no sequence s.none"),
            ("CREATE TABLE s.t (a integer NOT NULL, b integer DEFAULT a, PRIMARY KEY (a))", "cannot use column reference"),
            ("CREATE TABLE s.t (a integer NOT NULL, b boolean DEFAULT now(), PRIMARY KEY (a))", "is of type boolean but default expression is of type timestamp with time zone"),
            ("CREATE TABLE s.t (a integer NOT NULL, b integer DEFAULT random(), PRIMARY KEY (a))", "which the snail does not read"),
            ("CREATE SEQUENCE s.n; CREATE TABLE s.t (a bigint NOT NULL DEFAULT nextval('s.n') * 10, PRIMARY KEY (a))", "computes with nextval or the clock"),
            ("CREATE TABLE s.t (a integer NOT NULL, b integer GENERATED ALWAYS AS (a) STORED, c integer GENERATED ALWAYS AS (b) STORED, PRIMARY KEY (a))", "cannot use generated column \"b\""),
            ("CREATE TABLE s.t (a integer NOT NULL, b integer GENERATED ALWAYS AS (a), PRIMARY KEY (a)); CREATE INDEX x ON s.t (b)", "virtual generated columns"),
            ("CREATE TABLE s.t (a text GENERATED ALWAYS AS IDENTITY, PRIMARY KEY (a))", "identity column type"),
            ("CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a), UNIQUE (b) DEFERRABLE INITIALLY DEFERRED)", "INITIALLY DEFERRED is not held"),
            ("CREATE TABLE s.t (a integer NOT NULL REFERENCES s.u (a), PRIMARY KEY (a))", "a FOREIGN KEY is not held yet"),
            ("CREATE TEMP TABLE s.t (a integer NOT NULL, PRIMARY KEY (a))", "logged and lasting"),
            ("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a)) PARTITION BY RANGE (a)", "is not read: only a table's columns"),
            ("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a)); CREATE INDEX x ON s.t (a int8_ops)", "the operator class int8_ops is not read"),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
    }
}
