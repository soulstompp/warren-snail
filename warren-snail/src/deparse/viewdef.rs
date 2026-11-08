//! A query printed as PostgreSQL 18's ruleutils.c prints it: get_query_def and what it calls, the
//! names of the range table's relations and columns chosen as set_deparse_for_query chooses them,
//! plain or pretty, the target list and FROM list wrapped at a column.

use super::analyze::Node;
use super::cat::Cat;
use super::print::{quote, Printer, INDENT_STD, INDENT_VAR};
use super::query::{self, Bound, Frame, JoinKind, Jt, Query, RteKind, SetOp, Sort, SubKind, Target, Window};
use crate::oids::{full, Object};
use crate::Result;
use std::collections::HashMap;
use std::rc::Rc;

const INDENT_JOIN: i32 = 4;

/// deparse_columns: the names a range table entry's columns print under.
#[derive(Clone, Debug, Default)]
pub(crate) struct Colinfo {
    pub colnames: Vec<Option<String>>,
    pub new_colnames: Vec<String>,
    /// Whether new_colnames is filled yet, which colname_is_unique then reads.
    filled: bool,
    pub printaliases: bool,
    parent_using: Vec<String>,
    leftrti: usize,
    rightrti: usize,
    leftattnos: Vec<i16>,
    rightattnos: Vec<i16>,
    pub using_names: Vec<String>,
}

/// deparse_namespace: a query level's range table and the names chosen for it.
pub(crate) struct Dpns {
    pub query: Rc<Query>,
    pub names: Vec<Option<String>>,
    pub cols: Vec<Colinfo>,
}

/// What a query level is printed under: deparse_context's query parts.
#[derive(Clone, Default)]
pub(crate) struct Ctx {
    pub ns: Vec<Rc<Dpns>>,
    pub varprefix: bool,
    pub result: Option<Rc<Vec<String>>>,
    pub query: Option<Rc<Query>>,
    pub visible: bool,
    pub in_group_by: bool,
    pub var_in_order_by: bool,
    pub wrap: i32,
}

fn expand_to(c: &mut Colinfo, n: usize) {
    if c.colnames.len() < n {
        c.colnames.resize(n, None);
    }
}

/// colname_is_unique.
fn unique(name: &str, c: &Colinfo, using: &[String]) -> bool {
    !(c.colnames.iter().flatten().any(|x| x == name)
        || (c.filled && c.new_colnames.iter().any(|x| x == name))
        || c.parent_using.iter().any(|x| x == name)
        || using.iter().any(|x| x == name))
}

/// make_colname_unique.
fn make_unique(name: &str, c: &Colinfo, using: &[String]) -> String {
    if unique(name, c, using) {
        return name.to_string();
    }
    (1..)
        .map(|i| format!("{name}_{i}"))
        .find(|m| unique(m, c, using))
        .unwrap_or_default()
}

fn rti_of(jt: &Jt) -> usize {
    match jt {
        Jt::Ref(i) => *i,
        Jt::Join(j) => j.rtindex,
    }
}

struct Naming<'q> {
    q: &'q Query,
    cols: Vec<Colinfo>,
    unique_using: bool,
    using_names: Vec<String>,
}

impl Naming<'_> {
    /// has_dangerous_join_using.
    fn dangerous(&self, jt: &Jt) -> bool {
        let Jt::Join(j) = jt else {
            return false;
        };
        if j.alias.is_none() && !j.using.is_empty() {
            if let RteKind::Join { vars, merged, .. } = &self.q.rtable[j.rtindex - 1].kind {
                if vars.iter().take(*merged).any(|v| !matches!(v, Node::Var { .. })) {
                    return true;
                }
            }
        }
        self.dangerous(&j.left) || self.dangerous(&j.right)
    }

    /// set_using_names.
    fn using(&mut self, jt: &Jt, parent_using: &[String]) {
        let Jt::Join(j) = jt else {
            return;
        };
        let ji = j.rtindex - 1;
        let rte = &self.q.rtable[ji];
        let RteKind::Join { left, right, merged, .. } = &rte.kind else {
            return;
        };
        // identify_join_columns.
        let n = rte.ecols.len();
        let (mut la, mut ra) = (vec![0i16; n], vec![0i16; n]);
        let mut jcolno = 0;
        for &a in left {
            la[jcolno] = a;
            jcolno += 1;
        }
        for (rcolno, &a) in right.iter().enumerate() {
            if rcolno < *merged {
                ra[rcolno] = a;
            } else {
                ra[jcolno] = a;
                jcolno += 1;
            }
        }
        let (lrti, rrti) = (rti_of(&j.left), rti_of(&j.right));
        {
            let c = &mut self.cols[ji];
            c.leftrti = lrti;
            c.rightrti = rrti;
            c.leftattnos = la.clone();
            c.rightattnos = ra.clone();
        }
        let push_down = |cols: &mut Vec<Colinfo>, i: usize, name: &str| {
            if la[i] > 0 {
                let l = &mut cols[lrti - 1];
                expand_to(l, la[i] as usize);
                l.colnames[la[i] as usize - 1] = Some(name.to_string());
            }
            if ra[i] > 0 {
                let r = &mut cols[rrti - 1];
                expand_to(r, ra[i] as usize);
                r.colnames[ra[i] as usize - 1] = Some(name.to_string());
            }
        };
        if rte.alias.is_none() {
            let names = self.cols[ji].colnames.clone();
            for (i, c) in names.iter().enumerate() {
                if let Some(c) = c {
                    push_down(&mut self.cols, i, c);
                }
            }
        }
        let mut pu = parent_using.to_vec();
        if !j.using.is_empty() {
            expand_to(&mut self.cols[ji], j.using.len());
            for (i, u) in j.using.iter().enumerate() {
                let name = match self.cols[ji].colnames[i].clone() {
                    Some(c) => c,
                    None => {
                        let base = rte.alias.as_ref().and_then(|a| a.cols.get(i)).unwrap_or(u);
                        let n = make_unique(base, &self.cols[ji], &self.using_names);
                        if self.unique_using {
                            self.using_names.push(n.clone());
                        }
                        self.cols[ji].colnames[i] = Some(n.clone());
                        n
                    }
                };
                self.cols[ji].using_names.push(name.clone());
                pu.push(name.clone());
                push_down(&mut self.cols, i, &name);
            }
        }
        self.cols[lrti - 1].parent_using = pu.clone();
        self.cols[rrti - 1].parent_using = pu.clone();
        self.using(&j.left, &pu);
        self.using(&j.right, &pu);
    }

    /// set_relation_column_names.
    fn relation(&mut self, i: usize) {
        let rte = &self.q.rtable[i];
        let real: Vec<String> = match &rte.kind {
            RteKind::Relation { columns, .. } => columns.clone(),
            _ => rte.ecols.clone(),
        };
        expand_to(&mut self.cols[i], real.len());
        let mut changed = false;
        let mut new = Vec::new();
        for (k, r) in real.iter().enumerate() {
            let name = match self.cols[i].colnames[k].clone() {
                Some(n) => n,
                None => {
                    let base = rte.alias.as_ref().and_then(|a| a.cols.get(k)).unwrap_or(r);
                    let n = make_unique(base, &self.cols[i], &self.using_names);
                    self.cols[i].colnames[k] = Some(n.clone());
                    n
                }
            };
            changed |= name != *r;
            new.push(name);
        }
        let c = &mut self.cols[i];
        c.new_colnames = new;
        c.filled = true;
        c.printaliases = match &rte.kind {
            RteKind::Relation { .. } => changed,
            _ => rte.alias.as_ref().is_some_and(|a| !a.cols.is_empty()) || changed,
        };
    }

    /// set_join_column_names.
    fn join(&mut self, i: usize) {
        let rte = &self.q.rtable[i];
        let (l, r) = (self.cols[i].leftrti - 1, self.cols[i].rightrti - 1);
        let nold = rte.ecols.len();
        expand_to(&mut self.cols[i], nold);
        let mut changed = false;
        let start = self.cols[i].using_names.len();
        for k in start..nold {
            let (la, ra) = (self.cols[i].leftattnos[k], self.cols[i].rightattnos[k]);
            let real = if la > 0 {
                self.cols[l].colnames.get(la as usize - 1).cloned().flatten()
            } else if ra > 0 {
                self.cols[r].colnames.get(ra as usize - 1).cloned().flatten()
            } else {
                Some(rte.ecols[k].clone())
            };
            let Some(real) = real else {
                self.cols[i].colnames[k] = None;
                continue;
            };
            if rte.alias.is_none() {
                self.cols[i].colnames[k] = Some(real);
                continue;
            }
            if self.cols[i].colnames[k].is_none() {
                let base = rte.alias.as_ref().and_then(|a| a.cols.get(k)).unwrap_or(&real).clone();
                let n = make_unique(&base, &self.cols[i], &self.using_names);
                self.cols[i].colnames[k] = Some(n);
            }
            changed |= self.cols[i].colnames[k].as_deref() != Some(real.as_str());
        }
        // The columns a fresh reading would give: merged first, then each side's others.
        let c = self.cols[i].clone();
        let mut new = Vec::new();
        let mut at = 0;
        let (mut lmerged, mut rmerged) = (Vec::new(), Vec::new());
        while at < nold && c.leftattnos[at] != 0 && c.rightattnos[at] != 0 {
            new.push(c.colnames[at].clone().unwrap_or_default());
            lmerged.push(c.leftattnos[at]);
            rmerged.push(c.rightattnos[at]);
            at += 1;
        }
        for (side, merged) in [(l, &lmerged), (r, &rmerged)] {
            let child = &self.cols[side];
            let mut ic = 0usize;
            for _ in 0..child.new_colnames.len() {
                while ic < child.colnames.len() && child.colnames[ic].is_none() {
                    ic += 1;
                }
                ic += 1;
                if merged.contains(&(ic as i16)) {
                    continue;
                }
                while at < c.colnames.len() && c.colnames[at].is_none() {
                    at += 1;
                }
                new.push(c.colnames.get(at).cloned().flatten().unwrap_or_default());
                at += 1;
            }
        }
        let c = &mut self.cols[i];
        c.new_colnames = new;
        c.filled = true;
        c.printaliases = rte.alias.is_some() && changed;
    }
}

/// set_rtable_names: each entry's name, made unique against the levels above and those before it.
fn rtable_names(q: &Query, parents: &[Rc<Dpns>]) -> Vec<Option<String>> {
    let mut used: HashMap<String, u32> = HashMap::new();
    for p in parents {
        for n in p.names.iter().flatten() {
            used.insert(n.clone(), 0);
        }
    }
    let mut out = Vec::new();
    for rte in &q.rtable {
        let refname = match (&rte.alias, &rte.kind) {
            (Some(a), _) => Some(a.name.clone()),
            (None, RteKind::Relation { name, .. }) => Some(name.clone()),
            (None, RteKind::Join { .. }) => None,
            (None, _) => Some(rte.ename.clone()),
        };
        out.push(refname.map(|r| {
            if let Some(&counter) = used.get(&r) {
                let mut c = counter;
                loop {
                    c += 1;
                    let m = format!("{r}_{c}");
                    if !used.contains_key(&m) {
                        used.insert(r.clone(), c);
                        used.insert(m.clone(), 0);
                        break m;
                    }
                }
            } else {
                used.insert(r.clone(), 0);
                r
            }
        }));
    }
    out
}

impl Dpns {
    /// set_deparse_for_query.
    fn of(q: Rc<Query>, parents: &[Rc<Dpns>]) -> Dpns {
        let names = rtable_names(&q, parents);
        let mut n = Naming {
            q: &q,
            cols: vec![Colinfo::default(); q.rtable.len()],
            unique_using: false,
            using_names: Vec::new(),
        };
        n.unique_using = q.from.iter().any(|jt| n.dangerous(jt));
        for jt in &q.from {
            n.using(jt, &[]);
        }
        for i in 0..q.rtable.len() {
            if matches!(q.rtable[i].kind, RteKind::Join { .. }) {
                n.join(i);
            } else {
                n.relation(i);
            }
        }
        let cols = n.cols;
        Dpns { query: q, names, cols }
    }
}

/// Whether a relation's name alone finds it under the search path: pg_catalog's relations first.
fn relation_visible(cat: &Cat, name: &str, namespace: u32) -> bool {
    match namespace {
        11 => true,
        2200 => !crate::pgcatalog::built_in("pg_class", cat.release).iter().any(|r| {
            r.get("relname").cloned().flatten().as_deref() == Some(name)
                && r.get("relnamespace").cloned().flatten().as_deref() == Some("11")
        }),
        _ => false,
    }
}

impl Printer<'_, '_> {
    /// get_query_def: a query level printed under the levels around it.
    pub(crate) fn query_def(&mut self, q: Rc<Query>, result: Option<Rc<Vec<String>>>, visible: bool, start: i32) -> Result<()> {
        let parents = self.q.ns.clone();
        let varprefix = !parents.is_empty() || q.rtable.len() != 1;
        let dpns = Rc::new(Dpns::of(q.clone(), &parents));
        let mut ns = vec![dpns];
        ns.extend(parents);
        let ctx = Ctx {
            ns,
            varprefix,
            result,
            query: None,
            visible,
            in_group_by: false,
            var_in_order_by: false,
            wrap: self.q.wrap,
        };
        let saved = std::mem::replace(&mut self.q, ctx);
        let level = std::mem::replace(&mut self.level, start);
        let r = self.select_query(&q);
        self.q = saved;
        self.level = level;
        r
    }

    /// get_select_query_def.
    fn select_query(&mut self, q: &Rc<Query>) -> Result<()> {
        self.with_clause(q)?;
        self.q.query = Some(q.clone());
        let force = match &q.setop {
            Some(op) => {
                self.setop(op, q)?;
                true
            }
            None => {
                self.basic_select(q)?;
                false
            }
        };
        if !q.sort.is_empty() {
            self.keyword(" ORDER BY ", -INDENT_STD, INDENT_STD, 1);
            self.orderby(&q.sort, force)?;
        }
        if let Some(o) = &q.offset {
            self.keyword(" OFFSET ", -INDENT_STD, INDENT_STD, 0);
            self.expr(o, false)?;
        }
        if let Some(l) = &q.limit {
            if q.with_ties {
                self.keyword(" FETCH FIRST ", -INDENT_STD, INDENT_STD, 0);
                self.push("(");
                self.expr(l, false)?;
                self.push(") ROWS WITH TIES");
            } else {
                self.keyword(" LIMIT ", -INDENT_STD, INDENT_STD, 0);
                if matches!(l, Node::Const { text: None, .. }) {
                    self.push("ALL");
                } else {
                    self.expr(l, false)?;
                }
            }
        }
        Ok(())
    }

    /// get_with_clause.
    fn with_clause(&mut self, q: &Rc<Query>) -> Result<()> {
        if q.ctes.is_empty() {
            return Ok(());
        }
        if self.indent {
            self.level += INDENT_STD;
            self.push(" ");
        }
        let mut sep = if q.recursive { "WITH RECURSIVE " } else { "WITH " };
        for c in &q.ctes {
            self.push(sep);
            self.push(&quote(&c.name));
            if !c.cols.is_empty() {
                let cols: Vec<String> = c.cols.iter().map(|x| quote(x)).collect();
                self.push(&format!("({})", cols.join(", ")));
            }
            self.push(" AS ");
            match c.materialized {
                Some(true) => self.push("MATERIALIZED "),
                Some(false) => self.push("NOT MATERIALIZED "),
                None => {}
            }
            self.push("(");
            if self.indent {
                self.keyword("", 0, 0, 0);
            }
            self.query_def(Rc::new((*c.query).clone()), None, true, self.level)?;
            if self.indent {
                self.keyword("", 0, 0, 0);
            }
            self.push(")");
            sep = ", ";
        }
        if self.indent {
            self.level -= INDENT_STD;
            self.keyword("", 0, 0, 0);
        } else {
            self.push(" ");
        }
        Ok(())
    }

    /// get_simple_values_rte: the rows of a query that is a VALUES list and nothing else.
    fn simple_values(&self, q: &Query) -> Option<Vec<Vec<Node>>> {
        let mut found = None;
        for rte in &q.rtable {
            match &rte.kind {
                RteKind::Values { rows } if rte.in_from => {
                    if found.is_some() {
                        return None;
                    }
                    found = Some((rows, &rte.ecols));
                }
                RteKind::Relation { .. } if !rte.in_from => {}
                _ => return None,
            }
        }
        let (rows, ecols) = found?;
        if q.targets.len() != ecols.len() {
            return None;
        }
        for (colno, (t, c)) in q.targets.iter().zip(ecols).enumerate() {
            if t.junk {
                return None;
            }
            let name = self.q.result.as_ref().and_then(|r| r.get(colno).cloned()).or_else(|| t.name.clone());
            if name.as_deref() != Some(c.as_str()) {
                return None;
            }
        }
        Some(rows.clone())
    }

    /// get_values_def.
    fn values(&mut self, rows: &[Vec<Node>]) -> Result<()> {
        self.push("VALUES ");
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.push("(");
            for (k, col) in row.iter().enumerate() {
                if k > 0 {
                    self.push(",");
                }
                self.toplevel(col, false)?;
            }
            self.push(")");
        }
        Ok(())
    }

    /// get_basic_select_query.
    fn basic_select(&mut self, q: &Rc<Query>) -> Result<()> {
        if self.indent {
            self.level += INDENT_STD;
            self.push(" ");
        }
        if let Some(rows) = self.simple_values(q) {
            return self.values(&rows);
        }
        self.push("SELECT");
        if q.distinct {
            self.push(" DISTINCT");
        }
        self.target_list(&q.targets)?;
        self.print_from_list(q, " FROM ")?;
        if let Some(w) = &q.quals {
            self.keyword(" WHERE ", -INDENT_STD, INDENT_STD, 1);
            self.expr(w, false)?;
        }
        if !q.group.is_empty() {
            self.keyword(" GROUP BY ", -INDENT_STD, INDENT_STD, 1);
            let save = std::mem::replace(&mut self.q.in_group_by, true);
            for (i, g) in q.group.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.sortgroup(g, false)?;
            }
            self.q.in_group_by = save;
        }
        if let Some(h) = &q.having {
            self.keyword(" HAVING ", -INDENT_STD, INDENT_STD, 0);
            self.expr(h, false)?;
        }
        let mut first = true;
        for w in &q.windows {
            let Some(name) = &w.name else {
                continue;
            };
            if first {
                self.keyword(" WINDOW ", -INDENT_STD, INDENT_STD, 1);
                first = false;
            } else {
                self.push(", ");
            }
            self.push(&format!("{} AS ", quote(name)));
            self.window_spec(w)?;
        }
        Ok(())
    }

    /// The length of the buffer's last line, in bytes as strlen counts them.
    fn line_length(&self) -> usize {
        self.buf.rsplit('\n').next().map_or(0, str::len)
    }

    /// get_target_list.
    fn target_list(&mut self, targets: &[Target]) -> Result<()> {
        let mut sep = " ";
        let mut colno = 0;
        let mut last_multiline = false;
        for t in targets.iter().filter(|t| !t.junk) {
            self.push(sep);
            sep = ", ";
            colno += 1;
            let outer = std::mem::take(&mut self.buf);
            let attname = if matches!(t.expr, Node::Var { .. }) {
                self.variable(&t.expr, 0, true)
            } else {
                self.expr(&t.expr, true)?;
                (!self.q.visible).then(|| "?column?".to_string())
            };
            let colname = self
                .q
                .result
                .as_ref()
                .and_then(|r| r.get(colno - 1).cloned())
                .or_else(|| t.name.clone());
            if let Some(c) = colname {
                if attname.as_deref() != Some(c.as_str()) {
                    self.push(&format!(" AS {}", quote(&c)));
                }
            }
            let item = std::mem::replace(&mut self.buf, outer);
            if self.indent && self.q.wrap >= 0 {
                let leading = item.starts_with('\n');
                if leading {
                    while self.buf.ends_with(' ') {
                        self.buf.pop();
                    }
                } else if colno > 1 && (self.line_length() + item.len() > self.q.wrap as usize || last_multiline) {
                    self.keyword("", -INDENT_STD, INDENT_STD, INDENT_VAR);
                }
                last_multiline = if leading { item[1..].contains('\n') } else { item.contains('\n') };
            }
            self.buf.push_str(&item);
        }
        Ok(())
    }

    /// get_variable: a column by the name chosen for it, qualified where the level needs it;
    /// the name, where it is one.
    pub(crate) fn variable(&mut self, var: &Node, levelsup: u32, toplevel: bool) -> Option<String> {
        let Node::Var {
            attno,
            ty,
            typmod,
            rel,
            up,
            syn,
        } = var
        else {
            return None;
        };
        let net = (*up + levelsup) as usize;
        let Some(dpns) = self.q.ns.get(net).cloned() else {
            self.push(&format!("${attno}"));
            return None;
        };
        let (varno, varattno) = if syn.0 > 0 { (syn.0 as usize, syn.1) } else { (*rel as usize, *attno) };
        let Some(rte) = dpns.query.rtable.get(varno.wrapping_sub(1)) else {
            self.push(&format!("${attno}"));
            return None;
        };
        if let (RteKind::Join { vars, .. }, None) = (&rte.kind, &rte.alias) {
            if varattno > 0 {
                if let Some(av @ Node::Var { .. }) = vars.get(varattno as usize - 1) {
                    return self.variable(av, *up + levelsup, toplevel);
                }
            }
        }
        let refname = dpns.names[varno - 1].clone();
        let attname = (varattno != 0).then(|| {
            dpns.cols[varno - 1]
                .colnames
                .get(varattno as usize - 1)
                .cloned()
                .flatten()
                .unwrap_or_else(|| "?dropped?column?".into())
        });
        let mut need_prefix = self.q.varprefix || attname.is_none();
        if self.q.var_in_order_by && !self.q.in_group_by && !need_prefix {
            if let Some(q) = self.q.query.clone() {
                for (colno, t) in q.output().enumerate() {
                    let colname = self
                        .q
                        .result
                        .as_ref()
                        .and_then(|r| r.get(colno).cloned())
                        .or_else(|| t.name.clone());
                    if colname.is_some() && colname == attname && !query::same(var, &t.expr) {
                        need_prefix = true;
                        break;
                    }
                }
            }
        }
        if let (Some(r), true) = (&refname, need_prefix) {
            self.push(&format!("{}.", quote(r)));
        }
        match &attname {
            Some(a) => self.push(&quote(a)),
            None => {
                self.push("*");
                if toplevel {
                    let t = super::print::format_type(self.cat, *ty, *typmod);
                    self.push(&format!("::{t}"));
                }
            }
        }
        attname
    }

    /// get_from_clause.
    fn print_from_list(&mut self, q: &Query, prefix: &str) -> Result<()> {
        let mut first = true;
        for jt in &q.from {
            if let Jt::Ref(i) = jt {
                if !q.rtable[*i - 1].in_from {
                    continue;
                }
            }
            if first {
                self.keyword(prefix, -INDENT_STD, INDENT_STD, 2);
                first = false;
                self.print_from_entry(jt, q)?;
                continue;
            }
            self.push(", ");
            let outer = std::mem::take(&mut self.buf);
            self.print_from_entry(jt, q)?;
            let item = std::mem::replace(&mut self.buf, outer);
            if self.indent && self.q.wrap >= 0 {
                if item.starts_with('\n') {
                    while self.buf.ends_with(' ') {
                        self.buf.pop();
                    }
                } else if self.line_length() + item.len() > self.q.wrap as usize {
                    self.keyword("", -INDENT_STD, INDENT_STD, INDENT_VAR);
                }
            }
            self.buf.push_str(&item);
        }
        Ok(())
    }

    /// generate_relation_name: bare where the search path finds it and no WITH query hides it.
    fn relation_name(&self, name: &str, namespace: u32) -> String {
        let hidden = self.q.ns.iter().any(|d| d.query.ctes.iter().any(|c| c.name == name));
        if !hidden && relation_visible(self.cat, name, namespace) {
            quote(name)
        } else {
            format!("{}.{}", quote(&self.cat.namespace(namespace)), quote(name))
        }
    }

    /// get_from_clause_item.
    fn print_from_entry(&mut self, jt: &Jt, q: &Query) -> Result<()> {
        let dpns = self.q.ns[0].clone();
        match jt {
            Jt::Ref(i) => {
                let rte = &q.rtable[*i - 1];
                match &rte.kind {
                    RteKind::Relation { name, namespace, .. } => {
                        let n = self.relation_name(name, *namespace);
                        self.push(&n);
                    }
                    RteKind::Subquery(sub) => {
                        self.push("(");
                        self.query_def(Rc::new((**sub).clone()), None, true, self.level)?;
                        self.push(")");
                    }
                    RteKind::Values { rows } => {
                        self.push("(");
                        self.values(rows)?;
                        self.push(")");
                    }
                    RteKind::Cte { name } => self.push(&quote(name)),
                    RteKind::Join { .. } => {}
                }
                // get_rte_alias.
                let refname = dpns.names[*i - 1].clone().unwrap_or_default();
                let ci = &dpns.cols[*i - 1];
                let print = rte.alias.is_some()
                    || ci.printaliases
                    || match &rte.kind {
                        RteKind::Relation { name, .. } => refname != *name,
                        RteKind::Subquery(_) | RteKind::Values { .. } => true,
                        RteKind::Cte { name } => refname != *name,
                        RteKind::Join { .. } => false,
                    };
                if print {
                    self.push(&format!(" {}", quote(&refname)));
                }
                self.column_aliases(ci);
            }
            Jt::Join(j) => {
                let ci = dpns.cols[j.rtindex - 1].clone();
                let right_paren = self.paren
                    && match &j.right {
                        Jt::Ref(_) => false,
                        Jt::Join(r) => r.alias.is_none(),
                    };
                let parens = !self.paren || j.alias.is_some();
                if parens {
                    self.push("(");
                }
                self.print_from_entry(&j.left, q)?;
                let word = match j.kind {
                    JoinKind::Inner if j.quals.is_some() || !j.using.is_empty() => " JOIN ",
                    JoinKind::Inner => " CROSS JOIN ",
                    JoinKind::Left => " LEFT JOIN ",
                    JoinKind::Full => " FULL JOIN ",
                    JoinKind::Right => " RIGHT JOIN ",
                };
                self.keyword(word, -INDENT_STD, INDENT_STD, INDENT_JOIN);
                if right_paren {
                    self.push("(");
                }
                self.print_from_entry(&j.right, q)?;
                if right_paren {
                    self.push(")");
                }
                if !j.using.is_empty() {
                    let names: Vec<String> = ci.using_names.iter().map(|n| quote(n)).collect();
                    self.push(&format!(" USING ({})", names.join(", ")));
                } else if let Some(qu) = &j.quals {
                    self.push(" ON ");
                    if !self.paren {
                        self.push("(");
                    }
                    self.expr(qu, false)?;
                    if !self.paren {
                        self.push(")");
                    }
                } else if j.kind != JoinKind::Inner {
                    self.push(" ON TRUE");
                }
                if parens {
                    self.push(")");
                }
                if j.alias.is_some() {
                    let n = dpns.names[j.rtindex - 1].clone().unwrap_or_default();
                    self.push(&format!(" {}", quote(&n)));
                    self.column_aliases(&ci);
                }
            }
        }
        Ok(())
    }

    /// get_column_alias_list.
    fn column_aliases(&mut self, ci: &Colinfo) {
        if !ci.printaliases || ci.new_colnames.is_empty() {
            return;
        }
        let names: Vec<String> = ci.new_colnames.iter().map(|n| quote(n)).collect();
        self.push(&format!("({})", names.join(", ")));
    }

    /// get_setop_query.
    fn setop(&mut self, op: &SetOp, q: &Query) -> Result<()> {
        match op {
            SetOp::Leaf(i) => {
                let RteKind::Subquery(sub) = &q.rtable[*i - 1].kind else {
                    return Ok(());
                };
                let need = !sub.ctes.is_empty()
                    || !sub.sort.is_empty()
                    || sub.offset.is_some()
                    || sub.limit.is_some()
                    || sub.setop.is_some();
                if need {
                    self.push("(");
                }
                let (result, visible) = (self.q.result.clone(), self.q.visible);
                self.query_def(Rc::new((**sub).clone()), result, visible, self.level)?;
                if need {
                    self.push(")");
                }
            }
            SetOp::Op { op, all, left, right } => {
                let need = match left.as_ref() {
                    SetOp::Op { op: lop, all: lall, .. } => !(op == lop && all == lall),
                    SetOp::Leaf(_) => false,
                };
                let sub = if need {
                    self.push("(");
                    self.keyword("", INDENT_STD, 0, 0);
                    INDENT_STD
                } else {
                    0
                };
                self.setop(left, q)?;
                if need {
                    self.keyword(") ", -sub, 0, 0);
                } else if self.indent {
                    self.keyword("", -sub, 0, 0);
                } else {
                    self.push(" ");
                }
                self.push(&format!("{op} "));
                if *all {
                    self.push("ALL ");
                }
                let need = matches!(right.as_ref(), SetOp::Op { .. });
                let sub = if need {
                    self.push("(");
                    INDENT_STD
                } else {
                    0
                };
                self.keyword("", sub, 0, 0);
                let save = std::mem::replace(&mut self.q.visible, false);
                self.setop(right, q)?;
                self.q.visible = save;
                if self.indent {
                    self.level -= sub;
                }
                if need {
                    self.keyword(")", 0, 0, 0);
                }
            }
        }
        Ok(())
    }

    /// get_rule_sortgroupclause.
    fn sortgroup(&mut self, s: &Sort, force: bool) -> Result<()> {
        if force {
            self.push(&s.resno.to_string());
            return Ok(());
        }
        match &s.expr {
            Node::Const { .. } => self.constant(&s.expr, 1),
            Node::Var { .. } => {
                let save = std::mem::replace(&mut self.q.var_in_order_by, true);
                self.variable(&s.expr, 0, false);
                self.q.var_in_order_by = save;
            }
            e => {
                let need = self.paren || matches!(e, Node::Func { .. } | Node::Agg { .. } | Node::Window { .. });
                if need {
                    self.push("(");
                }
                self.expr(e, true)?;
                if need {
                    self.push(")");
                }
            }
        }
        Ok(())
    }

    /// get_rule_orderby.
    pub(crate) fn orderby(&mut self, sorts: &[Sort], force: bool) -> Result<()> {
        for (i, s) in sorts.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.sortgroup(s, force)?;
            if !s.desc {
                if s.nulls_first {
                    self.push(" NULLS FIRST");
                }
            } else {
                self.push(" DESC");
                if !s.nulls_first {
                    self.push(" NULLS LAST");
                }
            }
        }
        Ok(())
    }

    /// get_rule_windowspec.
    fn window_spec(&mut self, w: &Window) -> Result<()> {
        self.push("(");
        let mut space = false;
        if !w.partition.is_empty() {
            self.push("PARTITION BY ");
            for (i, p) in w.partition.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.sortgroup(p, false)?;
            }
            space = true;
        }
        if !w.order.is_empty() {
            if space {
                self.push(" ");
            }
            self.push("ORDER BY ");
            self.orderby(&w.order, false)?;
            space = true;
        }
        if let Some(f) = &w.frame {
            if space {
                self.push(" ");
            }
            self.frame(f)?;
        }
        self.push(")");
        Ok(())
    }

    /// get_window_frame_options.
    fn frame(&mut self, f: &Frame) -> Result<()> {
        self.push(&format!("{} ", f.units));
        if f.end.is_some() {
            self.push("BETWEEN ");
        }
        self.bound(&f.start)?;
        if let Some(e) = &f.end {
            self.push("AND ");
            self.bound(e)?;
        }
        self.buf.pop();
        Ok(())
    }

    fn bound(&mut self, b: &Bound) -> Result<()> {
        match b {
            Bound::UnboundedPreceding => self.push("UNBOUNDED PRECEDING "),
            Bound::UnboundedFollowing => self.push("UNBOUNDED FOLLOWING "),
            Bound::CurrentRow => self.push("CURRENT ROW "),
            Bound::Preceding(n) => {
                self.expr(n, false)?;
                self.push(" PRECEDING ");
            }
            Bound::Following(n) => {
                self.expr(n, false)?;
                self.push(" FOLLOWING ");
            }
        }
        Ok(())
    }

    /// get_agg_expr.
    pub(crate) fn agg(&mut self, n: &Node) -> Result<()> {
        let Node::Agg {
            oid,
            args,
            order,
            distinct,
            star,
            filter,
            variadic,
            ..
        } = n
        else {
            return Ok(());
        };
        let types: Vec<u32> = args.iter().map(Node::ty).collect();
        let name = self.function_name(*oid, &types);
        self.push(&format!("{name}({}", if *distinct { "DISTINCT " } else { "" }));
        if *star {
            self.push("*");
        } else {
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                if *variadic && i + 1 == args.len() {
                    self.push("VARIADIC ");
                }
                self.expr(a, true)?;
            }
        }
        if !order.is_empty() {
            self.push(" ORDER BY ");
            self.orderby(order, false)?;
        }
        if let Some(f) = filter {
            self.push(") FILTER (WHERE ");
            self.expr(f, false)?;
        }
        self.push(")");
        Ok(())
    }

    /// get_windowfunc_expr.
    pub(crate) fn window_call(&mut self, n: &Node) -> Result<()> {
        let Node::Window {
            oid,
            args,
            star,
            filter,
            win,
            ..
        } = n
        else {
            return Ok(());
        };
        let types: Vec<u32> = args.iter().map(Node::ty).collect();
        let name = self.function_name(*oid, &types);
        self.push(&format!("{name}("));
        if *star {
            self.push("*");
        } else {
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.expr(a, true)?;
            }
        }
        if let Some(f) = filter {
            self.push(") FILTER (WHERE ");
            self.expr(f, false)?;
        }
        self.push(") OVER ");
        let w = self.q.query.as_ref().and_then(|q| q.windows.get(*win).cloned());
        let Some(w) = w else {
            return Err(crate::Error::new(format!("could not find window clause for winref {}", win + 1)));
        };
        match &w.name {
            Some(name) => self.push(&quote(name)),
            None => self.window_spec(&w)?,
        }
        Ok(())
    }

    /// get_sublink_expr.
    pub(crate) fn sublink_expr(&mut self, n: &Node) -> Result<()> {
        let Node::SubLink { kind, test, query, .. } = n else {
            return Ok(());
        };
        self.push(if *kind == SubKind::Array { "ARRAY(" } else { "(" });
        let mut opname = String::new();
        if let Some((left, oid, right)) = test {
            self.expr(left, true)?;
            opname = self.operator_name(*oid, left.ty(), *right);
        }
        let need = match kind {
            SubKind::Exists => {
                self.push("EXISTS ");
                true
            }
            SubKind::Any => {
                if opname == "=" {
                    self.push(" IN ");
                } else {
                    self.push(&format!(" {opname} ANY "));
                }
                true
            }
            SubKind::All => {
                self.push(&format!(" {opname} ALL "));
                true
            }
            SubKind::Expr | SubKind::Array => false,
        };
        if need {
            self.push("(");
        }
        self.query_def(Rc::new((**query).clone()), None, false, self.level)?;
        self.push(if need { "))" } else { ")" });
        Ok(())
    }
}

/// pg_get_viewdef: a view's question as PostgreSQL prints it, None where the relation is no view.
pub(crate) fn viewdef(cat: &Cat, oid: u32, pretty: bool, wrap: i32) -> Result<Option<String>> {
    let catalog = cat.catalog;
    if let Some(Object::Class(name)) = catalog.oids.object(oid) {
        let Some(v) = catalog.views.iter().find(|v| full(&v.name) == *name) else {
            return Ok(None);
        };
        let (q, cols) = query::view_query(cat, v)?;
        let names: Vec<String> = cols.into_iter().map(|c| c.0).collect();
        let mut p = Printer {
            cat,
            paren: pretty,
            indent: true,
            level: 0,
            buf: String::new(),
            relations: Vec::new(),
            q: Ctx {
                wrap,
                ..Ctx::default()
            },
        };
        p.query_def(Rc::new(q), Some(Rc::new(names)), true, 0)?;
        p.buf.push(';');
        return Ok(Some(p.buf));
    }
    // A built-in view: the text PostgreSQL wrote, carried.
    let Some(name) = built_in_view(cat, oid) else {
        return Ok(None);
    };
    if wrap != 0 {
        return Err(crate::Error::new(format!(
            "pg_get_viewdef of {name} at a wrap column other than 0: a built-in view's text is carried as PostgreSQL wrote it, not its query"
        )));
    }
    Ok(crate::read::sysviews::carried_viewdef(&name, cat.release, pretty))
}

/// A built-in view's `schema.name` by its OID.
fn built_in_view(cat: &Cat, oid: u32) -> Option<String> {
    let rows = crate::pgcatalog::built_in("pg_class", cat.release);
    let r = rows.iter().find(|r| {
        r.get("oid").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(oid)
            && r.get("relkind").cloned().flatten().as_deref() == Some("v")
    })?;
    let ns: u32 = r.get("relnamespace").cloned().flatten()?.parse().ok()?;
    Some(format!("{}.{}", cat.namespace(ns), r.get("relname").cloned().flatten()?))
}

/// pg_get_ruledef: a rule as CREATE RULE writes it, a view's the view's question in a rule's words;
/// None where no rule has the OID.
pub(crate) fn ruledef(cat: &Cat, oid: u32, pretty: bool) -> Result<Option<String>> {
    let catalog = cat.catalog;
    if let Some(Object::Rule(view)) = catalog.oids.object(oid) {
        let Some(relid) = catalog.oids.of(&Object::Class(view.clone())) else {
            return Ok(None);
        };
        let Some(text) = viewdef(cat, relid, pretty, 0)? else {
            return Ok(None);
        };
        let (schema, name) = view.split_once('.').unwrap_or(("public", view));
        let namespace = query::namespace_oid(cat, schema);
        return Ok(Some(rule_text(cat, "_RETURN", schema, name, namespace, pretty, &text)));
    }
    // A built-in rule: a view's, its view's carried text; pg_settings' two, carried whole.
    let field = |r: &HashMap<&'static str, Option<String>>, k: &str| r.get(k).cloned().flatten();
    let rules = crate::pgcatalog::built_in("pg_rewrite", cat.release);
    let Some(rule) = rules.iter().find(|r| field(r, "oid").and_then(|s| s.parse::<u32>().ok()) == Some(oid)) else {
        return Ok(None);
    };
    let (Some(rulename), Some(ev_class)) = (field(rule, "rulename"), field(rule, "ev_class").and_then(|s| s.parse::<u32>().ok())) else {
        return Ok(None);
    };
    let classes = crate::pgcatalog::built_in("pg_class", cat.release);
    let Some(class) = classes.iter().find(|r| field(r, "oid").and_then(|s| s.parse::<u32>().ok()) == Some(ev_class)) else {
        return Ok(None);
    };
    let (Some(name), Some(namespace)) = (field(class, "relname"), field(class, "relnamespace").and_then(|s| s.parse::<u32>().ok())) else {
        return Ok(None);
    };
    let schema = cat.namespace(namespace);
    let relation = format!("{schema}.{name}");
    if rulename != "_RETURN" {
        return Ok(crate::read::sysviews::carried_ruledef(&format!("{relation}.{rulename}"), cat.release, pretty));
    }
    Ok(crate::read::sysviews::carried_viewdef(&relation, cat.release, pretty)
        .map(|text| rule_text(cat, &rulename, &schema, &name, namespace, pretty, &text)))
}

/// make_ruledef's words around a view's question: its relation qualified always, or, pretty, where
/// the search path does not find it.
fn rule_text(cat: &Cat, rulename: &str, schema: &str, name: &str, namespace: u32, pretty: bool, text: &str) -> String {
    let relation = if pretty && relation_visible(cat, name, namespace) {
        quote(name)
    } else {
        format!("{}.{}", quote(schema), quote(name))
    };
    format!("CREATE RULE {} AS\n    ON SELECT TO {relation} DO INSTEAD {text}", quote(rulename))
}
