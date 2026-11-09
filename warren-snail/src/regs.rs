//! The reg* types: an oid that PostgreSQL prints and reads as the name of the object it names. The
//! snail carries the built-in objects of each release from 14 to 19, the OIDs below 12000 each
//! release fixes the same on every machine (`regs.txt`), and names them by the release the file's
//! OIDs came from; the file's own objects it names by the names its DDL gave them (`crate::oids`).
//! Any other OID, every role's among them, prints bare, and a name neither holds is refused.

use crate::{Error, Result};
use std::collections::HashMap;
use std::sync::OnceLock;

/// The releases whose built-in objects the snail carries.
pub const RELEASES: std::ops::RangeInclusive<u8> = 14..=19;

/// The release a file's OIDs came from unless it names another: the newest.
pub const LATEST: u8 = 19;

/// The OIDs below this one each release fixes the same on every machine (`FirstUnpinnedObjectId`);
/// initdb hands out those from it on as it goes, so they differ from machine to machine.
pub const FIXED_BELOW: u32 = 12000;

/// Which catalog a reg* type names its objects from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegKind {
    Proc,
    Procedure,
    Oper,
    Operator,
    Class,
    Type,
    Role,
    Namespace,
    Collation,
    Config,
    Dictionary,
    Database,
}

impl RegKind {
    pub const ALL: [RegKind; 12] = [
        RegKind::Proc,
        RegKind::Procedure,
        RegKind::Oper,
        RegKind::Operator,
        RegKind::Class,
        RegKind::Type,
        RegKind::Role,
        RegKind::Namespace,
        RegKind::Collation,
        RegKind::Config,
        RegKind::Dictionary,
        RegKind::Database,
    ];

    /// The type's name in PostgreSQL.
    pub fn type_name(self) -> &'static str {
        match self {
            RegKind::Proc => "regproc",
            RegKind::Procedure => "regprocedure",
            RegKind::Oper => "regoper",
            RegKind::Operator => "regoperator",
            RegKind::Class => "regclass",
            RegKind::Type => "regtype",
            RegKind::Role => "regrole",
            RegKind::Namespace => "regnamespace",
            RegKind::Collation => "regcollation",
            RegKind::Config => "regconfig",
            RegKind::Dictionary => "regdictionary",
            RegKind::Database => "regdatabase",
        }
    }

    /// The kind whose type PostgreSQL names `name`.
    pub fn of_type_name(name: &str) -> Option<RegKind> {
        RegKind::ALL.into_iter().find(|k| k.type_name() == name)
    }

    /// The first release that has the type: regdatabase is 19's.
    pub fn since(self) -> u8 {
        match self {
            RegKind::Database => 19,
            _ => 14,
        }
    }

    /// Its section of the carried catalog: none for roles, which the snail never names.
    fn section(self) -> Option<&'static str> {
        Some(match self {
            RegKind::Proc => "proc",
            RegKind::Procedure => "procedure",
            RegKind::Oper => "oper",
            RegKind::Operator => "operator",
            RegKind::Class => "class",
            RegKind::Type => "type",
            RegKind::Role => return None,
            RegKind::Namespace => "namespace",
            RegKind::Collation => "collation",
            RegKind::Config => "config",
            RegKind::Dictionary => "dictionary",
            RegKind::Database => "database",
        })
    }

    /// What its objects are called in PostgreSQL's messages.
    fn object(self) -> &'static str {
        match self {
            RegKind::Proc | RegKind::Procedure => "function",
            RegKind::Oper | RegKind::Operator => "operator",
            RegKind::Class => "relation",
            RegKind::Type => "type",
            RegKind::Role => "role",
            RegKind::Namespace => "schema",
            RegKind::Collation => "collation",
            RegKind::Config => "text search configuration",
            RegKind::Dictionary => "text search dictionary",
            RegKind::Database => "database",
        }
    }
}

/// Whether PostgreSQL casts one reg* type to another: a function's two and an operator's two, each
/// to the other, binary-coercible.
pub fn casts_between(a: RegKind, b: RegKind) -> bool {
    use RegKind::*;
    matches!(
        (a, b),
        (Proc, Procedure) | (Procedure, Proc) | (Oper, Operator) | (Operator, Oper)
    )
}

/// Refuses a release the snail carries no catalog for, or one a kind is not yet in.
pub fn check_release(kind: Option<RegKind>, release: u8) -> Result<()> {
    if !RELEASES.contains(&release) {
        return Err(Error::new(format!(
            "PostgreSQL {release}: the snail carries the built-in objects of {} to {} only",
            RELEASES.start(),
            RELEASES.end()
        )));
    }
    match kind {
        Some(k) if release < k.since() => Err(Error::new(format!(
            "{} is PostgreSQL {}'s, and this file's OIDs came from {release}",
            k.type_name(),
            k.since()
        ))),
        _ => Ok(()),
    }
}

/// One row of the carried catalog: an OID, the releases that print it so, a bit a release from 14,
/// and its text.
struct Row {
    oid: u32,
    releases: u8,
    text: &'static str,
}

/// The releases a row's header names: all of them, or ranges such as `14-15,17`.
pub(crate) fn releases_of(spec: Option<&str>) -> u8 {
    let bit = |r: u8| 1u8 << (r - RELEASES.start());
    match spec {
        None => RELEASES.map(bit).fold(0, |a, b| a | b),
        Some(s) => s
            .split(',')
            .map(|part| match part.split_once('-') {
                Some((a, b)) => {
                    let (a, b): (u8, u8) = (a.parse().unwrap(), b.parse().unwrap());
                    (a..=b).map(bit).fold(0, |x, y| x | y)
                }
                None => bit(part.parse().unwrap()),
            })
            .fold(0, |a, b| a | b),
    }
}

/// The carried catalog, each section's rows by OID.
fn carried() -> &'static HashMap<&'static str, Vec<Row>> {
    static CARRIED: OnceLock<HashMap<&'static str, Vec<Row>>> = OnceLock::new();
    CARRIED.get_or_init(|| {
        let mut sections: HashMap<&'static str, Vec<Row>> = HashMap::new();
        let mut at = "";
        for line in include_str!("regs.txt").lines() {
            if let Some(name) = line.strip_prefix("= ") {
                at = name;
                continue;
            }
            let (head, text) = line
                .split_once('\t')
                .expect("a row of regs.txt holds a tab");
            let (oid, spec) = match head.split_once(' ') {
                Some((o, s)) => (o, Some(s)),
                None => (head, None),
            };
            sections.entry(at).or_default().push(Row {
                oid: oid.parse().expect("a row of regs.txt begins with an OID"),
                releases: releases_of(spec),
                text,
            });
        }
        sections
    })
}

/// A section's text for an OID under a release, where the carried catalog holds one.
fn carried_text(section: &str, oid: u32, release: u8) -> Option<&'static str> {
    let rows = carried().get(section)?;
    let bit = 1u8 << release.checked_sub(*RELEASES.start())?;
    let from = rows.partition_point(|r| r.oid < oid);
    rows[from..]
        .iter()
        .take_while(|r| r.oid == oid)
        .find(|r| r.releases & bit != 0)
        .map(|r| r.text)
}

/// Every row of a section under a release: its OID and text.
fn carried_rows(section: &str, release: u8) -> impl Iterator<Item = (u32, &'static str)> + '_ {
    let bit = 1u8 << (release - RELEASES.start());
    carried()
        .get(section)
        .into_iter()
        .flatten()
        .filter(move |r| r.releases & bit != 0)
        .map(|r| (r.oid, r.text))
}

/// An OID of `kind` as PostgreSQL's output function writes it under `release`: a built-in object's
/// name from the carried catalog, an object of the file held its own name; 0 as `-`, or `0` for an
/// operator; any other OID bare.
pub fn reg_text(kind: RegKind, oid: u32, release: u8) -> String {
    if oid == 0 {
        return if matches!(kind, RegKind::Oper | RegKind::Operator) {
            "0".into()
        } else {
            "-".into()
        };
    }
    if oid >= crate::oids::FIRST {
        if let Ok(Some(o)) = crate::oids::held_object(oid) {
            if let Some(t) = file_text(kind, &o, release) {
                return t;
            }
        }
    }
    kind.section()
        .and_then(|s| carried_text(s, oid, release))
        .map_or_else(|| oid.to_string(), str::to_string)
}

/// A file's object named as PostgreSQL's output functions name it: bare where the search path
/// finds it by its name alone, `public` searched after `pg_catalog`; otherwise qualified by its
/// schema. None where it is no object of `kind`.
fn file_text(kind: RegKind, o: &crate::oids::Object, release: u8) -> Option<String> {
    use crate::oids::Object as O;
    let q = crate::read::calls::quote_ident;
    let n = names(release);
    let shown = |name: &str, shadowed: bool| -> String {
        let (schema, own) = crate::oids::parts(name);
        if schema == "public" && !shadowed {
            q(own)
        } else {
            format!("{}.{}", q(schema), q(own))
        }
    };
    let own = |name: &str| crate::oids::parts(name).1.to_string();
    let in_catalog = |section: &'static str, name: &str| {
        n.named
            .contains_key(&(section, "pg_catalog".to_string(), own(name)))
    };
    Some(match (kind, o) {
        (RegKind::Class, O::Class(name)) => shown(name, in_catalog("class", name)),
        (RegKind::Type, O::Type(name)) => shown(name, n.typnames.contains_key(&own(name))),
        (RegKind::Type, O::Array(name)) => {
            format!("{}[]", shown(name, n.typnames.contains_key(&own(name))))
        }
        (RegKind::Namespace, O::Namespace(name)) => q(name),
        (RegKind::Collation, O::Collation(name)) => shown(name, in_catalog("collation", name)),
        (RegKind::Proc | RegKind::Procedure, O::Proc(name, args)) => {
            // regproc qualifies a name the search path finds more than one function of.
            let several = crate::oids::held_each(|oids| {
                oids.each()
                    .filter(|(_, x)| matches!(x, O::Proc(m, _) if m == name))
                    .count()
                    > 1
            })
            .unwrap_or(false);
            let shadowed = n.shared.contains_key(&("proc", own(name)));
            match kind {
                RegKind::Proc => shown(name, shadowed || several),
                _ => format!("{}({args})", shown(name, shadowed)),
            }
        }
        _ => return None,
    })
}

/// A name the carried catalog does not hold, looked for among the held file's objects: in its
/// schema, or in `public` where it names none, as the search path finds it after `pg_catalog`. A
/// schema that stands nowhere is refused first, as PostgreSQL's lookup of every kind of name but a
/// relation's refuses it.
fn file_named(
    parts: &[String],
    make: impl Fn(String) -> crate::oids::Object,
) -> Result<Option<u32>> {
    let name = match parts {
        [n] => format!("public.{n}"),
        [s, n] if s != "pg_catalog" => format!("{s}.{n}"),
        _ => return Ok(None),
    };
    let made = make(name);
    if let ([s, _], false) = (parts, matches!(made, crate::oids::Object::Class(_))) {
        schema_found(s)?;
    }
    crate::oids::held_oid(&made)
}

/// Refuses a name in a schema that stands nowhere, as PostgreSQL's lookup of a type's, a function's
/// or a collation's name does, where a file is held to say which of its schemas stand.
pub(crate) fn schema_found(schema: &str) -> Result<()> {
    if crate::oids::held_each(|_| ()).is_none() || schema_stands(schema)? {
        return Ok(());
    }
    Err(no_schema(schema))
}

/// Whether a schema stands for a name to be found in: one of the release's, `public` until the
/// held file drops it; or one the held file made.
fn schema_stands(schema: &str) -> Result<bool> {
    let dropped = crate::oids::held_each(|o| o.public_dropped()).unwrap_or(false);
    let release = matches!(schema, "pg_catalog" | "pg_toast" | "information_schema")
        || (schema == "public" && !dropped);
    Ok(release || crate::oids::held_oid(&crate::oids::Object::Namespace(schema.into()))?.is_some())
}

/// The refusal of a schema that stands nowhere, in PostgreSQL's words.
fn no_schema(schema: &str) -> Error {
    Error::new(format!("schema \"{schema}\" does not exist"))
}

/// White space as PostgreSQL's scanner takes it.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}

/// An identifier cut to the 63 bytes a name holds, at a character's end.
fn truncated(mut s: String) -> String {
    if s.len() > 63 {
        let mut at = 63;
        while !s.is_char_boundary(at) {
            at -= 1;
        }
        s.truncate(at);
    }
    s
}

/// A qualified name split as PostgreSQL's `SplitIdentifierString` splits it on dots: white space
/// around each part passed over, a quoted part kept as written with `""` as one quote, an unquoted
/// one lower-cased in ASCII; each cut to 63 bytes. None where the syntax is wrong or nothing is
/// named.
pub fn split_name(s: &str) -> Option<Vec<String>> {
    let b = s.as_bytes();
    let mut at = 0;
    while at < b.len() && is_space(b[at]) {
        at += 1;
    }
    if at == b.len() {
        return None;
    }
    let mut parts = Vec::new();
    loop {
        let part = if b[at] == b'"' {
            let mut name = Vec::new();
            at += 1;
            loop {
                let close = at + b[at..].iter().position(|&c| c == b'"')?;
                name.extend_from_slice(&b[at..close]);
                if b.get(close + 1) == Some(&b'"') {
                    name.push(b'"');
                    at = close + 2;
                } else {
                    at = close + 1;
                    break;
                }
            }
            String::from_utf8(name).ok()?
        } else {
            let start = at;
            while at < b.len() && b[at] != b'.' && !is_space(b[at]) {
                at += 1;
            }
            if at == start {
                return None;
            }
            s[start..at].to_ascii_lowercase()
        };
        parts.push(truncated(part));
        while at < b.len() && is_space(b[at]) {
            at += 1;
        }
        match b.get(at) {
            None => return Some(parts),
            Some(b'.') => {
                at += 1;
                while at < b.len() && is_space(b[at]) {
                    at += 1;
                }
            }
            Some(_) => return None,
        }
    }
}

/// What the carried catalog of one release answers names by.
#[derive(Default)]
struct Names {
    /// (section, schema, name) to its OID: the objects of one schema each name is unique in.
    named: HashMap<(&'static str, String, String), u32>,
    /// A function's or an operator's name to its OID, or none where several share it.
    shared: HashMap<(&'static str, String), Option<u32>>,
    /// (section, name, the argument types as their texts) to a function's or an operator's OID.
    signed: HashMap<(&'static str, String, String), u32>,
    /// A type's name to its OID, and each type's array.
    typnames: HashMap<String, u32>,
    arrays: HashMap<u32, u32>,
}

/// The name and the argument types of a function's or an operator's text, as `name(a,b)`.
fn signature(text: &str) -> Option<(String, String)> {
    let mut quoted = false;
    let open = text.bytes().position(|c| {
        if c == b'"' {
            quoted = !quoted;
        }
        c == b'(' && !quoted
    })?;
    let args = text[open + 1..].strip_suffix(')')?;
    Some((text[..open].to_string(), args.to_string()))
}

fn names(release: u8) -> &'static Names {
    static NAMES: OnceLock<Vec<Names>> = OnceLock::new();
    let all = NAMES.get_or_init(|| {
        RELEASES
            .map(|r| {
                let mut n = Names::default();
                for section in [
                    "class",
                    "collation",
                    "config",
                    "dictionary",
                    "namespace",
                    "database",
                ] {
                    for (oid, text) in carried_rows(section, r) {
                        let mut parts = split_name(text).expect("a carried name splits");
                        let name = parts.pop().unwrap();
                        let schema = parts.pop().unwrap_or_else(|| "pg_catalog".into());
                        n.named.insert((section, schema, name), oid);
                    }
                }
                for section in ["proc", "oper"] {
                    for (oid, text) in carried_rows(section, r) {
                        let parts = if section == "oper" {
                            text.split_once('.')
                                .map_or(vec![text.to_string()], |(s, o)| {
                                    vec![split_name(s).unwrap().remove(0), o.to_string()]
                                })
                        } else {
                            split_name(text).expect("a carried name splits")
                        };
                        let name = parts.last().unwrap().clone();
                        let e = n.shared.entry((section, name)).or_insert(Some(oid));
                        if parts.len() > 1 {
                            *e = None;
                        }
                    }
                }
                for section in ["procedure", "operator"] {
                    for (oid, text) in carried_rows(section, r) {
                        let (name, args) = signature(text).expect("a carried signature");
                        let name = if section == "procedure" {
                            split_name(&name).unwrap().pop().unwrap()
                        } else {
                            name
                        };
                        n.signed.insert((section, name, args), oid);
                    }
                }
                for (oid, text) in carried_rows("typname", r) {
                    n.typnames.insert(text.to_string(), oid);
                }
                for (oid, text) in carried_rows("array", r) {
                    n.arrays.insert(oid, text.parse().unwrap());
                }
                n
            })
            .collect()
    });
    &all[(release - RELEASES.start()) as usize]
}

/// The refusal of a name neither the carried catalog nor the file holds.
fn unknown(kind: RegKind, name: &str, release: u8) -> Error {
    Error::new(format!(
        "{} \"{name}\" does not exist, among PostgreSQL {release}'s built-in objects and the file's \
         own",
        kind.object()
    ))
}

/// A value of `kind` read as PostgreSQL's input function reads it, under the catalog of `release`:
/// `-` as 0 (to regoper and regoperator an operator's name), all digits as an oid, any other text as
/// a built-in object's name.
pub fn reg_parse(kind: RegKind, s: &str, release: u8) -> Result<u32> {
    check_release(Some(kind), release)?;
    let operator = matches!(kind, RegKind::Oper | RegKind::Operator);
    if s == "-" && !operator {
        return Ok(0);
    }
    if s.as_bytes().first().is_some_and(u8::is_ascii_digit) && s.bytes().all(|c| c.is_ascii_digit())
    {
        return Ok(crate::scalars::unsigned_in(s, 32, "oid")? as u32);
    }
    let n = names(release);
    let syntax = || Error::new("invalid name syntax");
    match kind {
        RegKind::Type => type_oid(s, release),
        RegKind::Procedure | RegKind::Operator => {
            let (parts, args) = name_and_args(s, operator, release)?;
            if !operator {
                // A function of the file's, by its name and its arguments' types.
                let texts: Option<Vec<String>> = args
                    .iter()
                    .map(|a| a.map(|t| reg_text(RegKind::Type, t, release)))
                    .collect();
                if let Some(texts) = texts {
                    let joined = texts.join(",");
                    if let Some(oid) =
                        file_named(&parts, |n| crate::oids::Object::Proc(n, joined.clone()))?
                    {
                        return Ok(oid);
                    }
                }
            }
            let name = in_catalog(&parts).ok_or_else(|| unknown(kind, s, release))?;
            if operator {
                if args.len() == 1 {
                    return Err(Error::new("missing argument"));
                }
                if args.len() != 2 {
                    return Err(Error::new("too many arguments"));
                }
            }
            let texts: Vec<String> = args
                .iter()
                .map(|a| match a {
                    None => Some("NONE".to_string()),
                    Some(t) => carried_text("type", *t, release).map(str::to_string),
                })
                .collect::<Option<_>>()
                .ok_or_else(|| unknown(kind, s, release))?;
            let section = if operator { "operator" } else { "procedure" };
            n.signed
                .get(&(section, name, texts.join(",")))
                .copied()
                .ok_or_else(|| unknown(kind, s, release))
        }
        RegKind::Proc | RegKind::Oper => {
            let parts = split_name(s).ok_or_else(syntax)?;
            let section = if operator { "oper" } else { "proc" };
            let carried = in_catalog(&parts).and_then(|name| n.shared.get(&(section, name)));
            match carried {
                Some(Some(oid)) => Ok(*oid),
                Some(None) if operator => {
                    Err(Error::new(format!("more than one operator named {s}")))
                }
                Some(None) => Err(Error::new(format!("more than one function named \"{s}\""))),
                None if !operator => match file_procs(&parts)?.as_slice() {
                    [oid] => Ok(*oid),
                    [] => Err(unknown(kind, s, release)),
                    _ => Err(Error::new(format!("more than one function named \"{s}\""))),
                },
                None => Err(unknown(kind, s, release)),
            }
        }
        RegKind::Role => {
            let parts = split_name(s).ok_or_else(syntax)?;
            if parts.len() != 1 {
                return Err(syntax());
            }
            Err(Error::new(format!(
                "role \"{}\": a role is the server's own, and the snail names none",
                parts[0]
            )))
        }
        RegKind::Namespace | RegKind::Database => {
            let parts = split_name(s).ok_or_else(syntax)?;
            if parts.len() != 1 {
                return Err(syntax());
            }
            let section = kind.section().unwrap();
            if kind == RegKind::Namespace && parts[0] == "public" {
                // public as the held file has it: the release's, or the one made again.
                if let Some(public) = crate::oids::held_each(|o| o.public()) {
                    return public.ok_or_else(|| no_schema("public"));
                }
            }
            if let Some(oid) = n
                .named
                .get(&(section, "pg_catalog".to_string(), parts[0].clone()))
            {
                return Ok(*oid);
            }
            if kind == RegKind::Namespace {
                if let Some(oid) =
                    crate::oids::held_oid(&crate::oids::Object::Namespace(parts[0].clone()))?
                {
                    return Ok(oid);
                }
                return Err(no_schema(&parts[0]));
            }
            Err(unknown(kind, &parts[0], release))
        }
        RegKind::Class | RegKind::Collation | RegKind::Config | RegKind::Dictionary => {
            qualified(kind, s, release)
        }
    }
}

/// An object named by a name its schema may qualify, unqualified found in pg_catalog, where every
/// built-in one the search path finds lies.
fn qualified(kind: RegKind, s: &str, release: u8) -> Result<u32> {
    let mut parts = split_name(s).ok_or_else(|| Error::new("invalid name syntax"))?;
    let shown = parts.join(".");
    if parts.len() > 2 {
        return Err(unknown(kind, &shown, release));
    }
    let given = parts.clone();
    let name = parts.pop().unwrap();
    let schema = parts.pop().unwrap_or_else(|| "pg_catalog".into());
    if let Some(oid) = names(release)
        .named
        .get(&(kind.section().unwrap(), schema, name))
    {
        return Ok(*oid);
    }
    let made = match kind {
        RegKind::Class => file_named(&given, crate::oids::Object::Class)?,
        RegKind::Collation => file_named(&given, crate::oids::Object::Collation)?,
        _ => None,
    };
    made.ok_or_else(|| unknown(kind, &shown, release))
}

/// The file's functions of a name, in its schema or in `public`.
fn file_procs(parts: &[String]) -> Result<Vec<u32>> {
    let name = match parts {
        [n] => format!("public.{n}"),
        [s, n] if s != "pg_catalog" => format!("{s}.{n}"),
        _ => return Ok(Vec::new()),
    };
    // Several snails name no object of theirs.
    crate::oids::held_oid(&crate::oids::Object::Namespace(String::new()))?;
    if let [s, _] = parts {
        schema_found(s)?;
    }
    Ok(crate::oids::held_each(|o| {
        o.each()
            .filter(|(_, x)| matches!(x, crate::oids::Object::Proc(m, _) if *m == name))
            .map(|(oid, _)| oid)
            .collect()
    })
    .unwrap_or_default())
}

/// A text cast to a regclass, as PostgreSQL's `text_regclass` reads it: a relation's name alone,
/// never `-` or a number.
pub fn class_of_text(s: &str, release: u8) -> Result<u32> {
    qualified(RegKind::Class, s, release)
}

/// A function's or an operator's name as the carried catalog names it: unqualified, or qualified
/// by pg_catalog, where every built-in function and operator lies.
fn in_catalog(parts: &[String]) -> Option<String> {
    match parts {
        [name] => Some(name.clone()),
        [schema, name] if schema == "pg_catalog" => Some(name.clone()),
        _ => None,
    }
}

/// A function's or an operator's name and its argument types, as PostgreSQL's
/// `parseNameAndArgTypes` reads them: `name(type, …)`, `NONE` for an operator's missing side.
fn name_and_args(s: &str, operator: bool, release: u8) -> Result<(Vec<String>, Vec<Option<u32>>)> {
    let wrong = |m: &str| Error::new(m.to_string());
    let b = s.as_bytes();
    let mut quoted = false;
    let open = b
        .iter()
        .position(|&c| {
            if c == b'"' {
                quoted = !quoted;
            }
            c == b'(' && !quoted
        })
        .ok_or_else(|| wrong("expected a left parenthesis"))?;
    let parts = split_name(&s[..open]).ok_or_else(|| wrong("invalid name syntax"))?;
    let rest = s[open + 1..].trim_end_matches(|c: char| c.is_ascii() && is_space(c as u8));
    let inner = rest
        .strip_suffix(')')
        .ok_or_else(|| wrong("expected a right parenthesis"))?;
    let mut args = Vec::new();
    let ib = inner.as_bytes();
    let mut at = 0;
    let mut had_comma = false;
    loop {
        while at < ib.len() && is_space(ib[at]) {
            at += 1;
        }
        if at == ib.len() {
            if had_comma {
                return Err(wrong("expected a type name"));
            }
            break;
        }
        let start = at;
        let (mut quoted, mut depth) = (false, 0i32);
        while at < ib.len() {
            match ib[at] {
                b'"' => quoted = !quoted,
                b',' if !quoted && depth == 0 => break,
                b'(' | b'[' if !quoted => depth += 1,
                b')' | b']' if !quoted => depth -= 1,
                _ => {}
            }
            at += 1;
        }
        if quoted || depth != 0 {
            return Err(wrong("improper type name"));
        }
        let name = inner[start..at].trim_end_matches(|c: char| c.is_ascii() && is_space(c as u8));
        had_comma = at < ib.len();
        if had_comma {
            at += 1;
        }
        args.push(if operator && name.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(type_oid(name, release)?)
        });
    }
    Ok((parts, args))
}

/// PostgreSQL's reserved keywords: never a type's name unquoted.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// PostgreSQL's keywords that may name a column but not a type unquoted, past those that begin a
/// type's own spelling.
const COLUMN_NAMES: &[&str] = &[
    "between",
    "coalesce",
    "exists",
    "extract",
    "greatest",
    "grouping",
    "inout",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "least",
    "merge_action",
    "none",
    "normalize",
    "nullif",
    "out",
    "overlay",
    "position",
    "precision",
    "row",
    "setof",
    "substring",
    "treat",
    "trim",
    "values",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// A name: lower-cased where unquoted, and whether it was quoted.
    Word(String, bool),
    Int(i64),
    Other(u8),
}

/// A type's name cut into words, numbers and marks as PostgreSQL's scanner cuts it.
fn tokens(s: &str) -> Result<Vec<Token>> {
    let invalid = || Error::new(format!("invalid type name \"{s}\""));
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let c = b[at];
        if is_space(c) {
            at += 1;
        } else if c == b'"' {
            let mut name = Vec::new();
            at += 1;
            loop {
                let close = at
                    + b[at..]
                        .iter()
                        .position(|&x| x == b'"')
                        .ok_or_else(invalid)?;
                name.extend_from_slice(&b[at..close]);
                if b.get(close + 1) == Some(&b'"') {
                    name.push(b'"');
                    at = close + 2;
                } else {
                    at = close + 1;
                    break;
                }
            }
            if name.is_empty() {
                return Err(Error::new("zero-length delimited identifier"));
            }
            out.push(Token::Word(
                truncated(String::from_utf8(name).map_err(|_| invalid())?),
                true,
            ));
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = at;
            while at < b.len()
                && (b[at].is_ascii_alphanumeric()
                    || b[at] == b'_'
                    || b[at] == b'$'
                    || b[at] >= 0x80)
            {
                at += 1;
            }
            out.push(Token::Word(
                truncated(s[start..at].to_ascii_lowercase()),
                false,
            ));
        } else if c.is_ascii_digit() {
            let start = at;
            while at < b.len() && b[at].is_ascii_digit() {
                at += 1;
            }
            if b.get(at)
                .is_some_and(|&x| x == b'.' || x.is_ascii_alphabetic())
            {
                return Err(Error::new(format!(
                    "{s}: a type modifier other than an integer is not read"
                )));
            }
            out.push(Token::Int(s[start..at].parse().map_err(|_| invalid())?));
        } else {
            out.push(Token::Other(c));
            at += 1;
        }
    }
    Ok(out)
}

/// A type's name read as PostgreSQL's `parseTypeString` reads it, under the catalog of `release`:
/// the SQL standard's spellings, a type's own name, qualified or not, its modifiers checked as its
/// type checks them, and array bounds. Its OID.
pub fn type_oid(s: &str, release: u8) -> Result<u32> {
    let toks = tokens(s)?;
    if toks.is_empty() {
        return Err(Error::new(format!("invalid type name \"{s}\"")));
    }
    let mut p = TypeReader {
        toks: &toks,
        at: 0,
        text: s,
        release,
    };
    let oid = p.type_name()?;
    if p.at != toks.len() {
        return Err(p.syntax());
    }
    Ok(oid)
}

struct TypeReader<'a> {
    toks: &'a [Token],
    at: usize,
    text: &'a str,
    release: u8,
}

/// The built-in types PostgreSQL's spellings name, by their OIDs.
mod oids {
    pub const BOOL: u32 = 16;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const JSON: u32 = 114;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
}

/// How a type checks its modifiers: the types that take any, and their arrays.
#[derive(Clone, Copy, PartialEq)]
enum Modifiers {
    Characters(&'static str),
    Bits(&'static str),
    Numeric,
    Precision,
    Interval,
}

fn modifiers_of(oid: u32, release: u8) -> Option<Modifiers> {
    use oids::*;
    let of = |t: u32| match t {
        BPCHAR => Some(Modifiers::Characters("char")),
        VARCHAR => Some(Modifiers::Characters("varchar")),
        BIT => Some(Modifiers::Bits("bit")),
        VARBIT => Some(Modifiers::Bits("varbit")),
        NUMERIC => Some(Modifiers::Numeric),
        TIME | TIMETZ | TIMESTAMP | TIMESTAMPTZ => Some(Modifiers::Precision),
        INTERVAL => Some(Modifiers::Interval),
        _ => None,
    };
    of(oid).or_else(|| {
        let n = names(release);
        n.arrays
            .iter()
            .find(|(_, &a)| a == oid)
            .and_then(|(&e, _)| of(e))
    })
}

impl TypeReader<'_> {
    fn syntax(&self) -> Error {
        Error::new(format!("syntax error in the type name \"{}\"", self.text))
    }

    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.at)
    }

    /// Whether the next token is the keyword `word`, unquoted; taken if so.
    fn keyword(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(w, false)) if w == word) {
            self.at += 1;
            return true;
        }
        false
    }

    fn mark(&mut self, c: u8) -> bool {
        if self.peek() == Some(&Token::Other(c)) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if self.mark(c) {
            Ok(())
        } else {
            Err(self.syntax())
        }
    }

    fn int(&mut self) -> Result<i64> {
        match self.peek() {
            Some(Token::Int(n)) => {
                let n = *n;
                self.at += 1;
                Ok(n)
            }
            _ => Err(self.syntax()),
        }
    }

    /// `(n)`, where it follows.
    fn length(&mut self) -> Result<Option<i64>> {
        if !self.mark(b'(') {
            return Ok(None);
        }
        let n = self.int()?;
        self.expect(b')')?;
        Ok(Some(n))
    }

    /// `(a, b, …)`, where it follows: each an integer, or one negated.
    fn modifier_list(&mut self) -> Result<Option<Vec<i64>>> {
        if !self.mark(b'(') {
            return Ok(None);
        }
        let mut list = Vec::new();
        loop {
            let negative = self.mark(b'-');
            let n = match self.peek() {
                Some(Token::Int(n)) => *n,
                Some(_) => {
                    return Err(Error::new(format!(
                        "{}: a type modifier other than an integer is not read",
                        self.text
                    )))
                }
                None => return Err(self.syntax()),
            };
            self.at += 1;
            list.push(if negative { -n } else { n });
            if self.mark(b')') {
                return Ok(Some(list));
            }
            self.expect(b',')?;
        }
    }

    fn type_name(&mut self) -> Result<u32> {
        if self.keyword("setof") {
            return Err(Error::new(format!("invalid type name \"{}\"", self.text)));
        }
        let oid = self.simple()?;
        let mut array = false;
        while self.mark(b'[') {
            if matches!(self.peek(), Some(Token::Int(_))) {
                self.at += 1;
            }
            self.expect(b']')?;
            array = true;
        }
        if !array && self.keyword("array") {
            if self.mark(b'[') {
                self.int()?;
                self.expect(b']')?;
            }
            array = true;
        }
        if !array {
            return Ok(oid);
        }
        if let Some(crate::oids::Object::Type(n)) = crate::oids::held_object(oid)? {
            if let Some(a) = crate::oids::held_oid(&crate::oids::Object::Array(n))? {
                return Ok(a);
            }
        }
        names(self.release)
            .arrays
            .get(&oid)
            .copied()
            .ok_or_else(|| {
                Error::new(format!(
                    "could not find array type for data type {}",
                    reg_text(RegKind::Type, oid, self.release)
                ))
            })
    }

    /// The type `oid`'s modifiers checked as its type checks them.
    fn modified(&self, oid: u32, list: &[i64]) -> Result<u32> {
        let invalid = || Error::new("invalid type modifier");
        match modifiers_of(oid, self.release) {
            None => Err(Error::new(format!(
                "type modifier is not allowed for type \"{}\"",
                reg_text(RegKind::Type, oid, self.release)
            ))),
            Some(Modifiers::Characters(name)) | Some(Modifiers::Bits(name)) => {
                let most = if matches!(modifiers_of(oid, self.release), Some(Modifiers::Bits(_))) {
                    10_485_760 * 8
                } else {
                    10_485_760
                };
                match list {
                    [n] if *n < 1 => Err(Error::new(format!(
                        "length for type {name} must be at least 1"
                    ))),
                    [n] if *n > most => Err(Error::new(format!(
                        "length for type {name} cannot exceed {most}"
                    ))),
                    [_] => Ok(oid),
                    _ => Err(invalid()),
                }
            }
            Some(Modifiers::Numeric) => {
                let precision = |p: i64| {
                    if (1..=1000).contains(&p) {
                        Ok(())
                    } else {
                        Err(Error::new(format!(
                            "NUMERIC precision {p} must be between 1 and 1000"
                        )))
                    }
                };
                match list {
                    [p] => precision(*p).map(|_| oid),
                    [p, s] => {
                        precision(*p)?;
                        if (-1000..=1000).contains(s) {
                            Ok(oid)
                        } else {
                            Err(Error::new(format!(
                                "NUMERIC scale {s} must be between -1000 and 1000"
                            )))
                        }
                    }
                    _ => Err(Error::new("invalid NUMERIC type modifier")),
                }
            }
            Some(Modifiers::Precision) => match list {
                [p] if *p < 0 => Err(Error::new(format!(
                    "{}({p}) precision must not be negative",
                    reg_text(RegKind::Type, oid, self.release)
                ))),
                [_] => Ok(oid),
                _ => Err(invalid()),
            },
            Some(Modifiers::Interval) => Err(Error::new(format!(
                "{}: an interval's modifiers written as a type's own are not read",
                self.text
            ))),
        }
    }

    fn simple(&mut self) -> Result<u32> {
        use oids::*;
        let (word, quoted) = match self.peek() {
            Some(Token::Word(w, q)) => (w.clone(), *q),
            _ => return Err(self.syntax()),
        };
        if !quoted {
            let at = self.at;
            self.at += 1;
            match word.as_str() {
                "int" | "integer" => return Ok(INT4),
                "smallint" => return Ok(INT2),
                "bigint" => return Ok(INT8),
                "real" => return Ok(FLOAT4),
                "boolean" => return Ok(BOOL),
                "json" => return Ok(JSON),
                "float" => {
                    return match self.length()? {
                        None => Ok(FLOAT8),
                        Some(p) if p < 1 => Err(Error::new(
                            "precision for type float must be at least 1 bit",
                        )),
                        Some(p) if p <= 24 => Ok(FLOAT4),
                        Some(p) if p <= 53 => Ok(FLOAT8),
                        Some(_) => Err(Error::new(
                            "precision for type float must be less than 54 bits",
                        )),
                    }
                }
                "double" if self.keyword("precision") => return Ok(FLOAT8),
                "decimal" | "dec" | "numeric" => {
                    return match self.modifier_list()? {
                        None => Ok(NUMERIC),
                        Some(list) => self.modified(NUMERIC, &list),
                    }
                }
                "bit" => {
                    let oid = if self.keyword("varying") { VARBIT } else { BIT };
                    return match self.modifier_list()? {
                        None => Ok(oid),
                        Some(list) => self.modified(oid, &list),
                    };
                }
                "character" | "char" | "nchar" | "national" | "varchar" => {
                    if word == "national" && !(self.keyword("character") || self.keyword("char")) {
                        return Err(self.syntax());
                    }
                    let oid = if word == "varchar" || self.keyword("varying") {
                        VARCHAR
                    } else {
                        BPCHAR
                    };
                    return match self.length()? {
                        None => Ok(oid),
                        Some(n) => self.modified(oid, &[n]),
                    };
                }
                "timestamp" | "time" => {
                    let precision = self.length()?;
                    let zoned = if self.keyword("with") {
                        true
                    } else if self.keyword("without") {
                        false
                    } else {
                        let oid = if word == "time" { TIME } else { TIMESTAMP };
                        return precision.map_or(Ok(oid), |p| self.modified(oid, &[p]));
                    };
                    if !(self.keyword("time") && self.keyword("zone")) {
                        return Err(self.syntax());
                    }
                    let oid = match (word.as_str(), zoned) {
                        ("time", false) => TIME,
                        ("time", true) => TIMETZ,
                        (_, false) => TIMESTAMP,
                        (_, true) => TIMESTAMPTZ,
                    };
                    return precision.map_or(Ok(oid), |p| self.modified(oid, &[p]));
                }
                "interval" => {
                    if self.length()?.is_some() {
                        return Ok(INTERVAL);
                    }
                    self.interval_fields()?;
                    return Ok(INTERVAL);
                }
                w if RESERVED.contains(&w) || COLUMN_NAMES.contains(&w) => {
                    return Err(self.syntax())
                }
                _ => self.at = at,
            }
        }
        // A type's own name, qualified or not, with any modifiers.
        let mut parts = vec![word];
        self.at += 1;
        while self.mark(b'.') {
            match self.peek() {
                Some(Token::Word(w, _)) => {
                    parts.push(w.clone());
                    self.at += 1;
                }
                _ => return Err(self.syntax()),
            }
        }
        if self.mark(b'%') {
            return Err(Error::new(format!(
                "{}: a type named by a column's %TYPE is not read",
                self.text
            )));
        }
        let shown = parts.join(".");
        let carried = match parts.as_slice() {
            [name] => names(self.release).typnames.get(name).copied(),
            [schema, name] if schema == "pg_catalog" => {
                names(self.release).typnames.get(name).copied()
            }
            _ => None,
        };
        let oid = match carried {
            Some(oid) => oid,
            None => file_named(&parts, crate::oids::Object::Type)?
                .ok_or_else(|| unknown(RegKind::Type, &shown, self.release))?,
        };
        match self.modifier_list()? {
            None => Ok(oid),
            Some(list) => self.modified(oid, &list),
        }
    }

    /// An interval's fields: `year`, `year to month`, `day to second(3)` and the rest.
    fn interval_fields(&mut self) -> Result<()> {
        let second = |p: &mut Self| -> Result<()> {
            p.length()?;
            Ok(())
        };
        if self.keyword("year") {
            if self.keyword("to") && !self.keyword("month") {
                return Err(self.syntax());
            }
        } else if self.keyword("month") {
        } else if self.keyword("day") {
            if self.keyword("to") {
                if self.keyword("second") {
                    second(self)?;
                } else if !(self.keyword("hour") || self.keyword("minute")) {
                    return Err(self.syntax());
                }
            }
        } else if self.keyword("hour") {
            if self.keyword("to") {
                if self.keyword("second") {
                    second(self)?;
                } else if !self.keyword("minute") {
                    return Err(self.syntax());
                }
            }
        } else if self.keyword("minute") {
            if self.keyword("to") {
                if !self.keyword("second") {
                    return Err(self.syntax());
                }
                second(self)?;
            }
        } else if self.keyword("second") {
            second(self)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_in_object_prints_its_name_and_any_other_oid_bare() {
        assert_eq!(reg_text(RegKind::Class, 1259, 19), "pg_class");
        assert_eq!(reg_text(RegKind::Class, 99_999_999, 19), "99999999");
        assert_eq!(reg_text(RegKind::Class, 0, 19), "-");
        assert_eq!(reg_text(RegKind::Oper, 0, 19), "0");
        assert_eq!(reg_text(RegKind::Role, 10, 19), "10");
        assert_eq!(reg_text(RegKind::Database, 5, 19), "postgres");
        assert_eq!(reg_text(RegKind::Proc, 1394, 19), "pg_catalog.abs");
        assert_eq!(reg_text(RegKind::Procedure, 3813, 14), "pg_is_in_backup()");
        assert_eq!(reg_text(RegKind::Procedure, 3813, 19), "xmltext(text)");
    }

    #[test]
    fn a_name_reads_back_as_its_oid() {
        let r = |k, s| reg_parse(k, s, 19);
        assert_eq!(r(RegKind::Class, "pg_class").unwrap(), 1259);
        assert_eq!(r(RegKind::Class, " PG_CLASS ").unwrap(), 1259);
        assert_eq!(r(RegKind::Class, "\"pg_class\"").unwrap(), 1259);
        assert_eq!(r(RegKind::Class, "pg_catalog.pg_proc").unwrap(), 1255);
        assert!(r(RegKind::Class, "\"PG_CLASS\"").is_err());
        assert!(r(RegKind::Class, "pg_toast_1255").is_err());
        assert_eq!(r(RegKind::Class, "pg_toast.pg_toast_1255").unwrap(), 2836);
        assert_eq!(r(RegKind::Class, "-").unwrap(), 0);
        assert_eq!(r(RegKind::Class, "4294967295").unwrap(), u32::MAX);
        assert!(r(RegKind::Class, "4294967296").is_err());
        assert!(r(RegKind::Class, " 7").is_err());
        assert_eq!(r(RegKind::Proc, "now").unwrap(), 1299);
        assert!(r(RegKind::Proc, "abs").is_err());
        assert!(r(RegKind::Proc, "pg_catalog.abs").is_err());
        assert!(r(RegKind::Proc, "public.now").is_err());
        assert_eq!(r(RegKind::Procedure, "abs(int4)").unwrap(), 1397);
        assert_eq!(
            r(RegKind::Procedure, "pg_catalog.abs( integer )").unwrap(),
            1397
        );
        assert_eq!(r(RegKind::Oper, "||/").unwrap(), 597);
        assert!(r(RegKind::Oper, "-").is_err());
        assert_eq!(r(RegKind::Operator, "+(int4,int4)").unwrap(), 551);
        assert_eq!(r(RegKind::Operator, "-(NONE,integer)").unwrap(), 558);
        assert!(r(RegKind::Operator, "-(integer)").is_err());
        assert_eq!(r(RegKind::Namespace, "pg_catalog").unwrap(), 11);
        assert!(r(RegKind::Namespace, "a.b").is_err());
        assert_eq!(r(RegKind::Collation, "\"C\"").unwrap(), 950);
        assert!(r(RegKind::Collation, "C").is_err());
        assert_eq!(r(RegKind::Config, "simple").unwrap(), 3748);
        assert!(r(RegKind::Config, "english").is_err());
        assert!(r(RegKind::Role, "pg_monitor").is_err());
        assert_eq!(r(RegKind::Role, "3373").unwrap(), 3373);
        assert!(reg_parse(RegKind::Database, "postgres", 18).is_err());
    }

    #[test]
    fn a_type_name_reads_as_parse_type_string_reads_it() {
        let t = |s| type_oid(s, 19);
        for (s, oid) in [
            ("integer", 23),
            ("int4", 23),
            ("pg_catalog.int4", 23),
            ("character varying(10)", 1043),
            ("char", 1042),
            ("\"char\"", 18),
            ("_int4", 1007),
            ("int4[]", 1007),
            ("int4 [3][]", 1007),
            ("integer ARRAY", 1007),
            ("double precision", 701),
            ("float(24)", 700),
            ("float(25)", 701),
            ("timestamp(3) with time zone", 1184),
            ("time without time zone", 1083),
            ("interval day to second(3)", 1186),
            ("bit varying(5)", 1562),
            ("numeric(5,-2)", 1700),
            ("national character varying", 1043),
            ("\"any\"", 2276),
        ] {
            assert_eq!(t(s).unwrap(), oid, "{s}");
        }
        for s in [
            "any",
            "setof int4",
            "varchar(0)",
            "int4(3)",
            "float(54)",
            "float(0)",
            "_int4[]",
            "nosuch",
            "public.int4",
            "",
            "double",
            "int4 extra",
            "numeric(1001)",
        ] {
            assert!(t(s).is_err(), "{s}");
        }
    }

    #[test]
    fn a_name_splits_as_split_identifier_string_splits_it() {
        assert_eq!(split_name(" a . \"B\"\"c\" ").unwrap(), ["a", "B\"c"]);
        assert_eq!(split_name("\"\"").unwrap(), [""]);
        assert!(split_name("a..b").is_none());
        assert!(split_name("a b").is_none());
        assert!(split_name("  ").is_none());
        assert!(split_name("\"a").is_none());
        assert_eq!(split_name(&"x".repeat(70)).unwrap(), ["x".repeat(63)]);
        assert_eq!(split_name(&"x".repeat(64)).unwrap(), ["x".repeat(63)]);
    }
}
