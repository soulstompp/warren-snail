//! The objects a stored tree names, as PostgreSQL's `find_expr_references_walker` finds them
//! (dependency.c): what a view's rule, a CHECK, a DEFAULT, an index's expressions and predicate,
//! and a domain's DEFAULT and CHECK depend on, for the rows of pg_depend.

use super::analyze::Node;
use super::cat::Cat;
use super::query::{Jt, Query, RteKind};
use crate::oids::Object;
use crate::Result;

/// The catalogs a row of pg_depend names, by their OIDs, as initdb fixes them.
pub(crate) const CLASS: u32 = 1259;
pub(crate) const TYPE: u32 = 1247;
pub(crate) const PROC: u32 = 1255;
pub(crate) const ATTRDEF: u32 = 2604;
pub(crate) const CONSTRAINT: u32 = 2606;
pub(crate) const TRIGGER: u32 = 2620;
pub(crate) const OPERATOR: u32 = 2617;
pub(crate) const REWRITE: u32 = 2618;
pub(crate) const CAST: u32 = 2605;
pub(crate) const NAMESPACE: u32 = 2615;
pub(crate) const COLLATION: u32 = 3456;
pub(crate) const OPCLASS: u32 = 2616;
const TS_CONFIG: u32 = 3602;
const TS_DICT: u32 = 3600;

/// The default collation's OID, which PostgreSQL never records a dependency on.
pub(crate) const DEFAULT_COLLATION: u32 = 100;

/// An object as pg_depend names it: its catalog, its OID, and a column's number (0 for the whole).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Addr {
    pub class: u32,
    pub oid: u32,
    pub sub: i32,
}

impl Addr {
    pub(crate) fn of(class: u32, oid: u32) -> Addr {
        Addr { class, oid, sub: 0 }
    }

    pub(crate) fn column(rel: u32, attno: i32) -> Addr {
        Addr {
            class: CLASS,
            oid: rel,
            sub: attno,
        }
    }

    /// Whether PostgreSQL holds it pinned, and so records no dependency on it: every object initdb
    /// makes below `FirstUnpinnedObjectId`, the public schema aside; or no object at all.
    pub(crate) fn pinned(&self) -> bool {
        self.oid == 0 || (self.oid < 12000 && !(self.class == NAMESPACE && self.oid == 2200))
    }
}

/// The objects sorted and each kept once, as `eliminate_duplicate_dependencies` keeps them: a
/// reference to a whole object gives way to one to a part of it.
pub(crate) fn eliminated(mut v: Vec<Addr>) -> Vec<Addr> {
    if v.len() <= 1 {
        return v;
    }
    v.sort_by(|a, b| {
        b.oid
            .cmp(&a.oid)
            .then(a.class.cmp(&b.class))
            .then((a.sub as u32).cmp(&(b.sub as u32)))
    });
    let mut out: Vec<Addr> = Vec::with_capacity(v.len());
    for a in v {
        if let Some(p) = out.last_mut() {
            if p.class == a.class && p.oid == a.oid {
                if p.sub == a.sub {
                    continue;
                }
                if p.sub == 0 {
                    p.sub = a.sub;
                    continue;
                }
            }
        }
        out.push(a);
    }
    out
}

/// What a tree names, gathered as the walker visits it: each query level's range table as the
/// relations its entries read, the innermost last; and every relation a range table entry of any
/// level reads, in the order the walk meets them.
pub(crate) struct Refs<'c, 'a> {
    cat: &'c Cat<'a>,
    pub addrs: Vec<Addr>,
    rtables: Vec<Vec<Option<u32>>>,
    read: Vec<u32>,
}

impl<'c, 'a> Refs<'c, 'a> {
    /// A walk of an expression over one relation, its columns that relation's; or over none.
    pub(crate) fn over(cat: &'c Cat<'a>, relid: Option<u32>) -> Refs<'c, 'a> {
        Refs {
            cat,
            addrs: Vec::new(),
            rtables: vec![relid.map_or_else(Vec::new, |r| vec![Some(r)])],
            read: Vec::new(),
        }
    }

    fn add(&mut self, class: u32, oid: u32, sub: i32) {
        self.addrs.push(Addr { class, oid, sub });
    }

    /// A query: each plain relation its range table reads, whole; each merged USING column's
    /// coercion; then every part of it, one level further in.
    pub(crate) fn query(&mut self, q: &Query) {
        let level: Vec<Option<u32>> = q
            .rtable
            .iter()
            .map(|r| match &r.kind {
                RteKind::Relation { name, namespace, .. } => Some(relation_oid(self.cat, name, *namespace)),
                _ => None,
            })
            .collect();
        for rel in level.iter().flatten() {
            self.add(CLASS, *rel, 0);
            if !self.read.contains(rel) {
                self.read.push(*rel);
            }
        }
        self.rtables.push(level);
        for r in &q.rtable {
            if let RteKind::Join { vars, merged, .. } = &r.kind {
                for v in vars.iter().take(*merged) {
                    if !matches!(v, Node::Var { .. }) {
                        self.node(v);
                    }
                }
            }
        }
        for t in &q.targets {
            self.node(&t.expr);
        }
        for j in &q.from {
            self.join_tree(j, q);
        }
        for n in q.quals.iter().chain(&q.having).chain(&q.offset).chain(&q.limit) {
            self.node(n);
        }
        for w in &q.windows {
            for s in w.partition.iter().chain(&w.order) {
                self.node(&s.expr);
            }
            if let Some(f) = &w.frame {
                for b in std::iter::once(&f.start).chain(&f.end) {
                    if let super::query::Bound::Preceding(n) | super::query::Bound::Following(n) = b {
                        self.node(n);
                    }
                }
            }
        }
        for r in &q.rtable {
            match &r.kind {
                RteKind::Subquery(sub) => self.query(sub),
                RteKind::Values { rows } => rows.iter().flatten().for_each(|n| self.node(n)),
                RteKind::Function { functions, .. } => functions.iter().for_each(|x| self.node(&x.call)),
                _ => {}
            }
        }
        for c in &q.ctes {
            self.query(&c.query);
        }
        self.rtables.pop();
    }

    fn join_tree(&mut self, j: &Jt, q: &Query) {
        let Jt::Join(j) = j else {
            return;
        };
        self.join_tree(&j.left, q);
        self.join_tree(&j.right, q);
        if let Some(n) = &j.quals {
            self.node(n);
        }
        // A USING or NATURAL join compares its merged columns, as the quals PostgreSQL keeps for
        // it: a column of a plain relation on either side is read.
        let Some(RteKind::Join { left, right, merged, .. }) = q.rtable.get(j.rtindex.wrapping_sub(1)).map(|r| &r.kind) else {
            return;
        };
        let level = self.rtables.last().cloned().unwrap_or_default();
        for (side, cols) in [(&j.left, left), (&j.right, right)] {
            let Jt::Ref(r) = side else {
                continue;
            };
            if let Some(Some(rel)) = level.get(r.wrapping_sub(1)) {
                for &c in cols.iter().take(*merged).filter(|&&c| c > 0) {
                    self.add(CLASS, *rel, i32::from(c));
                }
            }
        }
    }

    /// An expression, and everything under it.
    pub(crate) fn node(&mut self, n: &Node) {
        use Node as N;
        match n {
            N::Var { attno, rel, up, .. } => {
                if *attno != 0 {
                    let at = self.rtables.len().checked_sub(1 + *up as usize);
                    let found = at
                        .and_then(|i| self.rtables.get(i))
                        .and_then(|level| level.get((*rel as usize).wrapping_sub(1)).copied().flatten());
                    if let Some(relid) = found {
                        self.add(CLASS, relid, i32::from(*attno));
                    }
                }
                return;
            }
            N::Const { ty, text, .. } => {
                self.add(TYPE, *ty, 0);
                if let Some(t) = text {
                    if let Some(a) = named_by(self.cat, *ty, t) {
                        self.addrs.push(a);
                    }
                }
                return;
            }
            N::Func { oid, .. } | N::Agg { oid, .. } | N::Window { oid, .. } => self.add(PROC, *oid, 0),
            N::Op { oid, .. } | N::AnyAll { oid, .. } => self.add(OPERATOR, *oid, 0),
            N::Field { arg, name, ty, .. } => {
                let reltype = self.cat.ty(self.cat.base(arg.ty())).map_or(0, |t| t.relid);
                let num = super::row_fields(self.cat, reltype)
                    .iter()
                    .position(|f| f.0 == *name)
                    .map_or(0, |i| i as i32 + 1);
                if reltype != 0 && num != 0 {
                    self.add(CLASS, reltype, num);
                    // A view's fields are found only where its question was read.
                    let coll = crate::pgcatalog::attcollation_of(self.cat.catalog, reltype, num).unwrap_or(0);
                    if coll != 0 && coll != DEFAULT_COLLATION {
                        self.add(COLLATION, coll, 0);
                    }
                } else {
                    self.add(TYPE, *ty, 0);
                }
            }
            N::Relabel { ty, .. } | N::Io { ty, .. } | N::ArrayCoerce { ty, .. } | N::Domain { ty, .. } => {
                self.add(TYPE, *ty, 0)
            }
            N::Collate { coll, .. } => self.add(COLLATION, *coll, 0),
            N::Row { ty, .. } => self.add(TYPE, *ty, 0),
            N::SubLink { test, query, .. } => {
                if let Some((l, _, _)) = test {
                    self.node(l);
                }
                self.query(query);
                return;
            }
            _ => {}
        }
        self.children(n);
    }

    fn children(&mut self, n: &Node) {
        use Node as N;
        match n {
            N::Op { args, .. }
            | N::Func { args, .. }
            | N::Bool { args, .. }
            | N::Distinct { args, .. }
            | N::NullIf { args, .. }
            | N::AnyAll { args, .. }
            | N::Row { args, .. }
            | N::Coalesce { args, .. }
            | N::MinMax { args, .. }
            | N::Grouping { args }
            | N::List(args)
            | N::Array { elements: args, .. } => args.iter().for_each(|a| self.node(a)),
            N::Agg { args, filter, order, .. } => {
                args.iter().for_each(|a| self.node(a));
                order.iter().for_each(|s| self.node(&s.expr));
                if let Some(x) = filter {
                    self.node(x);
                }
            }
            N::Window { args, filter, .. } => {
                args.iter().for_each(|a| self.node(a));
                if let Some(x) = filter {
                    self.node(x);
                }
            }
            N::Relabel { arg, .. }
            | N::Io { arg, .. }
            | N::ArrayCoerce { arg, .. }
            | N::Domain { arg, .. }
            | N::NullTest { arg, .. }
            | N::BoolTest { arg, .. }
            | N::Collate { arg, .. }
            | N::Field { arg, .. }
            | N::Named { arg, .. } => self.node(arg),
            N::Case { arg, whens, default, .. } => {
                if let Some(a) = arg {
                    self.node(a);
                }
                for (w, r) in whens {
                    self.node(w);
                    self.node(r);
                }
                self.node(default);
            }
            N::Subscript { container, subs, .. } => {
                self.node(container);
                for (a, b, _) in subs {
                    a.iter().chain(b.iter()).for_each(|x| self.node(x));
                }
            }
            _ => {}
        }
    }
}

/// The OID of a relation a range table entry reads: the file's, or the carried catalog's.
fn relation_oid(cat: &Cat, name: &str, namespace: u32) -> u32 {
    let schema = match namespace {
        2200 => "public".to_string(),
        11 => "pg_catalog".to_string(),
        n => cat.namespace(n),
    };
    if let Some(oid) = cat.catalog.oids.of(&Object::Class(format!("{schema}.{name}"))) {
        return oid;
    }
    crate::pgcatalog::built_in("pg_class", cat.release)
        .iter()
        .find(|r| {
            r.get("relname").cloned().flatten().as_deref() == Some(name)
                && r.get("relnamespace").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(namespace)
        })
        .and_then(|r| r.get("oid").cloned().flatten()?.parse().ok())
        .unwrap_or(0)
}

/// The object a reg* constant names, where it names one that exists.
fn named_by(cat: &Cat, ty: u32, text: &str) -> Option<Addr> {
    use crate::regs::RegKind as K;
    let (kind, class) = match ty {
        24 => (K::Proc, PROC),
        2202 => (K::Procedure, PROC),
        2203 => (K::Oper, OPERATOR),
        2204 => (K::Operator, OPERATOR),
        2205 => (K::Class, CLASS),
        2206 => (K::Type, TYPE),
        4191 => (K::Collation, COLLATION),
        3734 => (K::Config, TS_CONFIG),
        3769 => (K::Dictionary, TS_DICT),
        4089 => (K::Namespace, NAMESPACE),
        _ => return None,
    };
    let oid = crate::regs::reg_parse(kind, text, cat.release).ok()?;
    (oid != 0).then(|| Addr::of(class, oid))
}

/// What a tree over one relation names, as `recordDependencyOnSingleRelExpr` sorts it: the
/// relation's own columns, then everything else, each kept once.
pub(crate) fn single_rel(cat: &Cat, node: &Node, relid: u32) -> (Vec<Addr>, Vec<Addr>) {
    let mut r = Refs::over(cat, (relid != 0).then_some(relid));
    r.node(node);
    eliminated(r.addrs)
        .into_iter()
        .partition(|a| a.class == CLASS && a.oid == relid && relid != 0)
}

/// What an expression over no relation names, as `recordDependencyOnExpr` gathers it: a domain's
/// DEFAULT's and its CHECKs'.
pub(crate) fn of_expr(cat: &Cat, node: &Node) -> Vec<Addr> {
    let mut r = Refs::over(cat, None);
    r.node(node);
    eliminated(r.addrs)
}

/// The collation a query's output column carries, as `assign_collations` gives a column read or
/// a COLLATE written: the column's own, the COLLATE's, the default for any other value of a
/// collatable type; none for a type without one.
pub(crate) fn output_collation(cat: &Cat, q: &Query, n: &Node) -> Result<u32> {
    /// The collation the column definition list names for a function entry's column `attno`
    /// (from 1), 0 where it names none.
    fn function_collation(functions: &[super::query::RangeFunc], attno: usize) -> u32 {
        let mut at = attno;
        for f in functions {
            let width = f.coldefs.as_ref().map_or(1, Vec::len);
            if at <= width {
                return f.coldefs.as_ref().map_or(0, |d| d[at - 1].3);
            }
            at -= width;
        }
        0
    }
    Ok(match n {
        Node::Collate { coll, .. } => *coll,
        Node::Var { attno, rel, up: 0, ty, .. } if *attno > 0 => {
            match q.rtable.get((*rel as usize).wrapping_sub(1)).map(|r| &r.kind) {
                Some(RteKind::Relation { name, namespace, .. }) => crate::pgcatalog::attcollation_of(
                    cat.catalog,
                    relation_oid(cat, name, *namespace),
                    i32::from(*attno),
                )?,
                Some(RteKind::Subquery(sub)) => match sub.output().nth(*attno as usize - 1) {
                    Some(t) => output_collation(cat, sub, &t.expr)?,
                    None => 0,
                },
                // A function's column the COLLATE of its column definition list names.
                Some(RteKind::Function { functions, .. })
                    if function_collation(functions, *attno as usize) != 0 =>
                {
                    function_collation(functions, *attno as usize)
                }
                _ if cat.collatable(*ty) => DEFAULT_COLLATION,
                _ => 0,
            }
        }
        Node::Relabel { arg, ty, .. } if cat.collatable(*ty) => match output_collation(cat, q, arg)? {
            0 => DEFAULT_COLLATION,
            c => c,
        },
        _ if cat.collatable(n.ty()) => DEFAULT_COLLATION,
        _ => 0,
    })
}

/// What a view's question names, as `recordDependencyOnExpr` gathers it for its rule.
pub(crate) fn of_query(cat: &Cat, q: &Query) -> Vec<Addr> {
    let mut r = Refs {
        cat,
        addrs: Vec::new(),
        rtables: Vec::new(),
        read: Vec::new(),
    };
    r.query(q);
    eliminated(r.addrs)
}

/// The relations a query's range tables read, at every level, its subqueries' and its CTEs' among
/// them, each once: those PostgreSQL's parse analysis opens and locks.
pub(crate) fn relations_read(cat: &Cat, q: &Query) -> Vec<u32> {
    let mut r = Refs::over(cat, None);
    r.rtables.clear();
    r.query(q);
    r.read
}

/// The distinct columns an expression reads, in the order the walk meets them, as `StoreRelCheck`
/// gathers a CHECK's keys.
pub(crate) fn columns_read(node: &Node) -> Vec<i16> {
    let mut out: Vec<i16> = Vec::new();
    node.walk(&mut |n| {
        if let Node::Var { attno, up: 0, .. } = n {
            if !out.contains(attno) {
                out.push(*attno);
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_object_gives_way_to_its_part_and_each_is_kept_once() {
        let t = |sub| Addr::column(16384, sub);
        let got = eliminated(vec![t(2), t(0), Addr::of(TYPE, 16390), t(2), t(1)]);
        assert_eq!(got, vec![Addr::of(TYPE, 16390), t(1), t(2)]);
        assert_eq!(eliminated(vec![t(0), t(0)]), vec![t(0)]);
        assert!(Addr::of(TYPE, 23).pinned() && !Addr::of(NAMESPACE, 2200).pinned());
        assert!(!Addr::of(COLLATION, 12345).pinned());
    }
}
