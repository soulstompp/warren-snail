//! The types held as PostgreSQL's text of each value: json and jsonb, jsonpath, xml, inet and cidr,
//! tsvector and tsquery, refcursor, aclitem, pg_snapshot and txid_snapshot; and the types only a
//! server makes, into its catalogs and its indexes' pages, which no literal gives: pg_node_tree and
//! the statistics types, copied only, and gtsvector and the BRIN summaries, NULL only. Each is read
//! as its input function reads it and printed as its output function writes it; those PostgreSQL
//! orders are keyed as their comparison orders them.

use crate::{Error, Result};

/// A type held as PostgreSQL's text of each value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Textual {
    /// json: its text as written, once its input reads it.
    Json,
    /// jsonb: its text as `jsonb_out` writes it, keyed as `jsonb_cmp` orders it.
    Jsonb,
    /// jsonpath: its text as written, once `jsonpath_in` reads it, printed as `jsonpath_out` writes
    /// it, which does not always read back into the same value.
    Jsonpath,
    /// xml: its text as written, once its input reads it.
    Xml,
    /// inet: its text as `inet_out` writes it, keyed as `network_cmp` orders it.
    Inet,
    /// cidr: its text as `cidr_out` writes it, keyed as inet is.
    Cidr,
    /// tsvector: its text as `tsvectorout` writes it, keyed as `tsvector_cmp` orders it.
    Tsvector,
    /// tsquery: a text `tsqueryin` reads back into the same value, keyed as `tsquery_cmp` orders
    /// it.
    Tsquery,
    /// refcursor: a cursor's name, as text.
    Refcursor,
    /// aclitem: a grantee, a grantor and their privileges, held as the text `aclitemout` writes with
    /// each role's OID, and on its pages as a group of three leaves; compared by `=` alone.
    Aclitem,
    /// pg_snapshot: a snapshot's xmin, its xmax and the transactions in progress, held as the text
    /// `pg_snapshot_out` writes, and on its pages as a tree of its own: xmin, xmax and a list of
    /// the transactions; no comparison.
    PgSnapshot,
    /// txid_snapshot: pg_snapshot's older name, a type of its own, read and written alike.
    TxidSnapshot,
}

impl Textual {
    pub const ALL: [Textual; 12] = [
        Textual::Json,
        Textual::Jsonb,
        Textual::Jsonpath,
        Textual::Xml,
        Textual::Inet,
        Textual::Cidr,
        Textual::Tsvector,
        Textual::Tsquery,
        Textual::Refcursor,
        Textual::Aclitem,
        Textual::PgSnapshot,
        Textual::TxidSnapshot,
    ];

    /// Its name, as PostgreSQL's `format_type` writes it.
    pub fn type_name(self) -> &'static str {
        match self {
            Textual::Json => "json",
            Textual::Jsonb => "jsonb",
            Textual::Jsonpath => "jsonpath",
            Textual::Xml => "xml",
            Textual::Inet => "inet",
            Textual::Cidr => "cidr",
            Textual::Tsvector => "tsvector",
            Textual::Tsquery => "tsquery",
            Textual::Refcursor => "refcursor",
            Textual::Aclitem => "aclitem",
            Textual::PgSnapshot => "pg_snapshot",
            Textual::TxidSnapshot => "txid_snapshot",
        }
    }

    /// Whether it is a snapshot: pg_snapshot or txid_snapshot.
    pub fn is_snapshot(self) -> bool {
        matches!(self, Textual::PgSnapshot | Textual::TxidSnapshot)
    }

    pub fn of_type_name(name: &str) -> Option<Textual> {
        Textual::ALL.into_iter().find(|t| t.type_name() == name)
    }

    /// Whether PostgreSQL has a B-tree operator class for it.
    pub fn ordered(self) -> bool {
        matches!(
            self,
            Textual::Jsonb | Textual::Inet | Textual::Cidr | Textual::Tsvector | Textual::Tsquery
        )
    }

    /// The B-tree operator class PostgreSQL takes for it where an index names none.
    pub fn opclass(self) -> Option<&'static str> {
        match self {
            Textual::Jsonb => Some("jsonb_ops"),
            Textual::Inet | Textual::Cidr => Some("inet_ops"),
            Textual::Tsvector => Some("tsvector_ops"),
            Textual::Tsquery => Some("tsquery_ops"),
            _ => None,
        }
    }
}

/// A type only a server makes, which no literal gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Internal {
    /// pg_node_tree: a node tree's text, as the server writes it into its catalogs; compared as
    /// text.
    NodeTree,
    /// pg_ndistinct: extended statistics' bytes; compared as bytea.
    Ndistinct,
    /// pg_dependencies: extended statistics' bytes; compared as bytea.
    Dependencies,
    /// pg_mcv_list: extended statistics' bytes; compared as bytea.
    McvList,
    /// gtsvector: a GiST key of a tsvector index; NULL only in a table.
    Gtsvector,
    /// pg_brin_bloom_summary: a BRIN bloom range's summary; NULL only in a table.
    BrinBloomSummary,
    /// pg_brin_minmax_multi_summary: a BRIN minmax-multi range's summary; NULL only in a table.
    BrinMinmaxMultiSummary,
}

impl Internal {
    pub const ALL: [Internal; 7] = [
        Internal::NodeTree,
        Internal::Ndistinct,
        Internal::Dependencies,
        Internal::McvList,
        Internal::Gtsvector,
        Internal::BrinBloomSummary,
        Internal::BrinMinmaxMultiSummary,
    ];

    /// Its name, as PostgreSQL's `format_type` writes it.
    pub fn type_name(self) -> &'static str {
        match self {
            Internal::NodeTree => "pg_node_tree",
            Internal::Ndistinct => "pg_ndistinct",
            Internal::Dependencies => "pg_dependencies",
            Internal::McvList => "pg_mcv_list",
            Internal::Gtsvector => "gtsvector",
            Internal::BrinBloomSummary => "pg_brin_bloom_summary",
            Internal::BrinMinmaxMultiSummary => "pg_brin_minmax_multi_summary",
        }
    }

    pub fn of_type_name(name: &str) -> Option<Internal> {
        Internal::ALL.into_iter().find(|t| t.type_name() == name)
    }

    /// Whether its values are bytes, compared as bytea's, rather than text.
    pub fn is_bytes(self) -> bool {
        !matches!(self, Internal::NodeTree)
    }

    /// Whether PostgreSQL casts it implicitly to text, through its output, and to bytea, as its
    /// bytes: the planner's statistics; its kin that hold only NULL cast to neither.
    pub fn casts(self) -> bool {
        matches!(
            self,
            Internal::Ndistinct | Internal::Dependencies | Internal::McvList
        )
    }

    /// Whether a table can hold a value of it at all, copied from a column of its type; the others
    /// hold only NULL.
    pub fn copied(self) -> bool {
        matches!(
            self,
            Internal::NodeTree | Internal::Ndistinct | Internal::Dependencies | Internal::McvList
        )
    }

    /// Whether PostgreSQL orders it, through the binary cast to text or bytea.
    pub fn ordered(self) -> bool {
        self.copied()
    }

    /// The B-tree operator class PostgreSQL takes for it where an index names none: text's or
    /// bytea's, through the binary cast.
    pub fn opclass(self) -> Option<&'static str> {
        match self {
            Internal::NodeTree => Some("text_ops"),
            Internal::Ndistinct | Internal::Dependencies | Internal::McvList => Some("bytea_ops"),
            _ => None,
        }
    }

    /// Why its text is not written, where PostgreSQL's output writes another than the snail would:
    /// pg_ndistinct's and pg_dependencies', which decode the statistics their bytes hold.
    /// pg_mcv_list's output is bytea's, as the snail writes it.
    pub fn unwritten(self) -> Option<Error> {
        matches!(self, Internal::Ndistinct | Internal::Dependencies).then(|| {
            Error::new(format!(
                "the output of {} is not written: PostgreSQL's decodes the statistics the snail \
                 holds as their bytes",
                self.type_name()
            ))
        })
    }

    /// Why a literal of it is refused, as PostgreSQL's input refuses one.
    pub fn refusal(self) -> Error {
        Error::new(match self {
            Internal::BrinMinmaxMultiSummary => {
                "cannot accept a value of type brin_minmax_multi_summary".to_string()
            }
            k => format!("cannot accept a value of type {}", k.type_name()),
        })
    }
}

/// A value of `kind` read as its input function reads it: the text the snail holds for it.
pub fn text_in(kind: Textual, s: &str) -> Result<String> {
    Ok(match kind {
        Textual::Refcursor => s.to_string(),
        Textual::Aclitem => {
            let (grantee, grantor, privileges) = aclitem_parse(s)?;
            aclitem_text(grantee, grantor, privileges)
        }
        Textual::Json => {
            crate::jsonb::json_check(s)?;
            s.to_string()
        }
        Textual::Jsonb => crate::jsonb::jsonb_in(s)?,
        Textual::Jsonpath => {
            crate::jsonpath::jsonpath_in(s)?;
            s.to_string()
        }
        Textual::Xml => crate::xml::xml_in(s)?,
        Textual::Inet => crate::inet::inet_in(s, false)?,
        Textual::Cidr => crate::inet::inet_in(s, true)?,
        Textual::Tsvector => crate::tsearch::tsvector_in(s)?,
        Textual::Tsquery => crate::tsearch::tsquery_in(s)?,
        Textual::PgSnapshot | Textual::TxidSnapshot => {
            let (xmin, xmax, xip) = snapshot_parse(s)?;
            snapshot_text(xmin, xmax, &xip)
        }
    })
}

/// A number as C's `strtoull` reads it in base 10 from byte `at`: white space passed over, a sign,
/// then digits, a number past 64 bits the greatest, one after `-` taken from 2^64; and the byte
/// after it, or `at` where no digit follows.
fn strtou64(b: &[u8], at: usize) -> (u64, usize) {
    let mut i = at;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let negative = b.get(i) == Some(&b'-');
    if matches!(b.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let start = i;
    let mut v: u64 = 0;
    let mut over = false;
    while i < b.len() && b[i].is_ascii_digit() {
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_add(u64::from(b[i] - b'0')))
        {
            Some(x) => v = x,
            None => over = true,
        }
        i += 1;
    }
    if i == start {
        return (0, at);
    }
    let v = if over {
        u64::MAX
    } else if negative {
        v.wrapping_neg()
    } else {
        v
    };
    (v, i)
}

/// A snapshot as `parse_snapshot` reads it: `xmin:xmax:` then the transactions in progress,
/// commas between, each read as `strtou64` reads it; xmin and xmax each of a transaction number
/// that is not 0 in its low 32 bits, xmax not before xmin, the transactions from xmin to before
/// xmax in order, a repeated one dropped. Its xmin, its xmax and its transactions.
pub fn snapshot_parse(s: &str) -> Result<(u64, u64, Vec<u64>)> {
    let bad = || {
        Error::new(format!(
            "invalid input syntax for type pg_snapshot: \"{s}\""
        ))
    };
    let b = s.as_bytes();
    let (xmin, p) = strtou64(b, 0);
    if b.get(p) != Some(&b':') {
        return Err(bad());
    }
    let (xmax, p) = strtou64(b, p + 1);
    if b.get(p) != Some(&b':') {
        return Err(bad());
    }
    let valid = |x: u64| x as u32 != 0;
    if !valid(xmin) || !valid(xmax) || xmax < xmin {
        return Err(bad());
    }
    let mut p = p + 1;
    let mut xip: Vec<u64> = Vec::new();
    let mut last = 0u64;
    while p < b.len() {
        let (v, end) = strtou64(b, p);
        p = end;
        if v < xmin || v >= xmax || v < last {
            return Err(bad());
        }
        if v != last {
            xip.push(v);
        }
        last = v;
        match b.get(p) {
            Some(b',') => p += 1,
            None => {}
            Some(_) => return Err(bad()),
        }
    }
    Ok((xmin, xmax, xip))
}

/// A snapshot as `pg_snapshot_out` writes it.
pub fn snapshot_text(xmin: u64, xmax: u64, xip: &[u64]) -> String {
    let xip: Vec<String> = xip.iter().map(u64::to_string).collect();
    format!("{xmin}:{xmax}:{}", xip.join(","))
}

/// A value of `kind` as its output function writes it, from the text the snail holds.
pub fn text_out(kind: Textual, held: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    match kind {
        Textual::Tsquery => Cow::Owned(crate::tsearch::tsquery_out(held)),
        Textual::Xml => Cow::Owned(crate::xml::xml_out(held)),
        Textual::Jsonpath => Cow::Owned(crate::jsonpath::jsonpath_in(held).unwrap_or_default()),
        _ => Cow::Borrowed(held),
    }
}

/// The bytes a value of `kind` is keyed by, from the text the snail holds: its class's key where
/// PostgreSQL orders it.
pub fn key_bytes(kind: Textual, held: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    use std::borrow::Cow;
    let text = || std::str::from_utf8(held).unwrap_or_default();
    match kind {
        Textual::Jsonb => Cow::Owned(crate::jsonb::jsonb_key(text())),
        Textual::Inet | Textual::Cidr => Cow::Owned(crate::inet::inet_key(text())),
        Textual::Tsvector => Cow::Owned(crate::tsearch::tsvector_key(text())),
        Textual::Tsquery => Cow::Owned(crate::tsearch::tsquery_key(text())),
        _ => Cow::Borrowed(held),
    }
}

impl Textual {
    /// Whether it is a network address type: inet or cidr, which compare with each other.
    pub fn is_network(self) -> bool {
        matches!(self, Textual::Inet | Textual::Cidr)
    }
}

/// Whether PostgreSQL assigns a value of `from` to a column of `to` without an explicit cast: its
/// own type, json and jsonb either way, inet into cidr and cidr into inet.
pub fn assignable(from: Textual, to: Textual) -> bool {
    from == to
        || matches!(
            (from, to),
            (Textual::Json, Textual::Jsonb)
                | (Textual::Jsonb, Textual::Json)
                | (Textual::Inet, Textual::Cidr)
                | (Textual::Cidr, Textual::Inet)
        )
}

/// A value of `from`, as the text the snail holds, cast to `to` as PostgreSQL's cast between them
/// takes it: json to jsonb by jsonb's input, jsonb to json as its text, inet to cidr by
/// `inet_to_cidr`, cidr to inet as inet writes it.
pub fn cast(from: Textual, to: Textual, held: &str) -> Result<String> {
    if from == to {
        return Ok(held.to_string());
    }
    match (from, to) {
        (Textual::Json, Textual::Jsonb) => text_in(Textual::Jsonb, held),
        (Textual::Jsonb, Textual::Json) => Ok(held.to_string()),
        (Textual::Inet, Textual::Cidr) => crate::inet::inet_to_cidr(held),
        (Textual::Cidr, Textual::Inet) => Ok(crate::inet::cidr_to_inet(held)),
        _ => Err(Error::new(format!(
            "cannot cast type {} to {}",
            from.type_name(),
            to.type_name()
        ))),
    }
}

/// A value held as its text cast to text, as PostgreSQL's casts take it: inet's and cidr's by
/// `network_show`, the mask always written; xml's stored text, by its binary cast; a node tree's as
/// it is; any other's as its output function writes it.
pub fn text_cast(ty: crate::catalog::ColumnType, held: &str) -> String {
    match ty {
        crate::catalog::ColumnType::Textual(t) if t.is_network() => crate::inet::inet_text(held),
        crate::catalog::ColumnType::Textual(Textual::Xml) => held.to_string(),
        crate::catalog::ColumnType::Textual(t) => text_out(t, held).into_owned(),
        _ => held.to_string(),
    }
}

/// Two values held as their text compared as PostgreSQL compares them: of one type, or inet beside
/// cidr, by their classes' keys; refused where PostgreSQL has no comparison between them.
pub fn compare(
    a: crate::catalog::ColumnType,
    x: &str,
    b: crate::catalog::ColumnType,
    y: &str,
) -> Result<std::cmp::Ordering> {
    use crate::catalog::ColumnType;
    let kin = match (a, b) {
        (ColumnType::Textual(s), ColumnType::Textual(t)) if s == Textual::Aclitem => s == t,
        (ColumnType::Textual(s), ColumnType::Textual(t)) => {
            s.ordered() && (s == t || s.is_network() && t.is_network())
        }
        (ColumnType::Internal(s), ColumnType::Internal(t)) => s.ordered() && s == t,
        _ => false,
    };
    if !kin {
        return Err(Error::new(format!(
            "operator does not exist: {} = {}",
            a.pg_name(),
            b.pg_name()
        )));
    }
    Ok(crate::strings::key_bytes(a, x.as_bytes()).cmp(&crate::strings::key_bytes(b, y.as_bytes())))
}

impl Textual {
    /// The type a quoted literal beside a value of it is read as: inet beside a cidr, as
    /// PostgreSQL takes the comparison through inet's operators; its own otherwise.
    pub fn literal_type(self) -> Textual {
        match self {
            Textual::Cidr => Textual::Inet,
            t => t,
        }
    }
}

/// The privilege letters `aclitemout` writes, a bit each in this order: PostgreSQL 19's
/// ACL_ALL_RIGHTS_STR.
const ACL_RIGHTS: &[u8] = b"arwdDxtXUCTcsAm";

/// The superuser PostgreSQL's `aclparse` takes for a grantor left out.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// An aclitem's role, as `getid` reads one: white space passed over, a name quoted or of letters,
/// digits and underscores. The snail names no role: one written as its OID's decimal digits is
/// that OID, and any name is refused by name.
fn acl_role<'a>(s: &'a str, empty_ok: bool) -> Result<(Option<u32>, &'a str)> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let mut name = String::new();
    let mut quoted_any = false;
    let mut in_quotes = false;
    let mut rest = s;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            quoted_any = true;
            if !in_quotes {
                in_quotes = true;
                rest = &s[i + 1..];
                continue;
            }
            if chars.peek().map(|&(_, n)| n) != Some('"') {
                in_quotes = false;
                rest = &s[i + 1..];
                continue;
            }
            chars.next();
        } else if !in_quotes && !(c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii()) {
            rest = &s[i..];
            break;
        }
        name.push(c);
        rest = &s[i + c.len_utf8()..];
    }
    if name.len() >= 64 {
        return Err(Error::new("identifier too long"));
    }
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if name.is_empty() {
        return if empty_ok {
            Ok((None, rest))
        } else {
            Err(Error::new("a name must follow the \"/\" sign"))
        };
    }
    match name.parse::<u32>() {
        Ok(oid) if !quoted_any && name.bytes().all(|b| b.is_ascii_digit()) => Ok((Some(oid), rest)),
        _ => Err(Error::new(format!(
            "role \"{name}\": a role is the server's own, and the snail names none"
        ))),
    }
}

/// An aclitem read as `aclitemin` reads one, its roles as the snail spells them: its grantee (0
/// for PUBLIC), its grantor and its privileges, the grant options in the high 32 bits.
pub fn aclitem_parse(s: &str) -> Result<(u32, u32, u64)> {
    let (grantee, mut rest) = acl_role(s, true)?;
    if !rest.starts_with('=') {
        return Err(Error::new("missing \"=\" sign"));
    }
    rest = &rest[1..];
    let (mut privs, mut goptions, mut read) = (0u64, 0u64, 0u64);
    while let Some(c) = rest
        .bytes()
        .next()
        .filter(|c| c.is_ascii_alphabetic() || *c == b'*')
    {
        if c == b'*' {
            goptions |= read;
        } else {
            let bit = ACL_RIGHTS.iter().position(|&r| r == c).ok_or_else(|| {
                Error::new(format!(
                    "invalid mode character: must be one of \"{}\"",
                    std::str::from_utf8(ACL_RIGHTS).unwrap_or_default()
                ))
            })?;
            read = 1 << bit;
        }
        privs |= read;
        rest = &rest[1..];
    }
    let grantor = match rest.strip_prefix('/') {
        Some(r) => {
            let (g, r) = acl_role(r, false)?;
            rest = r;
            g.unwrap_or(BOOTSTRAP_SUPERUSER)
        }
        None => BOOTSTRAP_SUPERUSER,
    };
    if !rest
        .trim_start_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return Err(Error::new(
            "extra garbage at the end of the ACL specification",
        ));
    }
    Ok((grantee.unwrap_or(0), grantor, privs | goptions << 32))
}

/// Whether an aclitem array of `dimensions` dimensions holds `item` as `aclcontains` finds it: an
/// element of the same grantee and grantor whose privileges and grant options hold all of
/// `item`'s. Refused where the array is not of one dimension or holds a NULL, as `check_acl`
/// refuses it.
pub fn aclcontains(
    elements: &[crate::composed::Item],
    dimensions: usize,
    item: &str,
) -> Result<bool> {
    use crate::composed::Item;
    use crate::read::statement::Datum;
    if dimensions != 1 {
        return Err(Error::new("ACL arrays must be one-dimensional"));
    }
    let held: Vec<&str> = elements
        .iter()
        .map(|e| match e {
            Item::Scalar(Datum::Text(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()
        .ok_or_else(|| Error::new("ACL arrays must not contain null values"))?;
    let (grantee, grantor, rights) = aclitem_parse(item)?;
    for s in held {
        let (g, r, held) = aclitem_parse(s)?;
        if g == grantee && r == grantor && held & rights == rights {
            return Ok(true);
        }
    }
    Ok(false)
}

/// An aclitem as `aclitemout` writes it where no role is named: its grantee's OID (none for
/// PUBLIC), `=`, each privilege's letter and `*` where it is granted with its option, `/`, its
/// grantor's OID.
pub fn aclitem_text(grantee: u32, grantor: u32, privileges: u64) -> String {
    let mut out = String::new();
    if grantee != 0 {
        out.push_str(&grantee.to_string());
    }
    out.push('=');
    for (i, &c) in ACL_RIGHTS.iter().enumerate() {
        if privileges & (1 << i) != 0 {
            out.push(c as char);
        }
        if privileges & (1 << (i + 32)) != 0 {
            out.push('*');
        }
    }
    out.push('/');
    out.push_str(&grantor.to_string());
    out
}

/// A text[] read as `array_in` reads one of one dimension: `{`, its elements split on commas, each
/// quoted with `"` or bare, a backslash taking the character after it as it is, a bare `NULL` in any
/// case none, white space around a bare element passed over, then `}`.
pub fn text_array_in(s: &str) -> Result<Vec<Option<String>>> {
    let wrong = || Error::new(format!("malformed array literal: \"{s}\""));
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let body = t
        .strip_prefix('{')
        .and_then(|b| b.strip_suffix('}'))
        .ok_or_else(wrong)?;
    let mut out = Vec::new();
    if body
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
    {
        return Ok(out);
    }
    let mut chars = body.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        let mut element = String::new();
        let mut quoted = false;
        let mut escaped_any = false;
        if chars.peek() == Some(&'"') {
            quoted = true;
            chars.next();
            loop {
                match chars.next().ok_or_else(wrong)? {
                    '"' => break,
                    '\\' => element.push(chars.next().ok_or_else(wrong)?),
                    c => element.push(c),
                }
            }
            while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                chars.next();
            }
        } else {
            let mut trailing = String::new();
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                if matches!(c, '{' | '}' | '"') {
                    return Err(wrong());
                }
                chars.next();
                if c == '\\' {
                    element.push_str(&trailing);
                    trailing.clear();
                    element.push(chars.next().ok_or_else(wrong)?);
                    escaped_any = true;
                } else if c.is_ascii_whitespace() {
                    trailing.push(c);
                } else {
                    element.push_str(&trailing);
                    trailing.clear();
                    element.push(c);
                }
            }
            if element.is_empty() && !escaped_any {
                return Err(wrong());
            }
        }
        out.push(
            (!(!quoted && !escaped_any && element.eq_ignore_ascii_case("NULL"))).then_some(element),
        );
        match chars.next() {
            None => return Ok(out),
            Some(',') => continue,
            Some(_) => return Err(wrong()),
        }
    }
}

/// Text values written as `array_out` writes a text[] of one dimension, each quoted.
pub fn text_array_out(elements: &[Option<String>]) -> String {
    let quoted: Vec<String> = elements
        .iter()
        .map(|e| match e {
            None => "NULL".to_string(),
            Some(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        })
        .collect();
    format!("{{{}}}", quoted.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_read_as_parse_snapshot_reads_it_and_written_as_pg_snapshot_out() {
        // PostgreSQL 18.6's answers on 28818, 2026-10-30: `SELECT '<spelling>'::pg_snapshot`.
        let taken = [
            ("10:20:12,12,15", "10:20:12,15"),
            (" 010:+20: 12,", "10:20:12"),
            ("4294967297:4294967297:", "4294967297:4294967297:"),
            ("-5:-1:", "18446744073709551611:18446744073709551615:"),
            ("1:1:", "1:1:"),
            (
                "18446744073709551616:18446744073709551617:",
                "18446744073709551615:18446744073709551615:",
            ),
            ("10:20:12,15,", "10:20:12,15"),
            ("1:2:+1", "1:2:1"),
            ("1:3:2,2,2", "1:3:2"),
        ];
        for (s, out) in taken {
            assert_eq!(text_in(Textual::PgSnapshot, s).unwrap(), out, "{s}");
            assert_eq!(text_in(Textual::TxidSnapshot, s).unwrap(), out, "{s}");
        }
        let refused = [
            "10:20:12,,15",
            "10:20:12 ",
            "10:20:5",
            "10:20:20",
            "4294967296:4294967297:",
            "-1:2:",
            "10:20:-11",
            ":5:",
            "10:20",
        ];
        for s in refused {
            let e = text_in(Textual::PgSnapshot, s).unwrap_err().to_string();
            assert!(
                e.contains("invalid input syntax for type pg_snapshot"),
                "{s}: {e}"
            );
        }
    }
}
