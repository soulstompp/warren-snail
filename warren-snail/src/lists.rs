//! The types, operators and operator classes PostgreSQL defines from 14 to 19 (`types.txt`,
//! `operators.txt` and `opclasses.txt`, drawn from each release's catalog), each held or refused by
//! name as the snail's own paths answer it: a type where a table's column of it is taken and an
//! operator class where an index of it is, as the DDL reader reads them; an operator where a
//! pipeline's step, a plain question and a WHERE each answer it over columns of its types.

use crate::catalog::Catalog;
use std::sync::OnceLock;

/// What a list names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kind {
    Type,
    Operator,
    Opclass,
}

/// One object of a list.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Listed {
    /// Its name as PostgreSQL's catalog prints it: a type as `regtype` does, an operator as
    /// `regoperator` does (`+(integer,integer)`, `-(NONE,integer)`), an operator class as
    /// `int4_ops USING btree`.
    pub name: &'static str,
    /// Its family, as PostgreSQL's catalog names it: a type's category, an operator's left
    /// operand's (its right's for a prefix operator), an operator class's method.
    pub family: &'static str,
    /// An operator class's key type, as `regtype` prints it; empty for the others.
    pub input: &'static str,
    /// The releases that define it, a bit a release from 14.
    releases: u8,
}

impl Listed {
    /// Whether the release defines it.
    pub fn in_release(&self, release: u8) -> bool {
        release
            .checked_sub(*crate::regs::RELEASES.start())
            .is_some_and(|b| b < 8 && self.releases & (1 << b) != 0)
    }

    /// An operator's name and its operands' types, the left one `None` for a prefix operator.
    pub fn operands(&self) -> (&'static str, Option<&'static str>, &'static str) {
        let (op, args) = self.name.split_once('(').unwrap_or((self.name, ")"));
        let (left, right) = args
            .trim_end_matches(')')
            .split_once(',')
            .unwrap_or(("NONE", ""));
        (op, (left != "NONE").then_some(left), right)
    }
}

/// Every object a list names, over the releases.
pub fn all(kind: Kind) -> &'static [Listed] {
    static TYPES: OnceLock<Vec<Listed>> = OnceLock::new();
    static OPERATORS: OnceLock<Vec<Listed>> = OnceLock::new();
    static OPCLASSES: OnceLock<Vec<Listed>> = OnceLock::new();
    let (cell, text) = match kind {
        Kind::Type => (&TYPES, include_str!("types.txt")),
        Kind::Operator => (&OPERATORS, include_str!("operators.txt")),
        Kind::Opclass => (&OPCLASSES, include_str!("opclasses.txt")),
    };
    cell.get_or_init(|| {
        text.lines()
            .map(|line| {
                let mut f = line.split('\t');
                let name = f.next().expect("a row of a list names an object");
                let releases = f.next().filter(|s| !s.is_empty());
                let family = f.next().expect("a row of a list names a family");
                Listed {
                    name,
                    family,
                    input: f.next().unwrap_or_default(),
                    releases: crate::regs::releases_of(releases),
                }
            })
            .collect()
    })
}

/// An object as the snail answers it: the statements it was probed by, and why the snail refuses
/// it, where it does.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Probed {
    pub listed: &'static Listed,
    /// The statements, as PostgreSQL may be given them too.
    pub probe: String,
    /// The snail's refusal, in its words.
    pub refused: Option<String>,
}

/// The schema every probe makes its objects in.
const SCHEMA: &str = "census";

/// The type a probe takes for a pseudo-type, any other as it is: integer where any type will do,
/// and the arrays, ranges and multiranges of integer; an enum and a composite the probe declares.
fn concrete(ty: &str) -> &str {
    match ty {
        "anyelement" | "anycompatible" | "anynonarray" => "integer",
        "anyarray" | "anycompatiblearray" => "integer[]",
        "anyrange" | "anycompatiblerange" => "int4range",
        "anymultirange" | "anycompatiblemultirange" => "int4multirange",
        "anyenum" => "census.e",
        "record" => "census.c",
        t => t,
    }
}

/// The types a probe declares, ahead of what names them.
const DECLARED: [(&str, &str); 2] = [
    ("census.e", "CREATE TYPE census.e AS ENUM ('a', 'b')"),
    ("census.c", "CREATE TYPE census.c AS (a integer)"),
];

/// Why the snail refuses a DDL under a release, read a statement at a time, where it refuses a
/// statement of it, or where the release has a type it names otherwise.
fn ddl_refused(ddl: &str, release: u8) -> Option<String> {
    match Catalog::read(ddl) {
        Ok(mut r) => r
            .statements
            .iter()
            .find_map(|s| s.refused.clone())
            .or_else(|| r.catalog.with_release(release).err().map(|e| e.to_string())),
        Err(e) => Some(e.to_string()),
    }
}

/// A table holding a column of each type, keyed by an integer of its own, with what it declares
/// first.
fn table_ddl(columns: &[(String, &str)]) -> String {
    let mut out: Vec<String> = DECLARED
        .iter()
        .filter(|(t, _)| columns.iter().any(|(_, c)| c == t))
        .map(|(_, d)| format!("{d};"))
        .collect();
    let cols: String = columns.iter().map(|(n, t)| format!(", {n} {t}")).collect();
    out.push(format!(
        "CREATE TABLE {SCHEMA}.t (k integer PRIMARY KEY{cols});"
    ));
    out.join("\n")
}

/// The types a release defines, each probed by a table holding a column of it.
pub fn types(release: u8) -> Vec<Probed> {
    all(Kind::Type)
        .iter()
        .filter(|t| t.in_release(release))
        .map(|t| {
            let probe = table_ddl(&[("c".into(), t.name)]);
            Probed {
                listed: t,
                refused: ddl_refused(&probe, release),
                probe,
            }
        })
        .collect()
}

/// The operator classes a release defines, each probed by an index of its method and itself on a
/// column of its key's type.
pub fn opclasses(release: u8) -> Vec<Probed> {
    all(Kind::Opclass)
        .iter()
        .filter(|c| c.in_release(release))
        .map(|c| {
            let (name, method) = c.name.split_once(" USING ").unwrap_or((c.name, ""));
            let probe = format!(
                "{}\nCREATE INDEX t_c ON {SCHEMA}.t USING {method} (c {name});",
                table_ddl(&[("c".into(), concrete(c.input))])
            );
            Probed {
                listed: c,
                refused: ddl_refused(&probe, release),
                probe,
            }
        })
        .collect()
}

/// A value of each type an operator's probe reads, as its input reads it.
#[cfg(feature = "standalone")]
const VALUES: &[(&str, &str)] = &[
    ("\"char\"", "a"),
    ("aclitem", "=r/10"),
    ("aclitem[]", "{=r/10}"),
    ("bigint", "2"),
    ("bit", "1"),
    ("bit varying", "101"),
    ("boolean", "true"),
    ("box", "(2,2),(0,0)"),
    ("bytea", "\\x0102"),
    ("census.c", "(1)"),
    ("census.e", "a"),
    ("character", "a"),
    ("cid", "1"),
    ("circle", "<(0,0),1>"),
    ("date", "2020-01-02"),
    ("double precision", "2"),
    ("inet", "192.168.0.1/24"),
    ("int4multirange", "{[1,3)}"),
    ("int4range", "[1,3)"),
    ("integer", "2"),
    ("integer[]", "{1,2}"),
    ("interval", "1 day"),
    ("json", "[1,2]"),
    ("jsonb", "[1,2]"),
    ("jsonpath", "$[0]"),
    ("line", "{1,-1,0}"),
    ("lseg", "[(0,0),(1,1)]"),
    ("macaddr", "08:00:2b:01:02:03"),
    ("macaddr8", "08:00:2b:01:02:03:04:05"),
    ("money", "2"),
    ("name", "a"),
    ("numeric", "2"),
    ("oid", "2"),
    ("oid8", "2"),
    ("oidvector", "1 2"),
    ("path", "[(0,0),(1,1)]"),
    ("pg_lsn", "0/16"),
    ("point", "(1,1)"),
    ("polygon", "((0,0),(1,1),(1,0))"),
    ("real", "2"),
    ("smallint", "2"),
    ("text", "a"),
    ("text[]", "{1}"),
    ("tid", "(0,1)"),
    ("time with time zone", "01:02:03+00"),
    ("time without time zone", "01:02:03"),
    ("timestamp with time zone", "2020-01-02 03:04:05+00"),
    ("timestamp without time zone", "2020-01-02 03:04:05"),
    ("tsquery", "a"),
    ("tsvector", "a"),
    ("uuid", "00000000-0000-0000-0000-000000000001"),
    ("xid", "2"),
    ("xid8", "2"),
];

/// The column of a type in an operator's probe.
#[cfg(feature = "standalone")]
fn column_of(ty: &str) -> String {
    let n: String = ty
        .replace("[]", " array")
        .chars()
        .filter(|&c| c != '"')
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("v_{n}")
}

/// The operators a release defines, each probed by a pipeline's step, a plain question and a WHERE
/// over the columns of a table holding a value of each of its operands' types, in a snail written
/// afresh at `scratch`, the probe named the step's; and the
/// statements that make the table whole, a column and a value of each type, for PostgreSQL to make
/// the same. The snail's table holds the columns and values it takes: an operator of a type whose
/// column or value it refuses is refused in those words.
#[cfg(feature = "standalone")]
pub fn operators(
    release: u8,
    scratch: &std::path::Path,
) -> crate::Result<(Vec<Probed>, Vec<String>)> {
    let ops: Vec<&'static Listed> = all(Kind::Operator)
        .iter()
        .filter(|o| o.in_release(release))
        .collect();
    operators_over(ops, VALUES, release, scratch)
}

/// Each operator given as `operators` answers it, a value of each operand type read from `values`.
#[cfg(feature = "standalone")]
fn operators_over(
    ops: Vec<&'static Listed>,
    values: &[(&str, &str)],
    release: u8,
    scratch: &std::path::Path,
) -> crate::Result<(Vec<Probed>, Vec<String>)> {
    use crate::read::Snail;
    use crate::Error;
    let mut wanted: Vec<(String, &'static str, String)> = Vec::new();
    for o in &ops {
        let (_, left, right) = o.operands();
        for t in left.into_iter().chain([right]).map(concrete) {
            if wanted.iter().all(|(_, w, _)| *w != t) {
                let value = values
                    .iter()
                    .find(|(v, _)| *v == t)
                    .map(|(_, v)| v.replace('\'', "''"))
                    .ok_or_else(|| {
                        Error::new(format!("no value of {t} to probe its operators by"))
                    })?;
                wanted.push((column_of(t), t, value));
            }
        }
    }
    let update = |c: &str, v: &str| format!("UPDATE {SCHEMA}.t SET {c} = '{v}' WHERE k = 1;");
    let first = format!("INSERT INTO {SCHEMA}.t (k) VALUES (1);");
    let whole: Vec<(String, &str)> = wanted.iter().map(|(c, t, _)| (c.clone(), *t)).collect();
    let mut setup: Vec<String> = table_ddl(&whole).lines().map(str::to_string).collect();
    setup.push(first.clone());
    setup.extend(wanted.iter().map(|(c, _, v)| update(c, v)));
    // Each type's column on its own, then its value: taken, or why not.
    let mut why: Vec<(&str, String)> = Vec::new();
    let mut columns: Vec<(String, &str)> = Vec::new();
    for (c, t, _) in &wanted {
        match ddl_refused(&table_ddl(&[(c.clone(), *t)]), release) {
            Some(r) => why.push((*t, r)),
            None => columns.push((c.clone(), *t)),
        }
    }
    let catalog = Catalog::parse(&table_ddl(&columns))?;
    let empty = crate::write::Rows {
        columns: catalog.tables[0]
            .columns
            .iter()
            .map(|c| crate::values::Values::new(c.ty))
            .collect(),
    };
    let _ = std::fs::remove_file(scratch);
    crate::write::write(scratch, &catalog, &[(format!("{SCHEMA}.t"), empty)])?;
    let mut snail = Snail::open_writing(scratch)?;
    snail.execute(&first)?;
    for (c, t) in &columns {
        let (_, _, v) = wanted
            .iter()
            .find(|(w, _, _)| w == c)
            .expect("a column of a type wanted");
        if let Err(e) = snail.execute(&update(c, v)) {
            why.push((*t, e.to_string()));
        }
    }
    let probed = ops
        .into_iter()
        .map(|o| {
            let (op, left, right) = o.operands();
            let (left, right) = (left.map(concrete), concrete(right));
            let (l, r) = (left.map(column_of), column_of(right));
            let expr = match &l {
                Some(l) => format!("{l} {op} {r}"),
                None => format!("{op} {r}"),
            };
            let mut read: Vec<&str> = l.iter().map(String::as_str).chain([r.as_str()]).collect();
            read.dedup();
            let probe = format!(
                "WITH s AS (SELECT {} FROM {SCHEMA}.t) SELECT {expr} AS x FROM s",
                read.join(", ")
            );
            let refused = match left
                .into_iter()
                .chain([right])
                .find_map(|t| why.iter().find(|(w, _)| *w == t))
            {
                Some((_, r)) => Some(r.clone()),
                None => [
                    probe.clone(),
                    format!("SELECT {expr} AS x FROM {SCHEMA}.t"),
                    format!("SELECT 1 AS x FROM {SCHEMA}.t WHERE ({expr}) IS NOT NULL"),
                ]
                .iter()
                .find_map(|q| snail.ask(q, None).err().map(|e| e.to_string())),
            };
            Probed {
                listed: o,
                probe,
                refused,
            }
        })
        .collect();
    Ok((probed, setup))
}

#[cfg(all(test, feature = "standalone"))]
mod tests {
    use super::*;

    #[test]
    fn an_operand_type_the_snail_refuses_refuses_its_operators_in_the_ddl_reader_s_words() {
        let held: &'static Listed = Box::leak(Box::new(Listed {
            name: "=(integer,integer)",
            family: "numeric types",
            input: "",
            releases: 0xff,
        }));
        let refused: &'static Listed = Box::leak(Box::new(Listed {
            name: "=(pg_attribute,pg_attribute)",
            family: "composite types",
            input: "",
            releases: 0xff,
        }));
        let path = std::env::temp_dir().join(format!("lists-refused-{}.snail", std::process::id()));
        let (of, setup) = operators_over(
            vec![held, refused],
            &[("integer", "2"), ("pg_attribute", "(1)")],
            19,
            &path,
        )
        .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(of[0].refused, None);
        assert_eq!(
            of[1].refused.as_deref(),
            Some("census.t.v_pg_attribute: column \"attmissingval\" has pseudo-type anyarray")
        );
        // The setup makes the table whole for PostgreSQL all the same.
        assert!(setup.iter().any(|s| s.contains(" v_pg_attribute pg_attribute")));
    }
}
