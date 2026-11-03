//! What sqlparser's PostgreSQL dialect takes of a statement past what PostgreSQL's grammar takes, or
//! reads into a field of a query no path of the snail reads: each refused by name, in PostgreSQL's
//! words where its grammar refuses it. A string where a label stands is read as PostgreSQL's grammar
//! reads it: after a name, the name a type's and the string its literal.

use crate::{Error, Result};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArguments, GroupByExpr,
    Ident, JoinConstraint, JoinOperator, LimitClause, NamedWindowExpr, ObjectName, OrderBy,
    OrderByExpr, OrderByKind, Query, Select, SelectFlavor, SelectItem, SetExpr, Statement,
    TableAlias, TableFactor, TableWithJoins, WindowFrameBound, WindowSpec, WindowType,
};
use sqlparser::tokenizer::Location;

/// Each statement's queries, wherever they stand, as PostgreSQL's grammar reads them.
pub(crate) fn checked(sql: &str, statements: &mut [Statement]) -> Result<()> {
    let walk = Walk { sql };
    for s in statements {
        walk.statement(s)?;
    }
    Ok(())
}

fn near(what: impl std::fmt::Display) -> Error {
    Error::new(format!("syntax error at or near \"{what}\""))
}

fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

struct Walk<'s> {
    sql: &'s str,
}

impl Walk<'_> {
    /// The statement's text from a place of it on.
    fn after(&self, from: Location) -> &str {
        let mut offset = 0;
        for (n, line) in self.sql.split_inclusive('\n').enumerate() {
            if n + 1 == from.line as usize {
                let at = line
                    .char_indices()
                    .nth((from.column as usize).saturating_sub(1))
                    .map_or(line.len(), |(i, _)| i);
                return &self.sql[offset + at..];
            }
            offset += line.len();
        }
        ""
    }

    fn statement(&self, s: &mut Statement) -> Result<()> {
        match s {
            Statement::Query(q) => {
                if let SetExpr::Select(s) = q.body.as_ref() {
                    if let Some(into) = &s.into {
                        return Err(made_into(into));
                    }
                }
                self.query(q)
            }
            Statement::Insert(i) => match &mut i.source {
                Some(q) => self.query(q),
                None => Ok(()),
            },
            Statement::Update(u) => {
                if !u.order_by.is_empty() {
                    return Err(near("ORDER"));
                }
                if u.limit.is_some() {
                    return Err(near("LIMIT"));
                }
                if u.or.is_some() {
                    return Err(near("OR"));
                }
                if u.output.is_some() {
                    return Err(near("OUTPUT"));
                }
                self.joined(&mut u.table)?;
                for a in &mut u.assignments {
                    self.expr(&mut a.value)?;
                }
                if let Some(e) = &mut u.selection {
                    self.expr(e)?;
                }
                Ok(())
            }
            Statement::Delete(d) => {
                if !d.order_by.is_empty() {
                    return Err(near("ORDER"));
                }
                if d.limit.is_some() {
                    return Err(near("LIMIT"));
                }
                if d.output.is_some() {
                    return Err(near("OUTPUT"));
                }
                if let Some(t) = d.tables.first() {
                    return Err(near(t));
                }
                for t in d.using.iter_mut().flatten() {
                    self.joined(t)?;
                }
                if let Some(e) = &mut d.selection {
                    self.expr(e)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn query(&self, q: &mut Query) -> Result<()> {
        if let Some(w) = &mut q.with {
            for c in &mut w.cte_tables {
                if let Some(f) = &c.from {
                    return Err(near(f));
                }
                self.alias(&c.alias)?;
                self.query(&mut c.query)?;
            }
        }
        self.body(&mut q.body)?;
        if let Some(o) = &mut q.order_by {
            self.order_by(o)?;
        }
        match &mut q.limit_clause {
            Some(LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            }) => {
                if !limit_by.is_empty() {
                    return Err(near("BY"));
                }
                if let Some(l) = limit {
                    self.expr(l)?;
                }
                if let Some(o) = offset {
                    self.expr(&mut o.value)?;
                }
            }
            Some(LimitClause::OffsetCommaLimit { .. }) => return Err(near(",")),
            None => {}
        }
        if let Some(f) = &mut q.fetch {
            if f.percent {
                return Err(near("PERCENT"));
            }
            if let Some(e) = &mut f.quantity {
                self.expr(e)?;
            }
        }
        if let Some(f) = &q.for_clause {
            let what = f.to_string();
            return Err(near(what.split_whitespace().nth(1).unwrap_or("FOR")));
        }
        if q.settings.is_some() {
            return Err(near("SETTINGS"));
        }
        if q.format_clause.is_some() {
            return Err(near("FORMAT"));
        }
        if !q.pipe_operators.is_empty() {
            return Err(near("|>"));
        }
        Ok(())
    }

    fn body(&self, b: &mut SetExpr) -> Result<()> {
        match b {
            SetExpr::Select(s) => self.select(s),
            SetExpr::Query(q) => self.query(q),
            SetExpr::SetOperation { left, right, .. } => {
                self.body(left)?;
                self.body(right)
            }
            SetExpr::Values(v) => {
                if v.value_keyword {
                    return Err(near("VALUE"));
                }
                if v.explicit_row {
                    return Err(Error::new(
                        "VALUES ROW(…) is not read: PostgreSQL reads ROW(…) as one value of a \
                         record; write each row in parentheses",
                    ));
                }
                for row in &mut v.rows {
                    for e in row.iter_mut() {
                        self.expr(e)?;
                    }
                }
                Ok(())
            }
            SetExpr::Table(_) => Err(Error::new("TABLE … is not read: write SELECT * FROM …")),
            SetExpr::Insert(s) | SetExpr::Update(s) | SetExpr::Delete(s) | SetExpr::Merge(s) => {
                self.statement(s)
            }
        }
    }

    fn select(&self, s: &mut Select) -> Result<()> {
        if let Some(t) = &s.top {
            return Err(match &t.quantity {
                Some(sqlparser::ast::TopQuantity::Constant(n)) => near(n),
                Some(sqlparser::ast::TopQuantity::Expr(_)) => near("("),
                None => near("TOP"),
            });
        }
        if s.select_modifiers.is_some() {
            return Err(Error::new(format!("{s}: its modifiers are not PostgreSQL's")));
        }
        if s.exclude.is_some() {
            return Err(near("EXCLUDE"));
        }
        if s.into.is_some() {
            return Err(Error::new("SELECT ... INTO is not allowed here"));
        }
        if let Some(v) = s.lateral_views.first() {
            let _ = v;
            return Err(near("LATERAL"));
        }
        if s.prewhere.is_some() {
            return Err(near("PREWHERE"));
        }
        if !s.connect_by.is_empty() {
            return Err(near("BY"));
        }
        if !s.cluster_by.is_empty() || !s.distribute_by.is_empty() || !s.sort_by.is_empty() {
            return Err(near("BY"));
        }
        if let Some(q) = &s.qualify {
            return Err(Error::new(format!(
                "QUALIFY {q} is not read: PostgreSQL has no QUALIFY; filter the windowed rows in \
                 an outer query"
            )));
        }
        if s.value_table_mode.is_some() {
            return Err(near("AS"));
        }
        if s.flavor != SelectFlavor::Standard {
            return Err(near("FROM"));
        }
        for item in &mut s.projection {
            self.item(item)?;
        }
        for t in &mut s.from {
            self.joined(t)?;
        }
        if let Some(e) = &mut s.selection {
            self.expr(e)?;
        }
        match &mut s.group_by {
            GroupByExpr::Expressions(es, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(near("WITH"));
                }
                for e in es {
                    self.expr(e)?;
                }
            }
            GroupByExpr::All(_) => {}
        }
        if let Some(e) = &mut s.having {
            self.expr(e)?;
        }
        for w in &mut s.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = &mut w.1 {
                self.window(spec)?;
            }
        }
        Ok(())
    }

    fn item(&self, item: &mut SelectItem) -> Result<()> {
        match item {
            SelectItem::UnnamedExpr(e) => self.expr(e),
            SelectItem::ExprWithAlias { expr, alias } if alias.quote_style == Some('\'') => {
                // The string's own place is not kept: the words between the name and the next quote.
                let written_as = self
                    .after(expr_end(expr))
                    .split('\'')
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .any(|w| w.eq_ignore_ascii_case("as"));
                let name = match expr {
                    Expr::Identifier(i) if !written_as => vec![i.clone()],
                    Expr::CompoundIdentifier(is) if !written_as => is.clone(),
                    _ => return Err(near(quoted(&alias.value))),
                };
                // A name and a string: the string a literal of the type the name names.
                let text = format!(
                    "{}::{}",
                    quoted(&alias.value),
                    ObjectName::from(name)
                );
                let mut cast = super::image::parse_expr(&text).map_err(|e| Error::new(e.to_string()))?;
                self.expr(&mut cast)?;
                *item = SelectItem::UnnamedExpr(cast);
                Ok(())
            }
            SelectItem::ExprWithAlias { expr, .. } => self.expr(expr),
            SelectItem::ExprWithAliases { aliases, .. } => Err(near(
                aliases.get(1).map(|a| a.to_string()).unwrap_or_else(|| "(".into()),
            )),
            SelectItem::QualifiedWildcard(_, o) | SelectItem::Wildcard(o) => {
                if o.opt_ilike.is_some() {
                    return Err(near("ILIKE"));
                }
                if o.opt_exclude.is_some() {
                    return Err(near("EXCLUDE"));
                }
                if o.opt_except.is_some() {
                    return Err(near("EXCEPT"));
                }
                if o.opt_replace.is_some() {
                    return Err(near("REPLACE"));
                }
                if o.opt_rename.is_some() {
                    return Err(near("RENAME"));
                }
                Ok(())
            }
        }
    }

    fn alias(&self, a: &TableAlias) -> Result<()> {
        if a.name.quote_style == Some('\'') {
            return Err(near(quoted(&a.name.value)));
        }
        if let Some(at) = &a.at {
            return Err(near(at));
        }
        Ok(())
    }

    fn joined(&self, t: &mut TableWithJoins) -> Result<()> {
        // PostgreSQL's grammar reads SEMI or ANTI before JOIN as the alias of the relation before it.
        for i in 0..t.joins.len() {
            let word = match &t.joins[i].join_operator {
                JoinOperator::Semi(_) => "semi",
                JoinOperator::Anti(_) => "anti",
                _ => continue,
            };
            let before = match i {
                0 => &mut t.relation,
                _ => &mut t.joins[i - 1].relation,
            };
            match alias_of(before) {
                Some(a @ None) => {
                    *a = Some(TableAlias {
                        explicit: false,
                        name: Ident::new(word),
                        columns: Vec::new(),
                        at: None,
                    })
                }
                _ => return Err(near(word.to_uppercase())),
            }
            let op = std::mem::replace(&mut t.joins[i].join_operator, JoinOperator::CrossApply);
            t.joins[i].join_operator = match op {
                JoinOperator::Semi(c) | JoinOperator::Anti(c) => JoinOperator::Join(c),
                other => other,
            };
        }
        self.factor(&mut t.relation)?;
        for j in &mut t.joins {
            if j.global {
                return Err(near("GLOBAL"));
            }
            self.factor(&mut j.relation)?;
            let c = match &mut j.join_operator {
                JoinOperator::Join(c)
                | JoinOperator::Inner(c)
                | JoinOperator::Left(c)
                | JoinOperator::LeftOuter(c)
                | JoinOperator::Right(c)
                | JoinOperator::RightOuter(c)
                | JoinOperator::FullOuter(c)
                | JoinOperator::CrossJoin(c) => c,
                JoinOperator::Semi(_) => return Err(near("SEMI")),
                JoinOperator::Anti(_) => return Err(near("ANTI")),
                JoinOperator::LeftSemi(_)
                | JoinOperator::RightSemi(_) => return Err(near("SEMI")),
                JoinOperator::LeftAnti(_)
                | JoinOperator::RightAnti(_) => return Err(near("ANTI")),
                JoinOperator::CrossApply => return Err(near("APPLY")),
                JoinOperator::OuterApply => return Err(near("OUTER")),
                JoinOperator::AsOf { .. } => return Err(near("MATCH_CONDITION")),
                JoinOperator::StraightJoin(_) => return Err(near("STRAIGHT_JOIN")),
                JoinOperator::ArrayJoin
                | JoinOperator::LeftArrayJoin
                | JoinOperator::InnerArrayJoin => return Err(near("ARRAY")),
            };
            if let JoinConstraint::On(e) = c {
                self.expr(e)?;
            }
        }
        Ok(())
    }

    fn factor(&self, t: &mut TableFactor) -> Result<()> {
        let alias = match t {
            TableFactor::Table {
                alias,
                args,
                with_hints,
                version,
                partitions,
                json_path,
                index_hints,
                ..
            } => {
                if !with_hints.is_empty() {
                    return Err(near("WITH"));
                }
                if version.is_some() {
                    return Err(near("FOR"));
                }
                if !partitions.is_empty() {
                    return Err(near("PARTITION"));
                }
                if json_path.is_some() {
                    return Err(near("["));
                }
                if !index_hints.is_empty() {
                    return Err(near("USE"));
                }
                if let Some(a) = args {
                    if a.settings.is_some() {
                        return Err(near("SETTINGS"));
                    }
                    for x in &mut a.args {
                        self.arg(x)?;
                    }
                }
                alias
            }
            TableFactor::Derived {
                subquery,
                alias,
                sample,
                ..
            } => {
                if sample.is_some() {
                    return Err(near("TABLESAMPLE"));
                }
                self.query(subquery)?;
                alias
            }
            TableFactor::Function { args, alias, .. } => {
                for x in args {
                    self.arg(x)?;
                }
                alias
            }
            TableFactor::UNNEST {
                alias,
                array_exprs,
                with_offset,
                with_offset_alias,
                ..
            } => {
                if *with_offset || with_offset_alias.is_some() {
                    return Err(near("OFFSET"));
                }
                for e in array_exprs {
                    self.expr(e)?;
                }
                alias
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                self.joined(table_with_joins)?;
                alias
            }
            TableFactor::JsonTable { .. } => {
                return Err(Error::new("JSON_TABLE in FROM is not read"))
            }
            TableFactor::XmlTable { .. } => {
                return Err(Error::new("XMLTABLE in FROM is not read"))
            }
            TableFactor::TableFunction { .. } => return Err(near("TABLE")),
            TableFactor::OpenJsonTable { .. } => return Err(near("OPENJSON")),
            TableFactor::Pivot { .. } => return Err(near("PIVOT")),
            TableFactor::Unpivot { .. } | TableFactor::UnpivotExpr { .. } => {
                return Err(near("UNPIVOT"))
            }
            TableFactor::MatchRecognize { .. } => return Err(near("MATCH_RECOGNIZE")),
            TableFactor::SemanticView { .. } => return Err(near("SEMANTIC_VIEW")),
        };
        match alias {
            Some(a) => self.alias(a),
            None => Ok(()),
        }
    }

    fn order_by(&self, o: &mut OrderBy) -> Result<()> {
        if o.interpolate.is_some() {
            return Err(near("INTERPOLATE"));
        }
        match &mut o.kind {
            OrderByKind::All(_) => Err(near("ALL")),
            OrderByKind::Expressions(es) => {
                for e in es {
                    self.ordered(e)?;
                }
                Ok(())
            }
        }
    }

    fn ordered(&self, o: &mut OrderByExpr) -> Result<()> {
        if o.with_fill.is_some() {
            return Err(near("WITH"));
        }
        self.expr(&mut o.expr)
    }

    fn window(&self, w: &mut WindowSpec) -> Result<()> {
        for e in &mut w.partition_by {
            self.expr(e)?;
        }
        for o in &mut w.order_by {
            self.ordered(o)?;
        }
        if let Some(f) = &mut w.window_frame {
            for b in std::iter::once(&mut f.start_bound).chain(f.end_bound.as_mut()) {
                if let WindowFrameBound::Preceding(Some(e)) | WindowFrameBound::Following(Some(e)) = b
                {
                    self.expr(e)?;
                }
            }
        }
        Ok(())
    }

    fn arg(&self, a: &mut FunctionArg) -> Result<()> {
        match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
            | FunctionArg::Named {
                arg: FunctionArgExpr::Expr(e),
                ..
            }
            | FunctionArg::ExprNamed {
                arg: FunctionArgExpr::Expr(e),
                ..
            } => self.expr(e),
            _ => Ok(()),
        }
    }

    /// The queries an expression holds, each read as a query is.
    fn expr(&self, e: &mut Expr) -> Result<()> {
        match e {
            Expr::Subquery(q) => self.query(q),
            Expr::Exists { subquery, .. } => self.query(subquery),
            Expr::InSubquery { expr, subquery, .. } => {
                self.expr(expr)?;
                self.query(subquery)
            }
            Expr::Function(f) => {
                match &mut f.args {
                    FunctionArguments::Subquery(q) => self.query(q)?,
                    FunctionArguments::List(l) => {
                        for a in &mut l.args {
                            self.arg(a)?;
                        }
                        for c in &mut l.clauses {
                            match c {
                                FunctionArgumentClause::OrderBy(os) => {
                                    for o in os {
                                        self.ordered(o)?;
                                    }
                                }
                                FunctionArgumentClause::Limit(x) => self.expr(x)?,
                                _ => {}
                            }
                        }
                    }
                    FunctionArguments::None => {}
                }
                for o in &mut f.within_group {
                    self.ordered(o)?;
                }
                if let Some(x) = &mut f.filter {
                    self.expr(x)?;
                }
                if let Some(WindowType::WindowSpec(w)) = &mut f.over {
                    self.window(w)?;
                }
                Ok(())
            }
            Expr::BinaryOp { left, right, .. }
            | Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right)
            | Expr::AnyOp { left, right, .. }
            | Expr::AllOp { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. } => {
                self.expr(expr)?;
                self.expr(pattern)
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::Cast { expr, .. }
            | Expr::Collate { expr, .. }
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsTrue(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsFalse(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsUnknown(expr)
            | Expr::IsNotUnknown(expr)
            | Expr::Extract { expr, .. }
            | Expr::Ceil { expr, .. }
            | Expr::Floor { expr, .. } => self.expr(expr),
            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                self.expr(timestamp)?;
                self.expr(time_zone)
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.expr(expr)?;
                self.expr(low)?;
                self.expr(high)
            }
            Expr::InList { expr, list, .. } => {
                self.expr(expr)?;
                list.iter_mut().try_for_each(|x| self.expr(x))
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(o) = operand {
                    self.expr(o)?;
                }
                for c in conditions {
                    self.expr(&mut c.condition)?;
                    self.expr(&mut c.result)?;
                }
                match else_result {
                    Some(x) => self.expr(x),
                    None => Ok(()),
                }
            }
            Expr::Tuple(xs) => xs.iter_mut().try_for_each(|x| self.expr(x)),
            Expr::Array(a) => a.elem.iter_mut().try_for_each(|x| self.expr(x)),
            Expr::Position { expr, r#in } => {
                self.expr(expr)?;
                self.expr(r#in)
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                self.expr(expr)?;
                for x in [substring_from, substring_for].into_iter().flatten() {
                    self.expr(x)?;
                }
                Ok(())
            }
            Expr::Trim {
                expr, trim_what, ..
            } => {
                self.expr(expr)?;
                match trim_what {
                    Some(x) => self.expr(x),
                    None => Ok(()),
                }
            }
            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                self.expr(expr)?;
                self.expr(overlay_what)?;
                self.expr(overlay_from)?;
                match overlay_for {
                    Some(x) => self.expr(x),
                    None => Ok(()),
                }
            }
            Expr::CompoundFieldAccess { root, .. } => self.expr(root),
            Expr::GroupingSets(sets) | Expr::Cube(sets) | Expr::Rollup(sets) => sets
                .iter_mut()
                .flatten()
                .try_for_each(|x| self.expr(x)),
            _ => Ok(()),
        }
    }
}

/// A statement's SELECT … INTO, which makes a table PostgreSQL's way: one the snail cannot hold.
fn made_into(into: &sqlparser::ast::SelectInto) -> Error {
    let made = match (into.temporary, into.unlogged) {
        (true, _) => "a temporary table, whose life is a session's, which no statement of the snail has",
        (_, true) => "an unlogged table, where every commit of the snail is durable",
        _ => "a table with no key, where a table the snail holds lies in the order its key gives",
    };
    Error::new(format!(
        "SELECT … INTO is not read: it makes {made}; declare the table with its primary key, then \
         INSERT INTO it … SELECT"
    ))
}

/// A FROM item's alias, where it takes one.
fn alias_of(t: &mut TableFactor) -> Option<&mut Option<TableAlias>> {
    match t {
        TableFactor::Table { alias, .. }
        | TableFactor::Derived { alias, .. }
        | TableFactor::Function { alias, .. }
        | TableFactor::UNNEST { alias, .. }
        | TableFactor::NestedJoin { alias, .. } => Some(alias),
        _ => None,
    }
}

/// Where an expression a label follows ends: a name's last part, or nothing known.
fn expr_end(e: &Expr) -> Location {
    let ident_end = |i: &Ident| i.span.end;
    match e {
        Expr::Identifier(i) => ident_end(i),
        Expr::CompoundIdentifier(is) => is.last().map(ident_end).unwrap_or(Location { line: 0, column: 0 }),
        _ => Location { line: 0, column: 0 },
    }
}
