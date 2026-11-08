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

    /// A schema's OID: pg_catalog's and public's as initdb fixes them, the file's own by its DDL.
    fn namespace(&self, schema: &str) -> u32 {
        match schema {
            "pg_catalog" => 11,
            "public" => 2200,
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
            let base = self
                .catalog
                .domains
                .iter()
                .find(|x| x.name == d)
                .and_then(|x| x.base_type);
            let mut f = base.map_or_else(|| varlena(oid, "i"), |b| self.facts(b, None));
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
            _ => Vec::new(),
        })
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
        for o in &tb.orders {
            out.push(IndexFacts {
                name: o.name.clone(),
                method: o.method,
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
            match out.iter_mut().find(|x| x.name == e.index) {
                Some(x) => x.keys.push(e.column as i16 + 1),
                None => out.push(IndexFacts {
                    name: e.index.clone(),
                    method: e.method,
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
            out.push(self.class(&name, 'v', reltype, 0, v.columns.len()));
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
            out.push(r);
            out.push(self.array_of(&n, oid, &base.align));
        }
        out
    }

    /// A relation's rows of pg_attribute, its columns numbered from 1.
    fn attribute(&self, rel: u32, num: i16, name: &str, ty: ColumnType, domain: Option<&str>) -> Row {
        let f = self.facts(ty, domain);
        let dims = match ty {
            ColumnType::Composed(r) if matches!(r.def(), crate::composed::Composed::Array { .. }) => 1,
            _ => 0,
        };
        row(&[
            ("attrelid", rel.to_string()),
            ("attname", name.to_string()),
            ("atttypid", f.oid.to_string()),
            ("attlen", f.len.clone()),
            ("attnum", num.to_string()),
            ("atttypmod", typmod_of(ty).to_string()),
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

    fn attributes(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            for (i, c) in tb.columns.iter().enumerate() {
                let mut r = self.attribute(rel, i as i16 + 1, &c.name, c.ty, c.domain.as_deref());
                r.insert("attnotnull", Some(t(c.not_null)));
                r.insert("atthasdef", Some(t(c.default.is_some())));
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
                for (k, &col) in i.keys.iter().chain(&i.include).enumerate() {
                    let (name, ty, domain) = match col {
                        c if c > 0 => {
                            let c = &tb.columns[c as usize - 1];
                            (c.name.clone(), c.ty, c.domain.as_deref())
                        }
                        _ => ("expr".to_string(), ColumnType::Text(None), None),
                    };
                    out.push(self.attribute(rel, k as i16 + 1, &name, ty, domain));
                }
            }
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
                let kind = match (i.primary, i.keyed || !i.exclusion, i.unique) {
                    (true, _, _) => 'p',
                    (_, true, _) if i.keyed => 'u',
                    (_, false, _) => 'x',
                    (_, _, true)
                        if self
                            .catalog
                            .oids
                            .of(&Object::Constraint(n.clone(), i.name.clone()))
                            .is_some() =>
                    {
                        'u'
                    }
                    _ => continue,
                };
                let index = self.oid(Object::Class(format!("{schema}.{}", i.name)));
                let mut r = self.constraint(&n, &i.name, kind, rel, index, &i.keys, i.deferrable);
                r.insert("conperiod", Some(t(i.keyed && i.exclusion)));
                out.push(r);
            }
            for (ci, c) in tb.checks.iter().enumerate() {
                let mut r = self.constraint(&n, &c.name, 'c', rel, 0, &[], false);
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

    fn indexes(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for tb in self.tables() {
            let n = full(&tb.name);
            let rel = self.oid(Object::Class(n.clone()));
            let schema = parts(&n).0;
            for i in self.indexes_of(tb) {
                let vector = |k: &[i16]| k.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" ");
                let all: Vec<i16> = i.keys.iter().chain(&i.include).copied().collect();
                let zeros = |n: usize| vec!["0"; n].join(" ");
                out.push(row(&[
                    (
                        "indexrelid",
                        self.oid(Object::Class(format!("{schema}.{}", i.name)))
                            .to_string(),
                    ),
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
                    ("indcollation", zeros(i.keys.len())),
                    ("indclass", zeros(i.keys.len())),
                    ("indoption", zeros(i.keys.len())),
                ]));
                let ioid = self.oid(Object::Class(format!("{schema}.{}", i.name)));
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
        self.catalog
            .functions
            .iter()
            .map(|f| {
                let n = full(&f.name);
                let (schema, own) = parts(&n);
                let args: Vec<String> = f
                    .args
                    .iter()
                    .map(|&a| self.facts(a, None).oid.to_string())
                    .collect();
                let o = self.catalog.oids.each().find_map(|(oid, x)| match x {
                    Object::Proc(m, a)
                        if *m == n
                            && *a
                                == f.args
                                    .iter()
                                    .map(|t| t.pg_name())
                                    .collect::<Vec<_>>()
                                    .join(",") =>
                    {
                        Some(oid)
                    }
                    _ => None,
                });
                row(&[
                    ("oid", o.unwrap_or(0).to_string()),
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
                    ("proisstrict", "f".into()),
                    ("proretset", "f".into()),
                    ("provolatile", "v".into()),
                    ("proparallel", "u".into()),
                    ("pronargs", f.args.len().to_string()),
                    ("pronargdefaults", "0".into()),
                    ("prorettype", "0".into()),
                    ("proargtypes", args.join(" ")),
                    ("prosrc", f.body.to_string()),
                ])
            })
            .collect()
    }

    fn ranges(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for ty in &self.catalog.types {
            if let crate::catalog::TypeKind::Range(r) = &ty.kind {
                let n = full(&ty.name);
                let sub = crate::regs::type_oid(&r.subtype, self.release).unwrap_or(0);
                let opc = self.types.get(&sub).map(|_| "0").unwrap_or("0");
                out.push(row(&[
                    ("rngtypid", self.oid(Object::Type(n.clone())).to_string()),
                    ("rngsubtype", sub.to_string()),
                    (
                        "rngmultitypid",
                        self.oid(Object::Type(full(&r.multirange))).to_string(),
                    ),
                    ("rngcollation", "0".into()),
                    ("rngsubopc", opc.into()),
                    ("rngcanonical", "0".into()),
                    ("rngsubdiff", "0".into()),
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

/// An index of a table's, as pg_class, pg_index and pg_constraint say it.
struct IndexFacts {
    name: String,
    method: Method,
    keys: Vec<i16>,
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
/// relation.
pub(crate) fn names_catalog(sql: &str) -> bool {
    crate::read::sysviews::words_of(sql)
        .iter()
        .any(|w| table_named(w).is_some() || crate::read::sysviews::view_named(w).is_some())
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
