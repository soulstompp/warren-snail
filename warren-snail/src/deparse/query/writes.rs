//! The writes as PostgreSQL's parse analysis reads them (analyze.c's transformInsertStmt,
//! transformUpdateStmt and transformDeleteStmt), each clause in its order, each value coerced to
//! the column it is written to: what types a write's parameters.

use super::{analyze, not_read, Builder, Ns, Query, Rte, RteKind, Target};
use crate::deparse::analyze::{folded, Context, Form, Node};
use crate::deparse::cat;
use crate::error::NOT_ASSIGNED;
use crate::{Error, Result};
use sqlparser::ast as A;

/// A column a write may write: its name, type and typmod.
struct Col {
    name: String,
    ty: u32,
    typmod: i32,
}

/// The table a write writes, as its namespace item gives it.
struct Written {
    ns: Ns,
    cols: Vec<Col>,
    name: String,
}

impl Written {
    fn of(ns: Ns, name: String) -> Written {
        let cols = ns
            .names
            .iter()
            .zip(&ns.cols)
            .map(|(n, c)| Col {
                name: n.clone(),
                ty: c.ty,
                typmod: c.typmod,
            })
            .collect();
        Written { ns, cols, name }
    }

    /// checkInsertTargets and transformUpdateTargetList: the column a name writes.
    fn column(&self, n: &str) -> Result<usize> {
        self.cols.iter().position(|c| c.name == n).ok_or_else(|| {
            Error::new(format!("column \"{n}\" of relation \"{}\" does not exist", self.name))
        })
    }
}

/// Whether an expression is the keyword DEFAULT, where a value may be written.
fn is_default(e: &A::Expr) -> bool {
    matches!(e, A::Expr::Identifier(i) if i.quote_style.is_none() && i.value.eq_ignore_ascii_case("default"))
}

/// A column's name in a write's list of them.
fn column_name(n: &A::ObjectName) -> Result<String> {
    match n.0.as_slice() {
        [p] => p.as_ident().map(folded).ok_or_else(|| not_read(n)),
        _ => Err(not_read(n)),
    }
}

impl Builder<'_, '_, '_> {
    /// A write: its range table, its WHERE, each value it writes and its RETURNING list.
    pub(super) fn write(&self, s: &A::Statement) -> Result<Query> {
        match s {
            A::Statement::Insert(i) => self.insert(i),
            A::Statement::Update(u) => self.update(u),
            A::Statement::Delete(d) => self.delete(d),
            other => Err(not_read(other)),
        }
    }

    /// setTargetTable: the table a write names, in the range table; in the namespace where
    /// `visible`.
    fn target(&self, t: &A::TableFactor, visible: bool) -> Result<Written> {
        let A::TableFactor::Table { name, alias, args: None, .. } = t else {
            return Err(not_read(t));
        };
        let alias = alias.as_ref().map(super::table_alias).transpose()?;
        self.target_named(name, alias, visible)
    }

    fn target_named(&self, name: &A::ObjectName, alias: Option<super::Alias>, visible: bool) -> Result<Written> {
        let (_, ns, _) = self.relation_item(name, alias, false)?;
        let bare = name.0.last().and_then(|p| p.as_ident()).map(folded).unwrap_or_default();
        if visible {
            self.scope.names.borrow_mut().push(ns.clone());
        }
        Ok(Written::of(ns, bare))
    }

    /// transformAssignedExpr: a value coerced to its column as an assignment coerces it.
    fn assigned(&self, n: Node, c: &Col) -> Result<Node> {
        let a = self.analyzer();
        let from = n.ty();
        if !a.can_coerce_all(&[from], &[c.ty], Context::Assignment) {
            return Err(Error::new(format!(
                "column \"{}\" is of type {} but expression is of type {}{NOT_ASSIGNED}",
                c.name,
                crate::deparse::print::format_type_be(self.cat, c.ty),
                crate::deparse::print::format_type_be(self.cat, from)
            )));
        }
        a.coerce_to_target(n, c.ty, c.typmod, Context::Assignment, Form::Implicit)
    }

    /// transformInsertRow: a row's values, each to the column it is written to.
    fn insert_row(&self, row: Vec<Option<Node>>, w: &Written, cols: &[usize], named: bool, out: &mut Vec<Node>) -> Result<()> {
        if row.len() > cols.len() {
            return Err(Error::new("INSERT has more expressions than target columns"));
        }
        if named && row.len() < cols.len() {
            return Err(Error::new("INSERT has more target columns than expressions"));
        }
        for (n, &c) in row.into_iter().zip(cols) {
            if let Some(n) = n {
                out.push(self.assigned(n, &w.cols[c])?);
            }
        }
        Ok(())
    }

    /// transformInsertStmt.
    fn insert(&self, i: &A::Insert) -> Result<Query> {
        if i.or.is_some() || i.ignore || !i.assignments.is_empty() || i.partitioned.is_some() || i.output.is_some() {
            return Err(not_read(i));
        }
        let A::TableObject::TableName(name) = &i.table else {
            return Err(not_read(i));
        };
        let alias = i.table_alias.as_ref().map(|a| super::Alias {
            name: folded(&a.alias),
            cols: Vec::new(),
        });
        // The table written is in the range table, and in no namespace its rows are read in.
        let w = self.target_named(name, alias, false)?;
        let cols: Vec<usize> = match i.columns.is_empty() {
            true => (0..w.cols.len()).collect(),
            false => i
                .columns
                .iter()
                .map(|c| w.column(&column_name(c)?))
                .collect::<Result<_>>()?,
        };
        let named = !i.columns.is_empty();
        let mut assigned = Vec::new();
        if let Some(q) = i.source.as_deref() {
            let plain_values = match q.body.as_ref() {
                A::SetExpr::Values(v) if q.with.is_none() && q.order_by.is_none() && q.limit_clause.is_none() && q.fetch.is_none() => Some(v),
                _ => None,
            };
            match plain_values {
                // Each row's values read, then written to their columns, a row at a time.
                Some(v) => {
                    let a = self.analyzer();
                    for r in &v.rows {
                        let row = r
                            .content
                            .iter()
                            .map(|e| match is_default(e) {
                                true => Ok(None),
                                false => a.expr(e).map(Some),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        self.insert_row(row, &w, &cols, named, &mut assigned)?;
                    }
                }
                // A SELECT read as a query of its own, its unknown outputs left unknown: a literal
                // or a parameter among them is written to its column as it stands, the others
                // as the query's columns.
                None => {
                    let mut sub = analyze(self.cat, q, Some(self.scope), false)?;
                    let mut row = Vec::new();
                    let mut copied = Vec::new();
                    for (k, t) in sub.targets.iter().enumerate().filter(|(_, t)| !t.junk) {
                        let unknown = t.expr.ty() == cat::UNKNOWN;
                        match &t.expr {
                            Node::Const { .. } | Node::Param { .. } if unknown => {
                                row.push(Some(t.expr.clone()));
                                if let Node::Param { n, .. } = t.expr {
                                    copied.push((k, n));
                                }
                            }
                            e => row.push(Some(Node::Var {
                                attno: k as i16 + 1,
                                ty: e.ty(),
                                typmod: e.typmod(),
                                rel: 0,
                                up: 0,
                                syn: (0, k as i16 + 1),
                            })),
                        }
                    }
                    self.insert_row(row, &w, &cols, named, &mut assigned)?;
                    // The parameter is the one node in both places, so the query's is settled too.
                    for (k, n) in copied {
                        sub.targets[k].expr = Node::Param {
                            n,
                            ty: crate::deparse::params::type_of(n),
                        };
                    }
                    self.scope.add(Rte {
                        kind: RteKind::Subquery(Box::new(sub)),
                        alias: None,
                        ename: "*SELECT*".into(),
                        ecols: Vec::new(),
                        in_from: false,
                    });
                }
            }
        }
        if let Some(on) = &i.on {
            let A::OnInsert::OnConflict(c) = on else {
                return Err(not_read(on));
            };
            if let A::OnConflictAction::DoUpdate(d) = &c.action {
                // transformOnConflictClause: the table and EXCLUDED, its proposed row.
                let excluded = Ns {
                    refname: "excluded".into(),
                    schema: None,
                    rel_visible: true,
                    cols_visible: false,
                    ..w.ns.clone()
                };
                *self.scope.names.borrow_mut() = vec![w.ns.clone(), excluded];
                assigned.extend(self.set_list(&d.assignments, &w)?);
                if let Some(e) = &d.selection {
                    let a = self.analyzer();
                    assigned.push(a.boolean(a.expr(e)?)?);
                }
            }
        }
        *self.scope.names.borrow_mut() = vec![w.ns.clone()];
        let targets = self.returning(i.returning.as_deref(), &w)?;
        Ok(Query {
            rtable: self.scope.rtable.borrow().clone(),
            targets,
            assigned,
            ..Query::default()
        })
    }

    /// transformUpdateStmt: its FROM, its WHERE, its RETURNING, then what it sets.
    fn update(&self, u: &A::Update) -> Result<Query> {
        if !u.order_by.is_empty() || u.limit.is_some() || u.or.is_some() || u.output.is_some() || !u.table.joins.is_empty() {
            return Err(not_read(u));
        }
        let w = self.target(&u.table.relation, false)?;
        // A FROM item reads no column of the table written.
        let mut from = Vec::new();
        let mut names = vec![w.ns.clone()];
        if let Some(A::UpdateTableFromKind::BeforeSet(f) | A::UpdateTableFromKind::AfterSet(f)) = &u.from {
            for twj in f {
                let (jt, _, ns) = self.table_with_joins(twj)?;
                from.push(jt);
                names.extend(ns);
            }
        }
        *self.scope.names.borrow_mut() = names;
        let a = self.analyzer();
        let quals = u.selection.as_ref().map(|e| a.boolean(a.expr(e)?)).transpose()?;
        let targets = self.returning(u.returning.as_deref(), &w)?;
        let assigned = self.set_list(&u.assignments, &w)?;
        Ok(Query {
            rtable: self.scope.rtable.borrow().clone(),
            from,
            quals,
            targets,
            assigned,
            ..Query::default()
        })
    }

    /// transformDeleteStmt: its USING, its WHERE and its RETURNING.
    fn delete(&self, d: &A::Delete) -> Result<Query> {
        if !d.tables.is_empty() || !d.order_by.is_empty() || d.limit.is_some() || d.output.is_some() {
            return Err(not_read(d));
        }
        let (A::FromTable::WithFromKeyword(f) | A::FromTable::WithoutKeyword(f)) = &d.from;
        let [t] = f.as_slice() else {
            return Err(not_read(d));
        };
        if !t.joins.is_empty() {
            return Err(not_read(d));
        }
        let w = self.target(&t.relation, false)?;
        let mut from = Vec::new();
        let mut names = vec![w.ns.clone()];
        for twj in d.using.iter().flatten() {
            let (jt, _, ns) = self.table_with_joins(twj)?;
            from.push(jt);
            names.extend(ns);
        }
        *self.scope.names.borrow_mut() = names;
        let a = self.analyzer();
        let quals = d.selection.as_ref().map(|e| a.boolean(a.expr(e)?)).transpose()?;
        let targets = self.returning(d.returning.as_deref(), &w)?;
        Ok(Query {
            rtable: self.scope.rtable.borrow().clone(),
            from,
            quals,
            targets,
            ..Query::default()
        })
    }

    /// transformUpdateTargetList: each value set coerced to its column; a list of columns set
    /// from a row's values, each read before any is written.
    fn set_list(&self, sets: &[A::Assignment], w: &Written) -> Result<Vec<Node>> {
        let a = self.analyzer();
        let mut out = Vec::new();
        for s in sets {
            match &s.target {
                A::AssignmentTarget::ColumnName(c) => {
                    let col = w.column(&column_name(c)?)?;
                    if is_default(&s.value) {
                        continue;
                    }
                    let n = a.expr(&s.value)?;
                    out.push(self.assigned(n, &w.cols[col])?);
                }
                A::AssignmentTarget::Tuple(cols) => {
                    let mut inner = &s.value;
                    while let A::Expr::Nested(x) = inner {
                        inner = x;
                    }
                    let values: &[A::Expr] = match inner {
                        A::Expr::Tuple(v) => v,
                        A::Expr::Function(_) | A::Expr::Subquery(_) => return Err(not_read(&s.value)),
                        _ => return Err(not_read(&s.value)),
                    };
                    let cols = cols
                        .iter()
                        .map(|c| w.column(&column_name(c)?))
                        .collect::<Result<Vec<_>>>()?;
                    if values.len() != cols.len() {
                        return Err(Error::new("number of columns does not match number of values"));
                    }
                    let row = values
                        .iter()
                        .map(|e| match is_default(e) {
                            true => Ok(None),
                            false => a.expr(e).map(Some),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    for (n, c) in row.into_iter().zip(cols) {
                        if let Some(n) = n {
                            out.push(self.assigned(n, &w.cols[c])?);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// transformReturningClause: read as a select list over the namespace the write left, the
    /// rows before and after named `old` and `new`, each output still unknown read as text.
    fn returning(&self, items: Option<&[A::SelectItem]>, w: &Written) -> Result<Vec<Target>> {
        let Some(items) = items else {
            return Ok(Vec::new());
        };
        {
            let mut names = self.scope.names.borrow_mut();
            for side in ["old", "new"] {
                if w.ns.refname != side {
                    names.push(Ns {
                        refname: side.into(),
                        schema: None,
                        rel_visible: true,
                        cols_visible: false,
                        ..w.ns.clone()
                    });
                }
            }
        }
        let mut targets = self.targets(items)?;
        for t in targets.iter_mut() {
            self.as_text(t)?;
        }
        Ok(targets)
    }
}
