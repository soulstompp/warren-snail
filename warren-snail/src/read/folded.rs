//! The literals PostgreSQL reads once, where their DDL is declared: a quoted literal of a
//! timestamptz, a timetz or a range or array of them, in a CHECK, a DEFAULT, a generated column, a
//! partial index's predicate, a domain or a view, is folded to its instant under the zone the
//! statement ran in. A snail keeps its DDL as text and reads it again under each statement's zone,
//! so such a literal naming no zone of its own is read alike only under the zone it was declared in,
//! UTC for every DDL a file was written with. Until a snail folds them, a session holding one keeps
//! to UTC, and a statement making one is taken only there.

use crate::catalog::{Catalog, Column, Table};
use sqlparser::ast::{Expr, Value};
use std::ops::ControlFlow;

/// Whether a type, as PostgreSQL names it, reads its text on the session's zone.
fn zone_typed(name: &str) -> bool {
    let l = name.to_lowercase();
    l.contains("with time zone") || l.contains("timestamptz") || l.contains("timetz") || l.contains("tstz")
}

/// Whether a column's type, or its domain's base, reads its text on the session's zone.
fn zone_column(catalog: &Catalog, c: &Column) -> bool {
    zone_typed(&c.ty.pg_name())
        || c.domain.as_ref().is_some_and(|d| {
            catalog
                .domains
                .iter()
                .any(|x| x.name.eq_ignore_ascii_case(d) && zone_typed(&x.base))
        })
}

/// The functions whose value is of a type read on the session's zone.
const ZONED_CALLS: [&str; 9] = [
    "now",
    "current_timestamp",
    "transaction_timestamp",
    "statement_timestamp",
    "clock_timestamp",
    "current_time",
    "to_timestamp",
    "make_timestamptz",
    "timezone",
];

/// Whether an expression both names something of a type read on the session's zone (a column of
/// `columns` by name, a cast or a typed literal of one, a call returning one) and holds a quoted
/// literal that reads as an instant or a time naming no zone of its own.
fn unfolded_expr(e: &Expr, columns: &dyn Fn(&str) -> bool, of_zone: bool) -> bool {
    let (mut zoned, mut literal) = (of_zone, false);
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        match x {
            Expr::Identifier(i) if columns(&i.value) => zoned = true,
            Expr::CompoundIdentifier(p) if p.last().is_some_and(|i| columns(&i.value)) => {
                zoned = true
            }
            Expr::Cast { data_type, .. } if zone_typed(&data_type.to_string()) => zoned = true,
            Expr::TypedString(t) => {
                if zone_typed(&t.data_type.to_string()) {
                    zoned = true;
                }
                if let Value::SingleQuotedString(s) = &t.value.value {
                    literal |= crate::times::reads_on_the_zone(s);
                }
            }
            Expr::Function(f)
                if ZONED_CALLS.contains(
                    &f.name.to_string().to_lowercase().rsplit('.').next().unwrap_or_default(),
                ) =>
            {
                zoned = true
            }
            Expr::Value(v) => {
                if let Value::SingleQuotedString(s) | Value::EscapedStringLiteral(s) = &v.value {
                    literal |= crate::times::reads_on_the_zone(s);
                }
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
    zoned && literal
}

/// A table's expressions read on its rows: its CHECKs, DEFAULTs, generated columns, its columns'
/// domains' and its partial indexes' predicates, each named.
fn table_unfolded(catalog: &Catalog, t: &Table) -> Option<String> {
    let columns = |n: &str| {
        t.columns
            .iter()
            .any(|c| (c.name == n || c.name.eq_ignore_ascii_case(n)) && zone_column(catalog, c))
    };
    for c in &t.checks {
        if unfolded_expr(&c.expr, &columns, false) {
            return Some(format!("{}'s CHECK {}", t.name, c.name));
        }
    }
    for c in &t.columns {
        // A default takes its column's type; a generated column's expression is read as it is.
        let of_zone = zone_column(catalog, c);
        if c.default.as_ref().is_some_and(|e| unfolded_expr(e, &columns, of_zone))
            || c.generated.as_ref().is_some_and(|g| unfolded_expr(&g.expr, &columns, false))
        {
            return Some(format!("{}.{}'s default", t.name, c.name));
        }
    }
    for o in &t.orders {
        if let Some(p) = &o.predicate {
            if crate::deparse::parse_expr(p).is_ok_and(|e| unfolded_expr(&e, &columns, false)) {
                return Some(format!("{}'s index {}", t.name, o.name));
            }
        }
    }
    None
}

/// The first object of the catalog holding a literal PostgreSQL folds where it is declared that
/// names no zone of its own, about a type read on the session's zone; none where none does.
pub(crate) fn unfolded(catalog: &Catalog) -> Option<String> {
    for t in &catalog.tables {
        if let Some(what) = table_unfolded(catalog, t) {
            return Some(what);
        }
    }
    for d in &catalog.domains {
        let value = |n: &str| zone_typed(&d.base) && n.eq_ignore_ascii_case("value");
        let checks = d.checks.iter().any(|c| unfolded_expr(&c.check, &value, false));
        let default = d
            .default
            .as_ref()
            .is_some_and(|e| unfolded_expr(e, &value, zone_typed(&d.base)));
        if checks || default {
            return Some(format!("domain {}", d.name));
        }
    }
    // A view's names are read against every table's columns: any of a type read on the zone.
    let any_column = |n: &str| {
        catalog.tables.iter().any(|t| {
            t.columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(n) && zone_column(catalog, c))
        })
    };
    for v in &catalog.views {
        let parsed = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::PostgreSqlDialect {},
            &v.query,
        );
        let Ok(statements) = parsed else { continue };
        let mut found = false;
        let _ = sqlparser::ast::visit_expressions(&statements, |e| {
            found |= unfolded_expr(e, &any_column, false);
            match found {
                true => ControlFlow::Break(()),
                false => ControlFlow::Continue(()),
            }
        });
        if found {
            return Some(format!("view {}", v.name));
        }
    }
    None
}

/// The refusal of a statement that would read such a literal under a zone other than the one it was
/// declared in.
pub(crate) fn refusal(what: &str, zone: &str) -> crate::Error {
    crate::Error::new(format!(
        "{what} holds a literal PostgreSQL reads once, under the zone its DDL ran in, and a snail \
         keeps its DDL as text, read again under each statement's: under TimeZone {zone} it would \
         be read otherwise, so the snail keeps to UTC for it; write the literal's offset in it, as \
         '2026-01-01 00:00+00'"
    ))
}
