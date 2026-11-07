//! The catalog the deparsers read: the carried pg_type, pg_operator, pg_proc, pg_cast,
//! pg_namespace and pg_collation of the file's release, indexed once a release, and the file's own
//! types and functions beside them, as its pg_type and pg_proc rows give them.

use crate::catalog::Catalog;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) const BOOL: u32 = 16;
pub(crate) const INT8: u32 = 20;
pub(crate) const INT4: u32 = 23;
pub(crate) const TEXT: u32 = 25;
pub(crate) const NUMERIC: u32 = 1700;
pub(crate) const UNKNOWN: u32 = 705;
pub(crate) const RECORD: u32 = 2249;
pub(crate) const RECORD_ARRAY: u32 = 2287;
pub(crate) const ANY: u32 = 2276;
pub(crate) const INTERNAL: u32 = 2281;
pub(crate) const OIDVECTOR: u32 = 30;
pub(crate) const INT2VECTOR: u32 = 22;
pub(crate) const INTERVAL: u32 = 1186;

pub(crate) const ANYELEMENT: u32 = 2283;
pub(crate) const ANYARRAY: u32 = 2277;
pub(crate) const ANYNONARRAY: u32 = 2776;
pub(crate) const ANYENUM: u32 = 3500;
pub(crate) const ANYRANGE: u32 = 3831;
pub(crate) const ANYMULTIRANGE: u32 = 4537;
pub(crate) const ANYCOMPATIBLE: u32 = 5077;
pub(crate) const ANYCOMPATIBLEARRAY: u32 = 5078;
pub(crate) const ANYCOMPATIBLENONARRAY: u32 = 5079;
pub(crate) const ANYCOMPATIBLERANGE: u32 = 5080;
pub(crate) const ANYCOMPATIBLEMULTIRANGE: u32 = 4538;

/// Whether a type is one of the polymorphic pseudo-types.
pub(crate) fn polymorphic(t: u32) -> bool {
    matches!(
        t,
        ANYELEMENT
            | ANYARRAY
            | ANYNONARRAY
            | ANYENUM
            | ANYRANGE
            | ANYMULTIRANGE
            | ANYCOMPATIBLE
            | ANYCOMPATIBLEARRAY
            | ANYCOMPATIBLENONARRAY
            | ANYCOMPATIBLERANGE
            | ANYCOMPATIBLEMULTIRANGE
    )
}

/// A row of pg_type, what the resolution reads of it.
#[derive(Clone, Debug)]
pub(crate) struct Type {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub typtype: char,
    pub category: char,
    pub preferred: bool,
    pub relid: u32,
    pub elem: u32,
    pub array: u32,
    pub subscript: String,
    pub base: u32,
    pub typmod: i32,
    pub collation: u32,
    pub len: i16,
}

/// A row of pg_operator.
#[derive(Clone, Debug)]
pub(crate) struct Oper {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub kind: char,
    pub left: u32,
    pub right: u32,
    pub result: u32,
}

/// A row of pg_proc.
#[derive(Clone, Debug)]
pub(crate) struct Proc {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub args: Vec<u32>,
    pub result: u32,
    pub variadic: u32,
    pub nargdefaults: usize,
}

/// A row of pg_cast.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cast {
    pub func: u32,
    pub context: char,
    pub method: char,
}

/// The rows of a release's catalog that resolution reads, indexed.
#[derive(Default)]
pub(crate) struct Rows {
    pub types: HashMap<u32, Type>,
    pub opers: HashMap<u32, Oper>,
    pub opers_named: HashMap<String, Vec<u32>>,
    pub procs: HashMap<u32, Proc>,
    pub procs_named: HashMap<String, Vec<u32>>,
    pub casts: HashMap<(u32, u32), Cast>,
    pub namespaces: HashMap<u32, String>,
}

type Raw = HashMap<&'static str, Option<String>>;

fn field<'a>(r: &'a Raw, k: &str) -> &'a str {
    r.get(k).and_then(|v| v.as_deref()).unwrap_or("")
}

fn num<T: std::str::FromStr + Default>(r: &Raw, k: &str) -> T {
    field(r, k).parse().unwrap_or_default()
}

fn truth(r: &Raw, k: &str) -> bool {
    field(r, k) == "t"
}

fn letter(r: &Raw, k: &str) -> char {
    field(r, k).chars().next().unwrap_or(' ')
}

fn type_row(r: &Raw) -> Type {
    Type {
        oid: num(r, "oid"),
        name: field(r, "typname").to_string(),
        namespace: num(r, "typnamespace"),
        typtype: letter(r, "typtype"),
        category: letter(r, "typcategory"),
        preferred: truth(r, "typispreferred"),
        relid: num(r, "typrelid"),
        elem: num(r, "typelem"),
        array: num(r, "typarray"),
        subscript: field(r, "typsubscript").to_string(),
        base: num(r, "typbasetype"),
        typmod: field(r, "typtypmod").parse().unwrap_or(-1),
        collation: num(r, "typcollation"),
        len: num(r, "typlen"),
    }
}

fn proc_row(r: &Raw) -> Proc {
    let oids = |k: &str| -> Vec<u32> {
        field(r, k)
            .split(' ')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    Proc {
        oid: num(r, "oid"),
        name: field(r, "proname").to_string(),
        namespace: num(r, "pronamespace"),
        args: oids("proargtypes"),
        result: num(r, "prorettype"),
        variadic: num(r, "provariadic"),
        nargdefaults: num(r, "pronargdefaults"),
    }
}

impl Rows {
    fn add_type(&mut self, t: Type) {
        self.types.insert(t.oid, t);
    }

    fn add_proc(&mut self, p: Proc) {
        self.procs_named.entry(p.name.clone()).or_default().push(p.oid);
        self.procs.insert(p.oid, p);
    }

    fn carried(release: u8) -> Rows {
        let mut rows = Rows::default();
        for r in crate::pgcatalog::built_in("pg_type", release) {
            rows.add_type(type_row(&r));
        }
        for r in crate::pgcatalog::built_in("pg_proc", release) {
            rows.add_proc(proc_row(&r));
        }
        for r in crate::pgcatalog::built_in("pg_operator", release) {
            let o = Oper {
                oid: num(&r, "oid"),
                name: field(&r, "oprname").to_string(),
                namespace: num(&r, "oprnamespace"),
                kind: letter(&r, "oprkind"),
                left: num(&r, "oprleft"),
                right: num(&r, "oprright"),
                result: num(&r, "oprresult"),
            };
            rows.opers_named.entry(o.name.clone()).or_default().push(o.oid);
            rows.opers.insert(o.oid, o);
        }
        for r in crate::pgcatalog::built_in("pg_cast", release) {
            rows.casts.insert(
                (num(&r, "castsource"), num(&r, "casttarget")),
                Cast {
                    func: num(&r, "castfunc"),
                    context: letter(&r, "castcontext"),
                    method: letter(&r, "castmethod"),
                },
            );
        }
        for r in crate::pgcatalog::built_in("pg_namespace", release) {
            rows.namespaces
                .insert(num(&r, "oid"), field(&r, "nspname").to_string());
        }
        rows
    }
}

/// The carried rows of a release, indexed on first use.
fn carried(release: u8) -> Arc<Rows> {
    static CARRIED: OnceLock<Mutex<HashMap<u8, Arc<Rows>>>> = OnceLock::new();
    let all = CARRIED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
    all.entry(release)
        .or_insert_with(|| Arc::new(Rows::carried(release)))
        .clone()
}

/// The catalog a deparse reads: the release's carried rows, and the file's own beside them.
pub(crate) struct Cat<'a> {
    pub catalog: &'a Catalog,
    pub release: u8,
    carried: Arc<Rows>,
    own: Rows,
}

impl<'a> Cat<'a> {
    pub(crate) fn of(catalog: &'a Catalog) -> Cat<'a> {
        let mut own = Rows::default();
        for r in crate::pgcatalog::file_rows(catalog, "pg_type") {
            own.add_type(type_row(&r));
        }
        for r in crate::pgcatalog::file_rows(catalog, "pg_proc") {
            own.add_proc(proc_row(&r));
        }
        for r in crate::pgcatalog::file_rows(catalog, "pg_namespace") {
            own.namespaces
                .insert(num(&r, "oid"), field(&r, "nspname").to_string());
        }
        Cat {
            catalog,
            release: catalog.release,
            carried: carried(catalog.release),
            own,
        }
    }

    pub(crate) fn ty(&self, oid: u32) -> Option<&Type> {
        self.own.types.get(&oid).or_else(|| self.carried.types.get(&oid))
    }

    pub(crate) fn oper(&self, oid: u32) -> Option<&Oper> {
        self.carried.opers.get(&oid)
    }

    pub(crate) fn proc(&self, oid: u32) -> Option<&Proc> {
        self.own.procs.get(&oid).or_else(|| self.carried.procs.get(&oid))
    }

    pub(crate) fn cast(&self, source: u32, target: u32) -> Option<Cast> {
        self.carried.casts.get(&(source, target)).copied()
    }

    pub(crate) fn namespace(&self, oid: u32) -> String {
        self.own
            .namespaces
            .get(&oid)
            .or_else(|| self.carried.namespaces.get(&oid))
            .cloned()
            .unwrap_or_default()
    }

    /// The schemas a name not qualified is looked for in, in order: pg_catalog, then public.
    pub(crate) fn path(&self) -> [u32; 2] {
        [11, 2200]
    }

    /// The operators of a name and kind in the schemas searched, an earlier schema's hiding a later
    /// one's of the same arguments; or those of the schema named.
    pub(crate) fn opers_named(&self, name: &str, kind: char, schema: Option<u32>) -> Vec<&Oper> {
        let mut out: Vec<&Oper> = Vec::new();
        let ids = self.carried.opers_named.get(name).into_iter().flatten();
        let mut all: Vec<&Oper> = ids.filter_map(|o| self.oper(*o)).filter(|o| o.kind == kind).collect();
        let rank = |ns: u32| self.path().iter().position(|p| *p == ns);
        match schema {
            Some(s) => all.retain(|o| o.namespace == s),
            None => {
                all.retain(|o| rank(o.namespace).is_some());
                all.sort_by_key(|o| rank(o.namespace));
            }
        }
        for o in all {
            if !out.iter().any(|x| x.left == o.left && x.right == o.right) {
                out.push(o);
            }
        }
        out
    }

    /// The functions of a name in the schemas searched, or the schema named, an earlier schema's
    /// hiding a later one's of the same arguments.
    pub(crate) fn procs_named(&self, name: &str, schema: Option<u32>) -> Vec<&Proc> {
        let ids = self
            .own
            .procs_named
            .get(name)
            .into_iter()
            .flatten()
            .chain(self.carried.procs_named.get(name).into_iter().flatten());
        let mut all: Vec<&Proc> = ids.filter_map(|p| self.proc(*p)).collect();
        let rank = |ns: u32| self.path().iter().position(|p| *p == ns);
        match schema {
            Some(s) => all.retain(|p| p.namespace == s),
            None => {
                all.retain(|p| rank(p.namespace).is_some());
                all.sort_by_key(|p| rank(p.namespace));
            }
        }
        let mut out: Vec<&Proc> = Vec::new();
        for p in all {
            if !out.iter().any(|x| x.args == p.args) {
                out.push(p);
            }
        }
        out
    }

    /// A type's OID by its schema's OID and name.
    pub(crate) fn type_named(&self, schema: Option<u32>, name: &str) -> Option<u32> {
        let find = |rows: &Rows| {
            rows.types
                .values()
                .filter(|t| t.name == name)
                .filter(|t| schema.is_none_or(|s| t.namespace == s))
                .min_by_key(|t| self.path().iter().position(|p| *p == t.namespace).unwrap_or(9))
                .map(|t| t.oid)
        };
        find(&self.own).or_else(|| find(&self.carried))
    }

    /// The base type of a domain, any other type itself.
    pub(crate) fn base(&self, t: u32) -> u32 {
        let mut t = t;
        for _ in 0..32 {
            match self.ty(t) {
                Some(x) if x.typtype == 'd' => t = x.base,
                _ => break,
            }
        }
        t
    }

    /// The base type of a domain and the typmod it gives, any other type itself with `typmod`.
    pub(crate) fn base_and_typmod(&self, t: u32, typmod: i32) -> (u32, i32) {
        let mut t = t;
        let mut m = typmod;
        for _ in 0..32 {
            match self.ty(t) {
                Some(x) if x.typtype == 'd' => {
                    m = x.typmod;
                    t = x.base;
                }
                _ => break,
            }
        }
        (t, m)
    }

    pub(crate) fn category(&self, t: u32) -> char {
        if t == UNKNOWN {
            return 'X';
        }
        self.ty(t).map_or('U', |x| x.category)
    }

    pub(crate) fn preferred(&self, t: u32) -> bool {
        self.ty(t).is_some_and(|x| x.preferred)
    }

    /// The element type of a true array, none for any other.
    pub(crate) fn element(&self, t: u32) -> u32 {
        match self.ty(t) {
            Some(x) if x.elem != 0 && x.len == -1 && x.subscript == "6179" => {
                x.elem
            }
            _ => 0,
        }
    }

    pub(crate) fn array_of(&self, t: u32) -> u32 {
        self.ty(t).map_or(0, |x| x.array)
    }

    /// Whether a type is a composite one.
    pub(crate) fn complex(&self, t: u32) -> bool {
        t == RECORD || self.ty(t).is_some_and(|x| x.typtype == 'c')
    }

    pub(crate) fn collatable(&self, t: u32) -> bool {
        self.ty(self.base(t)).is_some_and(|x| x.collation != 0)
    }
}
