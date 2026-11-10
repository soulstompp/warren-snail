//! The definitions of a file's constraints and indexes, as pg_get_constraintdef_worker and
//! pg_get_indexdef_worker print them.

use super::analyze::{Analyzer, Context, Form, Node};
use super::cat::Cat;
use super::print::{format_type, quote, Printer};
use crate::catalog::{Action, Method, Table};
use crate::oids::{full, parts, Object};
use crate::Result;

/// One key of an index as its definition prints it.
struct Key {
    /// A column's number, or none for an expression.
    column: Option<usize>,
    /// The expression as written, where it is one.
    expr: Option<String>,
    collation: Option<String>,
    opclass: String,
    params: Option<String>,
    desc: bool,
    nulls_first: bool,
}

/// An index as its definition prints it.
struct Index<'c> {
    table: &'c Table,
    name: String,
    method: Method,
    keys: Vec<Key>,
    include: Vec<usize>,
    unique: bool,
    nulls_distinct: bool,
    options: Vec<(String, String)>,
    predicate: Option<String>,
    exclusion: Option<Vec<String>>,
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Btree => "btree",
        Method::Hash => "hash",
        Method::Gist => "gist",
        Method::Gin => "gin",
        Method::Spgist => "spgist",
        Method::Brin => "brin",
    }
}

fn method_oid(m: Method) -> u32 {
    match m {
        Method::Btree => 403,
        Method::Hash => 405,
        Method::Gist => 783,
        Method::Gin => 2742,
        Method::Spgist => 4000,
        Method::Brin => 3580,
    }
}

/// The collation a key's COLLATE names, where it names one.
pub(crate) fn key_collation(key: &str) -> Option<String> {
    split_collate(key).1
}

/// An operator class's OID by its name under an access method, pg_catalog's.
pub(crate) fn opclass_oid(release: u8, name: &str, am: u32) -> u32 {
    crate::pgcatalog::built_in("pg_opclass", release)
        .iter()
        .find(|r| {
            r.get("opcname").cloned().flatten().as_deref() == Some(name)
                && r.get("opcmethod").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(am)
        })
        .and_then(|r| r.get("oid").cloned().flatten()?.parse().ok())
        .unwrap_or(0)
}

/// The default B-tree operator class of a type, by its OID; 0 where it has none.
pub(crate) fn default_btree_opclass(cat: &Cat, ty: u32) -> u32 {
    default_opclass(cat, ty, 403).map_or(0, |n| opclass_oid(cat.release, &n, 403))
}

/// A key written with a COLLATE: the key, and the collation.
fn split_collate(key: &str) -> (String, Option<String>) {
    match super::parse_expr(key) {
        Ok(sqlparser::ast::Expr::Collate { expr, collation }) => (
            expr.to_string(),
            Some(crate::catalog::object_name(&collation)),
        ),
        _ => (key.to_string(), None),
    }
}

/// The index of the file an OID names, gathered from the order, BRIN index or element keys that
/// hold it.
fn index_of(catalog: &crate::catalog::Catalog, oid: u32) -> Option<Index<'_>> {
    let Object::Class(n) = catalog.oids.object(oid)? else {
        return None;
    };
    let (schema, own) = parts(n);
    for t in &catalog.tables {
        if parts(&full(&t.name)).0 != schema {
            continue;
        }
        let key_of = |level: Option<usize>, written: &str, opclass: &str, params: Option<String>, desc: bool, nulls_first: bool| {
            let (written, collation) = split_collate(written);
            let column = level.filter(|&c| t.columns.get(c).is_some_and(|x| quote(&x.name) == written || x.name == written));
            Key {
                column,
                expr: column.is_none().then_some(written),
                collation,
                opclass: opclass.to_string(),
                params,
                desc,
                nulls_first,
            }
        };
        let order = t.orders.iter().find(|o| o.name == own);
        let elements: Vec<&crate::catalog::ElementKey> = t.element_keys.iter().filter(|e| e.index == own).collect();
        if order.is_some() || !elements.is_empty() {
            let mut keys: Vec<(usize, Key)> = elements
                .iter()
                .map(|e| (e.key, key_of(Some(e.column), &t.columns[e.column].name, &e.opclass, e.params.clone(), false, false)))
                .collect();
            if let Some(o) = order {
                let mut at = 0;
                for (i, l) in o.levels.iter().enumerate() {
                    while keys.iter().any(|(k, _)| *k == at) {
                        at += 1;
                    }
                    let desc = o.desc.get(i).copied().unwrap_or(false);
                    let nf = o.nulls_first.get(i).copied().unwrap_or(false);
                    let declared_first = if desc { !nf } else { nf };
                    let column = match l {
                        crate::catalog::Level::Column(c) => Some(*c),
                        _ => None,
                    };
                    keys.push((
                        at,
                        key_of(
                            column,
                            o.keys.get(i).map(String::as_str).unwrap_or(""),
                            o.opclasses.get(i).map(String::as_str).unwrap_or(""),
                            o.params.get(i).cloned().flatten(),
                            desc,
                            declared_first,
                        ),
                    ));
                    at += 1;
                }
            }
            keys.sort_by_key(|k| k.0);
            let e = elements.first();
            return Some(Index {
                table: t,
                name: own.to_string(),
                method: order.map_or_else(|| e.map_or(Method::Gin, |e| e.method), |o| o.method),
                keys: keys.into_iter().map(|k| k.1).collect(),
                include: order.map_or_else(|| e.map_or_else(Vec::new, |e| e.include.clone()), |o| o.include.clone()),
                unique: order.is_some_and(|o| (o.unique && o.exclusion.is_none()) || o.constraint.is_some()),
                nulls_distinct: order.is_none_or(|o| o.nulls_distinct),
                options: order.map_or_else(|| e.map_or_else(Vec::new, |e| e.options.clone()), |o| o.options.clone()),
                predicate: order.map_or_else(|| e.and_then(|e| e.predicate.clone()), |o| o.predicate.clone()),
                exclusion: order.and_then(|o| o.exclusion.as_ref().map(|x| x.operators.clone())),
            });
        }
        if let Some(b) = t.brins.iter().find(|b| b.name == own) {
            let keys = b
                .levels
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let column = match l {
                        crate::catalog::Level::Column(c) => Some(*c),
                        _ => None,
                    };
                    key_of(
                        column,
                        b.keys.get(i).map(String::as_str).unwrap_or(""),
                        b.opclasses.get(i).map(String::as_str).unwrap_or(""),
                        b.params.get(i).cloned().flatten(),
                        false,
                        false,
                    )
                })
                .collect();
            return Some(Index {
                table: t,
                name: own.to_string(),
                method: Method::Brin,
                keys,
                include: Vec::new(),
                unique: false,
                nulls_distinct: true,
                options: b.options.clone(),
                predicate: b.predicate.clone(),
                exclusion: None,
            });
        }
    }
    None
}

/// Whether a value of `src` is binary coercible to `target`, as IsBinaryCoercible reads it.
fn binary_coercible(cat: &Cat, src: u32, target: u32) -> bool {
    use super::cat as c;
    let src = cat.base(src);
    if src == target || target == c::ANY || target == c::ANYELEMENT {
        return true;
    }
    let ty = cat.ty(src);
    match target {
        c::ANYARRAY | c::ANYCOMPATIBLEARRAY => return cat.element(src) != 0,
        c::ANYNONARRAY | c::ANYCOMPATIBLENONARRAY => return cat.element(src) == 0,
        c::ANYENUM => return ty.is_some_and(|t| t.typtype == 'e'),
        c::ANYRANGE | c::ANYCOMPATIBLERANGE => return ty.is_some_and(|t| t.typtype == 'r'),
        c::ANYMULTIRANGE | c::ANYCOMPATIBLEMULTIRANGE => return ty.is_some_and(|t| t.typtype == 'm'),
        c::RECORD => return cat.complex(src),
        _ => {}
    }
    cat.cast(src, target)
        .is_some_and(|k| k.method == 'b' && k.context == 'i')
}

/// The default operator class of a type under a method, as GetDefaultOpClass finds it.
fn default_opclass(cat: &Cat, ty: u32, am: u32) -> Option<String> {
    let ty = cat.base(ty);
    let category = cat.category(ty);
    let (mut exact, mut preferred, mut compatible) = (Vec::new(), Vec::new(), Vec::new());
    for r in crate::pgcatalog::built_in("pg_opclass", cat.release) {
        let get = |k: &str| r.get(k).cloned().flatten().unwrap_or_default();
        if get("opcmethod").parse::<u32>().ok() != Some(am) || get("opcdefault") != "t" {
            continue;
        }
        let input: u32 = get("opcintype").parse().unwrap_or(0);
        let name = get("opcname");
        if input == ty {
            exact.push(name);
        } else if exact.is_empty() && binary_coercible(cat, ty, input) {
            if cat.category(input) == category && cat.preferred(input) {
                preferred.push(name);
            } else {
                compatible.push(name);
            }
        }
    }
    match (exact.len(), preferred.len(), compatible.len()) {
        (1, _, _) => exact.pop(),
        (0, 1, _) => preferred.pop(),
        (0, 0, 1) => compatible.pop(),
        _ => None,
    }
}

/// A reloption's value as get_reloptions writes it: bare where it is an identifier needing no
/// quotes, quoted otherwise.
fn reloption(name: &str, value: &str) -> String {
    let v = value.trim_matches('\'');
    if quote(v) == v {
        format!("{name}={v}")
    } else {
        format!("{name}={}", super::print::literal(v))
    }
}

/// An expression of a table's printed as deparse_expression_pretty prints it.
fn printed(cat: &Cat, t: &Table, rel: u32, node: &Node, pretty: bool) -> Result<String> {
    let _ = rel;
    let mut p = Printer {
        cat,
        paren: pretty,
        indent: true,
        level: 0,
        buf: String::new(),
        relations: vec![(t.columns.iter().map(|c| c.name.clone()).collect(), None)],
        q: Default::default(),
    };
    p.expr(node, false)?;
    Ok(p.buf)
}

fn analyzer<'c, 'a>(cat: &'c Cat<'a>, t: &Table) -> Analyzer<'c, 'a> {
    Analyzer {
        cat,
        columns: super::columns_of(cat, t),
        value: None,
        scope: None,
    }
}

/// Whether an expression prints as a function call does, so that an index key needs no
/// parentheses around it.
fn looks_like_function(n: &Node) -> bool {
    matches!(
        n,
        Node::Func { form: Form::Call | Form::Sql, .. }
            | Node::NullIf { .. }
            | Node::Coalesce { .. }
            | Node::MinMax { .. }
            | Node::SqlValue { .. }
    )
}

/// pg_get_indexdef_worker: the whole definition, or one column's (`colno` from 1), or an EXCLUDE's.
fn index_text(cat: &Cat, ix: &Index, colno: usize, pretty: bool) -> Result<String> {
    let t = ix.table;
    let rel = cat.catalog.oids.of(&Object::Class(full(&t.name))).unwrap_or(0);
    let a = analyzer(cat, t);
    let attrs_only = colno != 0;
    let constraint = ix.exclusion.is_some() && colno == 0;
    let mut buf = String::new();
    if !attrs_only {
        if constraint {
            buf.push_str(&format!("EXCLUDE USING {} (", method_name(ix.method)));
        } else {
            let relname = if pretty {
                crate::regs::reg_text(crate::regs::RegKind::Class, rel, cat.release)
            } else {
                let n = full(&t.name);
                let (s, o) = parts(&n);
                format!("{}.{}", quote(s), quote(o))
            };
            buf.push_str(&format!(
                "CREATE {}INDEX {} ON {relname} USING {} (",
                if ix.unique { "UNIQUE " } else { "" },
                quote(&ix.name),
                method_name(ix.method)
            ));
        }
    }
    let nkeys = ix.keys.len();
    let all: Vec<Option<&Key>> = ix.keys.iter().map(Some).chain(ix.include.iter().map(|_| None)).collect();
    let mut sep = "";
    for (keyno, key) in all.iter().enumerate() {
        if colno == 0 && keyno == nkeys {
            buf.push_str(") INCLUDE (");
            sep = "";
        }
        if colno == 0 {
            buf.push_str(sep);
        }
        sep = ", ";
        let shown = colno == 0 || colno == keyno + 1;
        let (keytype, keycol) = match key {
            None => {
                let c = &t.columns[ix.include[keyno - nkeys]];
                if shown {
                    buf.push_str(&quote(&c.name));
                }
                (super::column_type(cat, c).0, Some(c))
            }
            Some(k) => match (k.column, &k.expr) {
                (Some(c), _) => {
                    let col = &t.columns[c];
                    if shown {
                        buf.push_str(&quote(&col.name));
                    }
                    (super::column_type(cat, col).0, Some(col))
                }
                (None, Some(e)) => {
                    let node = a.expr(&super::parse_expr(e)?)?;
                    let s = printed(cat, t, rel, &node, pretty)?;
                    if shown {
                        if looks_like_function(&node) {
                            buf.push_str(&s);
                        } else {
                            buf.push_str(&format!("({s})"));
                        }
                    }
                    (node.ty(), None)
                }
                _ => (0, None),
            },
        };
        let Some(k) = key else {
            continue;
        };
        if attrs_only || !shown {
            continue;
        }
        // Its COLLATE, where it is another than its column's own: the one the column's DDL names,
        // else its type's.
        if let Some(c) = &k.collation {
            if !keycol.is_some_and(|col| crate::catalog::own_collation(col, c)) {
                buf.push_str(&format!(" COLLATE {}", collation_shown(c)));
            }
        }
        let default = default_opclass(cat, keytype, method_oid(ix.method));
        if (k.params.is_some() || default.as_deref() != Some(k.opclass.as_str())) && !k.opclass.is_empty() {
            buf.push_str(&format!(" {}", quote(&k.opclass)));
        }
        if let Some(p) = &k.params {
            let opts: Vec<String> = p
                .trim_matches(|c| c == '(' || c == ')')
                .split(',')
                .filter_map(|o| o.split_once('=').map(|(n, v)| reloption(n.trim(), v.trim())))
                .collect();
            buf.push_str(&format!(" ({})", opts.join(", ")));
        }
        if matches!(ix.method, Method::Btree) {
            if k.desc {
                buf.push_str(" DESC");
                if !k.nulls_first {
                    buf.push_str(" NULLS LAST");
                }
            } else if k.nulls_first {
                buf.push_str(" NULLS FIRST");
            }
        }
        if let Some(ops) = &ix.exclusion {
            if let Some(op) = ops.get(keyno) {
                let p = Printer {
                    cat,
                    paren: false,
                    indent: false,
                    level: 0,
                    buf: String::new(),
                    relations: Vec::new(),
                    q: Default::default(),
                };
                let found = a.op_lookup(op, 'b', keytype, keytype);
                let shown = found.map_or_else(|| op.clone(), |o| p.operator_name(o, keytype, keytype));
                buf.push_str(&format!(" WITH {shown}"));
            }
        }
    }
    if !attrs_only {
        buf.push(')');
        if !ix.nulls_distinct {
            buf.push_str(" NULLS NOT DISTINCT");
        }
        if !ix.options.is_empty() {
            let opts: Vec<String> = ix.options.iter().map(|(n, v)| reloption(n, v)).collect();
            buf.push_str(&format!(" WITH ({})", opts.join(", ")));
        }
        if let Some(pred) = &ix.predicate {
            let node = a.boolean(a.expr(&super::parse_expr(pred)?)?)?;
            let s = printed(cat, t, rel, &node, pretty)?;
            if constraint {
                buf.push_str(&format!(" WHERE ({s})"));
            } else {
                buf.push_str(&format!(" WHERE {s}"));
            }
        }
    }
    Ok(buf)
}

fn collation_shown(c: &str) -> String {
    let n = full(c);
    let (s, o) = parts(&n);
    if s == "public" || s == "pg_catalog" || !c.contains('.') {
        quote(o)
    } else {
        format!("{}.{}", quote(s), quote(o))
    }
}

/// pg_get_indexdef.
pub(crate) fn indexdef(cat: &Cat, oid: u32, colno: usize, pretty: bool) -> Result<Option<String>> {
    match index_of(cat.catalog, oid) {
        Some(ix) => {
            if colno > ix.keys.len() + ix.include.len() {
                return Ok(Some(String::new()));
            }
            let ix = Index { exclusion: None, ..ix };
            index_text(cat, &ix, colno, pretty).map(Some)
        }
        None => Ok(None),
    }
}

fn names(t: &Table, cols: &[usize]) -> Vec<String> {
    cols.iter().map(|&c| quote(&t.columns[c].name)).collect()
}

/// decompile_column_index_array: the names, PERIOD before the last where it is one.
fn column_list(t: &Table, cols: &[usize], period: bool) -> String {
    let mut n = names(t, cols);
    if period {
        if let Some(last) = n.last_mut() {
            *last = format!("PERIOD {last}");
        }
    }
    n.join(", ")
}

/// pg_get_constraintdef.
pub(crate) fn constraintdef(cat: &Cat, oid: u32, pretty: bool) -> Result<Option<String>> {
    let catalog = cat.catalog;
    let Some(Object::Constraint(owner, name)) = catalog.oids.object(oid) else {
        return Ok(None);
    };
    let mut tail = String::new();
    let body = if let Some(t) = catalog.tables.iter().find(|t| full(&t.name) == *owner) {
        let rel = catalog.oids.of(&Object::Class(owner.clone())).unwrap_or(0);
        let own = parts(owner).1;
        if let Some(o) = t.orders.iter().find(|o| o.name == *name && (o.constraint.is_some() || o.exclusion.is_some())) {
            match (&o.constraint, &o.exclusion) {
                (Some(k), _) => {
                    let cols: Vec<usize> = o.levels.iter().map(|l| l.column()).collect();
                    let mut s = String::from(if k.primary { "PRIMARY KEY " } else { "UNIQUE " });
                    if !k.primary && !o.nulls_distinct {
                        s.push_str("NULLS NOT DISTINCT ");
                    }
                    s.push_str(&format!("({}", names(t, &cols).join(", ")));
                    if o.without_overlaps() {
                        s.push_str(" WITHOUT OVERLAPS");
                    }
                    s.push(')');
                    if !o.include.is_empty() {
                        s.push_str(&format!(" INCLUDE ({})", names(t, &o.include).join(", ")));
                    }
                    if k.deferrable {
                        tail.push_str(" DEFERRABLE");
                    }
                    s
                }
                (None, Some(x)) => {
                    let ix = index_of(catalog, catalog.oids.of(&Object::Class(format!("{}.{}", parts(owner).0, o.name))).unwrap_or(0));
                    let Some(ix) = ix else {
                        return Ok(None);
                    };
                    if x.deferrable {
                        tail.push_str(" DEFERRABLE");
                    }
                    index_text(cat, &ix, 0, pretty)?
                }
                _ => return Ok(None),
            }
        } else if let Some((i, k)) = t.checks.iter().enumerate().find(|(_, k)| k.name == *name) {
            let a = analyzer(cat, t);
            let node = a.boolean(a.expr(&k.expr)?)?;
            let _ = i;
            if !k.enforced {
                tail.push_str(" NOT ENFORCED");
            } else if !k.valid {
                tail.push_str(" NOT VALID");
            }
            format!("CHECK ({})", printed(cat, t, rel, &node, pretty)?)
        } else if let Some(f) = t.foreign_keys.iter().find(|f| f.name == *name) {
            let parent = catalog.tables.iter().find(|p| full(&p.name) == full(&f.parent));
            let Some(parent) = parent else {
                return Ok(None);
            };
            let poid = catalog.oids.of(&Object::Class(full(&f.parent))).unwrap_or(0);
            let mut s = format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                column_list(t, &f.columns, f.period),
                crate::regs::reg_text(crate::regs::RegKind::Class, poid, cat.release),
                column_list(parent, &f.parent_columns, f.period)
            );
            if f.full {
                s.push_str(" MATCH FULL");
            }
            let word = |a: &Action| match a {
                Action::NoAction => None,
                Action::Restrict => Some("RESTRICT"),
                Action::Cascade => Some("CASCADE"),
                Action::SetNull(_) => Some("SET NULL"),
                Action::SetDefault(_) => Some("SET DEFAULT"),
            };
            if let Some(w) = word(&f.on_update) {
                s.push_str(&format!(" ON UPDATE {w}"));
            }
            if let Some(w) = word(&f.on_delete) {
                s.push_str(&format!(" ON DELETE {w}"));
            }
            if let Action::SetNull(cols) | Action::SetDefault(cols) = &f.on_delete {
                if !cols.is_empty() {
                    s.push_str(&format!(" ({})", names(t, cols).join(", ")));
                }
            }
            if f.deferrable {
                tail.push_str(" DEFERRABLE");
            }
            if f.deferred {
                tail.push_str(" INITIALLY DEFERRED");
            }
            if !f.enforced {
                tail.push_str(" NOT ENFORCED");
            } else if !f.valid {
                tail.push_str(" NOT VALID");
            }
            s
        } else if let Some(c) = t
            .columns
            .iter()
            .find(|c| c.not_null && format!("{own}_{}_not_null", c.name) == *name)
        {
            format!("NOT NULL {}", quote(&c.name))
        } else {
            return Ok(None);
        }
    } else if let Some(d) = catalog.domains.iter().find(|d| full(&d.name) == *owner) {
        let ty = catalog.oids.of(&Object::Type(owner.clone())).unwrap_or(0);
        if let Some(k) = d.checks.iter().find(|k| k.name == *name) {
            let (base, typmod) = cat.base_and_typmod(ty, -1);
            let a = Analyzer {
                cat,
                columns: Vec::new(),
                value: Some((base, typmod)),
                scope: None,
            };
            let node = a.boolean(a.expr(&k.check)?)?;
            if !k.valid {
                tail.push_str(" NOT VALID");
            }
            let mut p = Printer {
                cat,
                paren: pretty,
                indent: true,
                level: 0,
                buf: String::new(),
                relations: Vec::new(),
                q: Default::default(),
            };
            p.expr(&node, false)?;
            format!("CHECK ({})", p.buf)
        } else if d.not_null_name.as_deref() == Some(name.as_str()) || d.not_null {
            "NOT NULL".to_string()
        } else {
            return Ok(None);
        }
    } else {
        return Ok(None);
    };
    let _ = (Context::Implicit, format_type);
    Ok(Some(format!("{body}{tail}")))
}

/// An index's trees as pg_index keeps them: its key expressions as one list, none where it has
/// none, and its predicate; with the table they read.
/// An index's key expressions as one list, its predicate, and the table they read.
type Trees = (Option<Node>, Option<Node>, u32);

pub(crate) fn index_trees(cat: &Cat, oid: u32) -> Result<Option<Trees>> {
    let Some(ix) = index_of(cat.catalog, oid) else {
        return Ok(None);
    };
    let t = ix.table;
    let rel = cat.catalog.oids.of(&Object::Class(full(&t.name))).unwrap_or(0);
    let a = analyzer(cat, t);
    let mut exprs = Vec::new();
    for k in &ix.keys {
        if let (None, Some(e)) = (k.column, &k.expr) {
            exprs.push(a.expr(&super::parse_expr(e)?)?);
        }
    }
    let pred = match &ix.predicate {
        Some(p) => Some(a.boolean(a.expr(&super::parse_expr(p)?)?)?),
        None => None,
    };
    Ok(Some(((!exprs.is_empty()).then_some(Node::List(exprs)), pred, rel)))
}

/// pg_get_triggerdef: a trigger of the file's as pg_get_triggerdef_worker writes it, as the
/// statement that would make it: its table schema-qualified, or where pretty named as the search
/// path finds it, the table its constraint names after FROM so in either, its function by its
/// name; none for an OID that names no trigger.
pub(crate) fn triggerdef(cat: &Cat, oid: u32, pretty: bool) -> Result<Option<String>> {
    use crate::regs::{reg_text, RegKind};
    let catalog = cat.catalog;
    if !matches!(catalog.oids.object(oid), Some(Object::Trigger(..))) {
        return Ok(None);
    }
    let own = oid.to_string();
    let Some(row) = crate::pgcatalog::file_rows(catalog, "pg_trigger")
        .into_iter()
        .find(|r| r.get("oid").cloned().flatten().as_deref() == Some(own.as_str()))
    else {
        return Ok(None);
    };
    let text = |c: &str| row.get(c).cloned().flatten().unwrap_or_default();
    let number = |c: &str| text(c).parse::<u32>().unwrap_or(0);
    let tgtype = number("tgtype");
    let constraint = number("tgconstraint") != 0;
    let mut s = format!(
        "CREATE {}TRIGGER {} ",
        if constraint { "CONSTRAINT " } else { "" },
        quote(&text("tgname"))
    );
    s.push_str(match (tgtype & TRIGGER_BEFORE != 0, tgtype & TRIGGER_INSTEAD != 0) {
        (true, _) => "BEFORE",
        (_, true) => "INSTEAD OF",
        _ => "AFTER",
    });
    let events: Vec<&str> = [
        (TRIGGER_INSERT, "INSERT"),
        (TRIGGER_DELETE, "DELETE"),
        (TRIGGER_UPDATE, "UPDATE"),
        (TRIGGER_TRUNCATE, "TRUNCATE"),
    ]
    .into_iter()
    .filter(|(bit, _)| tgtype & bit != 0)
    .map(|(_, e)| e)
    .collect();
    let rel = number("tgrelid");
    let on = match (pretty, catalog.oids.object(rel)) {
        (false, Some(Object::Class(name))) => {
            let (schema, table) = parts(name);
            format!("{}.{}", quote(schema), quote(table))
        }
        _ => reg_text(RegKind::Class, rel, cat.release),
    };
    s.push_str(&format!(" {} ON {on} ", events.join(" OR ")));
    if constraint {
        let other = number("tgconstrrelid");
        if other != 0 {
            s.push_str(&format!("FROM {} ", reg_text(RegKind::Class, other, cat.release)));
        }
        if text("tgdeferrable") != "t" {
            s.push_str("NOT ");
        }
        s.push_str("DEFERRABLE INITIALLY ");
        s.push_str(if text("tginitdeferred") == "t" { "DEFERRED " } else { "IMMEDIATE " });
    }
    s.push_str(if tgtype & TRIGGER_ROW != 0 { "FOR EACH ROW " } else { "FOR EACH STATEMENT " });
    s.push_str(&format!(
        "EXECUTE FUNCTION {}()",
        reg_text(RegKind::Proc, number("tgfoid"), cat.release)
    ));
    Ok(Some(s))
}

/// pg_trigger.tgtype's bits, as trigger.h numbers them.
const TRIGGER_ROW: u32 = 1;
const TRIGGER_BEFORE: u32 = 2;
const TRIGGER_INSERT: u32 = 4;
const TRIGGER_DELETE: u32 = 8;
const TRIGGER_UPDATE: u32 = 16;
const TRIGGER_TRUNCATE: u32 = 32;
const TRIGGER_INSTEAD: u32 = 64;

/// Whether an index of the file has expression keys, and whether it has a predicate.
pub(crate) fn index_has_trees(catalog: &crate::catalog::Catalog, oid: u32) -> (bool, bool) {
    match index_of(catalog, oid) {
        Some(ix) => (
            ix.keys.iter().any(|k| k.column.is_none()),
            ix.predicate.is_some(),
        ),
        None => (false, false),
    }
}

const INDOPTION_DESC: i16 = 1;
const INDOPTION_NULLS_FIRST: i16 = 2;

/// Each key of a file's index as pg_index's `indclass` and `indoption` hold it: the OID of the
/// operator class it names, else of its type's default under the method; DESC and NULLS FIRST as
/// their bits where the method orders, as a B-tree alone does. None for an OID no index of the
/// file's has.
pub(crate) fn index_classes(cat: &Cat, oid: u32) -> Result<Option<(Vec<u32>, Vec<i16>)>> {
    let Some(ix) = index_of(cat.catalog, oid) else {
        return Ok(None);
    };
    let t = ix.table;
    let a = analyzer(cat, t);
    let am = method_oid(ix.method);
    let mut classes = Vec::new();
    let mut options = Vec::new();
    for k in &ix.keys {
        let named = k.opclass.strip_prefix("pg_catalog.").unwrap_or(&k.opclass);
        let class = match named.is_empty() {
            false => Some(named.to_string()),
            true => {
                let keytype = match (k.column, &k.expr) {
                    (Some(c), _) => super::column_type(cat, &t.columns[c]).0,
                    (None, Some(e)) => a.expr(&super::parse_expr(e)?)?.ty(),
                    _ => 0,
                };
                default_opclass(cat, keytype, am)
            }
        };
        classes.push(class.map_or(0, |n| opclass_oid(cat.release, &n, am)));
        options.push(match ix.method {
            Method::Btree => {
                (if k.desc { INDOPTION_DESC } else { 0 })
                    | (if k.nulls_first { INDOPTION_NULLS_FIRST } else { 0 })
            }
            _ => 0,
        });
    }
    Ok(Some((classes, options)))
}
