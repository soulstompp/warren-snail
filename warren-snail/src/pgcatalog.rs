//! pg_catalog's tables as relations a question reads: each the built-in rows of the file's release,
//! carried as that release's initdb leaves them (`pgcatalog.txt`), and the rows of the file's own
//! objects, by the OIDs its DDL gave them, in the columns the release defines. A column only a
//! running server could fill (an owner, a privilege list, a node tree, a planner's statistic, a
//! frozen transaction ID, a file node) is refused by name where a statement asks it; a table only a
//! server holds (its roles, databases, tablespaces, statistics, large objects, subscriptions) is
//! refused whole.

use crate::catalog::{Catalog, ColumnType, Method, Table};
use crate::oids::{full, parts, Object};
use crate::regs::RegKind;
use crate::{Error, Result};
use std::collections::HashMap;
use std::sync::OnceLock;

/// The tables only a server holds, each with what it holds.
const SERVERS_OWN: [(&str, &str); 16] = [
    ("pg_authid", "roles"),
    ("pg_auth_members", "roles"),
    ("pg_database", "other databases"),
    ("pg_db_role_setting", "roles' and databases' settings"),
    ("pg_tablespace", "tablespaces"),
    ("pg_shdepend", "what depends on roles and databases"),
    ("pg_shdescription", "comments on shared objects"),
    ("pg_shseclabel", "security labels on shared objects"),
    ("pg_replication_origin", "replication"),
    ("pg_statistic", "statistics"),
    ("pg_statistic_ext_data", "statistics"),
    ("pg_largeobject", "large objects"),
    ("pg_largeobject_metadata", "large objects"),
    ("pg_parameter_acl", "privileges on settings"),
    ("pg_subscription", "replication"),
    ("pg_subscription_rel", "replication"),
];

/// One definition of a carried table: the releases it holds for, a bit a release from 14; its
/// columns; and its rows, each the releases it holds for and its text as COPY writes it.
struct Definition {
    releases: u8,
    columns: Vec<&'static str>,
    rows: Vec<(u8, &'static str)>,
}

fn carried() -> &'static HashMap<&'static str, Vec<Definition>> {
    static CARRIED: OnceLock<HashMap<&'static str, Vec<Definition>>> = OnceLock::new();
    CARRIED.get_or_init(|| {
        let mut all: HashMap<&'static str, Vec<Definition>> = HashMap::new();
        let mut at: Option<&'static str> = None;
        for line in include_str!("pgcatalog.txt").lines() {
            if let Some(t) = line.strip_prefix("= ") {
                at = Some(t);
                continue;
            }
            let table = at.expect("a row of pgcatalog.txt follows its table");
            if let Some(d) = line.strip_prefix("~ ") {
                let mut words = d.split(' ');
                let releases = crate::regs::releases_of(words.next());
                all.entry(table).or_default().push(Definition {
                    releases,
                    columns: words.collect(),
                    rows: Vec::new(),
                });
                continue;
            }
            let (spec, row) = line.split_once('\t').expect("a carried row names its releases");
            let def = all
                .get_mut(table)
                .and_then(|d| d.last_mut())
                .expect("a carried row follows its definition");
            def.rows.push((crate::regs::releases_of(Some(spec)), row));
        }
        all
    })
}

/// A field of COPY's text: NULL as none, its backslash escapes read.
fn unescaped(f: &str) -> Option<String> {
    if f == "\\N" {
        return None;
    }
    if !f.contains('\\') {
        return Some(f.to_string());
    }
    let mut out = Vec::with_capacity(f.len());
    let b = f.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + u32::from(b[i] - b'0');
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
                continue;
            }
            c => out.push(c),
        }
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// A text COPY wrote, read back: NULL as none.
pub(crate) fn unescaped_text(f: &str) -> Option<String> {
    unescaped(f)
}

fn bit(release: u8) -> u8 {
    1 << (release - crate::regs::RELEASES.start())
}

/// A carried table's built-in rows under a release, each field by its column's name.
pub(crate) fn built_in(table: &str, release: u8) -> Vec<HashMap<&'static str, Option<String>>> {
    let Some(defs) = carried().get(table) else {
        return Vec::new();
    };
    let Some(def) = defs.iter().find(|d| d.releases & bit(release) != 0) else {
        return Vec::new();
    };
    def.rows
        .iter()
        .filter(|(r, _)| r & bit(release) != 0)
        .map(|(_, row)| {
            def.columns
                .iter()
                .copied()
                .zip(row.split('\t').map(unescaped))
                .collect()
        })
        .collect()
}

/// obj_description: the description pg_description holds for an object, of sub-object 0, of the
/// catalog `class` names, pg_catalog's relation of exactly that name, or of any where none is
/// given, the first held. The file's own objects' are those COMMENT ON gave them; the built-in
/// objects' are carried, public's gone where the file dropped public or given another.
pub(crate) fn description(objoid: u32, class: Option<&str>, release: u8) -> Option<String> {
    let classoid = match class {
        Some(name) => Some(crate::regs::catalog_class(name, release)?),
        None => None,
    };
    described(objoid, classoid, 0, release)
}

/// col_description: the description pg_description holds for a relation's column, by its number.
pub(crate) fn column_description(relation: u32, column: i32, release: u8) -> Option<String> {
    described(relation, Some(crate::deparse::depend::CLASS), column, release)
}

/// The description pg_description holds for an object's sub-object, of the catalog given or of
/// any: the file's first, then the release's.
fn described(objoid: u32, classoid: Option<u32>, sub: i32, release: u8) -> Option<String> {
    type Index = std::sync::Arc<HashMap<u32, Vec<(u32, i32, String)>>>;
    static INDEXES: OnceLock<std::sync::Mutex<HashMap<u8, Index>>> = OnceLock::new();
    let own = crate::oids::held_each(|o| {
        o.described()
            .iter()
            .find(|d| {
                d.objoid == objoid && d.objsubid == sub && classoid.is_none_or(|k| k == d.classoid)
            })
            .map(|d| d.text.clone())
    })
    .flatten();
    if own.is_some() {
        return own;
    }
    let index = {
        let all = INDEXES.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
        all.entry(release)
            .or_insert_with(|| {
                let mut by: HashMap<u32, Vec<(u32, i32, String)>> = HashMap::new();
                for r in built_in("pg_description", release) {
                    let n = |c: &str| r.get(c).cloned().flatten();
                    let (Some(o), Some(c), Some(s), Some(d)) =
                        (n("objoid"), n("classoid"), n("objsubid"), n("description"))
                    else {
                        continue;
                    };
                    if let (Ok(o), Ok(c), Ok(s)) = (o.parse(), c.parse(), s.parse()) {
                        by.entry(o).or_default().push((c, s, d));
                    }
                }
                std::sync::Arc::new(by)
            })
            .clone()
    };
    let dropped = crate::oids::held_each(|o| o.public_dropped()).unwrap_or(false);
    index
        .get(&objoid)?
        .iter()
        .filter(|(c, _, _)| {
            !(dropped && objoid == crate::oids::PUBLIC && *c == crate::deparse::depend::NAMESPACE)
        })
        .find(|(c, s, _)| *s == sub && classoid.is_none_or(|k| k == *c))
        .map(|(_, _, d)| d.clone())
}

/// A carried table's built-in row under a release whose column holds an OID, found through an
/// index made once for that table, column and release.
pub(crate) fn built_in_by(table: &'static str, column: &'static str, oid: u32, release: u8) -> Option<Row> {
    type Index = std::sync::Arc<HashMap<u32, Row>>;
    type Indexes = std::sync::Mutex<HashMap<(&'static str, &'static str, u8), Index>>;
    static INDEXES: OnceLock<Indexes> = OnceLock::new();
    let all = INDEXES.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let index = {
        let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
        all.entry((table, column, release))
            .or_insert_with(|| {
                let mut by: HashMap<u32, Row> = HashMap::new();
                for r in built_in(table, release) {
                    if let Some(k) = r.get(column).cloned().flatten().and_then(|s| s.parse().ok()) {
                        by.entry(k).or_insert(r);
                    }
                }
                std::sync::Arc::new(by)
            })
            .clone()
    };
    index.get(&oid).cloned()
}

/// Whether a table is one of pg_catalog's the snail answers.
fn is_answered(table: &str) -> bool {
    carried().contains_key(table)
}

/// The catalog table a relation's name names: `pg_catalog.x`, or `x` alone, pg_catalog searched
/// first; none for any other name.
pub(crate) fn table_named(name: &str) -> Option<&'static str> {
    let bare = match name.split_once('.') {
        Some(("pg_catalog", t)) => t,
        Some(_) => return None,
        None => name,
    };
    carried()
        .get_key_value(bare)
        .map(|(k, _)| *k)
        .or_else(|| SERVERS_OWN.iter().find(|(t, _)| *t == bare).map(|(t, _)| *t))
}

/// Refused where a table is only a server's.
pub(crate) fn answered(table: &str) -> Result<()> {
    match SERVERS_OWN.iter().find(|(t, _)| *t == table) {
        Some((t, holds)) => Err(Error::new(format!(
            "pg_catalog.{t} holds {holds}, a running server's own, and a snail has no server: it is \
             not read"
        ))),
        None => Ok(()),
    }
}

/// What a column of a catalog table holds that only a running server could fill, where it does.
fn servers_column(column: &str, ty: ColumnType) -> Option<&'static str> {
    use crate::textual::Internal;
    if matches!(ty, ColumnType::Internal(Internal::NodeTree)) {
        return Some("a server's parse tree");
    }
    if ty.pg_name() == "aclitem[]" {
        return Some("privileges, a server's roles'");
    }
    if ty.pg_name() == "anyarray" {
        return Some("a server's statistics");
    }
    if column.ends_with("owner") && ty == ColumnType::Oid || column == "relowner" {
        return Some("an owner, a server's role");
    }
    Some(match column {
        "relpages" | "reltuples" | "relallvisible" | "relallfrozen" => "a planner's statistics",
        "relfrozenxid" | "relminmxid" => "a server's transaction IDs",
        "relfilenode" => "a server's file node",
        _ => return None,
    })
}

/// A catalog table as a relation: its columns' names and types, each column's refusal where only a
/// server could fill it, and its rows.
pub(crate) struct Relation {
    pub(crate) names: Vec<String>,
    pub(crate) types: Vec<ColumnType>,
    pub(crate) refused: Vec<Option<String>>,
    pub(crate) rows: Vec<Vec<Option<String>>>,
}

/// The key of one of PostgreSQL's unique catalog indexes that an object's row takes: the index, the
/// key by its names, which two writers making the object write alike, and its columns and values as
/// a duplicate's DETAIL writes them.
pub(crate) struct NameKey {
    pub(crate) index: &'static str,
    pub(crate) names: String,
    pub(crate) columns: &'static str,
    pub(crate) values: String,
}

impl NameKey {
    /// PostgreSQL's refusal of a second row of the key, its DETAIL after a colon.
    pub(crate) fn duplicate(&self) -> Error {
        Error::new(format!(
            "duplicate key value violates unique constraint \"{}\": Key ({})=({}) already exists.",
            self.index, self.columns, self.values
        ))
    }
}

/// The key a schema's row of pg_namespace, a relation's of pg_class, a type's or an array's of
/// pg_type, or a function's of pg_proc takes, as the catalog names the object; none for any other.
pub(crate) fn name_key(catalog: &Catalog, o: &Object) -> Option<NameKey> {
    let f = File::of(catalog);
    let in_schema = |n: &str, own: String, index, columns| {
        let (schema, _) = parts(n);
        NameKey {
            index,
            names: format!("{schema}.{own}"),
            columns,
            values: format!("{own}, {}", f.namespace(schema)),
        }
    };
    Some(match o {
        Object::Namespace(n) => NameKey {
            index: "pg_namespace_nspname_index",
            names: n.clone(),
            columns: "nspname",
            values: n.clone(),
        },
        Object::Class(n) => in_schema(
            n,
            parts(n).1.to_string(),
            "pg_class_relname_nsp_index",
            "relname, relnamespace",
        ),
        Object::Type(n) => in_schema(
            n,
            parts(n).1.to_string(),
            "pg_type_typname_nsp_index",
            "typname, typnamespace",
        ),
        Object::Array(n) => in_schema(
            n,
            format!("_{}", parts(n).1),
            "pg_type_typname_nsp_index",
            "typname, typnamespace",
        ),
        Object::Proc(n, args) => {
            let (schema, own) = parts(n);
            // Its arguments' types by their OIDs, as proargtypes holds them.
            let held = catalog.functions.iter().find(|x| {
                full(&x.name) == *n
                    && x.args.iter().map(|t| t.pg_name()).collect::<Vec<_>>().join(",") == *args
            });
            let oids: Vec<String> = match held {
                Some(x) => x.args.iter().map(|&a| f.facts(a, None).oid.to_string()).collect(),
                None => args
                    .split(',')
                    .filter(|a| !a.is_empty())
                    .map(|a| {
                        crate::regs::reg_parse(RegKind::Type, a, catalog.release)
                            .unwrap_or(0)
                            .to_string()
                    })
                    .collect(),
            };
            NameKey {
                index: "pg_proc_proname_args_nsp_index",
                names: format!("{schema}.{own}({args})"),
                columns: "proname, proargtypes, pronamespace",
                values: format!("{own}, {}, {}", oids.join(" "), f.namespace(schema)),
            }
        }
        _ => return None,
    })
}

/// A catalog's objects that stand alone, each by its OID and the names of what it holds: a table
/// its own, its indexes' and its element strands'; a type its own and a range's multirange's; a
/// function none, being one of several of its name; any other its own.
fn alone(c: &Catalog) -> Vec<(u32, Vec<String>)> {
    let mut out = Vec::new();
    let mut add = |o: Object, names: Vec<String>| {
        if let Some(oid) = c.oids.of(&o) {
            out.push((oid, names));
        }
    };
    for t in c.tables.iter().filter(|t| t.declared()) {
        let n = full(&t.name);
        let (schema, _) = parts(&n);
        let mut names = vec![n.clone()];
        names.extend(t.orders.iter().map(|o| format!("{schema}.{}", o.name)));
        names.extend(t.brins.iter().map(|b| format!("{schema}.{}", b.name)));
        names.extend(
            c.tables
                .iter()
                .filter(|x| x.elements_of.as_deref() == Some(t.name.as_str()))
                .map(|x| full(&x.name)),
        );
        add(Object::Class(n), names);
    }
    for n in c.views.iter().map(|v| &v.name).chain(c.sequences.iter().map(|s| &s.name)) {
        add(Object::Class(full(n)), vec![full(n)]);
    }
    for t in &c.types {
        let mut names = vec![full(&t.name)];
        if let crate::catalog::TypeKind::Range(r) = &t.kind {
            names.push(full(&r.multirange));
        }
        add(Object::Type(full(&t.name)), names);
    }
    for d in &c.domains {
        add(Object::Type(full(&d.name)), vec![full(&d.name)]);
    }
    for f in &c.functions {
        add(function_object(f), Vec::new());
    }
    for k in &c.collations {
        add(Object::Collation(full(&k.name)), vec![full(&k.name)]);
    }
    for (oid, o) in c.oids.each() {
        if let Object::Namespace(n) = o {
            out.push((oid, vec![n.clone()]));
        }
    }
    out
}

/// The catalog a transaction of one snapshot reads the catalog tables' rows from where another
/// writer's DDL moved the catalog since its snapshot: `own`, the catalog as committed with its own
/// statements on it, less each object another made since `snapshot`, with each another dropped since
/// as its snapshot held it, by OID; `committed` is the catalog as committed.
pub(crate) fn seen(own: &Catalog, committed: &Catalog, snapshot: &Catalog) -> Catalog {
    use std::collections::BTreeSet;
    let then: BTreeSet<u32> = alone(snapshot).into_iter().map(|(o, _)| o).collect();
    let now: Vec<(u32, Vec<String>)> = alone(committed);
    let made: BTreeSet<u32> = now
        .iter()
        .map(|(o, _)| *o)
        .filter(|o| !then.contains(o))
        .collect();
    let standing: BTreeSet<u32> = alone(own).into_iter().map(|(o, _)| o).collect();
    let now: BTreeSet<u32> = now.into_iter().map(|(o, _)| o).collect();
    let dropped: Vec<(u32, Vec<String>)> = alone(snapshot)
        .into_iter()
        .filter(|(o, _)| !now.contains(o) && !standing.contains(o))
        .collect();
    let gone: Vec<String> = alone(own)
        .into_iter()
        .filter(|(o, _)| made.contains(o))
        .flat_map(|(_, names)| names)
        .collect();
    let back_oids: BTreeSet<u32> = dropped.iter().map(|(o, _)| *o).collect();
    let back: Vec<String> = dropped.into_iter().flat_map(|(_, names)| names).collect();
    let gone_name = |n: &str| gone.iter().any(|g| g == n);
    let back_name = |n: &str| back.iter().any(|g| g == n);
    // A function goes or comes back by its OID, the others of its name standing as they do.
    let gone_object = |o: &Object| match o {
        Object::Proc(..) => own.oids.of(o).is_some_and(|x| made.contains(&x)) || gone_name(o.name()),
        _ => gone_name(o.name()),
    };
    let back_object = |oid: u32, o: &Object| match o {
        Object::Proc(..) => back_oids.contains(&oid) || back_name(o.name()),
        _ => back_name(o.name()),
    };
    let mut seen = own.clone();
    seen.tables.retain(|t| !gone_name(&full(&t.name)));
    seen.views.retain(|v| !gone_name(&full(&v.name)));
    seen.sequences.retain(|s| !gone_name(&full(&s.name)));
    seen.types.retain(|t| !gone_name(&full(&t.name)));
    seen.domains.retain(|d| !gone_name(&full(&d.name)));
    seen.functions.retain(|f| !gone_object(&function_object(f)));
    seen.collations.retain(|k| !gone_name(&full(&k.name)));
    let named = |n: &String| back_name(&full(n));
    seen.tables.extend(snapshot.tables.iter().filter(|t| named(&t.name)).cloned());
    seen.views.extend(snapshot.views.iter().filter(|v| named(&v.name)).cloned());
    seen.sequences.extend(snapshot.sequences.iter().filter(|s| named(&s.name)).cloned());
    seen.types.extend(snapshot.types.iter().filter(|t| named(&t.name)).cloned());
    seen.domains.extend(snapshot.domains.iter().filter(|d| named(&d.name)).cloned());
    seen.collations.extend(snapshot.collations.iter().filter(|k| named(&k.name)).cloned());
    seen.functions.extend(
        snapshot
            .functions
            .iter()
            .filter(|f| {
                let o = function_object(f);
                snapshot.oids.of(&o).is_some_and(|x| back_object(x, &o))
            })
            .cloned(),
    );
    let added: Vec<(u32, Object)> = snapshot
        .oids
        .each()
        .filter(|(oid, o)| back_object(*oid, o))
        .map(|(oid, o)| (oid, o.clone()))
        .collect();
    seen.oids = std::sync::Arc::new(own.oids.kept(gone_object, &added));
    seen
}

/// The object a function is to the OIDs: its name and its arguments' types as PostgreSQL names
/// them.
fn function_object(f: &crate::catalog::Function) -> Object {
    let args: Vec<String> = f.args.iter().map(|t| t.pg_name()).collect();
    Object::Proc(full(&f.name), args.join(","))
}

/// A catalog table under the file's release: its built-in rows, then the file's own.
pub(crate) fn relation(catalog: &Catalog, table: &str) -> Result<Relation> {
    answered(table)?;
    if !is_answered(table) {
        return Err(Error::new(format!("pg_catalog.{table} is not in the snail")));
    }
    let release = catalog.release;
    let fields = crate::rowtypes::fields_at(table, release)?;
    let names: Vec<String> = fields.iter().map(|f| f.0.clone()).collect();
    // A column of a pseudo-type, a server's statistics, is held as text: it is refused where asked.
    let types: Vec<ColumnType> = fields
        .iter()
        .map(|f| f.1.unwrap_or(ColumnType::Text(None)))
        .collect();
    let refused = fields
        .iter()
        .map(|(n, t)| {
            let why = match t {
                None => Some("a server's statistics"),
                Some(t) => servers_column(n, *t),
            };
            why.map(|why| format!("pg_catalog.{table}.{n} holds {why}"))
        })
        .collect();
    let mut rows: Vec<HashMap<&str, Option<String>>> = built_in(table, release);
    if catalog.oids.public_dropped() {
        rows.retain(|r| !of_public(table, r));
    }
    // A description the file gave a built-in object, `public`'s, stands in place of the release's.
    if table == "pg_description" {
        let own = catalog.oids.described();
        rows.retain(|r| {
            let is = |c: &str, v: String| r.get(c).cloned().flatten() == Some(v);
            !own.iter().any(|d| {
                is("objoid", d.objoid.to_string())
                    && is("classoid", d.classoid.to_string())
                    && is("objsubid", d.objsubid.to_string())
            })
        });
    }
    let file = File::of(catalog);
    rows.extend(file.rows(table)?);
    if table == "pg_type" {
        for r in rows.iter_mut() {
            if let Some(Some(h)) = r.get("typdefaultbin").cloned() {
                let text = crate::deparse::text_of_tree(catalog, &h, 0, false)
                    .map_err(|e| Error::new(format!("typdefault {h}: {e}")))?;
                r.insert("typdefault", text);
            }
        }
    }
    let rows = rows
        .into_iter()
        .map(|r| names.iter().map(|n| r.get(n.as_str()).cloned().flatten()).collect())
        .collect();
    Ok(Relation {
        names,
        types,
        refused,
        rows,
    })
}

/// Whether a carried row is the release's `public`'s, which a DROP SCHEMA public takes: its row of
/// pg_namespace, its comment and its initial privileges.
fn of_public(table: &str, r: &HashMap<&str, Option<String>>) -> bool {
    let is = |c: &str, v: u32| r.get(c).cloned().flatten() == Some(v.to_string());
    match table {
        "pg_namespace" => is("oid", crate::oids::PUBLIC),
        "pg_description" | "pg_init_privs" => {
            is("objoid", crate::oids::PUBLIC) && is("classoid", crate::deparse::depend::NAMESPACE)
        }
        _ => false,
    }
}

/// The rows of a catalog table the file's own objects make, none for a table they make none of.
pub(crate) fn file_rows(catalog: &Catalog, table: &str) -> Vec<Row> {
    File::of(catalog).rows(table).unwrap_or_default()
}

/// The OID of a column's type, as pg_attribute's atttypid gives it.
pub(crate) fn type_oid_of(catalog: &Catalog, ty: ColumnType, domain: Option<&str>) -> u32 {
    File::of(catalog).facts(ty, domain).oid
}

/// A row of the file's own, each field by its column's name: a column it leaves out is NULL.
type Row = HashMap<&'static str, Option<String>>;

fn row(fields: &[(&'static str, String)]) -> Row {
    fields.iter().map(|(k, v)| (*k, Some(v.clone()))).collect()
}

fn t(b: bool) -> String {
    if b { "t" } else { "f" }.to_string()
}

/// A one-dimensional text array as its output writes it: an element quoted where it is empty,
/// spelled NULL, or holds a brace, a comma, a quote, a backslash or white space.
fn text_array(items: &[String]) -> String {
    let each: Vec<String> = items
        .iter()
        .map(|s| {
            let quoted = s.is_empty()
                || s.eq_ignore_ascii_case("null")
                || s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_ascii_whitespace());
            match quoted {
                true => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
                false => s.clone(),
            }
        })
        .collect();
    format!("{{{}}}", each.join(","))
}

/// The functions a constraint's internal triggers call, by their OIDs, the same in every release.
const UNIQUE_KEY_RECHECK: u32 = 1250;
const RI_FKEY_CHECK_INS: u32 = 1644;
const RI_FKEY_CHECK_UPD: u32 = 1645;

/// A trigger's type as pg_trigger's tgtype holds it: for each row (1), after the statement's change,
/// on INSERT (4), DELETE (8) or UPDATE (16).
const ROW_INSERT: u8 = 1 | 4;
const ROW_DELETE: u8 = 1 | 8;
const ROW_UPDATE: u8 = 1 | 16;
const ROW_INSERT_UPDATE: u8 = 1 | 4 | 16;

/// The function a foreign key's action on DELETE, or on UPDATE, calls: `RI_FKey_cascade_del` and
/// its kin, each action's DELETE function followed by its UPDATE one.
fn ri_action(a: &crate::catalog::Action, delete: bool) -> u32 {
    use crate::catalog::Action as A;
    let on_delete = match a {
        A::Cascade => 1646,
        A::Restrict => 1648,
        A::SetNull(_) => 1650,
        A::SetDefault(_) => 1652,
        A::NoAction => 1654,
    };
    if delete {
        on_delete
    } else {
        on_delete + 1
    }
}

/// What PostgreSQL's pg_type says of a type a column of the file is of.
#[derive(Clone)]
struct TypeFacts {
    oid: u32,
    len: String,
    byval: String,
    align: String,
    storage: String,
    collatable: bool,
}

/// The file's catalog, read for the rows its objects make.
struct File<'a> {
    catalog: &'a Catalog,
    release: u8,
    types: std::sync::Arc<HashMap<u32, HashMap<&'static str, Option<String>>>>,
}

/// A release's built-in rows of pg_type by OID, read once.
fn built_in_types(release: u8) -> std::sync::Arc<HashMap<u32, HashMap<&'static str, Option<String>>>> {
    type Types = std::sync::Arc<HashMap<u32, HashMap<&'static str, Option<String>>>>;
    static TYPES: OnceLock<std::sync::Mutex<HashMap<u8, Types>>> = OnceLock::new();
    let all = TYPES.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
    all.entry(release)
        .or_insert_with(|| {
            std::sync::Arc::new(
                built_in("pg_type", release)
                    .into_iter()
                    .filter_map(|r| {
                        let oid = r.get("oid")?.as_deref()?.parse().ok()?;
                        Some((oid, r))
                    })
                    .collect(),
            )
        })
        .clone()
}

/// The default collation's OID.
const DEFAULT_COLLATION: u32 = 100;

/// Whether pg_catalog holds a collation of that name under a release, for UTF8 or any encoding.
pub(crate) fn is_built_in_collation(name: &str, release: u8) -> bool {
    let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
    built_in_collations(release).contains_key(bare)
}

/// A release's built-in collations by name: those of any encoding or of UTF8, a snail's, read once.
fn built_in_collations(release: u8) -> std::sync::Arc<HashMap<String, u32>> {
    type Named = std::sync::Arc<HashMap<String, u32>>;
    static COLLATIONS: OnceLock<std::sync::Mutex<HashMap<u8, Named>>> = OnceLock::new();
    let all = COLLATIONS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
    all.entry(release)
        .or_insert_with(|| {
            std::sync::Arc::new(
                built_in("pg_collation", release)
                    .into_iter()
                    .filter(|r| r.get("collnamespace").cloned().flatten().as_deref() == Some("11"))
                    .filter(|r| matches!(r.get("collencoding").cloned().flatten().as_deref(), Some("-1" | "6")))
                    .filter_map(|r| {
                        let oid = r.get("oid")?.as_deref()?.parse().ok()?;
                        Some((r.get("collname")?.clone()?, oid))
                    })
                    .collect(),
            )
        })
        .clone()
}

/// An access method's OID, as initdb fixes it.
fn am_oid(m: Method) -> u32 {
    match m {
        Method::Btree => 403,
        Method::Hash => 405,
        Method::Gist => 783,
        Method::Gin => 2742,
        Method::Spgist => 4000,
        Method::Brin => 3580,
    }
}

/// A type's typmod, as PostgreSQL's input of its modifiers writes it: -1 where it has none.
pub(crate) fn typmod_of(ty: ColumnType) -> i32 {
    use ColumnType as C;
    match ty {
        C::Text(Some(n)) | C::Bpchar(Some(n)) if n != C::VARCHAR => n as i32 + 4,
        C::Bit(n) | C::Varbit(Some(n)) => n as i32,
        C::Numeric { precision, scale } => {
            ((i32::from(precision) << 16) | (i32::from(scale) & 0x7ff)) + 4
        }
        C::NumericText(Some((p, s))) => ((i32::from(p) << 16) | (i32::from(s) & 0x7ff)) + 4,
        C::Time(Some(p)) | C::Timetz(Some(p)) | C::Timestamp(Some(p)) => i32::from(p),
        C::TimestamptzRounded(p) => i32::from(p),
        C::Interval { range, precision } => {
            if range == crate::times::INTERVAL_FULL_RANGE && precision.is_none() {
                -1
            } else {
                (i32::from(range) << 16) | precision.map_or(0xffff, i32::from)
            }
        }
        _ => -1,
    }
}

impl<'a> File<'a> {
    fn of(catalog: &'a Catalog) -> File<'a> {
        let types = built_in_types(catalog.release);
        File {
            catalog,
            release: catalog.release,
            types,
        }
    }

    fn oid(&self, o: Object) -> u32 {
        self.catalog.oids.of(&o).unwrap_or(0)
    }

    /// A schema's OID: pg_catalog's and public's as initdb fixes them, the file's own by its DDL, a
    /// public made again among them.
    fn namespace(&self, schema: &str) -> u32 {
        match schema {
            "pg_catalog" => 11,
            "public" => self.catalog.oids.public().unwrap_or(0),
            "pg_toast" => 99,
            s => self.oid(Object::Namespace(s.to_string())),
        }
    }

    fn proc(&self, name: &str) -> String {
        crate::regs::reg_parse(RegKind::Proc, name, self.release)
            .unwrap_or(0)
            .to_string()
    }

    /// What pg_type says of a type of the file's columns: a built-in's as carried, a type of the
    /// file's by its kind.
    fn facts(&self, ty: ColumnType, domain: Option<&str>) -> TypeFacts {
        let built = |oid: u32| -> Option<TypeFacts> {
            let r = self.types.get(&oid)?;
            let f = |k: &str| r.get(k).cloned().flatten().unwrap_or_default();
            Some(TypeFacts {
                oid,
                len: f("typlen"),
                byval: f("typbyval"),
                align: f("typalign"),
                storage: f("typstorage"),
                collatable: f("typcollation") != "0",
            })
        };
        let varlena = |oid: u32, align: &str| TypeFacts {
            oid,
            len: "-1".into(),
            byval: "f".into(),
            align: align.into(),
            storage: "x".into(),
            collatable: false,
        };
        if let Some(d) = domain {
            let oid = self.oid(Object::Type(full(d)));
            let held = self.catalog.domains.iter().find(|x| x.name == d);
            let base = held.and_then(|x| x.base_type);
            // A row type, a composite's or a table's, is aligned on a double.
            let row = held.is_some_and(|x| {
                self.catalog.types.iter().any(|t| {
                    t.name == x.base && matches!(t.kind, crate::catalog::TypeKind::Composite { .. })
                }) || self.catalog.tables.iter().any(|t| t.name == x.base)
            });
            let align = if row { "d" } else { "i" };
            let mut f = base.map_or_else(|| varlena(oid, align), |b| self.facts(b, None));
            f.oid = oid;
            return f;
        }
        if let ColumnType::Composed(r) = ty {
            use crate::composed::Composed as K;
            match r.def() {
                K::Enum { name, .. } => {
                    return TypeFacts {
                        oid: self.oid(Object::Type(full(name))),
                        len: "4".into(),
                        byval: "t".into(),
                        align: "i".into(),
                        storage: "p".into(),
                        collatable: false,
                    }
                }
                K::Record { name, .. } if !name.is_empty() => {
                    let oid = self.oid(Object::Type(full(name)));
                    if oid != 0 {
                        return varlena(oid, "d");
                    }
                }
                K::Range { name, .. } | K::Multirange { name, .. } => {
                    let oid = self.oid(Object::Type(full(name)));
                    if oid != 0 {
                        return varlena(oid, "d");
                    }
                }
                K::Array { element, domain } => {
                    let of = self.facts(*element, domain.as_deref());
                    let oid = match self.file_type_name(*element, domain.as_deref()) {
                        Some(n) => self.oid(Object::Array(full(&n))),
                        None => self.types.get(&of.oid).and_then(|r| {
                            r.get("typarray").cloned().flatten()?.parse().ok()
                        })
                        .unwrap_or(0),
                    };
                    let align = if of.align == "d" { "d" } else { "i" };
                    let mut f = varlena(oid, align);
                    f.collatable = of.collatable;
                    return f;
                }
                _ => {}
            }
        }
        let oid = crate::regs::type_oid(&type_text(ty), self.release).unwrap_or(0);
        built(oid).unwrap_or_else(|| varlena(oid, "i"))
    }

    /// The name of a type of the file's own, where it is one.
    fn file_type_name(&self, ty: ColumnType, domain: Option<&str>) -> Option<String> {
        if let Some(d) = domain {
            return Some(d.to_string());
        }
        let ColumnType::Composed(r) = ty else {
            return None;
        };
        use crate::composed::Composed as K;
        match r.def() {
            K::Enum { name, .. }
            | K::Range { name, .. }
            | K::Multirange { name, .. }
            | K::Record { name, .. }
                if self.catalog.oids.of(&Object::Type(full(name))).is_some() =>
            {
                Some(name.clone())
            }
            _ => None,
        }
    }

    /// The rows of a catalog table the file's own objects make.
    fn rows(&self, table: &str) -> Result<Vec<Row>> {
        Ok(match table {
            "pg_namespace" => self.namespaces(),
            "pg_class" => self.classes(),
            "pg_type" => self.pg_types(),
            "pg_attribute" => self.attributes(),
            "pg_attrdef" => self.defaults(),
            "pg_constraint" => self.constraints(),
            "pg_index" => self.indexes(),
            "pg_enum" => self.labels(),
            "pg_sequence" => self.sequences(),
            "pg_collation" => self.collations(),
            "pg_proc" => self.procs(),
            "pg_range" => self.ranges(),
            "pg_rewrite" => self.rules(),
            "pg_cast" => self.casts(),
            "pg_depend" => self.depends(),
            "pg_trigger" => self.triggers(),
            "pg_description" => self.descriptions(),
            _ => Vec::new(),
        })
    }

    /// The descriptions COMMENT ON gave the file's objects.
    fn descriptions(&self) -> Vec<Row> {
        self.catalog
            .oids
            .described()
            .iter()
            .map(|d| {
                row(&[
                    ("objoid", d.objoid.to_string()),
                    ("classoid", d.classoid.to_string()),
                    ("objsubid", d.objsubid.to_string()),
                    ("description", d.text.clone()),
                ])
            })
            .collect()
    }

    /// A collation's OID by the name a COLLATE gives it, found as PostgreSQL's search path finds
    /// it: pg_catalog's for the file's encoding first, then the file's own; 0 where none is.
    fn collation_oid(&self, name: &str) -> u32 {
        let built = built_in_collations(self.release);
        if let Some(o) = built.get(name) {
            return *o;
        }
        if let Some(("pg_catalog", n)) = name.rsplit_once('.') {
            return built.get(n).copied().unwrap_or(0);
        }
        self.catalog.oids.of(&Object::Collation(full(name))).unwrap_or(0)
    }

    /// The collation a type takes where nothing names one: a domain's own or its base's, the
    /// default one for any other collatable type, none for the rest.
    fn type_collation(&self, ty: ColumnType, domain: Option<&str>) -> u32 {
        if let Some(d) = domain {
            return self.domain_collation(d, 0);
        }
        if let ColumnType::Composed(r) = ty {
            if let crate::composed::Composed::Array { domain: Some(d), .. } = r.def() {
                return self.domain_collation(d, 0);
            }
        }
        // A built-in type's own, as its row of pg_type carries it (`name`'s is C).
        let oid = self.facts(ty, None).oid;
        self.types
            .get(&oid)
            .and_then(|r| r.get("typcollation").cloned().flatten()?.parse().ok())
            .unwrap_or(0)
    }

    /// A domain's typcollation: its COLLATE's, else its base type's.
    fn domain_collation(&self, d: &str, depth: u8) -> u32 {
        let Some(x) = self.catalog.domains.iter().find(|x| full(&x.name) == full(d)) else {
            return 0;
        };
        if let Some(c) = &x.collation {
            return self.collation_oid(c);
        }
        let over = self.catalog.domains.iter().any(|y| full(&y.name) == full(&x.base));
        if over && depth < 32 {
            return self.domain_collation(&x.base, depth + 1);
        }
        match x.base_type {
            Some(b) => self.type_collation(b, None),
            None => crate::regs::type_oid(&x.base, self.release)
                .ok()
                .and_then(|o| self.types.get(&o))
                .and_then(|r| r.get("typcollation").cloned().flatten()?.parse().ok())
                .unwrap_or(0),
        }
    }

    /// A table column's collation, as pg_attribute's attcollation gives it.
    fn column_collation(&self, c: &crate::catalog::Column) -> u32 {
        match &c.collation {
            Some(n) => self.collation_oid(n),
            None => self.type_collation(c.ty, c.domain.as_deref()),
        }
    }

    /// A composite's field's type and collation, by the type's name as its DDL gives it.
    fn field_facts(&self, ty: &str, collation: Option<&str>) -> (u32, u32) {
        let oid = crate::regs::type_oid(ty, self.release)
            .ok()
            .or_else(|| self.catalog.oids.of(&Object::Type(full(ty))))
            .unwrap_or(0);
        let coll = match collation {
            Some(n) => self.collation_oid(n),
            None if self.catalog.domains.iter().any(|d| full(&d.name) == full(ty)) => self.domain_collation(ty, 0),
            None => self
                .types
                .get(&oid)
                .and_then(|r| r.get("typcollation").cloned().flatten()?.parse().ok())
                .unwrap_or(0),
        };
        (oid, coll)
    }

    /// A function's OID by its name and its arguments' types as PostgreSQL names them, by commas.
    fn proc_oid(&self, name: &str, args: &str) -> u32 {
        self.catalog
            .oids
            .each()
            .find_map(|(oid, x)| match x {
                Object::Proc(m, a) if m == name && a == args => Some(oid),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn namespaces(&self) -> Vec<Row> {
        self.catalog
            .oids
            .each()
            .filter_map(|(oid, o)| match o {
                Object::Namespace(n) => Some(row(&[
                    ("oid", oid.to_string()),
                    ("nspname", n.clone()),
                    ("nspowner", "10".into()),
                ])),
                _ => None,
            })
            .collect()
    }

    /// A relation's row of pg_class: its kind, its row type, its access method and its columns.
    fn class(&self, name: &str, kind: char, reltype: u32, am: u32, natts: usize) -> Row {
        let (schema, own) = parts(name);
        row(&[
            ("oid", self.oid(Object::Class(name.to_string())).to_string()),
            ("relname", own.to_string()),
            ("relnamespace", self.namespace(schema).to_string()),
            ("reltype", reltype.to_string()),
            ("reloftype", "0".into()),
            ("relowner", "10".into()),
            ("relam", am.to_string()),
            ("relfilenode", "0".into()),
            ("reltablespace", "0".into()),
            ("relpages", "0".into()),
            ("reltuples", "-1".into()),
            ("relallvisible", "0".into()),
            ("relallfrozen", "0".into()),
            ("reltoastrelid", "0".into()),
            ("relhasindex", "f".into()),
            ("relisshared", "f".into()),
            ("relpersistence", "p".into()),
            ("relkind", kind.to_string()),
            ("relnatts", natts.to_string()),
            ("relchecks", "0".into()),
            ("relhasrules", t(kind == 'v')),
            ("relhastriggers", "f".into()),
            ("relhassubclass", "f".into()),
            ("relrowsecurity", "f".into()),
            ("relforcerowsecurity", "f".into()),
            ("relispopulated", "t".into()),
            ("relreplident", if kind == 'r' { "d" } else { "n" }.into()),
            ("relispartition", "f".into()),
            ("relrewrite", "0".into()),
            ("relfrozenxid", "0".into()),
            ("relminmxid", "0".into()),
        ])
    }

    fn tables(&self) -> impl Iterator<Item = &Table> {
        self.catalog.tables.iter().filter(|t| t.declared())
    }

    /// Each index of a table, by its name: its method, its key columns as the table's columns
    /// numbered from 1 (0 for an expression), the columns its INCLUDE names, whether it is unique and
    /// whether it stands for the primary key.
    fn indexes_of(&self, tb: &Table) -> Vec<IndexFacts> {
        let mut out: Vec<IndexFacts> = Vec::new();
        // A key's collation: its COLLATE's, else its column's; an expression here keys no text.
        let collation = |written: Option<&String>, l: &crate::catalog::Level| -> u32 {
            if let Some(n) = written.and_then(|k| crate::deparse::defs::key_collation(k)) {
                return self.collation_oid(&n);
            }
            match l {
                crate::catalog::Level::Column(c) => self.column_collation(&tb.columns[*c]),
                _ => 0,
            }
        };
        for o in &tb.orders {
            out.push(IndexFacts {
                name: o.name.clone(),
                method: o.method,
                collations: o
                    .levels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| collation(o.keys.get(i), l))
                    .collect(),
                keys: o
                    .levels
                    .iter()
                    .map(|l| match l {
                        crate::catalog::Level::Column(c) => *c as i16 + 1,
                        _ => 0,
                    })
                    .collect(),
                include: o.include.iter().map(|&c| c as i16 + 1).collect(),
                unique: o.unique,
                primary: o.constraint.as_ref().is_some_and(|k| k.primary),
                exclusion: o.exclusion.is_some(),
                clustered: o.clustered && o.table_order,
                nulls_distinct: o.nulls_distinct,
                keyed: o.constraint.is_some(),
                deferrable: o.constraint.as_ref().is_some_and(|k| k.deferrable)
                    || o.exclusion.as_ref().is_some_and(|x| x.deferrable),
            });
        }
        for b in &tb.brins {
            out.push(IndexFacts {
                name: b.name.clone(),
                method: Method::Brin,
                collations: b
                    .levels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| collation(b.keys.get(i), l))
                    .collect(),
                keys: b
                    .levels
                    .iter()
                    .map(|l| match l {
                        crate::catalog::Level::Column(c) => *c as i16 + 1,
                        _ => 0,
                    })
                    .collect(),
                include: Vec::new(),
                unique: false,
                primary: false,
                exclusion: false,
                clustered: false,
                nulls_distinct: true,
                keyed: false,
                deferrable: false,
            });
        }
        for e in &tb.element_keys {
            let coll = self.column_collation(&tb.columns[e.column]);
            match out.iter_mut().find(|x| x.name == e.index) {
                Some(x) => {
                    x.keys.push(e.column as i16 + 1);
                    x.collations.push(coll);
                }
                None => out.push(IndexFacts {
                    name: e.index.clone(),
                    method: e.method,
                    collations: vec![coll],
                    keys: vec![e.column as i16 + 1],
                    include: e.include.iter().map(|&c| c as i16 + 1).collect(),
                    unique: false,
                    primary: false,
                    exclusion: false,
                    clustered: false,
                    nulls_distinct: true,
                    keyed: false,
                    deferrable: false,
                }),
            }
        }
        out
    }

    fn classes(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for tb in self.tables() {
            let name = full(&tb.name);
            let mut r = self.class(
                &name,
                'r',
                self.oid(Object::Type(name.clone())),
                0,
                tb.columns.len(),
            );
            let indexes = self.indexes_of(tb);
            r.insert("relhasindex", Some(t(!indexes.is_empty())));
            r.insert("relchecks", Some(tb.checks.len().to_string()));
            // An enforced foreign key gives both its tables PostgreSQL's triggers that check it.
            let checked = tb.foreign_keys.iter().any(|f| f.enforced)
                || self.tables().any(|o| {
                    o.foreign_keys
                        .iter()
                        .any(|f| f.enforced && full(&f.parent) == name)
                });
            r.insert("relhastriggers", Some(t(checked)));
            out.push(r);
            let schema = parts(&name).0;
            for i in indexes {
                let n = i.keys.len() + i.include.len();
                out.push(self.class(&format!("{schema}.{}", i.name), 'i', 0, am_oid(i.method), n));
            }
        }
        for s in &self.catalog.sequences {
            out.push(self.class(&full(&s.name), 'S', 0, 0, 3));
        }
        for v in &self.catalog.views {
            let name = full(&v.name);
            let reltype = self.oid(Object::Type(name.clone()));
            let mut r = self.class(&name, 'v', reltype, 0, v.columns.len());
            if let Some(option) = v.check_option {
                r.insert("reloptions", Some(format!("{{check_option={option}}}")));
            }
            out.push(r);
        }
        for ty in &self.catalog.types {
            if let crate::catalog::TypeKind::Composite { fields } = &ty.kind {
                let name = full(&ty.name);
                let reltype = self.oid(Object::Type(name.clone()));
                out.push(self.class(&name, 'c', reltype, 0, fields.len()));
            }
        }
        out
    }

    /// A type's row of pg_type.
    #[allow(clippy::too_many_arguments)]
    fn pg_type(
        &self,
        oid: u32,
        name: &str,
        kind: char,
        category: char,
        facts: &TypeFacts,
        io: (&str, &str, &str, &str),
        relid: u32,
        elem: u32,
        array: u32,
    ) -> Row {
        let (schema, own) = parts(name);
        let mut r = row(&[
            ("oid", oid.to_string()),
            ("typname", own.to_string()),
            ("typnamespace", self.namespace(schema).to_string()),
            ("typowner", "10".into()),
            ("typlen", facts.len.clone()),
            ("typbyval", facts.byval.clone()),
            ("typtype", kind.to_string()),
            ("typcategory", category.to_string()),
            ("typispreferred", "f".into()),
            ("typisdefined", "t".into()),
            ("typdelim", ",".into()),
            ("typrelid", relid.to_string()),
            ("typsubscript", "0".into()),
            ("typelem", elem.to_string()),
            ("typarray", array.to_string()),
            ("typinput", self.proc(io.0)),
            ("typoutput", self.proc(io.1)),
            ("typreceive", self.proc(io.2)),
            ("typsend", self.proc(io.3)),
            ("typmodin", "0".into()),
            ("typmodout", "0".into()),
            ("typanalyze", "0".into()),
            ("typalign", facts.align.clone()),
            ("typstorage", facts.storage.clone()),
            ("typnotnull", "f".into()),
            ("typbasetype", "0".into()),
            ("typtypmod", "-1".into()),
            ("typndims", "0".into()),
            ("typcollation", if facts.collatable { "100" } else { "0" }.into()),
        ]);
        r.insert("typdefault", None);
        r
    }

    /// The array type PostgreSQL makes beside a type.
    fn array_of(&self, name: &str, elem: u32, align: &str) -> Row {
        let (schema, own) = parts(name);
        let facts = TypeFacts {
            oid: 0,
            len: "-1".into(),
            byval: "f".into(),
            align: align.into(),
            storage: "x".into(),
            collatable: false,
        };
        let oid = self.oid(Object::Array(name.to_string()));
        let mut r = self.pg_type(
            oid,
            &format!("{schema}._{own}"),
            'b',
            'A',
            &facts,
            ("array_in", "array_out", "array_recv", "array_send"),
            0,
            elem,
            0,
        );
        r.insert("typsubscript", Some(self.proc("array_subscript_handler")));
        r.insert("typanalyze", Some(self.proc("array_typanalyze")));
        r
    }

    fn pg_types(&self) -> Vec<Row> {
        let mut out = Vec::new();
        let composite = |own: &str, relid: u32| (own.to_string(), relid);
        let mut rowtypes: Vec<(String, u32)> = Vec::new();
        for tb in self.tables() {
            let n = full(&tb.name);
            rowtypes.push(composite(&n, self.oid(Object::Class(n.clone()))));
        }
        for v in &self.catalog.views {
            let n = full(&v.name);
            rowtypes.push(composite(&n, self.oid(Object::Class(n.clone()))));
        }
        for ty in &self.catalog.types {
            if let crate::catalog::TypeKind::Composite { .. } = ty.kind {
                let n = full(&ty.name);
                rowtypes.push(composite(&n, self.oid(Object::Class(n.clone()))));
            }
        }
        let record = TypeFacts {
            oid: 0,
            len: "-1".into(),
            byval: "f".into(),
            align: "d".into(),
            storage: "x".into(),
            collatable: false,
        };
        for (n, relid) in rowtypes {
            let oid = self.oid(Object::Type(n.clone()));
            let array = self.oid(Object::Array(n.clone()));
            out.push(self.pg_type(
                oid,
                &n,
                'c',
                'C',
                &record,
                ("record_in", "record_out", "record_recv", "record_send"),
                relid,
                0,
                array,
            ));
            out.push(self.array_of(&n, oid, "d"));
        }
        for ty in &self.catalog.types {
            let n = full(&ty.name);
            let oid = self.oid(Object::Type(n.clone()));
            let array = self.oid(Object::Array(n.clone()));
            match &ty.kind {
                crate::catalog::TypeKind::Enum { .. } => {
                    let facts = TypeFacts {
                        oid,
                        len: "4".into(),
                        byval: "t".into(),
                        align: "i".into(),
                        storage: "p".into(),
                        collatable: false,
                    };
                    out.push(self.pg_type(
                        oid,
                        &n,
                        'e',
                        'E',
                        &facts,
                        ("enum_in", "enum_out", "enum_recv", "enum_send"),
                        0,
                        0,
                        array,
                    ));
                    out.push(self.array_of(&n, oid, "i"));
                }
                crate::catalog::TypeKind::Range(r) => {
                    let mut range = self.pg_type(
                        oid,
                        &n,
                        'r',
                        'R',
                        &record,
                        ("range_in", "range_out", "range_recv", "range_send"),
                        0,
                        0,
                        array,
                    );
                    range.insert("typanalyze", Some(self.proc("range_typanalyze")));
                    out.push(range);
                    out.push(self.array_of(&n, oid, "d"));
                    let m = full(&r.multirange);
                    let moid = self.oid(Object::Type(m.clone()));
                    let marray = self.oid(Object::Array(m.clone()));
                    let mut multi = self.pg_type(
                        moid,
                        &m,
                        'm',
                        'R',
                        &record,
                        (
                            "multirange_in",
                            "multirange_out",
                            "multirange_recv",
                            "multirange_send",
                        ),
                        0,
                        0,
                        marray,
                    );
                    multi.insert("typanalyze", Some(self.proc("multirange_typanalyze")));
                    out.push(multi);
                    out.push(self.array_of(&m, moid, "d"));
                }
                crate::catalog::TypeKind::Composite { .. } => {}
            }
        }
        for d in &self.catalog.domains {
            let n = full(&d.name);
            let oid = self.oid(Object::Type(n.clone()));
            let array = self.oid(Object::Array(n.clone()));
            // Its base type: as the snail holds it, else by the name the DDL gives it.
            let base = match d.base_type {
                Some(b) => self.facts(b, None),
                None => {
                    let named = crate::regs::type_oid(&d.base, self.release).unwrap_or(0);
                    let mut f = record.clone();
                    if let Some(r) = self.types.get(&named) {
                        let g = |k: &str| r.get(k).cloned().flatten().unwrap_or_default();
                        f = TypeFacts {
                            oid: named,
                            len: g("typlen"),
                            byval: g("typbyval"),
                            align: g("typalign"),
                            storage: g("typstorage"),
                            collatable: g("typcollation") != "0",
                        };
                    } else if named != 0 {
                        f.oid = named;
                    }
                    f
                }
            };
            // The base type's row: a built-in's as carried, one of the file's as made above.
            let base_oid = base.oid.to_string();
            let of_base = |k: &str| -> Option<String> {
                self.types
                    .get(&base.oid)
                    .and_then(|r| r.get(k).cloned().flatten())
                    .or_else(|| {
                        out.iter()
                            .find(|r| r.get("oid").cloned().flatten().as_deref() == Some(base_oid.as_str()))
                            .and_then(|r| r.get(k).cloned().flatten())
                    })
            };
            let out_fn = of_base("typoutput").unwrap_or_else(|| "0".into());
            let send_fn = of_base("typsend").unwrap_or_else(|| "0".into());
            let category = of_base("typcategory")
                .and_then(|c| c.chars().next())
                .unwrap_or('U');
            let mut r = self.pg_type(
                oid,
                &n,
                'd',
                category,
                &base,
                ("domain_in", "-", "domain_recv", "-"),
                0,
                0,
                array,
            );
            r.insert("typoutput", Some(out_fn));
            r.insert("typsend", Some(send_fn));
            r.insert("typbasetype", Some(base.oid.to_string()));
            r.insert(
                "typtypmod",
                Some(d.base_type.map_or(-1, typmod_of).to_string()),
            );
            r.insert("typnotnull", Some(t(d.not_null)));
            if d.default.is_some() {
                r.insert(
                    "typdefaultbin",
                    Some(crate::deparse::tree_handle("domaindefault", oid, 0)),
                );
            }
            // A domain's collation, its COLLATE's or its base's; its array's the same.
            let coll = self.domain_collation(&d.name, 0).to_string();
            r.insert("typcollation", Some(coll.clone()));
            out.push(r);
            let mut array = self.array_of(&n, oid, &base.align);
            array.insert("typcollation", Some(coll));
            out.push(array);
        }
        out
    }

    /// A relation's rows of pg_attribute, its columns numbered from 1.
    fn attribute(&self, rel: u32, num: i16, name: &str, ty: ColumnType, domain: Option<&str>) -> Row {
        use crate::composed::Composed;
        let f = self.facts(ty, domain);
        let array = match ty {
            ColumnType::Composed(r) => match r.def() {
                Composed::Array { element, domain } => Some((*element, domain.is_some())),
                _ => None,
            },
            _ => None,
        };
        // A domain's typmod is its own; an array's is its element's, an array of a domain none.
        let (typmod, dims) = match (domain, array) {
            (Some(_), _) => (-1, 0),
            (None, Some((_, true))) => (-1, 1),
            (None, Some((element, false))) => (typmod_of(element), 1),
            (None, None) => (typmod_of(ty), 0),
        };
        row(&[
            ("attrelid", rel.to_string()),
            ("attname", name.to_string()),
            ("atttypid", f.oid.to_string()),
            ("attlen", f.len.clone()),
            ("attnum", num.to_string()),
            ("atttypmod", typmod.to_string()),
            ("attndims", dims.to_string()),
            ("attbyval", f.byval.clone()),
            ("attalign", f.align.clone()),
            ("attstorage", f.storage.clone()),
            ("attcompression", String::new()),
            ("attnotnull", "f".into()),
            ("atthasdef", "f".into()),
            ("atthasmissing", "f".into()),
            ("attidentity", String::new()),
            ("attgenerated", String::new()),
            ("attisdropped", "f".into()),
            ("attislocal", "t".into()),
            ("attinhcount", "0".into()),
            ("attcollation", if f.collatable { "100" } else { "0" }.into()),
        ])
    }

    /// A column's row of pg_attribute by its type's OID: a view's or a composite's, whose type the
    /// snail reads by OID; its length, passing, alignment and storage its type's.
    #[allow(clippy::too_many_arguments)]
    fn attribute_of(&self, rel: u32, num: i16, name: &str, typid: u32, typmod: i32, coll: u32, dims: i16) -> Row {
        let own;
        let r = match self.types.get(&typid) {
            Some(r) => r,
            None => {
                own = self
                    .pg_types()
                    .into_iter()
                    .find(|r| r.get("oid").cloned().flatten() == Some(typid.to_string()))
                    .unwrap_or_default();
                &own
            }
        };
        let f = |k: &str| r.get(k).cloned().flatten().unwrap_or_default();
        row(&[
            ("attrelid", rel.to_string()),
            ("attname", name.to_string()),
            ("atttypid", typid.to_string()),
            ("attlen", f("typlen")),
            ("attnum", num.to_string()),
            ("atttypmod", typmod.to_string()),
            ("attndims", dims.to_string()),
            ("attbyval", f("typbyval")),
            ("attalign", f("typalign")),
            ("attstorage", f("typstorage")),
            ("attcompression", String::new()),
            ("attnotnull", "f".into()),
            ("atthasdef", "f".into()),
            ("atthasmissing", "f".into()),
            ("attidentity", String::new()),
            ("attgenerated", String::new()),
            ("attisdropped", "f".into()),
            ("attislocal", "t".into()),
            ("attinhcount", "0".into()),
            ("attcollation", coll.to_string()),
        ])
    }

    /// The type an index key holds where it is another than its column's, as ConstructTupleDescriptor
    /// finds it: its operator class's `opckeytype`, an array class's `anyelement` read as the
    /// column's element type, else the method's own (a hash index's int4).
    fn stored_type(&self, method: Method, class: Option<u32>, ty: ColumnType) -> Option<u32> {
        const INT4: u32 = 23;
        const ANYARRAY: u32 = 2277;
        const ANYELEMENT: u32 = 2283;
        let row = class.and_then(|c| built_in_by("pg_opclass", "oid", c, self.release));
        let field = |k: &str| -> u32 {
            row.as_ref()
                .and_then(|r| r.get(k).cloned().flatten())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        };
        match field("opckeytype") {
            ANYELEMENT if field("opcintype") == ANYARRAY => match ty {
                ColumnType::Composed(r) => match r.def() {
                    crate::composed::Composed::Array { element, domain } => {
                        Some(self.facts(*element, domain.as_deref()).oid)
                    }
                    _ => None,
                },
                _ => None,
            },
            0 if method == Method::Hash => Some(INT4),
            0 => None,
            key => Some(key),
        }
    }

    /// A row of pg_attribute given another type, a built-in one: its length, passing, alignment
    /// and storage that type's, its typmod none.
    fn retyped(&self, r: &mut Row, ty: u32) {
        let Some(t) = self.types.get(&ty) else {
            return;
        };
        let f = |k: &str| t.get(k).cloned().flatten();
        r.insert("atttypid", Some(ty.to_string()));
        r.insert("atttypmod", Some("-1".into()));
        r.insert("attlen", f("typlen"));
        r.insert("attbyval", f("typbyval"));
        r.insert("attalign", f("typalign"));
        r.insert("attstorage", f("typstorage"));
    }

    fn attributes(&self) -> Vec<Row> {
        let mut out = Vec::new();
        let mut cat = None;
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            for (i, c) in tb.columns.iter().enumerate() {
                let mut r = self.attribute(rel, i as i16 + 1, &c.name, c.ty, c.domain.as_deref());
                r.insert("attndims", Some(c.dims.to_string()));
                r.insert("attcollation", Some(self.column_collation(c).to_string()));
                r.insert("attnotnull", Some(t(c.not_null)));
                // A generation is held in pg_attrdef as a DEFAULT is.
                r.insert("atthasdef", Some(t(c.default.is_some() || c.generated.is_some())));
                r.insert(
                    "attidentity",
                    Some(match &c.identity {
                        Some(i) if i.always => "a".into(),
                        Some(_) => "d".into(),
                        None => String::new(),
                    }),
                );
                r.insert(
                    "attgenerated",
                    Some(match &c.generated {
                        Some(g) if g.stored => "s".into(),
                        Some(_) => "v".into(),
                        None => String::new(),
                    }),
                );
                out.push(r);
            }
            // The system columns a snail's table has: ctid and tableoid.
            for (num, name, ty) in [(-1, "ctid", ColumnType::Tid), (-6, "tableoid", ColumnType::Oid)] {
                let mut r = self.attribute(rel, num, name, ty, None);
                r.insert("attnotnull", Some("t".into()));
                out.push(r);
            }
            let schema = parts(&n).0;
            for i in self.indexes_of(tb) {
                let rel = self.oid(Object::Class(format!("{schema}.{}", i.name)));
                let cat = cat.get_or_insert_with(|| crate::deparse::cat::Cat::of(self.catalog));
                let classes = crate::deparse::defs::index_classes(cat, rel)
                    .ok()
                    .flatten()
                    .map(|(c, _)| c)
                    .unwrap_or_default();
                for (k, &col) in i.keys.iter().chain(&i.include).enumerate() {
                    let (name, ty, domain, dims) = match col {
                        c if c > 0 => {
                            let c = &tb.columns[c as usize - 1];
                            (c.name.clone(), c.ty, c.domain.as_deref(), Some(c.dims))
                        }
                        _ => ("expr".to_string(), ColumnType::Text(None), None, None),
                    };
                    let mut r = self.attribute(rel, k as i16 + 1, &name, ty, domain);
                    if let Some(d) = dims {
                        r.insert("attndims", Some(d.to_string()));
                    }
                    // A key's collation is the index's; an INCLUDE column has none.
                    let coll = i.collations.get(k).copied().unwrap_or(0);
                    r.insert("attcollation", Some(coll.to_string()));
                    // A key holds the type its operator class stores, else its method's.
                    if k < i.keys.len() {
                        let stored = self.stored_type(i.method, classes.get(k).copied(), ty);
                        if let Some(stored) = stored.filter(|&s| Some(s.to_string()) != r["atttypid"]) {
                            self.retyped(&mut r, stored);
                        }
                    }
                    out.push(r);
                }
            }
        }
        // A composite type's fields, as its relation's columns.
        for ty in &self.catalog.types {
            let crate::catalog::TypeKind::Composite { fields } = &ty.kind else {
                continue;
            };
            let rel = self.oid(Object::Class(full(&ty.name)));
            for (i, (name, fty, fcoll)) in fields.iter().enumerate() {
                let (typid, coll) = self.field_facts(fty, fcoll.as_deref());
                let parsed = sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
                    .try_with_sql(fty)
                    .and_then(|mut p| p.parse_data_type())
                    .ok();
                let typmod = parsed
                    .as_ref()
                    .and_then(|dt| crate::catalog::held_type(dt).ok())
                    .map_or(-1, typmod_of);
                let dims = parsed.as_ref().map_or(0, array_dims);
                out.push(self.attribute_of(rel, i as i16 + 1, name, typid, typmod, coll, dims));
            }
        }
        // A view's columns, as its question gives them; not while one is being read for them.
        if !READING_VIEWS.with(|r| r.replace(true)) {
            let cat = crate::deparse::cat::Cat::of(self.catalog);
            for v in &self.catalog.views {
                let rel = self.oid(Object::Class(full(&v.name)));
                if let Ok((q, cols)) = crate::deparse::query::view_query(&cat, v) {
                    for (i, ((name, typid, typmod), t)) in cols.iter().zip(q.output()).enumerate() {
                        let coll = crate::deparse::depend::output_collation(&cat, &q, &t.expr);
                        out.push(self.attribute_of(rel, i as i16 + 1, name, *typid, *typmod, coll, 0));
                    }
                }
            }
            READING_VIEWS.with(|r| r.set(false));
        }
        for s in &self.catalog.sequences {
            let rel = self.oid(Object::Class(full(&s.name)));
            for (i, (name, ty)) in [
                ("last_value", ColumnType::Int8),
                ("log_cnt", ColumnType::Int8),
                ("is_called", ColumnType::Boolean),
            ]
            .into_iter()
            .enumerate()
            {
                let mut r = self.attribute(rel, i as i16 + 1, name, ty, None);
                r.insert("attnotnull", Some("t".into()));
                out.push(r);
            }
        }
        out
    }

    fn defaults(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            for (i, c) in tb.columns.iter().enumerate() {
                if c.default.is_some() || c.generated.is_some() {
                    out.push(row(&[
                        (
                            "oid",
                            self.oid(Object::Default(n.clone(), c.name.clone())).to_string(),
                        ),
                        ("adrelid", rel.to_string()),
                        ("adnum", (i + 1).to_string()),
                        ("adbin", crate::deparse::tree_handle("attrdef", rel, i as i64 + 1)),
                    ]));
                }
            }
        }
        out
    }

    /// A constraint's row of pg_constraint.
    #[allow(clippy::too_many_arguments)]
    fn constraint(
        &self,
        table: &str,
        name: &str,
        kind: char,
        rel: u32,
        index: u32,
        keys: &[i16],
        deferrable: bool,
    ) -> Row {
        let schema = parts(table).0;
        let list = |k: &[i16]| {
            format!(
                "{{{}}}",
                k.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
            )
        };
        let mut r = row(&[
            (
                "oid",
                self.oid(Object::Constraint(table.to_string(), name.to_string()))
                    .to_string(),
            ),
            ("conname", name.to_string()),
            ("connamespace", self.namespace(schema).to_string()),
            ("contype", kind.to_string()),
            ("condeferrable", t(deferrable)),
            ("condeferred", "f".into()),
            ("conenforced", "t".into()),
            ("convalidated", "t".into()),
            ("conrelid", rel.to_string()),
            ("contypid", "0".into()),
            ("conindid", index.to_string()),
            ("conparentid", "0".into()),
            ("confrelid", "0".into()),
            ("confupdtype", " ".into()),
            ("confdeltype", " ".into()),
            ("confmatchtype", " ".into()),
            ("conislocal", "t".into()),
            ("coninhcount", "0".into()),
            ("connoinherit", "f".into()),
            ("conperiod", "f".into()),
        ]);
        r.insert(
            "conkey",
            (!keys.is_empty()).then(|| list(keys)),
        );
        r
    }

    fn constraints(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            let schema = parts(&n).0;
            for i in self.indexes_of(tb) {
                let Some(kind) = self.index_constraint(&n, &i) else {
                    continue;
                };
                let index = self.oid(Object::Class(format!("{schema}.{}", i.name)));
                let mut r = self.constraint(&n, &i.name, kind, rel, index, &i.keys, i.deferrable);
                r.insert("conperiod", Some(t(i.keyed && i.exclusion)));
                out.push(r);
            }
            let cat = (!tb.checks.is_empty()).then(|| crate::deparse::cat::Cat::of(self.catalog));
            for (ci, c) in tb.checks.iter().enumerate() {
                // Its keys: the columns its expression reads, each once, in the order met.
                let keys = cat
                    .as_ref()
                    .and_then(|cat| crate::deparse::stored_tree(cat, "check", rel, ci as i64).ok())
                    .map(|node| crate::deparse::depend::columns_read(&node))
                    .unwrap_or_default();
                let mut r = self.constraint(&n, &c.name, 'c', rel, 0, &keys, false);
                r.insert("conbin", Some(crate::deparse::tree_handle("check", rel, ci as i64)));
                r.insert("conenforced", Some(t(c.enforced)));
                r.insert("convalidated", Some(t(c.valid)));
                out.push(r);
            }
            for f in &tb.foreign_keys {
                let keys: Vec<i16> = f.columns.iter().map(|&c| c as i16 + 1).collect();
                let parent = full(&f.parent);
                let mut r = self.constraint(&n, &f.name, 'f', rel, 0, &keys, false);
                r.insert(
                    "confrelid",
                    Some(self.oid(Object::Class(parent.clone())).to_string()),
                );
                let pkeys: Vec<i16> = f.parent_columns.iter().map(|&c| c as i16 + 1).collect();
                r.insert(
                    "confkey",
                    Some(format!(
                        "{{{}}}",
                        pkeys.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
                    )),
                );
                let pschema = parts(&parent).0;
                r.insert(
                    "conindid",
                    Some(
                        self.oid(Object::Class(format!("{pschema}.{}", f.key)))
                            .to_string(),
                    ),
                );
                let code = |a: &crate::catalog::Action| -> String {
                    use crate::catalog::Action as A;
                    match a {
                        A::NoAction => "a",
                        A::Restrict => "r",
                        A::Cascade => "c",
                        A::SetNull(_) => "n",
                        A::SetDefault(_) => "d",
                    }
                    .into()
                };
                r.insert("confupdtype", Some(code(&f.on_update)));
                r.insert("confdeltype", Some(code(&f.on_delete)));
                r.insert("confmatchtype", Some(if f.full { "f" } else { "s" }.into()));
                r.insert("condeferrable", Some(t(f.deferrable)));
                r.insert("condeferred", Some(t(f.deferred)));
                r.insert("conenforced", Some(t(f.enforced)));
                r.insert("convalidated", Some(t(f.valid)));
                r.insert("conperiod", Some(t(f.period)));
                if let crate::catalog::Action::SetNull(c) | crate::catalog::Action::SetDefault(c) = &f.on_delete {
                    if !c.is_empty() {
                        let k: Vec<String> = c.iter().map(|x| (x + 1).to_string()).collect();
                        r.insert("confdelsetcols", Some(format!("{{{}}}", k.join(","))));
                    }
                }
                out.push(r);
            }
            for c in tb.columns.iter().enumerate().filter(|(_, c)| c.not_null) {
                if self.release < 18 {
                    break;
                }
                let own = parts(&n).1;
                let cname = format!("{own}_{}_not_null", c.1.name);
                out.push(self.constraint(&n, &cname, 'n', rel, 0, &[c.0 as i16 + 1], false));
            }
        }
        for d in &self.catalog.domains {
            let n = full(&d.name);
            let ty = self.oid(Object::Type(n.clone()));
            if d.not_null && self.release >= 17 {
                let own = parts(&n).1;
                let cname = d.not_null_name.clone().unwrap_or_else(|| format!("{own}_not_null"));
                let mut r = self.constraint(&n, &cname, 'n', 0, 0, &[], false);
                r.insert("contypid", Some(ty.to_string()));
                out.push(r);
            }
            for (ki, k) in d.checks.iter().enumerate() {
                let mut r = self.constraint(&n, &k.name, 'c', 0, 0, &[], false);
                r.insert("conbin", Some(crate::deparse::tree_handle("domaincheck", ty, ki as i64)));
                r.insert("contypid", Some(ty.to_string()));
                r.insert("convalidated", Some(t(k.valid)));
                out.push(r);
            }
        }
        out
    }

    /// The rows of pg_trigger the file's constraints make, as PostgreSQL's DDL makes them: each
    /// enforced foreign key's action on DELETE and on UPDATE on the table it references and its
    /// check on INSERT and on UPDATE on its own; each deferrable key's or EXCLUDE's recheck. Each is
    /// internal, named by its own OID, and deferrable as its constraint is.
    fn triggers(&self) -> Vec<Row> {
        use crate::oids::trigger as at;
        let mut out = Vec::new();
        let mut push = |table: &str,
                        constraint: &str,
                        place: u8,
                        prefix: &str,
                        on: u32,
                        function: u32,
                        tgtype: u8,
                        other: u32,
                        index: u32,
                        (deferrable, deferred): (bool, bool)| {
            let oid = self.oid(Object::Trigger(table.to_string(), constraint.to_string(), place));
            let mut r = row(&[
                ("oid", oid.to_string()),
                ("tgrelid", on.to_string()),
                ("tgparentid", "0".into()),
                ("tgname", format!("{prefix}{oid}")),
                ("tgfoid", function.to_string()),
                ("tgtype", tgtype.to_string()),
                ("tgenabled", "O".into()),
                ("tgisinternal", "t".into()),
                ("tgconstrrelid", other.to_string()),
                ("tgconstrindid", index.to_string()),
                (
                    "tgconstraint",
                    self.oid(Object::Constraint(table.to_string(), constraint.to_string()))
                        .to_string(),
                ),
                ("tgdeferrable", t(deferrable)),
                ("tginitdeferred", t(deferred)),
                ("tgnargs", "0".into()),
                ("tgattr", String::new()),
                ("tgargs", "\\x".into()),
            ]);
            for absent in ["tgqual", "tgoldtable", "tgnewtable"] {
                r.insert(absent, None);
            }
            out.push(r);
        };
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            let schema = parts(&n).0;
            for i in self.indexes_of(tb) {
                if !i.deferrable || self.index_constraint(&n, &i).is_none() {
                    continue;
                }
                let index = self.oid(Object::Class(format!("{schema}.{}", i.name)));
                let prefix = if i.primary { "PK_ConstraintTrigger_" } else { "Unique_ConstraintTrigger_" };
                let kind = (true, false);
                push(&n, &i.name, at::RECHECK, prefix, rel, UNIQUE_KEY_RECHECK, ROW_INSERT_UPDATE, 0, index, kind);
            }
            for f in tb.foreign_keys.iter().filter(|f| f.enforced) {
                let parent = full(&f.parent);
                let prel = self.oid(Object::Class(parent.clone()));
                let index = self.oid(Object::Class(format!("{}.{}", parts(&parent).0, f.key)));
                let kind = (f.deferrable, f.deferred);
                // An action other than NO ACTION is never deferred, whatever its key says.
                let acting = |a: &crate::catalog::Action| match a {
                    crate::catalog::Action::NoAction => kind,
                    _ => (false, false),
                };
                let (del, upd) = (ri_action(&f.on_delete, true), ri_action(&f.on_update, false));
                let a = "RI_ConstraintTrigger_a_";
                let c = "RI_ConstraintTrigger_c_";
                let (on_del, on_upd) = (acting(&f.on_delete), acting(&f.on_update));
                push(&n, &f.name, at::ACTION_DELETE, a, prel, del, ROW_DELETE, rel, index, on_del);
                push(&n, &f.name, at::ACTION_UPDATE, a, prel, upd, ROW_UPDATE, rel, index, on_upd);
                push(&n, &f.name, at::CHECK_INSERT, c, rel, RI_FKEY_CHECK_INS, ROW_INSERT, prel, index, kind);
                push(&n, &f.name, at::CHECK_UPDATE, c, rel, RI_FKEY_CHECK_UPD, ROW_UPDATE, prel, index, kind);
            }
        }
        out
    }

    /// The kind of the constraint an index of a table stands for, where it stands for one.
    fn index_constraint(&self, table: &str, i: &IndexFacts) -> Option<char> {
        match (i.primary, i.keyed || !i.exclusion, i.unique) {
            (true, _, _) => Some('p'),
            (_, true, _) if i.keyed => Some('u'),
            (_, false, _) => Some('x'),
            (_, _, true)
                if self
                    .catalog
                    .oids
                    .of(&Object::Constraint(table.to_string(), i.name.clone()))
                    .is_some() =>
            {
                Some('u')
            }
            _ => None,
        }
    }

    /// The rows of pg_depend the file's objects make, as PostgreSQL's DDL records them: each
    /// object's on its schema, its parts, its types and collations, the objects its stored trees
    /// name, and what it is made beside; never one on an object PostgreSQL holds pinned.
    fn depends(&self) -> Vec<Row> {
        use crate::deparse::depend::{self as d, Addr};
        let _named = crate::oids::named(self.catalog.oids.clone());
        let cat = crate::deparse::cat::Cat::of(self.catalog);
        let mut out = Deps::default();
        let class = |name: &str| Addr::of(d::CLASS, self.oid(Object::Class(name.to_string())));
        let ty = |name: &str| Addr::of(d::TYPE, self.oid(Object::Type(name.to_string())));
        let array = |name: &str| Addr::of(d::TYPE, self.oid(Object::Array(name.to_string())));
        let schema_of = |name: &str| Addr::of(d::NAMESPACE, self.namespace(parts(name).0));
        let collation = |oid: u32| (oid != DEFAULT_COLLATION).then(|| Addr::of(d::COLLATION, oid));
        let constraint = |table: &str, name: &str| {
            Addr::of(
                d::CONSTRAINT,
                self.oid(Object::Constraint(table.to_string(), name.to_string())),
            )
        };
        let trigger = |table: &str, name: &str, place: u8| {
            Addr::of(
                d::TRIGGER,
                self.oid(Object::Trigger(table.to_string(), name.to_string(), place)),
            )
        };
        let tree = |kind: &str, oid: u32, part: i64| crate::deparse::stored_tree(&cat, kind, oid, part).ok();
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = class(&n);
            out.many(rel, vec![schema_of(&n)], 'n');
            for (i, c) in tb.columns.iter().enumerate() {
                let col = Addr::column(rel.oid, i as i32 + 1);
                out.one(col, Addr::of(d::TYPE, self.facts(c.ty, c.domain.as_deref()).oid), 'n');
                if let Some(k) = collation(self.column_collation(c)) {
                    out.one(col, k, 'n');
                }
            }
            out.one(ty(&n), rel, 'i');
            out.one(array(&n), ty(&n), 'i');
            for (i, c) in tb.columns.iter().enumerate() {
                if c.default.is_none() && c.generated.is_none() {
                    continue;
                }
                let def = Addr::of(d::ATTRDEF, self.oid(Object::Default(n.clone(), c.name.clone())));
                let generated = if c.generated.is_some() { 'i' } else { 'a' };
                out.one(def, Addr::column(rel.oid, i as i32 + 1), generated);
                if let Some(node) = tree("attrdef", rel.oid, i as i64 + 1) {
                    let (own, other) = d::single_rel(&cat, &node, rel.oid);
                    out.all(def, &own, 'n');
                    out.all(def, &other, 'n');
                }
            }
            if self.release >= 18 {
                let own = parts(&n).1;
                for (i, c) in tb.columns.iter().enumerate().filter(|(_, c)| c.not_null) {
                    let con = constraint(&n, &format!("{own}_{}_not_null", c.name));
                    out.one(con, Addr::column(rel.oid, i as i32 + 1), 'a');
                }
            }
            for (ci, k) in tb.checks.iter().enumerate() {
                let con = constraint(&n, &k.name);
                let Some(node) = tree("check", rel.oid, ci as i64) else {
                    continue;
                };
                let cols = d::columns_read(&node);
                let keys = match cols.is_empty() {
                    true => vec![rel],
                    false => cols.iter().map(|&c| Addr::column(rel.oid, c.into())).collect(),
                };
                out.many(con, keys, 'a');
                let (own, other) = d::single_rel(&cat, &node, rel.oid);
                out.all(con, &own, 'n');
                out.all(con, &other, 'n');
            }
            let schema = parts(&n).0;
            for i in self.indexes_of(tb) {
                let index = class(&format!("{schema}.{}", i.name));
                let cols: Vec<Addr> = i
                    .keys
                    .iter()
                    .chain(&i.include)
                    .map(|&k| if k > 0 { Addr::column(rel.oid, k.into()) } else { rel })
                    .collect();
                match self.index_constraint(&n, &i) {
                    Some(_) => {
                        let con = constraint(&n, &i.name);
                        out.many(con, cols, 'a');
                        out.one(index, con, 'i');
                        if i.deferrable {
                            out.one(trigger(&n, &i.name, crate::oids::trigger::RECHECK), con, 'i');
                        }
                    }
                    None => {
                        let simple: Vec<Addr> = cols.into_iter().filter(|a| a.sub != 0).collect();
                        out.many(index, if simple.is_empty() { vec![rel] } else { simple }, 'a');
                    }
                }
                out.many(index, i.collations.iter().filter_map(|&c| collation(c)).collect(), 'n');
                let (exprs, pred) = crate::deparse::defs::index_has_trees(self.catalog, index.oid);
                for (has, kind) in [(exprs, "indexprs"), (pred, "indpred")] {
                    if let Some(node) = tree(kind, index.oid, 0).filter(|_| has) {
                        let (own, other) = d::single_rel(&cat, &node, rel.oid);
                        out.all(index, &own, 'a');
                        out.all(index, &other, 'n');
                    }
                }
            }
            for f in &tb.foreign_keys {
                let con = constraint(&n, &f.name);
                let keys = f.columns.iter().map(|&c| Addr::column(rel.oid, c as i32 + 1)).collect();
                out.many(con, keys, 'a');
                let parent = full(&f.parent);
                let prel = class(&parent);
                let mut refs: Vec<Addr> = f
                    .parent_columns
                    .iter()
                    .map(|&c| Addr::column(prel.oid, c as i32 + 1))
                    .collect();
                refs.push(class(&format!("{}.{}", parts(&parent).0, f.key)));
                out.many(con, refs, 'n');
                if f.enforced {
                    use crate::oids::trigger as at;
                    for place in [at::ACTION_DELETE, at::ACTION_UPDATE, at::CHECK_INSERT, at::CHECK_UPDATE] {
                        out.one(trigger(&n, &f.name, place), con, 'i');
                    }
                }
            }
        }
        for s in &self.catalog.sequences {
            let n = full(&s.name);
            let seq = class(&n);
            out.many(seq, vec![schema_of(&n)], 'n');
            let Some((t, c)) = &s.owned_by else {
                continue;
            };
            let owner = full(t);
            let column = self
                .catalog
                .tables
                .iter()
                .find(|x| full(&x.name) == owner)
                .and_then(|x| x.columns.iter().position(|y| y.name == *c));
            if let Some(i) = column {
                let kind = if s.identity { 'i' } else { 'a' };
                out.one(seq, Addr::column(class(&owner).oid, i as i32 + 1), kind);
            }
        }
        for v in &self.catalog.views {
            let n = full(&v.name);
            let view = class(&n);
            out.many(view, vec![schema_of(&n)], 'n');
            let analyzed = crate::deparse::query::view_query(&cat, v).ok();
            if let Some((q, cols)) = &analyzed {
                for (i, ((_, t, _), target)) in cols.iter().zip(q.output()).enumerate() {
                    let col = Addr::column(view.oid, i as i32 + 1);
                    out.one(col, Addr::of(d::TYPE, *t), 'n');
                    if let Some(k) = collation(d::output_collation(&cat, q, &target.expr)) {
                        out.one(col, k, 'n');
                    }
                }
            }
            out.one(ty(&n), view, 'i');
            out.one(array(&n), ty(&n), 'i');
            let rule = Addr::of(d::REWRITE, self.oid(Object::Rule(n.clone())));
            out.one(rule, view, 'i');
            if let Some((q, _)) = &analyzed {
                // Before 16 a view's question kept OLD and NEW, the view itself, in its range table.
                let mut refs = d::of_query(&cat, q);
                if self.release < 16 {
                    refs.push(view);
                    refs = d::eliminated(refs);
                }
                out.all(rule, &refs, 'n');
            }
        }
        let own_types = self.pg_types();
        for t in &self.catalog.types {
            let n = full(&t.name);
            let typ = ty(&n);
            out.many(typ, vec![schema_of(&n)], 'n');
            out.one(array(&n), typ, 'i');
            match &t.kind {
                crate::catalog::TypeKind::Enum { .. } => {}
                crate::catalog::TypeKind::Composite { fields } => {
                    let rel = class(&n);
                    out.one(rel, typ, 'i');
                    for (i, (_, fty, fcoll)) in fields.iter().enumerate() {
                        let col = Addr::column(rel.oid, i as i32 + 1);
                        let (toid, coll) = self.field_facts(fty, fcoll.as_deref());
                        out.one(col, Addr::of(d::TYPE, toid), 'n');
                        if let Some(k) = collation(coll) {
                            out.one(col, k, 'n');
                        }
                    }
                }
                crate::catalog::TypeKind::Range(r) => {
                    let f = self.range_facts(&cat, r);
                    let mut refs = vec![
                        Addr::of(d::TYPE, f.sub),
                        Addr::of(d::OPCLASS, f.opclass),
                        Addr::of(d::PROC, f.subdiff),
                    ];
                    if f.collation != 0 {
                        refs.push(Addr::of(d::COLLATION, f.collation));
                    }
                    out.many(typ, refs, 'n');
                    let m = full(&r.multirange);
                    let multi = ty(&m);
                    out.many(multi, vec![schema_of(&m)], 'n');
                    out.one(array(&m), multi, 'i');
                    out.one(multi, typ, 'i');
                    // The five constructors DefineRange makes, each on its schema, its result and
                    // its arguments, and made a part of its range or multirange.
                    let sub_key = crate::regs::type_oid(&r.subtype, crate::regs::LATEST)
                        .map(|o| crate::regs::reg_text(RegKind::Type, o, crate::regs::LATEST))
                        .unwrap_or_else(|_| r.subtype.clone());
                    let shown = match parts(&n) {
                        ("public", x) => x.to_string(),
                        _ => n.clone(),
                    };
                    let made = [
                        (&n, format!("{sub_key},{sub_key}"), vec![f.sub, f.sub], typ),
                        (&n, format!("{sub_key},{sub_key},text"), vec![f.sub, f.sub, 25], typ),
                        (&m, String::new(), vec![], multi),
                        (&m, shown.clone(), vec![typ.oid], multi),
                        (&m, format!("{shown}[]"), vec![array(&n).oid], multi),
                    ];
                    for (name, key, args, owner) in made {
                        let p = Addr::of(d::PROC, self.proc_oid(name, &key));
                        let mut refs = vec![schema_of(name), Addr::of(LANGUAGE, 12), owner];
                        refs.extend(args.iter().map(|&a| Addr::of(d::TYPE, a)));
                        out.many(p, refs, 'n');
                        out.one(p, owner, 'i');
                    }
                    let cast = Addr::of(d::CAST, self.oid(Object::Cast(n.clone())));
                    let by = Addr::of(d::PROC, self.proc_oid(&m, &shown));
                    out.many(cast, vec![typ, multi, by], 'i');
                }
            }
        }
        for dm in &self.catalog.domains {
            let n = full(&dm.name);
            let typ = ty(&n);
            let base = own_types
                .iter()
                .find(|r| r.get("oid").cloned().flatten() == Some(typ.oid.to_string()))
                .and_then(|r| r.get("typbasetype").cloned().flatten()?.parse().ok())
                .unwrap_or(0);
            let coll = collation(self.domain_collation(&dm.name, 0));
            let mut refs = vec![schema_of(&n), Addr::of(d::TYPE, base)];
            refs.extend(coll);
            out.many(typ, refs, 'n');
            if let Some(node) = tree("domaindefault", typ.oid, 0).filter(|_| dm.default.is_some()) {
                out.all(typ, &d::of_expr(&cat, &node), 'n');
            }
            out.many(array(&n), coll.into_iter().collect(), 'n');
            out.one(array(&n), typ, 'i');
            if dm.not_null && self.release >= 17 {
                let own = parts(&n).1;
                let name = dm.not_null_name.clone().unwrap_or_else(|| format!("{own}_not_null"));
                out.one(constraint(&n, &name), typ, 'a');
            }
            for (ki, k) in dm.checks.iter().enumerate() {
                let con = constraint(&n, &k.name);
                out.one(con, typ, 'a');
                if let Some(node) = tree("domaincheck", typ.oid, ki as i64) {
                    out.all(con, &d::of_expr(&cat, &node), 'n');
                }
            }
        }
        for f in &self.catalog.functions {
            let n = full(&f.name);
            let key = f.args.iter().map(|t| t.pg_name()).collect::<Vec<_>>().join(",");
            let p = Addr::of(d::PROC, self.proc_oid(&n, &key));
            let mut refs = vec![
                schema_of(&n),
                Addr::of(LANGUAGE, 14),
                Addr::of(d::TYPE, self.facts(f.returns, None).oid),
            ];
            refs.extend(f.args.iter().map(|&a| Addr::of(d::TYPE, self.facts(a, None).oid)));
            // What a body in SQL's own words and its DEFAULTs name: through 18 each recorded on its
            // own after the rest, from 19 gathered with the rest and recorded once.
            let body = match crate::deparse::funcs::own_body(&cat, f) {
                Ok(Some(q)) => d::of_query(&cat, &q),
                _ => Vec::new(),
            };
            let defaults = match f.defaults.is_empty() {
                true => Vec::new(),
                false => crate::deparse::defaults_of(&cat, f)
                    .map(|nodes| d::of_expr(&cat, &crate::deparse::analyze::Node::List(nodes)))
                    .unwrap_or_default(),
            };
            if self.release >= 19 {
                refs.extend(body);
                refs.extend(defaults);
                out.many(p, refs, 'n');
            } else {
                out.many(p, refs, 'n');
                out.many(p, body, 'n');
                out.many(p, defaults, 'n');
            }
        }
        for c in &self.catalog.collations {
            let n = full(&c.name);
            let coll = Addr::of(d::COLLATION, self.oid(Object::Collation(n.clone())));
            out.many(coll, vec![schema_of(&n)], 'n');
        }
        out.rows
    }

    fn indexes(&self) -> Vec<Row> {
        let mut out = Vec::new();
        let cat = crate::deparse::cat::Cat::of(self.catalog);
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            let schema = parts(&n).0;
            for i in self.indexes_of(tb) {
                let vector = |k: &[i16]| k.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" ");
                let all: Vec<i16> = i.keys.iter().chain(&i.include).copied().collect();
                let ioid = self.oid(Object::Class(format!("{schema}.{}", i.name)));
                let (classes, options) = crate::deparse::defs::index_classes(&cat, ioid)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| (vec![0; i.keys.len()], vec![0; i.keys.len()]));
                let words = |v: Vec<String>| v.join(" ");
                out.push(row(&[
                    ("indexrelid", ioid.to_string()),
                    ("indrelid", rel.to_string()),
                    ("indnatts", all.len().to_string()),
                    ("indnkeyatts", i.keys.len().to_string()),
                    ("indisunique", t(i.unique || i.keyed)),
                    ("indnullsnotdistinct", t(!i.nulls_distinct)),
                    ("indisprimary", t(i.primary)),
                    ("indisexclusion", t(i.exclusion)),
                    ("indimmediate", "t".into()),
                    ("indisclustered", t(i.clustered)),
                    ("indisvalid", "t".into()),
                    ("indcheckxmin", "f".into()),
                    ("indisready", "t".into()),
                    ("indislive", "t".into()),
                    ("indisreplident", "f".into()),
                    ("indkey", vector(&all)),
                    (
                        "indcollation",
                        i.collations.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" "),
                    ),
                    ("indclass", words(classes.iter().map(u32::to_string).collect())),
                    ("indoption", words(options.iter().map(i16::to_string).collect())),
                ]));
                let (exprs, pred) = crate::deparse::defs::index_has_trees(self.catalog, ioid);
                if let Some(r) = out.last_mut() {
                    if exprs {
                        r.insert("indexprs", Some(crate::deparse::tree_handle("indexprs", ioid, 0)));
                    }
                    if pred {
                        r.insert("indpred", Some(crate::deparse::tree_handle("indpred", ioid, 0)));
                    }
                }
            }
        }
        out
    }

    fn labels(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for ty in &self.catalog.types {
            if let crate::catalog::TypeKind::Enum { labels } = &ty.kind {
                let n = full(&ty.name);
                let oid = self.oid(Object::Type(n.clone()));
                for (i, l) in labels.iter().enumerate() {
                    out.push(row(&[
                        (
                            "oid",
                            self.oid(Object::Label(n.clone(), l.clone())).to_string(),
                        ),
                        ("enumtypid", oid.to_string()),
                        ("enumsortorder", (i + 1).to_string()),
                        ("enumlabel", l.clone()),
                    ]));
                }
            }
        }
        out
    }

    fn sequences(&self) -> Vec<Row> {
        self.catalog
            .sequences
            .iter()
            .map(|s| {
                let ty = crate::regs::type_oid(&s.ty.column().pg_name(), self.release).unwrap_or(0);
                row(&[
                    ("seqrelid", self.oid(Object::Class(full(&s.name))).to_string()),
                    ("seqtypid", ty.to_string()),
                    ("seqstart", s.start.to_string()),
                    ("seqincrement", s.increment.to_string()),
                    ("seqmax", s.max.to_string()),
                    ("seqmin", s.min.to_string()),
                    ("seqcache", s.cache.to_string()),
                    ("seqcycle", t(s.cycle)),
                ])
            })
            .collect()
    }

    fn collations(&self) -> Vec<Row> {
        self.catalog
            .collations
            .iter()
            .map(|c| {
                let n = full(&c.name);
                let (schema, own) = parts(&n);
                let provider = match c.provider.as_str() {
                    "icu" => "i",
                    "builtin" => "b",
                    _ => "c",
                };
                let mut r = row(&[
                    ("oid", self.oid(Object::Collation(n.clone())).to_string()),
                    ("collname", own.to_string()),
                    ("collnamespace", self.namespace(schema).to_string()),
                    ("collowner", "10".into()),
                    ("collprovider", provider.into()),
                    ("collisdeterministic", t(c.deterministic)),
                    ("collencoding", "-1".into()),
                ]);
                r.insert("collcollate", c.lc_collate.clone());
                r.insert("collctype", c.lc_ctype.clone());
                r.insert("colllocale", c.locale.clone());
                r.insert("colliculocale", c.locale.clone().filter(|_| provider == "i"));
                r.insert("collicurules", c.rules.clone());
                r
            })
            .collect()
    }

    fn procs(&self) -> Vec<Row> {
        let proc_oid = |n: &str, args: &str| {
            self.catalog
                .oids
                .each()
                .find_map(|(oid, x)| match x {
                    Object::Proc(m, a) if m == n && a == args => Some(oid),
                    _ => None,
                })
                .unwrap_or(0)
        };
        let mut out = Vec::new();
        for f in &self.catalog.functions {
            let n = full(&f.name);
            let (schema, own) = parts(&n);
            let args: Vec<String> = f
                .args
                .iter()
                .map(|&a| self.facts(a, None).oid.to_string())
                .collect();
            let key = f.args.iter().map(|t| t.pg_name()).collect::<Vec<_>>().join(",");
            let oid = proc_oid(&n, &key);
            let mut fields = vec![
                ("oid", oid.to_string()),
                ("proname", own.to_string()),
                ("pronamespace", self.namespace(schema).to_string()),
                ("proowner", "10".into()),
                ("prolang", "14".into()),
                ("procost", "100".into()),
                ("prorows", "0".into()),
                ("provariadic", "0".into()),
                ("prosupport", "0".into()),
                ("prokind", "f".into()),
                ("prosecdef", "f".into()),
                ("proleakproof", "f".into()),
                ("proisstrict", t(f.strict)),
                ("proretset", "f".into()),
                ("provolatile", f.volatility.to_string()),
                ("proparallel", f.parallel.to_string()),
                ("pronargs", f.args.len().to_string()),
                ("pronargdefaults", f.defaults.len().to_string()),
                ("prorettype", self.facts(f.returns, None).oid.to_string()),
                ("proargtypes", args.join(" ")),
                ("prosrc", f.source.clone()),
            ];
            // Every parameter's name where any has one, an empty one for each that has none.
            if f.arg_names.iter().any(Option::is_some) {
                let names: Vec<String> = f.arg_names.iter().map(|n| n.clone().unwrap_or_default()).collect();
                fields.push(("proargnames", text_array(&names)));
            }
            // The DEFAULTs and a body in SQL's own words are parse trees, each named by its object.
            if !f.defaults.is_empty() {
                fields.push(("proargdefaults", crate::deparse::tree_handle("procdefaults", oid, 0)));
            }
            if f.form != crate::catalog::BodyForm::Quoted {
                fields.push(("prosqlbody", crate::deparse::tree_handle("procbody", oid, 0)));
            }
            out.push(row(&fields));
        }
        // Each range's constructors, as DefineRange makes them: the range's of two bounds and of two
        // and their inclusivity, the multirange's of none, of one range and of any number.
        for ty in &self.catalog.types {
            let crate::catalog::TypeKind::Range(r) = &ty.kind else {
                continue;
            };
            let n = full(&ty.name);
            let m = full(&r.multirange);
            let sub = crate::regs::type_oid(&r.subtype, self.release).unwrap_or(0);
            let sub_key = crate::regs::type_oid(&r.subtype, crate::regs::LATEST)
                .map(|o| crate::regs::reg_text(crate::regs::RegKind::Type, o, crate::regs::LATEST))
                .unwrap_or_else(|_| r.subtype.clone());
            let shown = match parts(&n) {
                ("public", x) => x.to_string(),
                _ => n.clone(),
            };
            let range = self.oid(Object::Type(n.clone()));
            let range_array = self.oid(Object::Array(n.clone()));
            let multi = self.oid(Object::Type(m.clone()));
            let made = [
                (&n, format!("{sub_key},{sub_key}"), format!("{sub} {sub}"), range, false, "range_constructor2"),
                (&n, format!("{sub_key},{sub_key},text"), format!("{sub} {sub} 25"), range, false, "range_constructor3"),
                (&m, String::new(), String::new(), multi, true, "multirange_constructor0"),
                (&m, shown.clone(), range.to_string(), multi, true, "multirange_constructor1"),
                (&m, format!("{shown}[]"), range_array.to_string(), multi, true, "multirange_constructor2"),
            ];
            for (name, key, args, ret, strict, src) in made {
                let (schema, own) = parts(name);
                let variadic = src == "multirange_constructor2";
                let mut fields = vec![
                    ("oid", proc_oid(name, &key).to_string()),
                    ("proname", own.to_string()),
                    ("pronamespace", self.namespace(schema).to_string()),
                    ("proowner", "10".into()),
                    ("prolang", "12".into()),
                    ("procost", "1".into()),
                    ("prorows", "0".into()),
                    ("provariadic", if variadic { range.to_string() } else { "0".into() }),
                    ("prosupport", "0".into()),
                    ("prokind", "f".into()),
                    ("prosecdef", "f".into()),
                    ("proleakproof", "f".into()),
                    ("proisstrict", t(strict)),
                    ("proretset", "f".into()),
                    ("provolatile", "i".into()),
                    ("proparallel", "s".into()),
                    ("pronargs", args.split(' ').filter(|a| !a.is_empty()).count().to_string()),
                    ("pronargdefaults", "0".into()),
                    ("prorettype", ret.to_string()),
                    ("proargtypes", args.clone()),
                    ("prosrc", src.into()),
                ];
                if variadic {
                    fields.push(("proallargtypes", format!("{{{range_array}}}")));
                    fields.push(("proargmodes", "{v}".into()));
                }
                out.push(row(&fields));
            }
        }
        out
    }

    /// What a range's row of pg_range says, as DefineRange finds it: its subtype, its collation (its
    /// COLLATION's, else the subtype's), its subtype's operator class (the one named, else the
    /// default B-tree one), and its subtype_diff function.
    fn range_facts(&self, cat: &crate::deparse::cat::Cat, r: &crate::catalog::RangeType) -> RangeFacts {
        let sub = crate::regs::type_oid(&r.subtype, self.release)
            .ok()
            .or_else(|| self.catalog.oids.of(&Object::Type(full(&r.subtype))))
            .unwrap_or(0);
        let collation = if cat.collatable(sub) {
            match &r.collation {
                Some(c) => self.collation_oid(c),
                None => cat.ty(sub).map_or(0, |t| t.collation),
            }
        } else {
            0
        };
        let opclass = match &r.opclass {
            Some(n) => crate::deparse::defs::opclass_oid(self.release, n, 403),
            None => crate::deparse::defs::default_btree_opclass(cat, sub),
        };
        let function = |name: &Option<String>, args: &[u32], returns: Option<u32>| -> u32 {
            let Some(name) = name else { return 0 };
            let (schema, own) = match name.rsplit_once('.') {
                Some((s, n)) => (Some(crate::deparse::query::namespace_oid(cat, s)), n),
                None => (None, name.as_str()),
            };
            cat.procs_named(own, schema)
                .into_iter()
                .find(|p| p.args == args && returns.is_none_or(|t| p.result == t))
                .map_or(0, |p| p.oid)
        };
        RangeFacts {
            sub,
            collation,
            opclass,
            subdiff: function(&r.subtype_diff, &[sub, sub], Some(701)),
        }
    }

    fn ranges(&self) -> Vec<Row> {
        let mut out = Vec::new();
        let mut cat = None;
        for ty in &self.catalog.types {
            if let crate::catalog::TypeKind::Range(r) = &ty.kind {
                let n = full(&ty.name);
                let cat = cat.get_or_insert_with(|| crate::deparse::cat::Cat::of(self.catalog));
                let f = self.range_facts(cat, r);
                out.push(row(&[
                    ("rngtypid", self.oid(Object::Type(n.clone())).to_string()),
                    ("rngsubtype", f.sub.to_string()),
                    (
                        "rngmultitypid",
                        self.oid(Object::Type(full(&r.multirange))).to_string(),
                    ),
                    ("rngcollation", f.collation.to_string()),
                    ("rngsubopc", f.opclass.to_string()),
                    // A range naming a CANONICAL is refused by name.
                    ("rngcanonical", "0".into()),
                    ("rngsubdiff", f.subdiff.to_string()),
                ]));
            }
        }
        out
    }

    /// Each range's cast to its multirange, as DefineRange makes it: explicit, by the multirange's
    /// constructor of one range.
    fn casts(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for ty in &self.catalog.types {
            if let crate::catalog::TypeKind::Range(r) = &ty.kind {
                let n = full(&ty.name);
                let m = full(&r.multirange);
                let shown = match parts(&n) {
                    ("public", x) => x.to_string(),
                    _ => n.clone(),
                };
                out.push(row(&[
                    ("oid", self.oid(Object::Cast(n.clone())).to_string()),
                    ("castsource", self.oid(Object::Type(n.clone())).to_string()),
                    ("casttarget", self.oid(Object::Type(m.clone())).to_string()),
                    ("castfunc", self.proc_oid(&m, &shown).to_string()),
                    ("castcontext", "e".into()),
                    ("castmethod", "f".into()),
                ]));
            }
        }
        out
    }

    fn rules(&self) -> Vec<Row> {
        self.catalog
            .views
            .iter()
            .map(|v| {
                let n = full(&v.name);
                row(&[
                    ("oid", self.oid(Object::Rule(n.clone())).to_string()),
                    ("rulename", "_RETURN".into()),
                    ("ev_class", self.oid(Object::Class(n)).to_string()),
                    ("ev_type", "1".into()),
                    ("ev_enabled", "O".into()),
                    ("is_instead", "t".into()),
                ])
            })
            .collect()
    }
}

/// pg_language's OID, a catalog a row of pg_depend names.
const LANGUAGE: u32 = 2612;

thread_local! {
    /// Whether the file's views are being read for their columns, which reading one may ask again.
    static READING_VIEWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The dimensions a type's name writes: `int[]` and `int ARRAY[3]` one, `int[][]` two.
pub(crate) fn array_dims(dt: &sqlparser::ast::DataType) -> i16 {
    use sqlparser::ast::{ArrayElemTypeDef as E, DataType as D};
    match dt {
        D::Array(
            E::SquareBracket(inner, _)
            | E::AngleBracket(inner)
            | E::Parenthesis(inner)
            | E::Qualified(inner, _),
        ) => 1 + array_dims(inner),
        _ => 0,
    }
}

/// pg_depend's rows as they are recorded: each from an object to those it depends on, by how.
#[derive(Default)]
struct Deps {
    rows: Vec<Row>,
}

impl Deps {
    /// As `recordDependencyOn`: one dependency, unless the object depended on is pinned.
    fn one(&mut self, on: crate::deparse::depend::Addr, to: crate::deparse::depend::Addr, kind: char) {
        self.all(on, &[to], kind);
    }

    /// As `recordMultipleDependencies`: each dependency in turn, none on a pinned object; none from
    /// an object the file gave no OID.
    fn all(&mut self, on: crate::deparse::depend::Addr, to: &[crate::deparse::depend::Addr], kind: char) {
        if on.oid == 0 {
            return;
        }
        for a in to.iter().filter(|a| !a.pinned()) {
            self.rows.push(row(&[
                ("classid", on.class.to_string()),
                ("objid", on.oid.to_string()),
                ("objsubid", on.sub.to_string()),
                ("refclassid", a.class.to_string()),
                ("refobjid", a.oid.to_string()),
                ("refobjsubid", a.sub.to_string()),
                ("deptype", kind.to_string()),
            ]));
        }
    }

    /// As `record_object_address_dependencies`: the objects each kept once first.
    fn many(&mut self, on: crate::deparse::depend::Addr, to: Vec<crate::deparse::depend::Addr>, kind: char) {
        let to = crate::deparse::depend::eliminated(to);
        self.all(on, &to, kind);
    }
}

/// A column's collation, as pg_attribute's attcollation gives it: of a file's table or composite,
/// or a built-in relation's as carried.
pub(crate) fn attcollation_of(catalog: &Catalog, relid: u32, attno: i32) -> u32 {
    let f = File::of(catalog);
    let at = (attno - 1) as usize;
    if let Some(Object::Class(n)) = catalog.oids.object(relid) {
        if let Some(t) = catalog.tables.iter().find(|t| full(&t.name) == *n) {
            return t.columns.get(at).map_or(0, |c| f.column_collation(c));
        }
        for ty in &catalog.types {
            if let crate::catalog::TypeKind::Composite { fields } = &ty.kind {
                if full(&ty.name) == *n {
                    return fields.get(at).map_or(0, |(_, t, c)| f.field_facts(t, c.as_deref()).1);
                }
            }
        }
        if let Some(v) = catalog.views.iter().find(|v| full(&v.name) == *n) {
            let cat = crate::deparse::cat::Cat::of(catalog);
            if let Ok((q, _)) = crate::deparse::query::view_query(&cat, v) {
                if let Some(t) = q.output().nth(at) {
                    return crate::deparse::depend::output_collation(&cat, &q, &t.expr);
                }
            }
        }
        return 0;
    }
    built_in("pg_attribute", catalog.release)
        .iter()
        .find(|r| {
            r.get("attrelid").cloned().flatten() == Some(relid.to_string())
                && r.get("attnum").cloned().flatten() == Some(attno.to_string())
        })
        .and_then(|r| r.get("attcollation").cloned().flatten()?.parse().ok())
        .unwrap_or(0)
}

/// What a range's row of pg_range says of it.
struct RangeFacts {
    sub: u32,
    collation: u32,
    opclass: u32,
    subdiff: u32,
}

/// An index of a table's, as pg_class, pg_index and pg_constraint say it.
struct IndexFacts {
    name: String,
    method: Method,
    keys: Vec<i16>,
    /// Each key's collation, 0 for a key of a type that has none.
    collations: Vec<u32>,
    include: Vec<i16>,
    unique: bool,
    primary: bool,
    exclusion: bool,
    clustered: bool,
    nulls_distinct: bool,
    /// Whether it stands for a PRIMARY KEY or UNIQUE, WITHOUT OVERLAPS among them.
    keyed: bool,
    deferrable: bool,
}

/// A type's name as PostgreSQL's type reader reads it, without its modifiers.
fn type_text(t: ColumnType) -> String {
    use ColumnType as C;
    match t {
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
            crate::composed::Composed::Array { element, .. } => format!("{}[]", type_text(*element)),
            _ => t.pg_name(),
        },
        t => t.pg_name(),
    }
}

/// Whether a statement names one of pg_catalog's tables, or one of PostgreSQL's views, as a
/// relation, or one of information_schema's functions that read them.
pub(crate) fn names_catalog(sql: &str) -> bool {
    crate::read::sysviews::words_of(sql).iter().any(|w| {
        table_named(w).is_some()
            || crate::read::sysviews::view_named(w).is_some()
            || crate::read::sysviews::names_schema_function(w)
    })
}

/// The name the file stands under as a database inside PostgreSQL's views, which their
/// `current_database()` gives and pg_database's row holds; no answer shows it.
pub(crate) const DATABASE: &str = "snail";

/// A catalog table as one of PostgreSQL's views reads it: as `relation` gives it, each owner read
/// as the role its row names; pg_authid those roles alone, pg_database the file alone,
/// under the name `DATABASE`, its encoding UTF8 and its collation the file's; any other table only
/// a server holds empty. What the view's columns show of them is refused where a statement asks it.
pub(crate) fn relation_in_view(catalog: &Catalog, table: &str) -> Result<Relation> {
    if answered(table).is_ok() {
        let mut r = relation(catalog, table)?;
        for (n, refused) in r.names.iter().zip(r.refused.iter_mut()) {
            if n.ends_with("owner") {
                *refused = None;
            }
        }
        return Ok(r);
    }
    let fields = crate::rowtypes::fields_at(table, catalog.release)?;
    let collation = match catalog.collation.is_empty() {
        true => crate::file::COLLATION,
        false => &catalog.collation,
    };
    let value = |column: &str| -> Option<String> {
        Some(
            match (table, column) {
                ("pg_database", "datname") => DATABASE,
                ("pg_database", "encoding") => "6",
                ("pg_database", "datlocprovider") => "c",
                ("pg_database", "datistemplate" | "dathasloginevt") => "f",
                ("pg_database", "datallowconn") => "t",
                ("pg_database", "datconnlimit") => "-1",
                ("pg_database", "dattablespace") => "1663",
                ("pg_database", "datcollate" | "datctype") => collation,
                _ => return None,
            }
            .to_string(),
        )
    };
    // The roles the carried rows and the file's own name as owners: the bootstrap superuser, 10,
    // and pg_database_owner, 6171, which owns public from 15 on.
    let role = |oid: &str| -> Vec<Option<String>> {
        fields
            .iter()
            .map(|f| (f.0 == "oid").then(|| oid.to_string()))
            .collect()
    };
    let rows = match table {
        "pg_authid" => vec![role("10"), role("6171")],
        "pg_database" => vec![fields.iter().map(|f| value(&f.0)).collect()],
        _ => Vec::new(),
    };
    Ok(Relation {
        names: fields.iter().map(|f| f.0.clone()).collect(),
        types: fields
            .iter()
            .map(|f| f.1.unwrap_or(ColumnType::Text(None)))
            .collect(),
        refused: vec![None; fields.len()],
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_carried_catalog_reads_back_each_release() {
        for release in crate::regs::RELEASES {
            let types = built_in("pg_type", release);
            assert!(types.len() > 400, "{release}: {}", types.len());
            let int4 = types
                .iter()
                .find(|r| r.get("oid").cloned().flatten().as_deref() == Some("23"))
                .unwrap();
            assert_eq!(int4.get("typname").cloned().flatten().as_deref(), Some("int4"));
            assert_eq!(int4.get("typlen").cloned().flatten().as_deref(), Some("4"));
        }
        assert_eq!(unescaped("a\\tb\\\\c\\101"), Some("a\tb\\cA".into()));
        assert_eq!(unescaped("\\N"), None);
    }
}
