//! The DDL: each table, its order and its indexes, as PostgreSQL's own statements declare them, and
//! the types, domains, sequences and collations the DDL declares beside them.
//!
//! The order is the index `ALTER TABLE … CLUSTER ON` names, or the primary key where none is named.
//! Every other index is held in an order of its own, and so is a primary key that is not the order,
//! and each UNIQUE.

use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, CastKind, CeilFloorKind, CharacterLength, DataType, DateTimeField,
    ExactNumberInfo, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName,
    ObjectNamePart, TimezoneInfo, Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnType {
    Int2,
    Int4,
    Int8,
    Timestamptz,
    /// Text, compared by its bytes, with the most characters a value may hold: none for text, and
    /// `VARCHAR` for varchar without a length.
    Text(Option<u32>),
    /// Bytes, compared byte by byte.
    Bytes,
    /// A numeric of at most 38 digits, `scale` of them after the point, held as the integer of its
    /// smallest unit: in 64 bits up to 18 digits, past them in 128, as Parquet's decimal in the
    /// fewest bytes that hold them. Keyed with NaN one past its greatest integer, and held in the
    /// file as a group of two leaves, its decimal and beside it whether it is NaN; as its decimal
    /// alone in formats 7 and 8, which hold no NaN.
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
    /// money: a count of its smallest unit, read and printed under the lc_monetary it was written
    /// under.
    Money(crate::monetary::Monetary),
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
    /// bpchar, or character(n): text compared without its trailing blanks, a value of character(n)
    /// padded with blanks to its n characters; any length where none is named.
    Bpchar(Option<u32>),
    /// name: at most 63 bytes, compared by its bytes.
    Name,
    /// bit(n): exactly n bits, held as its key.
    Bit(u32),
    /// bit varying(n): at most n bits, any number where none is named, held as its key, and in the
    /// file as a group of two leaves, its length and its bits.
    Varbit(Option<u32>),
    /// A numeric no Parquet decimal holds: unconstrained, or past 38 digits, or of a scale below 0
    /// or past its precision; its precision and scale where its DDL names them. Held as
    /// PostgreSQL's text of each value, and keyed on its class.
    NumericText(Option<(u16, i16)>),
    /// A composed type: an array, a composite or a table's row type, a range or a multirange, or an
    /// enum, its definition held apart.
    Composed(crate::composed::TypeRef),
    /// A date: days since 1970-01-01, its infinities the ends of 32 bits.
    Date,
    /// A time of day: microseconds since midnight, 24:00:00 the last of them, rounded to the
    /// digits after the second its precision names, where it names one.
    Time(Option<u8>),
    /// A time of day and its zone, in seconds west of UTC as PostgreSQL holds it: a group of two
    /// leaves, compared by the time on the UTC clock, then the zone.
    Timetz(Option<u8>),
    /// A timestamp: microseconds since 1970-01-01 on a clock of no zone, its infinities the ends
    /// of 64 bits.
    Timestamp(Option<u8>),
    /// An interval: months, days and microseconds, held as one leaf of its 16 bytes, compared by its
    /// total of 30-day months and 24-hour days; `range` its fields as PostgreSQL's typmod masks them.
    Interval {
        range: u16,
        precision: Option<u8>,
    },
    /// A timestamptz rounded to the digits after the second its precision names.
    TimestamptzRounded(u8),
    /// A type held as PostgreSQL's text of each value: json, jsonb, jsonpath, xml, inet, cidr,
    /// tsvector, tsquery, refcursor, aclitem, pg_snapshot or txid_snapshot.
    Textual(crate::textual::Textual),
    /// A type only a server makes, which no literal gives: pg_node_tree and the statistics types,
    /// held as copied, and gtsvector and the BRIN summaries, NULL only.
    Internal(crate::textual::Internal),
    /// A geometric type: a point, a line, an lseg, a box or a circle its float8s, each a leaf of a
    /// group; a path or a polygon a tree of its own, its points a list.
    Geometric(crate::geo::GeoKind),
}

impl ColumnType {
    /// The length of a varchar declared without one: more characters than any value can hold.
    pub const VARCHAR: u32 = u32::MAX;

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
            ColumnType::Numeric { precision, .. } if precision > 18 => {
                Some(crate::numeric::decimal_bytes(precision))
            }
            ColumnType::Int8
            | ColumnType::Timestamptz
            | ColumnType::TimestamptzRounded(_)
            | ColumnType::Numeric { .. }
            | ColumnType::Xid8
            | ColumnType::PgLsn
            | ColumnType::Money(_)
            | ColumnType::Macaddr8
            | ColumnType::Oid8
            | ColumnType::Float8 => Some(8),
            ColumnType::Macaddr => Some(6),
            ColumnType::Uuid => Some(16),
            // Its two leaves, 4 bytes each.
            ColumnType::Tid => Some(8),
            // Its grantee and grantor as oids, its privileges as 64 unsigned bits.
            ColumnType::Textual(crate::textual::Textual::Aclitem) => Some(16),
            // A float8 a leaf; a path's and a polygon's points vary.
            ColumnType::Geometric(k) => k.fixed_leaves().map(|n| 8 * n),
            // Its bits as a fixed array of bytes, the last padded.
            ColumnType::Bit(n) => Some((n as usize).div_ceil(8)),
            ColumnType::Date => Some(4),
            ColumnType::Time(_) | ColumnType::Timestamp(_) => Some(8),
            ColumnType::Timetz(_) => Some(12),
            ColumnType::Interval { .. } => Some(16),
            ColumnType::Text(_)
            | ColumnType::Bytes
            | ColumnType::Boolean
            | ColumnType::Bpchar(_)
            | ColumnType::Name
            | ColumnType::Varbit(_)
            | ColumnType::NumericText(_)
            | ColumnType::Textual(_)
            | ColumnType::Internal(_) => None,
            ColumnType::Composed(_) => None,
        }
    }

    /// The bits of its key, for a type keyed as an unsigned number held in the signed range of its
    /// width: 0 for any other.
    pub fn unsigned_bits(self) -> u32 {
        match self {
            ColumnType::Oid | ColumnType::Reg(..) | ColumnType::Xid | ColumnType::Cid => 32,
            ColumnType::Xid8 | ColumnType::PgLsn | ColumnType::Macaddr8 | ColumnType::Oid8 => 64,
            ColumnType::Macaddr => 48,
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
        match self {
            ColumnType::Xid | ColumnType::Cid => false,
            // An array or a composite orders by its parts.
            ColumnType::Composed(r) => match r.def() {
                crate::composed::Composed::Array { element, .. } => element.ordered(),
                crate::composed::Composed::Record { fields, .. } => {
                    fields.iter().all(|f| f.ty.ordered())
                }
                _ => true,
            },
            ColumnType::Geometric(_) => false,
            ColumnType::Textual(t) => t.ordered(),
            ColumnType::Internal(t) => t.ordered(),
            _ => true,
        }
    }

    /// Whether PostgreSQL has an equality it groups and finds DISTINCT values by, a B-tree's or a
    /// hash's: every type it orders, xid and cid, aclitem; an array or a composite where each of
    /// its parts has one.
    pub fn grouped(self) -> bool {
        if let ColumnType::Composed(_) = self {
            return self.unequal_part().is_none();
        }
        self.ordered()
            || matches!(
                self,
                ColumnType::Xid
                    | ColumnType::Cid
                    | ColumnType::Textual(crate::textual::Textual::Aclitem)
            )
    }

    /// The part PostgreSQL finds no equality for, which an array's or a composite's `=` asks of
    /// each element or field: the type itself, an array's element's, or a composite's first such
    /// field's; none where it has one.
    pub fn unequal_part(self) -> Option<ColumnType> {
        match self {
            ColumnType::Composed(r) => match r.def() {
                crate::composed::Composed::Array { element, .. } => element.unequal_part(),
                crate::composed::Composed::Record { fields, .. } => {
                    fields.iter().find_map(|f| f.ty.unequal_part())
                }
                _ => None,
            },
            ty if ty.grouped() => None,
            ty => Some(ty),
        }
    }

    /// Whether PostgreSQL decides its comparisons a row at a time, so that only the pipeline reads
    /// them: a geometric value's, an array's whose elements' type has no equality, which `=`
    /// refuses only where the two arrays are of one shape, and the planner's statistics' and their
    /// kin's, which compare as text or bytea by what they meet.
    pub fn compared_by_rows(self) -> bool {
        match self {
            ColumnType::Geometric(_) => true,
            ColumnType::Internal(t) => t.is_bytes(),
            ColumnType::Composed(r) => {
                matches!(r.def(), crate::composed::Composed::Array { element, .. } if !element.grouped())
            }
            _ => false,
        }
    }

    /// Whether its values are keyed in 128 bits.
    pub fn is_wide(self) -> bool {
        matches!(self, ColumnType::Uuid | ColumnType::Interval { .. })
            || matches!(self, ColumnType::Numeric { precision, .. } if precision > 18)
    }

    /// Whether it is a numeric, held in either form.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            ColumnType::Numeric { .. } | ColumnType::NumericText(_)
        )
    }

    /// A numeric's precision and scale, none where it is unconstrained; none at all for any other
    /// type.
    pub fn numeric_typmod(self) -> Option<Option<(u32, i32)>> {
        match self {
            ColumnType::Numeric { precision, scale } => {
                Some(Some((u32::from(precision), i32::from(scale))))
            }
            ColumnType::NumericText(t) => Some(t.map(|(p, s)| (u32::from(p), i32::from(s)))),
            _ => None,
        }
    }

    /// The numeric of that precision and scale, or unconstrained, in the form that holds it: a
    /// Parquet decimal where one does, its text otherwise.
    pub fn numeric(typmod: Option<(u32, i32)>) -> ColumnType {
        match typmod {
            Some((p, s)) if (1..=38).contains(&p) && s >= 0 && s as u32 <= p => {
                ColumnType::Numeric {
                    precision: p as u8,
                    scale: s as u8,
                }
            }
            t => ColumnType::NumericText(t.map(|(p, s)| (p as u16, s as i16))),
        }
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
        ) || matches!(self, ColumnType::Numeric { precision, .. } if precision <= 18)
    }

    /// The bits a value takes where the type is narrower than 4 bytes, so that the cut passes over
    /// it.
    pub fn thin_bits(self) -> Option<u64> {
        match self {
            ColumnType::Boolean => Some(1),
            // bit(1) to bit(24), written in 1 to 3 bytes.
            ColumnType::Bit(n) if n <= 24 => Some(u64::from(n).div_ceil(8) * 8),
            _ => None,
        }
    }

    pub fn is_text(self) -> bool {
        matches!(self, ColumnType::Text(_))
    }

    /// Whether it is a string type, text under a collation: text, varchar, bpchar or name.
    pub fn is_string(self) -> bool {
        matches!(
            self,
            ColumnType::Text(_) | ColumnType::Bpchar(_) | ColumnType::Name
        )
    }

    /// Whether it is a bit string: bit(n) or bit varying.
    pub fn is_bits(self) -> bool {
        matches!(self, ColumnType::Bit(_) | ColumnType::Varbit(_))
    }

    /// Whether a level of it is keyed by bytes that need not be text: bytea's, a bit string's, the
    /// statistics types', or the key of the class of a value held as its text that PostgreSQL
    /// orders (jsonb, inet, cidr, tsvector, tsquery).
    pub fn keyed_by_bytes(self) -> bool {
        match self {
            ColumnType::Textual(t) => t.ordered() || t == crate::textual::Textual::Aclitem,
            ColumnType::Internal(t) => t.is_bytes(),
            t => matches!(t, ColumnType::Bytes) || t.is_bits(),
        }
    }

    /// Whether its values are held as bytes rather than text: bytea's, a bit string's key, the
    /// statistics types'.
    pub fn held_as_bytes(self) -> bool {
        match self {
            ColumnType::Geometric(_) => true,
            ColumnType::Internal(t) => t.is_bytes(),
            t => matches!(t, ColumnType::Bytes) || t.is_bits(),
        }
    }

    /// Whether its values are bytes of varying length, held as Parquet's byte arrays: a string's,
    /// bytea's, a numeric's held as its text, or a value of a type held as its text or only a server
    /// makes.
    pub fn is_byte_array(self) -> bool {
        matches!(
            self,
            ColumnType::Bytes
                | ColumnType::NumericText(_)
                | ColumnType::Textual(_)
                | ColumnType::Internal(_)
                | ColumnType::Geometric(_)
        ) || self.is_string()
    }

    /// Whether a level of it is keyed by text compared by its bytes: a string's, the key of a
    /// numeric's class where it is held as its text, a composed value's key written as text that
    /// sorts as the key does, or a pg_node_tree's text.
    pub fn keyed_by_text(self) -> bool {
        matches!(
            self,
            ColumnType::NumericText(_) | ColumnType::Internal(crate::textual::Internal::NodeTree)
        ) || self.is_string()
            || self.is_nested()
    }

    /// Whether it is a timestamptz, rounded or not.
    pub fn is_timestamptz(self) -> bool {
        matches!(
            self,
            ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_)
        )
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
            ColumnType::TimestamptzRounded(p) => format!("timestamptz({p})"),
            ColumnType::Text(None) => "text".into(),
            ColumnType::Text(Some(ColumnType::VARCHAR)) => "varchar".into(),
            ColumnType::Text(Some(n)) => format!("varchar({n})"),
            ColumnType::Bytes => "bytea".into(),
            ColumnType::Numeric { precision, scale } => format!("numeric({precision},{scale})"),
            ColumnType::Boolean => "boolean".into(),
            ColumnType::Uuid => "uuid".into(),
            other => other.pg_name(),
        }
    }

    /// Whether a value of the type holds money: money itself, or a composed type with money among
    /// its parts.
    pub fn holds_money(self) -> bool {
        use crate::composed::Composed;
        match self {
            ColumnType::Money(_) => true,
            ColumnType::Composed(r) => match r.def() {
                Composed::Array { element, .. } => element.holds_money(),
                Composed::Record { fields, .. } => fields.iter().any(|f| f.ty.holds_money()),
                Composed::Range { subtype, .. } => subtype.holds_money(),
                Composed::Multirange { range, .. } => range.holds_money(),
                _ => false,
            },
            _ => false,
        }
    }

    /// The type with its money, and the money its parts hold, read and printed under `m`.
    pub fn with_monetary(self, m: crate::monetary::Monetary) -> ColumnType {
        use crate::composed::Composed;
        match self {
            ColumnType::Money(_) => ColumnType::Money(m),
            ColumnType::Composed(r) => crate::composed::type_of(match r.def() {
                Composed::Array { element, domain } => Composed::Array {
                    element: element.with_monetary(m),
                    domain: domain.clone(),
                },
                Composed::Record { name, fields } => Composed::Record {
                    name: name.clone(),
                    fields: fields
                        .iter()
                        .map(|f| crate::composed::Field {
                            ty: f.ty.with_monetary(m),
                            ..f.clone()
                        })
                        .collect(),
                },
                Composed::Range {
                    name,
                    subtype,
                    discrete,
                } => Composed::Range {
                    name: name.clone(),
                    subtype: subtype.with_monetary(m),
                    discrete: *discrete,
                },
                Composed::Multirange { name, range } => Composed::Multirange {
                    name: name.clone(),
                    range: range.with_monetary(m),
                },
                other => other.clone(),
            }),
            ty => ty,
        }
    }

    /// The type as PostgreSQL's `format_type` names it.
    pub fn pg_name(self) -> String {
        match self {
            ColumnType::Int2 => "smallint".into(),
            ColumnType::Int4 => "integer".into(),
            ColumnType::Int8 => "bigint".into(),
            ColumnType::Timestamptz => "timestamp with time zone".into(),
            ColumnType::TimestamptzRounded(p) => format!("timestamp({p}) with time zone"),
            ColumnType::Text(None) => "text".into(),
            ColumnType::Text(Some(ColumnType::VARCHAR)) => "character varying".into(),
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
            ColumnType::Money(_) => "money".into(),
            ColumnType::Macaddr => "macaddr".into(),
            ColumnType::Macaddr8 => "macaddr8".into(),
            ColumnType::Float4 => "real".into(),
            ColumnType::Float8 => "double precision".into(),
            ColumnType::Reg(kind, _) => kind.type_name().into(),
            ColumnType::Oid8 => "oid8".into(),
            ColumnType::Tid => "tid".into(),
            ColumnType::Bpchar(None) => "bpchar".into(),
            ColumnType::Bpchar(Some(n)) => format!("character({n})"),
            ColumnType::Name => "name".into(),
            ColumnType::Bit(n) => format!("bit({n})"),
            ColumnType::Varbit(None) => "bit varying".into(),
            ColumnType::Varbit(Some(n)) => format!("bit varying({n})"),
            ColumnType::NumericText(None) => "numeric".into(),
            ColumnType::NumericText(Some((p, s))) => format!("numeric({p},{s})"),
            ColumnType::Composed(r) => r.def().pg_name(),
            ColumnType::Date => "date".into(),
            ColumnType::Time(p) => format!("time{} without time zone", precision_of(p)),
            ColumnType::Timetz(p) => format!("time{} with time zone", precision_of(p)),
            ColumnType::Timestamp(p) => format!("timestamp{} without time zone", precision_of(p)),
            ColumnType::Interval { range, precision } => {
                format!(
                    "interval{}{}",
                    interval_fields(range),
                    precision_of(precision)
                )
            }
            ColumnType::Textual(t) => t.type_name().into(),
            ColumnType::Internal(t) => t.type_name().into(),
            ColumnType::Geometric(k) => k.name().into(),
        }
    }

    /// The B-tree operator class PostgreSQL takes for a key of this type where its index names
    /// none; xid's and cid's are hash ones, as they have no other.
    pub fn default_opclass(self) -> &'static str {
        match self {
            ColumnType::Int2 => "int2_ops",
            ColumnType::Int4 => "int4_ops",
            ColumnType::Int8 => "int8_ops",
            ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => "timestamptz_ops",
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
            ColumnType::Money(_) => "money_ops",
            ColumnType::Macaddr => "macaddr_ops",
            ColumnType::Macaddr8 => "macaddr8_ops",
            ColumnType::Float4 => "float4_ops",
            ColumnType::Float8 => "float8_ops",
            // A reg* type has none of its own: PostgreSQL takes oid's, to which it is binary-coercible.
            ColumnType::Reg(..) => "oid_ops",
            ColumnType::Oid8 => "oid8_ops",
            ColumnType::Tid => "tid_ops",
            ColumnType::Bpchar(_) => "bpchar_ops",
            ColumnType::Name => "name_ops",
            ColumnType::Bit(_) => "bit_ops",
            ColumnType::Varbit(_) => "varbit_ops",
            ColumnType::NumericText(_) => "numeric_ops",
            ColumnType::Date => "date_ops",
            ColumnType::Time(_) => "time_ops",
            ColumnType::Timetz(_) => "timetz_ops",
            ColumnType::Timestamp(_) => "timestamp_ops",
            ColumnType::Interval { .. } => "interval_ops",
            ColumnType::Composed(r) => r.def().default_opclass(),
            ColumnType::Textual(t) => t.opclass().unwrap_or(""),
            ColumnType::Internal(t) => t.opclass().unwrap_or(""),
            ColumnType::Geometric(_) => "",
        }
    }

    /// The hash operator class PostgreSQL takes for a key of this type where its index names none;
    /// none where it has none: money, the bit strings, the text search types, json, jsonpath, xml,
    /// refcursor, the geometric types, and an array, a composite or a range of a part with none.
    pub fn hash_opclass(self) -> Option<&'static str> {
        Some(match self {
            ColumnType::Money(_) | ColumnType::Bit(_) | ColumnType::Varbit(_) => return None,
            ColumnType::Geometric(_) => return None,
            ColumnType::Textual(crate::textual::Textual::Aclitem) => "aclitem_ops",
            ColumnType::Textual(t) => match t {
                crate::textual::Textual::Jsonb
                | crate::textual::Textual::Inet
                | crate::textual::Textual::Cidr => t.opclass()?,
                _ => return None,
            },
            ColumnType::Composed(r) => {
                use crate::composed::Composed;
                let parts = match r.def() {
                    Composed::Array { element, .. } => element.hash_opclass().is_some(),
                    Composed::Record { fields, .. } => {
                        fields.iter().all(|f| f.ty.hash_opclass().is_some())
                    }
                    Composed::Range { subtype, .. } => subtype.hash_opclass().is_some(),
                    Composed::Multirange { range, .. } => range.hash_opclass().is_some(),
                    Composed::Enum { .. } => true,
                    Composed::Held(t) => t.hash_opclass().is_some(),
                };
                if !parts {
                    return None;
                }
                r.def().default_opclass()
            }
            ty => match ty.default_opclass() {
                "" => return None,
                o => o,
            },
        })
    }

    /// The bits of an integer type.
    pub fn integer_bits(self) -> u32 {
        match self {
            ColumnType::Int2 => 16,
            ColumnType::Int4 => 32,
            _ => 64,
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
            // Its grantee and grantor, oids, and its privileges, the grant options in the high half.
            ColumnType::Textual(crate::textual::Textual::Aclitem) => &[
                ("grantee", ColumnType::Oid, 32),
                ("grantor", ColumnType::Oid, 32),
                ("privileges", ColumnType::Xid8, 64),
            ],
            // Its float8s, in the order PostgreSQL lays them out.
            ColumnType::Geometric(crate::geo::GeoKind::Point) => {
                &[("x", ColumnType::Float8, 0), ("y", ColumnType::Float8, 0)]
            }
            ColumnType::Geometric(crate::geo::GeoKind::Line) => &[
                ("a", ColumnType::Float8, 0),
                ("b", ColumnType::Float8, 0),
                ("c", ColumnType::Float8, 0),
            ],
            ColumnType::Geometric(crate::geo::GeoKind::Lseg) => &[
                ("x1", ColumnType::Float8, 0),
                ("y1", ColumnType::Float8, 0),
                ("x2", ColumnType::Float8, 0),
                ("y2", ColumnType::Float8, 0),
            ],
            ColumnType::Geometric(crate::geo::GeoKind::Box) => &[
                ("high_x", ColumnType::Float8, 0),
                ("high_y", ColumnType::Float8, 0),
                ("low_x", ColumnType::Float8, 0),
                ("low_y", ColumnType::Float8, 0),
            ],
            ColumnType::Geometric(crate::geo::GeoKind::Circle) => &[
                ("x", ColumnType::Float8, 0),
                ("y", ColumnType::Float8, 0),
                ("radius", ColumnType::Float8, 0),
            ],
            // Its length as an integer is held, its bits as bytea, the last byte padded.
            ColumnType::Varbit(_) => &[
                ("length", ColumnType::Int4, 0),
                ("bits", ColumnType::Bytes, 0),
            ],
            ColumnType::Timetz(_) => &[
                ("time", ColumnType::Time(None), 0),
                ("zone", ColumnType::Int4, 0),
            ],
            _ => &[],
        }
    }

    /// The definition whose tree of leaves holds its values, each leaf with its levels: a composed
    /// type's own but an enum's, and the tree a path, a polygon or a snapshot is held as.
    pub fn tree(self) -> Option<crate::composed::TypeRef> {
        match self {
            ColumnType::Composed(r) if crate::composed::is_nested(r) => Some(r),
            ty => crate::composed::held_tree(ty),
        }
    }

    /// How many leaves its Parquet type tree has.
    pub fn leaf_count(self) -> usize {
        match self.tree() {
            Some(r) => crate::composed::leaves(r).len(),
            None if matches!(self, ColumnType::Numeric { .. }) => 2,
            None => self.fields().len().max(1),
        }
    }

    /// Whether its values are held as a tree of leaves rather than as one: a tid, a value of a fixed
    /// group, a path, a polygon or a snapshot, or a composed type but an enum.
    pub fn is_group(self) -> bool {
        match self {
            ColumnType::Composed(r) => crate::composed::is_nested(r),
            _ => self.leaf_count() > 1,
        }
    }

    /// Whether it is a type held as a tree of leaves of its own, each with its levels, though no
    /// composed type: a path, a polygon or a snapshot.
    pub fn is_held_tree(self) -> bool {
        !matches!(self, ColumnType::Composed(_)) && self.tree().is_some()
    }

    /// Whether it is a composed type held as a tree of leaves, each with its levels: every composed
    /// type but an enum.
    pub fn is_nested(self) -> bool {
        matches!(self, ColumnType::Composed(r) if crate::composed::is_nested(r))
    }

    /// The leaves of its Parquet type tree: the value itself, or a leaf a field of the group it is.
    pub fn leaves(self) -> Vec<Leaf> {
        if let Some(r) = self.tree() {
            return crate::composed::leaves(r).to_vec();
        }
        // A numeric's decimal, and beside it whether it is NaN.
        if let ColumnType::Numeric { .. } = self {
            return [
                (crate::numeric::VALUE_LEAF, self),
                (crate::numeric::NAN_LEAF, ColumnType::Boolean),
            ]
            .into_iter()
            .map(|(name, ty)| Leaf {
                field: Some(name),
                ..Leaf::whole(ty)
            })
            .collect();
        }
        match self.fields() {
            [] => vec![Leaf::whole(self)],
            fields => fields
                .iter()
                .map(|&(name, ty, bits)| Leaf {
                    field: Some(name),
                    ty,
                    bits,
                    nested: None,
                    thin: false,
                    repeated: false,
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
    /// For a leaf of a composed column's tree, that type and the leaf's place among its leaves.
    pub nested: Option<(crate::composed::TypeRef, u16)>,
    /// Whether the cut passes over it, as over a boolean: an array's dimensions and lower bounds.
    pub thin: bool,
    /// Whether it lies under a list, so that a row holds any number of its values.
    pub repeated: bool,
}

impl Leaf {
    /// The one leaf of a column held as its value itself.
    pub(crate) fn whole(ty: ColumnType) -> Leaf {
        Leaf {
            field: None,
            ty,
            bits: ty.unsigned_bits(),
            nested: None,
            thin: false,
            repeated: false,
        }
    }

    /// A leaf of a composed column's tree: its path below the column joined by points, its type,
    /// the composed type and its place among that type's leaves.
    pub(crate) fn nested(
        field: &'static str,
        ty: ColumnType,
        bits: u32,
        of: crate::composed::TypeRef,
        index: u16,
        thin: bool,
        repeated: bool,
    ) -> Leaf {
        Leaf {
            field: Some(field),
            ty,
            bits,
            nested: Some((of, index)),
            thin,
            repeated,
        }
    }

    /// Its path in the file: the column's name, then its field's, or its path in its column's tree.
    pub fn path(&self, column: &str) -> Vec<String> {
        if let Some((r, i)) = self.nested {
            return std::iter::once(column.to_string())
                .chain(crate::composed::leaf_path(r, i as usize).iter().cloned())
                .collect();
        }
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
        (_, Text(_) | Bpchar(_) | Name)
            | (
                Int2 | Int4 | Int8 | Numeric { .. },
                Int2 | Int4 | Int8 | Numeric { .. }
            )
            | (
                Timestamptz | TimestamptzRounded(_),
                Timestamptz | TimestamptzRounded(_)
            )
            | (
                Date,
                Date | Timestamp(_) | Timestamptz | TimestamptzRounded(_)
            )
            | (
                Timestamp(_),
                Date | Time(_) | Timestamp(_) | Timestamptz | TimestamptzRounded(_)
            )
            | (
                Timestamptz | TimestamptzRounded(_),
                Date | Time(_) | Timetz(_) | Timestamp(_)
            )
            | (Time(_), Time(_) | Timetz(_) | Interval { .. })
            | (Timetz(_), Time(_) | Timetz(_))
            | (Interval { .. }, Interval { .. } | Time(_))
            | (Bytes, Bytes)
            | (Boolean, Boolean)
            | (Uuid, Uuid)
            | (Int2 | Int4 | Int8 | Oid, Oid)
            | (Oid, Int4 | Int8)
            | (Int4 | Int8 | Numeric { .. } | Money(_), Money(_))
            | (Money(_), Numeric { .. })
            | (Text(_) | Bpchar(_) | Char, Char)
            | (Bit(_) | Varbit(_), Bit(_) | Varbit(_))
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
        // A numeric in either form, as the typmod'd one is.
        (NumericText(_), t) => assignable(ColumnType::numeric(Some((9, 2))), t),
        (f, NumericText(_)) => assignable(f, ColumnType::numeric(Some((9, 2)))),
        (Reg(a, _), Reg(b, _)) => a == b || crate::regs::casts_between(a, b),
        (Composed(a), Composed(b)) => a == b,
        // text to regclass is regclass(text), an implicit cast; to any other reg* only explicit.
        (Text(_), Reg(crate::regs::RegKind::Class, _)) => true,
        (Textual(a), Textual(b)) => crate::textual::assignable(a, b),
        (Internal(a), Internal(b)) => a == b,
        (Geometric(a), Geometric(b)) => a == b,
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
    /// The domain its DDL names as its type, whose constraints its values meet; its type is the
    /// domain's base type.
    pub domain: Option<String>,
    /// The collation its COLLATE names, where that one calls a library, ICU or glibc, to order
    /// text.
    pub library: Option<crate::collations::Spec>,
}

impl Column {
    /// Whether its text is ordered by a library collation, glibc's LC_COLLATE C and POSIX aside,
    /// which order by the bytes.
    pub fn collated(&self) -> bool {
        self.ty.is_string() && self.library.as_ref().is_some_and(|s| !s.collate_is_c())
    }

    /// The collator its text is ordered by, where `collated`.
    pub fn orderer(&self) -> Result<Option<&'static crate::collations::Collator>> {
        if self.collated() {
            self.collator()
        } else {
            Ok(None)
        }
    }

    /// What an order with a level on it records the level was cut under: its collation and the
    /// version its library gives, where `collated`; nothing otherwise.
    pub fn cut_under(&self) -> Result<String> {
        match (self.orderer()?, &self.library) {
            (Some(c), Some(spec)) => Ok(spec.cut_under(c.version())),
            _ => Ok(String::new()),
        }
    }

    /// The collator its text is ordered by and its case mapped by, where its collation calls a
    /// library: refused by name by a build without the feature `collations`.
    pub fn collator(&self) -> Result<Option<&'static crate::collations::Collator>> {
        match &self.library {
            None => Ok(None),
            Some(spec) => {
                let name = self.collation.as_deref().unwrap_or("default");
                if !cfg!(feature = "collations") {
                    return Err(crate::collations::unbuilt(name));
                }
                crate::collations::open(spec, name).map(|(_, c)| Some(c))
            }
        }
    }

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
    /// A double precision column times 10 to the power given, floored: a count of its places' units,
    /// as a DBA's `floor(latitude * 1000)` keys thousandths of a degree.
    Scaled(usize, u32),
}

impl Level {
    /// The table column the level is read from.
    pub fn column(self) -> usize {
        match self {
            Level::Column(c) | Level::MonthUtc(c) | Level::Scaled(c, _) => c,
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

/// The access method an index names: the B-tree's, or another, whose strand is of its own kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    Btree,
    /// A strand on the key's B-tree order, `=` one place of it; aclitem, cid and xid on their
    /// bytes.
    Hash,
    Gist,
    Spgist,
    Gin,
    Brin,
}

impl Method {
    pub const ALL: [Method; 6] = [
        Method::Btree,
        Method::Hash,
        Method::Gist,
        Method::Spgist,
        Method::Gin,
        Method::Brin,
    ];

    /// Its name, as `pg_am` holds it.
    pub fn name(self) -> &'static str {
        match self {
            Method::Btree => "btree",
            Method::Hash => "hash",
            Method::Gist => "gist",
            Method::Spgist => "spgist",
            Method::Gin => "gin",
            Method::Brin => "brin",
        }
    }

    pub fn of_name(name: &str) -> Option<Method> {
        Method::ALL.into_iter().find(|m| m.name() == name)
    }

    /// Whether PostgreSQL's method holds a UNIQUE index (`amcanunique`).
    pub fn unique(self) -> bool {
        self == Method::Btree
    }

    /// Whether it takes more than one key (`amcanmulticol`).
    pub fn several_keys(self) -> bool {
        !matches!(self, Method::Hash | Method::Spgist)
    }

    /// Whether it takes INCLUDE (`amcaninclude`).
    pub fn includes(self) -> bool {
        matches!(self, Method::Btree | Method::Gist | Method::Spgist)
    }

    /// Whether it orders its keys, taking ASC, DESC and NULLS FIRST or LAST (`amcanorder`).
    pub fn orders(self) -> bool {
        self == Method::Btree
    }

    /// Whether a table may be clustered on it (`amclusterable`).
    pub fn clusters(self) -> bool {
        matches!(self, Method::Btree | Method::Gist)
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Order {
    pub name: String,
    /// The access method its index names.
    pub method: Method,
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
    /// A partial index's predicate, as written: the strand holds only the rows it admits.
    pub predicate: Option<String>,
    /// The EXCLUDE constraint it stands for, where it stands for one.
    pub exclusion: Option<Exclusion>,
}

/// An EXCLUDE constraint: no two rows whose values each level's operator finds conflicting, a row
/// holding a NULL in one conflicting with none.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Exclusion {
    /// Each level's operator, as written.
    pub operators: Vec<String>,
    /// DEFERRABLE, checked at each statement's end, as PostgreSQL checks one INITIALLY IMMEDIATE.
    pub deferrable: bool,
}

impl Order {
    /// Whether it stands for a PRIMARY KEY or UNIQUE `WITHOUT OVERLAPS`: an EXCLUDE on GiST, `=` on
    /// each key but its last, `&&` on its last, a range or a multirange.
    pub fn without_overlaps(&self) -> bool {
        self.constraint.is_some() && self.exclusion.is_some()
    }

    /// Where a table column sits among the columns this order holds.
    pub fn slot(&self, column: usize) -> Option<usize> {
        self.columns.iter().position(|&c| c == column)
    }

    /// The leaves this order holds, by their slots: each column it holds, its leaves in turn, each
    /// with its table column.
    pub fn leaves(&self, table: &Table) -> Vec<(usize, Leaf)> {
        self.columns
            .iter()
            .flat_map(|&c| table.column_leaves(c).into_iter().map(move |l| (c, l)))
            .collect()
    }

    /// The slots of a table column's leaves among those this order holds.
    pub fn slots(&self, table: &Table, column: usize) -> Option<std::ops::Range<usize>> {
        let at = self.slot(column)?;
        let start = self.columns[..at]
            .iter()
            .map(|&c| table.leaf_count(c))
            .sum::<usize>();
        Some(start..start + table.leaf_count(column))
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

/// What a foreign key does to the rows that reference a row of its parent deleted, or whose key
/// changed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Refused where a row still references the key once the round's changes are checked.
    NoAction,
    /// Refused where a row references the key, whatever else the round wrote.
    Restrict,
    /// The referencing rows deleted, or given the new key.
    Cascade,
    /// The referencing columns set NULL: those the DDL lists, ON DELETE only; every one where it
    /// lists none.
    SetNull(Vec<usize>),
    /// The referencing columns set to their DEFAULT, listed as SET NULL's are.
    SetDefault(Vec<usize>),
}

impl ForeignKey {
    /// The columns an action sets: those it lists, or every referencing column.
    pub fn set_by(&self, action: &Action) -> Vec<usize> {
        match action {
            Action::SetNull(c) | Action::SetDefault(c) if !c.is_empty() => c.clone(),
            _ => self.columns.clone(),
        }
    }
}

/// A FOREIGN KEY: a row's reference is in its parent's key, unless the reference holds a NULL.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ForeignKey {
    pub name: String,
    /// The table's referencing columns, in the order written.
    pub columns: Vec<usize>,
    /// The table referenced.
    pub parent: String,
    /// The parent's columns, each matching the referencing column at its place.
    pub parent_columns: Vec<usize>,
    /// The parent's order that stands for the PRIMARY KEY, UNIQUE or unique index referenced.
    pub key: String,
    /// MATCH FULL: a reference holding a NULL holds only NULLs. Otherwise MATCH SIMPLE: a
    /// reference holding a NULL references nothing.
    pub full: bool,
    pub on_delete: Action,
    pub on_update: Action,
    pub deferrable: bool,
    /// INITIALLY DEFERRED: checked at the commit, unless SET CONSTRAINTS says otherwise.
    pub deferred: bool,
    /// NOT ENFORCED checks nothing and acts on nothing.
    pub enforced: bool,
    /// Whether every row was checked: false where it was added NOT VALID, or NOT ENFORCED, and not
    /// validated since.
    pub valid: bool,
    /// PERIOD: its last column a range or a multirange, which the periods of the parent's rows
    /// matching the other columns by `=` together hold.
    pub period: bool,
}

impl ForeignKey {
    /// The columns a reference matches by `=`: every one, or all but its PERIOD.
    pub fn equal_columns(&self) -> &[usize] {
        &self.columns[..self.columns.len() - usize::from(self.period)]
    }
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
    /// Its foreign keys, each referencing a table of the file.
    pub foreign_keys: Vec<ForeignKey>,
    /// The lc_monetary its money reads and prints under: the file's.
    pub monetary: crate::monetary::Monetary,
    /// The format of the file it lies in, whose leaves its columns are held as.
    pub format: u32,
}

impl Table {
    /// The leaves of its columns, in its columns' order, each with its column.
    pub fn leaves(&self) -> Vec<(usize, Leaf)> {
        (0..self.columns.len())
            .flat_map(|c| self.column_leaves(c).into_iter().map(move |l| (c, l)))
            .collect()
    }

    /// Whether a column may hold NaN: a numeric(p, s) in a file of format 9; formats 7 and 8 hold
    /// none.
    pub fn may_hold_nan(&self, column: usize) -> bool {
        matches!(self.columns[column].ty, ColumnType::Numeric { .. })
            && self.format >= crate::file::FORMAT
    }

    /// The leaves a column is held as: its type's, a numeric(p, s) that may hold no NaN its decimal
    /// alone.
    pub fn column_leaves(&self, column: usize) -> Vec<Leaf> {
        let ty = self.columns[column].ty;
        match ty {
            ColumnType::Numeric { .. } if !self.may_hold_nan(column) => vec![Leaf::whole(ty)],
            _ => ty.leaves(),
        }
    }

    /// How many leaves a column is held as.
    pub fn leaf_count(&self, column: usize) -> usize {
        match self.columns[column].ty {
            ColumnType::Numeric { .. } if !self.may_hold_nan(column) => 1,
            ty => ty.leaf_count(),
        }
    }

    /// Whether a column's values are held as a tree of leaves rather than as one.
    pub fn is_group(&self, column: usize) -> bool {
        match self.columns[column].ty {
            ColumnType::Numeric { .. } if !self.may_hold_nan(column) => false,
            ty => ty.is_group(),
        }
    }

    /// A column's values as the leaves it is held as hold them, one `Values` a leaf.
    pub fn split_leaves(
        &self,
        column: usize,
        values: crate::values::Values,
    ) -> Vec<crate::values::Values> {
        if self.is_group(column) {
            values.into_leaves(self.columns[column].ty)
        } else {
            vec![values]
        }
    }

    /// Where a column's first leaf lies among the table's leaves.
    pub fn first_leaf(&self, column: usize) -> usize {
        (0..column).map(|c| self.leaf_count(c)).sum()
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

    /// The order whose first levels are the columns given, in whatever order: the table's own
    /// order first, then its indexes in turn.
    pub fn order_led_by(&self, columns: &[usize]) -> Option<usize> {
        let mut want = columns.to_vec();
        want.sort_unstable();
        self.orders.iter().position(|o| {
            let mut lead: Vec<usize> = o
                .levels
                .iter()
                .take(want.len())
                .filter_map(|l| match l {
                    Level::Column(c) => Some(*c),
                    Level::MonthUtc(_) | Level::Scaled(..) => None,
                })
                .collect();
            lead.sort_unstable();
            lead == want
        })
    }

    /// What `write` says of each foreign key whose columns lead none of the table's orders: a
    /// change of its parent's key reads the table whole.
    pub fn keys_read_whole(&self) -> Vec<String> {
        self.foreign_keys
            .iter()
            .filter(|k| k.enforced && self.order_led_by(k.equal_columns()).is_none())
            .map(|k| {
                let columns: Vec<&str> = k
                    .equal_columns()
                    .iter()
                    .map(|&c| self.columns[c].name.as_str())
                    .collect();
                format!(
                    "{}: no order or index leads with {}, the columns of its foreign key {}, so a \
                     key of {} taken away reads {} whole",
                    self.name,
                    columns.join(", "),
                    k.name,
                    k.parent,
                    self.name
                )
            })
            .collect()
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
        self.bytes_casing(&[]).is_some()
    }

    /// How it orders text and maps case where the snail holds it: by its bytes as `bytes_casing`
    /// says, or through its library with the feature `collations`.
    pub fn casing(&self, declared: &[Collation]) -> Option<Casing> {
        if let Some(k) = self.bytes_casing(declared) {
            return Some(k);
        }
        let spec = self.spec(declared)?;
        crate::collations::open(&spec, &self.name)
            .ok()
            .map(|(id, _)| Casing::Library(id))
    }

    /// The library collation it is, where it orders text otherwise than by its bytes: a copy's as
    /// its source's.
    pub fn spec(&self, declared: &[Collation]) -> Option<crate::collations::Spec> {
        use crate::collations::Spec;
        if let Some(f) = &self.from {
            return library_spec(f, declared);
        }
        if self.bytes_casing(declared).is_some() {
            return None;
        }
        match self.provider.as_str() {
            "icu" => Some(Spec::icu(
                self.locale.as_deref().unwrap_or("und"),
                self.rules.as_deref(),
                self.deterministic,
            )),
            "libc" => {
                let collate = self.lc_collate.as_ref().or(self.locale.as_ref())?;
                let ctype = self.lc_ctype.as_ref().or(self.locale.as_ref())?;
                Some(Spec::libc(collate, ctype))
            }
            _ => None,
        }
    }

    /// How it maps case where it orders text by its bytes: a copy's as its source's, among the
    /// built-in ones and `declared`; none where it orders text otherwise.
    pub fn bytes_casing(&self, declared: &[Collation]) -> Option<Casing> {
        if let Some(f) = &self.from {
            return bytes_casing(f, declared);
        }
        if !self.deterministic {
            return None;
        }
        let collate = self.lc_collate.as_ref().or(self.locale.as_ref())?;
        let ctype = self.lc_ctype.as_ref().or(self.locale.as_ref())?;
        match self.provider.as_str() {
            "libc" => match (collate.as_str(), ctype.as_str()) {
                ("C" | "POSIX" | "C.utf8" | "C.UTF-8", "C" | "POSIX") => Some(Casing::Ascii),
                ("C" | "POSIX" | "C.utf8" | "C.UTF-8", "C.utf8" | "C.UTF-8") => Some(Casing::Libc),
                _ => None,
            },
            "builtin" => match collate.as_str() {
                "C" => Some(Casing::Ascii),
                "C.UTF-8" | "C.utf8" => Some(Casing::Simple),
                "PG_UNICODE_FAST" => Some(Casing::Full),
                _ => None,
            },
            _ => None,
        }
    }
}

/// How a collation the snail holds orders text and maps case: without the feature `collations`, each
/// by its bytes, differing only in case; with it, `Library` through its library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Casing {
    /// ASCII's letters alone: C, POSIX, ucs_basic.
    Ascii,
    /// PostgreSQL's own Unicode tables, a character to a character: pg_c_utf8.
    Simple,
    /// PostgreSQL's own Unicode tables, a character to its full mapping: pg_unicode_fast.
    Full,
    /// glibc's C.UTF-8, a character to a character: libc C.utf8, and the database's default.
    Libc,
    /// A collation that calls its library, ICU or glibc, ordering text and mapping case as the
    /// library does: the collator `collations::get` gives by this number.
    Library(u16),
}

/// Why a collation is refused without the feature `collations`.
pub(crate) const NOT_HELD: &str =
    "text is compared by its bytes, so only a collation of byte order \
     is read without the feature collations: C, POSIX, ucs_basic, pg_c_utf8, pg_unicode_fast, \
     C.utf8, or one the DDL makes like them";

/// How the collation named orders text and maps case, where the snail holds it: by its bytes as
/// `bytes_casing` says, or, with the feature `collations`, through its library.
pub fn collation_held(name: &str, declared: &[Collation]) -> Option<Casing> {
    collation_casing(name, declared).ok()
}

/// How the collation named orders text and maps case, or why the snail does not hold it, by name.
pub fn collation_casing(name: &str, declared: &[Collation]) -> Result<Casing> {
    if let Some(k) = bytes_casing(name, declared) {
        return Ok(k);
    }
    match library_spec(name, declared) {
        Some(spec) if cfg!(feature = "collations") => {
            crate::collations::open(&spec, name).map(|(id, _)| Casing::Library(id))
        }
        Some(_) => Err(Error::new(format!("COLLATE {name}: {NOT_HELD}"))),
        None => Err(Error::new(format!(
            "collation \"{name}\" for encoding \"UTF8\" does not exist"
        ))),
    }
}

/// How the collation named maps case, where it orders text by its bytes: one of PostgreSQL's
/// built-in collations ordering text by its bytes, the database's default (libc C.UTF-8, as every
/// database on the certificates' server), or one `declared` like them.
pub fn bytes_casing(name: &str, declared: &[Collation]) -> Option<Casing> {
    let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
    match bare {
        "C" | "POSIX" | "ucs_basic" => Some(Casing::Ascii),
        "pg_c_utf8" => Some(Casing::Simple),
        "pg_unicode_fast" => Some(Casing::Full),
        "C.utf8" | "default" => Some(Casing::Libc),
        _ => declared
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.bytes_casing(declared)),
    }
}

/// The library collation the name names, where it orders text otherwise than by its bytes: one
/// `declared`, or one of those initdb makes: ICU's `-x-icu` collations and `unicode`, and glibc's
/// UTF-8 locales by their names and by their names less `.utf8`. With the feature `collations`,
/// only one the machine's libraries hold; without it, any other name, which it cannot look up.
pub fn library_spec(name: &str, declared: &[Collation]) -> Option<crate::collations::Spec> {
    use crate::collations::Spec;
    if let Some(c) = declared.iter().find(|c| c.name == name) {
        return c.spec(declared);
    }
    let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
    if bytes_casing(bare, &[]).is_some() {
        return None;
    }
    if bare == "unicode" {
        return Some(Spec::icu("und", None, true));
    }
    let built = cfg!(feature = "collations");
    if let Some(tag) = bare.strip_suffix("-x-icu") {
        if built && !crate::collations::icu_locales().iter().any(|t| t == tag) {
            return None;
        }
        return Some(Spec::icu(tag, None, true));
    }
    if built {
        return crate::collations::libc_locale(bare).map(|l| Spec::libc(&l, &l));
    }
    let l = if bare.contains('.') {
        bare.to_string()
    } else {
        format!("{bare}.utf8")
    };
    Some(Spec::libc(&l, &l))
}

/// A view: its name, the names its column list gives its columns, and its question as written.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct View {
    pub name: String,
    pub columns: Vec<String>,
    pub query: String,
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
    /// The views the DDL creates, each read as a step of the question that names it.
    pub views: Vec<View>,
    /// The PostgreSQL release the file's OIDs came from, whose catalog names its reg* values.
    pub release: u8,
    /// The lc_monetary the file's money was written under, which every money value a statement
    /// reads or makes reads and prints under.
    pub monetary: crate::monetary::Monetary,
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
    /// A double precision column times 10 to the power given, floored.
    Scaled(usize, u32),
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
        DataType::Timestamp(Some(p), TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::TimestamptzRounded((*p).min(6) as u8)
        }
        DataType::Timestamp(p, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
            ColumnType::Timestamp(time_precision(*p))
        }
        DataType::Time(p, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
            ColumnType::Time(time_precision(*p))
        }
        DataType::Time(p, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::Timetz(time_precision(*p))
        }
        DataType::Date => ColumnType::Date,
        DataType::Interval { fields, precision } => ColumnType::Interval {
            range: match fields {
                None => crate::times::INTERVAL_FULL_RANGE,
                Some(f) => crate::times::interval_range(&f.to_string().to_lowercase()).ok_or_else(
                    || Error::new(format!("{}: no such interval fields", spelled(dt))),
                )?,
            },
            precision: time_precision(*precision),
        },
        DataType::Text => ColumnType::Text(None),
        DataType::Varchar(length)
        | DataType::CharacterVarying(length)
        | DataType::CharVarying(length) => ColumnType::Text(match length {
            None => Some(ColumnType::VARCHAR),
            Some(CharacterLength::IntegerLength { length, unit: None }) => {
                Some(type_length(*length, "varchar", MAX_LENGTH)?)
            }
            Some(_) => {
                return Err(Error::new(format!(
                    "{}: PostgreSQL counts a length in characters only",
                    spelled(dt)
                )))
            }
        }),
        DataType::Bytea => ColumnType::Bytes,
        DataType::Char(length) | DataType::Character(length) => match length {
            None => ColumnType::Bpchar(Some(1)),
            Some(CharacterLength::IntegerLength { length, unit: None }) => {
                ColumnType::Bpchar(Some(type_length(*length, "char", MAX_LENGTH)?))
            }
            Some(_) => {
                return Err(Error::new(format!(
                    "{}: PostgreSQL counts a length in characters only",
                    spelled(dt)
                )))
            }
        },
        DataType::Bit(length) => ColumnType::Bit(match length {
            None => 1,
            Some(n) => type_length(*n, "bit", MAX_LENGTH * 8)?,
        }),
        DataType::BitVarying(length) | DataType::VarBit(length) => ColumnType::Varbit(
            length
                .map(|n| type_length(n, "bit varying", MAX_LENGTH * 8))
                .transpose()?,
        ),
        DataType::Boolean | DataType::Bool => ColumnType::Boolean,
        DataType::Uuid => ColumnType::Uuid,
        DataType::Regclass => ColumnType::Reg(crate::regs::RegKind::Class, crate::regs::LATEST),
        DataType::JSON => ColumnType::Textual(crate::textual::Textual::Json),
        DataType::JSONB => ColumnType::Textual(crate::textual::Textual::Jsonb),
        DataType::TsVector => ColumnType::Textual(crate::textual::Textual::Tsvector),
        DataType::TsQuery => ColumnType::Textual(crate::textual::Textual::Tsquery),
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
                "money" => ColumnType::Money(crate::monetary::Monetary::C),
                "macaddr" => ColumnType::Macaddr,
                "macaddr8" => ColumnType::Macaddr8,
                "oid8" => ColumnType::Oid8,
                "tid" => ColumnType::Tid,
                "bpchar" => ColumnType::Bpchar(None),
                "name" => ColumnType::Name,
                "date" => ColumnType::Date,
                "time" => ColumnType::Time(None),
                "timetz" => ColumnType::Timetz(None),
                "timestamp" => ColumnType::Timestamp(None),
                "timestamptz" => ColumnType::Timestamptz,
                "interval" => ColumnType::Interval {
                    range: crate::times::INTERVAL_FULL_RANGE,
                    precision: None,
                },
                other if crate::textual::Textual::of_type_name(other).is_some() => {
                    ColumnType::Textual(crate::textual::Textual::of_type_name(other).unwrap())
                }
                other if crate::geo::GeoKind::of_name(other).is_some() => {
                    ColumnType::Geometric(crate::geo::GeoKind::of_name(other).unwrap())
                }
                other if crate::textual::Internal::of_type_name(other).is_some() => {
                    ColumnType::Internal(crate::textual::Internal::of_type_name(other).unwrap())
                }
                other => match crate::regs::RegKind::of_type_name(other) {
                    Some(kind) => ColumnType::Reg(kind, crate::regs::LATEST),
                    None => {
                        return Err(Error::new(format!("the type {} is not held", spelled(dt))))
                    }
                },
            }
        }
        // bpchar(n) and nchar(n), as PostgreSQL's grammar reads them.
        DataType::Custom(name, args) if args.len() == 1 => {
            let n = object_name(name);
            match n.strip_prefix("pg_catalog.").unwrap_or(&n) {
                "bpchar" | "nchar" => {
                    let length = args[0].trim().parse::<u64>().map_err(|_| {
                        Error::new(format!("{}: invalid type modifier", spelled(dt)))
                    })?;
                    ColumnType::Bpchar(Some(type_length(length, "char", MAX_LENGTH)?))
                }
                _ => return Err(Error::new(format!("the type {} is not held", spelled(dt)))),
            }
        }
        DataType::Numeric(info) | DataType::Decimal(info) => {
            let (p, s) = match info {
                ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
                ExactNumberInfo::Precision(p) => (*p, 0),
                ExactNumberInfo::None => return Ok(ColumnType::numeric(None)),
            };
            // As numerictypmodin refuses them.
            if !(1..=u64::from(crate::numeric::PRECISION_MAX)).contains(&p) {
                return Err(Error::new(format!(
                    "NUMERIC precision {p} must be between 1 and {}",
                    crate::numeric::PRECISION_MAX
                )));
            }
            if !(i64::from(crate::numeric::SCALE_MIN)..=i64::from(crate::numeric::SCALE_MAX))
                .contains(&s)
            {
                return Err(Error::new(format!(
                    "NUMERIC scale {s} must be between {} and {}",
                    crate::numeric::SCALE_MIN,
                    crate::numeric::SCALE_MAX
                )));
            }
            ColumnType::numeric(Some((p as u32, s as i32)))
        }
        other => {
            return Err(Error::new(format!(
                "the type {} is not held",
                spelled(other)
            )))
        }
    })
}

/// The most characters a string type's modifier names, and a bit string's eighth of its bits:
/// PostgreSQL's MaxAttrSize.
const MAX_LENGTH: u64 = 10 * 1024 * 1024;

/// The length a type's modifier names, held to 1 and `most`, as PostgreSQL's typmod input holds it.
fn type_length(n: u64, name: &str, most: u64) -> Result<u32> {
    if n < 1 {
        return Err(Error::new(format!(
            "length for type {name} must be at least 1"
        )));
    }
    if n > most {
        return Err(Error::new(format!(
            "length for type {name} cannot exceed {most}"
        )));
    }
    Ok(n as u32)
}

/// The digits after the second a time type's typmod keeps: PostgreSQL takes more than 6 as 6.
fn time_precision(p: Option<u64>) -> Option<u8> {
    p.map(|p| p.min(6) as u8)
}

/// An interval's fields as its name writes them, from PostgreSQL's typmod mask: a field `f`'s bit
/// is `1 << f`, month 1, year 2, day 3, hour 10, minute 11 and second 12.
fn interval_fields(range: u16) -> &'static str {
    let (y, m, d, h, mi, s) = (1 << 2, 1 << 1, 1 << 3, 1 << 10, 1 << 11, 1 << 12);
    match range {
        r if r == y => " year",
        r if r == m => " month",
        r if r == d => " day",
        r if r == h => " hour",
        r if r == mi => " minute",
        r if r == s => " second",
        r if r == y | m => " year to month",
        r if r == d | h => " day to hour",
        r if r == d | h | mi => " day to minute",
        r if r == d | h | mi | s => " day to second",
        r if r == h | mi => " hour to minute",
        r if r == h | mi | s => " hour to second",
        r if r == mi | s => " minute to second",
        _ => "",
    }
}

/// A time type's precision as its name writes it.
fn precision_of(p: Option<u8>) -> String {
    p.map(|p| format!("({p})")).unwrap_or_default()
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
            // An alias hides the table's own name, as PostgreSQL reads it.
            let named = match alias {
                Some(a) => qualifier == a,
                None => qualifier == bare || qualifier == table,
            };
            if named {
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
                // A COLLATE that orders as its column does: one of byte order on a column of byte
                // order, or its column's own library collation.
                Shape::Column(c)
                    if columns[c].ty.is_string()
                        && match &columns[c].library {
                            None => bytes_casing(&named, &[]).is_some(),
                            Some(own) => library_spec(&named, &[]).as_ref() == Some(own),
                        } =>
                {
                    Ok(Shape::Column(c))
                }
                Shape::Column(c)
                    if columns[c].ty.is_string() && collation_held(&named, &[]).is_some() =>
                {
                    Err(Error::new(format!(
                        "{expr} COLLATE {named}: a column read under a collation that orders \
                         otherwise than its own is not read here"
                    )))
                }
                Shape::Column(c) if !columns[c].ty.is_string() => Err(Error::new(format!(
                    "collations are not supported by type {}",
                    columns[c].ty.pg_name()
                ))),
                _ => Err(Error::new(format!("{expr} COLLATE {named}: {NOT_HELD}"))),
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
        // floor(column * 10^k), a double precision column counted in its places' units.
        Expr::Floor {
            expr: inner,
            field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
        } => scaled(expr, inner, table, alias, columns, functions, env),
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
                Shape::Column(c) if columns[c].ty.is_timestamptz() => Ok(Shape::Clock(c)),
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
                Shape::Column(c) if columns[c].ty.is_timestamptz() => Err(Error::new(format!(
                    "the month of {} needs a clock: extract on a timestamptz is stable, and \
                         PostgreSQL refuses it as an index expression",
                    columns[c].name
                ))),
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

/// The shape of `floor(column * 10^k)` on a double precision column, k from 1 to 9: a count of its
/// k places' units.
fn scaled(
    f: &Expr,
    arg: &Expr,
    table: &str,
    alias: Option<&str>,
    columns: &[Column],
    functions: &[Function],
    env: &[Shape],
) -> Result<Shape> {
    let refused = || {
        Error::new(format!(
            "{f} is not a level the snail reads: floor of a double precision column times a \
             power of ten is"
        ))
    };
    let mut arg = arg;
    while let Expr::Nested(e) = arg {
        arg = e;
    }
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Multiply,
        right,
    } = arg
    else {
        return Err(refused());
    };
    let power = |e: &Expr| match e {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => {
                (1..=9u32).find(|&k| *n == format!("1{}", "0".repeat(k as usize)))
            }
            _ => None,
        },
        _ => None,
    };
    let (column, k) = match (power(left), power(right)) {
        (None, Some(k)) => (left, k),
        (Some(k), None) => (right, k),
        _ => return Err(refused()),
    };
    match shape(column, table, alias, columns, functions, env)? {
        Shape::Column(c) if columns[c].ty == ColumnType::Float8 => Ok(Shape::Scaled(c, k)),
        _ => Err(refused()),
    }
}

/// The level a key's expression is. A timestamptz column on the UTC clock keys as the column itself.
pub(crate) fn level(s: Shape) -> Level {
    match s {
        Shape::Column(c) | Shape::Clock(c) => Level::Column(c),
        Shape::MonthUtc(c) => Level::MonthUtc(c),
        Shape::Scaled(c, k) => Level::Scaled(c, k),
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

    /// The DDL read whole, as `parse` reads it, its first `kept` statements as the file's own: a
    /// library collation they name taken by a build without the feature `collations`, and refused
    /// only where it is used.
    pub fn parse_kept(ddl: &str, kept: usize) -> Result<Catalog> {
        let reading = crate::ddl::read_ddl_kept(ddl, true, kept)?;
        match reading.statements.iter().find_map(|s| s.refused.as_ref()) {
            Some(why) => Err(Error::new(why.clone())),
            None => Ok(Catalog {
                ddl: ddl.to_string(),
                ..reading.catalog
            }),
        }
    }

    /// The number of statements a DDL holds.
    pub fn statements_in(ddl: &str) -> Result<usize> {
        crate::ddl::statements_in(ddl)
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

    /// A view by its name, or the last part of it.
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|v| v.name == name).or_else(|| {
            self.views
                .iter()
                .find(|v| v.name.rsplit('.').next() == Some(name))
        })
    }

    /// Whether a schema of that name is the database's own, or one an object of the file names.
    pub(crate) fn holds_schema(&self, schema: &str) -> bool {
        let named = |n: &str| n.rsplit_once('.').is_some_and(|(s, _)| s == schema);
        matches!(
            schema,
            "public" | "pg_catalog" | "pg_toast" | "information_schema"
        ) || self.tables.iter().any(|t| named(&t.name))
            || self.sequences.iter().any(|s| named(&s.name))
            || self.types.iter().any(|t| named(&t.name))
            || self.domains.iter().any(|d| named(&d.name))
            || self.collations.iter().any(|c| named(&c.name))
            || self.functions.iter().any(|f| named(&f.name))
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

    /// Every money value the catalog's columns, domains and functions hold, read and printed
    /// under the lc_monetary `recorded` names for the file's money: refused where a money column has
    /// none, where two name different locales, and where this build reads no such locale.
    pub fn with_monetary(&mut self, recorded: &[(String, String, String)]) -> Result<()> {
        let mut file: Option<(String, &str)> = None;
        for t in &self.tables {
            for c in t.columns.iter().filter(|c| c.ty.holds_money()) {
                let named = format!("{}.{}", t.name, c.name);
                // A column holding money among its parts takes the file's where it records none.
                let l = match recorded
                    .iter()
                    .find(|(a, b, _)| *a == t.name && *b == c.name)
                {
                    Some((_, _, l)) => l,
                    None if matches!(c.ty, ColumnType::Money(_)) => {
                        return Err(Error::new(format!(
                            "{named} is money, and the file records no lc_monetary for it"
                        )))
                    }
                    None => continue,
                };
                match &file {
                    Some((other, was)) if !crate::monetary::Monetary::names_same(was, l) => {
                        return Err(Error::new(format!(
                            "{named} is money written under lc_monetary {l}, and {other} under \
                             {was}: the snail answers under one"
                        )))
                    }
                    Some(_) => {}
                    None => file = Some((named, l)),
                }
            }
        }
        let m = match &file {
            Some((named, l)) => crate::monetary::Monetary::open(l)
                .map_err(|e| Error::new(format!("{named} is money: {e}")))?,
            None => crate::monetary::Monetary::C,
        };
        self.monetary = m;
        for t in &mut self.tables {
            t.monetary = m;
        }
        // A DDL's money is C's as it is read.
        if m.is_c() {
            return Ok(());
        }
        let mut types: Vec<&mut ColumnType> = Vec::new();
        for t in &mut self.tables {
            types.extend(t.columns.iter_mut().map(|c| &mut c.ty));
        }
        for f in &mut self.functions {
            types.extend(f.args.iter_mut());
        }
        types.extend(self.domains.iter_mut().filter_map(|d| d.base_type.as_mut()));
        for ty in types {
            *ty = ty.with_monetary(m);
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
        let e = refused("CREATE TABLE s.t (a int2vector NOT NULL, PRIMARY KEY (a));");
        assert!(e.contains("is not held"), "{e}");
    }

    #[test]
    fn a_numeric_is_held_in_the_form_its_typmod_allows_and_bytea_as_a_level() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a bigint NOT NULL, k bytea, m numeric(9,2) NOT NULL, \
             w numeric(38,2), u numeric, x decimal(3,-1), y numeric(3,5), z numeric(40), \
             PRIMARY KEY (a));",
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
                },
                ColumnType::Numeric {
                    precision: 38,
                    scale: 2
                },
                ColumnType::NumericText(None),
                ColumnType::NumericText(Some((3, -1))),
                ColumnType::NumericText(Some((3, 5))),
                ColumnType::NumericText(Some((40, 0))),
            ]
        );
        let names: Vec<String> = types.iter().map(|t| t.pg_name()).collect();
        assert_eq!(
            names[4..],
            ["numeric", "numeric(3,-1)", "numeric(3,5)", "numeric(40,0)"]
        );
        for (ddl, why) in [
            (
                "CREATE TABLE s.t (a numeric(1001) NOT NULL, PRIMARY KEY (a));",
                "NUMERIC precision 1001 must be between 1 and 1000",
            ),
            (
                "CREATE TABLE s.t (a numeric(5,1001) NOT NULL, PRIMARY KEY (a));",
                "NUMERIC scale 1001 must be between -1000 and 1000",
            ),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
        let c = Catalog::parse("CREATE TABLE s.t (k bytea NOT NULL, PRIMARY KEY (k));").unwrap();
        assert_eq!(c.tables[0].orders[0].levels, [Level::Column(0)]);
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
                ColumnType::Text(Some(ColumnType::VARCHAR))
            ]
        );
        assert_eq!(types[4].pg_name(), "character varying");
        assert_eq!(types[1].width(), None);
    }

    #[test]
    fn a_collation_not_ordered_by_bytes_is_refused_without_the_feature() {
        let ddl =
            "CREATE TABLE s.t (a integer NOT NULL, b text COLLATE \"en_US\", PRIMARY KEY (a));";
        if cfg!(feature = "collations") {
            assert!(Catalog::parse(ddl).unwrap().tables[0].columns[1].collated());
            let e = refused(
                "CREATE TABLE s.t (a integer NOT NULL, b text COLLATE \"xx_YY\", PRIMARY KEY (a));",
            );
            assert!(e.contains("does not exist"), "{e}");
        } else {
            let e = refused(ddl);
            assert!(e.contains("only a collation of byte order"), "{e}");
            let e = refused("CREATE COLLATION s.pt (provider = icu, locale = 'pt-PT')");
            assert!(e.contains("only a collation of byte order"), "{e}");
        }
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
             CREATE TABLE s.u (a s.unknown NOT NULL, PRIMARY KEY (a));
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
        assert!(
            read[1]
                .1
                .unwrap()
                .contains("the type s.unknown is not held"),
            "{:?}",
            read[1]
        );
        assert_eq!(read[2].1, Some("ua is on s.u, which is refused"));
        assert!(read[3]
            .1
            .unwrap()
            .contains("data type integer has no default operator class for access method \"gin\""));
        assert_eq!(read[4].1, None);
        assert_eq!(
            r.catalog.tables[0]
                .order("tp")
                .unwrap()
                .predicate
                .as_deref(),
            Some("a > 0")
        );
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
        assert_eq!(again.tables[0].orders.len(), 3);
        let e = refused(
            "CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a)); CREATE INDEX x ON s.t USING foo (a);",
        );
        assert!(e.contains("access method \"foo\" does not exist"), "{e}");
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
                "CREATE COLLATION s.pt (provider = libc, locale = 'pt_PT.utf8', deterministic = false)",
                "nondeterministic collations not supported",
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
            ("CREATE TABLE s.t (a integer NOT NULL REFERENCES s.u (a), PRIMARY KEY (a))", "relation \"s.u\" does not exist"),
            ("CREATE TEMP TABLE s.t (a integer NOT NULL, PRIMARY KEY (a))", "logged and lasting"),
            ("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a)) PARTITION BY RANGE (a)", "is not read: only a table's columns"),
            ("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a)); CREATE INDEX x ON s.t (a int8_ops)", "the operator class int8_ops is not read"),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
    }
}
