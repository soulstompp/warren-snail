//! A statement that writes, in PostgreSQL's SQL, as the snail reads it: rows inserted from values or
//! from a question, rows a question finds updated or deleted, and the statements that begin and end
//! a transaction.

use super::question::{instant, number, text_literal};
use crate::catalog::{ident, object_name, Catalog, ColumnType, Table};
use crate::{Error, Result};
use sqlparser::ast::{
    AssignmentTarget, Expr, FromTable, Insert, ObjectName, ObjectNamePart, Query, SelectItem,
    SetExpr, Statement as Sql, TableFactor, TableObject, TableWithJoins, UnaryOperator, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// One value of a row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Datum {
    Null,
    /// An integer, an instant's microseconds, or a numeric's smallest units.
    Int(i64),
    Text(String),
    Bytes(Vec<u8>),
}

/// A row of a table, its values in the table's column order.
pub type Row = Vec<Datum>;

/// Where an inserted row's value comes from: a literal, or a column of the question's answer.
#[derive(Clone, Debug)]
pub enum Fill {
    Literal(Datum),
    Answer(usize),
}

#[derive(Clone, Debug)]
pub enum Source {
    Values(Vec<Row>),
    /// A question, and for each of the table's columns where its value comes from.
    Select {
        question: String,
        fill: Vec<Fill>,
    },
}

#[derive(Clone, Debug)]
pub enum Statement {
    Begin,
    Commit,
    Rollback,
    Insert {
        table: usize,
        source: Source,
    },
    /// The question that finds the rows, every column of the table in its order, and the values set.
    Update {
        table: usize,
        find: String,
        sets: Vec<(usize, Datum)>,
    },
    Delete {
        table: usize,
        find: String,
    },
    /// A table, an index or a table's order declared: its DDL, and what it declares.
    Declare {
        ddl: String,
        kind: &'static str,
    },
}

impl Statement {
    pub fn kind(&self) -> &'static str {
        match self {
            Statement::Begin => "BEGIN",
            Statement::Commit => "COMMIT",
            Statement::Rollback => "ROLLBACK",
            Statement::Insert { .. } => "INSERT",
            Statement::Update { .. } => "UPDATE",
            Statement::Delete { .. } => "DELETE",
            Statement::Declare { kind, .. } => kind,
        }
    }
}

fn refused(what: &str) -> Error {
    Error::new(format!("{what} is not written"))
}

fn table_index(catalog: &Catalog, name: &ObjectName) -> Result<usize> {
    let name = object_name(name);
    catalog
        .tables
        .iter()
        .position(|t| t.name == name)
        .ok_or_else(|| Error::new(format!("{name} is not in the snail")))
}

fn column_index(table: &Table, name: &ObjectName) -> Result<usize> {
    let last = match name.0.last() {
        Some(ObjectNamePart::Identifier(i)) => ident(i),
        _ => return Err(Error::new(format!("{name} is not a column"))),
    };
    table
        .columns
        .iter()
        .position(|c| c.name == last)
        .ok_or_else(|| Error::new(format!("{} has no column {last}", table.name)))
}

/// An expression with its casts taken off.
fn uncast(e: &Expr) -> &Expr {
    match e {
        Expr::Cast { expr, .. } | Expr::Nested(expr) => uncast(expr),
        other => other,
    }
}

/// Whether an expression is a literal, a value written out.
fn is_literal(e: &Expr) -> bool {
    match uncast(e) {
        Expr::Value(_) => true,
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => is_literal(expr),
        _ => false,
    }
}

/// A literal as a value of column `c`.
pub fn datum(e: &Expr, table: &Table, c: usize) -> Result<Datum> {
    let e = uncast(e);
    if let Expr::Value(v) = e {
        if matches!(v.value, Value::Null) {
            return Ok(Datum::Null);
        }
    }
    let col = &table.columns[c];
    let wrong = || Error::new(format!("{e} is not a value of {}.{}", table.name, col.name));
    Ok(match col.ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => {
            let v = number(e).ok_or_else(wrong)?;
            let fits = match col.ty {
                ColumnType::Int2 => i16::try_from(v).is_ok(),
                ColumnType::Int4 => i32::try_from(v).is_ok(),
                _ => true,
            };
            if !fits {
                return Err(Error::new(format!(
                    "{v} is out of range for {}.{}",
                    table.name, col.name
                )));
            }
            Datum::Int(v)
        }
        ColumnType::Timestamptz => Datum::Int(instant(&text_literal(e).map_err(|_| wrong())?)?),
        ColumnType::Text(_) => Datum::Text(text_literal(e).map_err(|_| wrong())?),
        ColumnType::Bytes => Datum::Bytes(super::pipeline::hex_bytes(
            &text_literal(e).map_err(|_| wrong())?,
        )?),
        ColumnType::Numeric { precision, scale } => {
            let text = match e {
                Expr::Value(v) => match &v.value {
                    Value::Number(s, _) => s.clone(),
                    Value::SingleQuotedString(s) => s.clone(),
                    _ => return Err(wrong()),
                },
                Expr::UnaryOp {
                    op: UnaryOperator::Minus,
                    expr,
                } => match expr.as_ref() {
                    Expr::Value(v) => match &v.value {
                        Value::Number(s, _) => format!("-{s}"),
                        _ => return Err(wrong()),
                    },
                    _ => return Err(wrong()),
                },
                _ => return Err(wrong()),
            };
            Datum::Int(crate::catalog::numeric_units(&text, precision, scale)?)
        }
    })
}

/// The question that finds a table's rows, every column in the table's order.
fn finder(table: &Table, relation: &TableWithJoins, selection: Option<&Expr>) -> String {
    let columns = table
        .columns
        .iter()
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    match selection {
        Some(w) => format!("SELECT {columns} FROM {relation} WHERE {w}"),
        None => format!("SELECT {columns} FROM {relation}"),
    }
}

/// The one table a statement names, read alone.
fn one_table(catalog: &Catalog, t: &TableWithJoins) -> Result<usize> {
    if !t.joins.is_empty() {
        return Err(refused("a write that joins"));
    }
    match &t.relation {
        TableFactor::Table { name, .. } => table_index(catalog, name),
        _ => Err(Error::new("a write names a table")),
    }
}

fn insert(catalog: &Catalog, i: &Insert) -> Result<Statement> {
    if i.on.is_some() {
        return Err(Error::new(
            "ON CONFLICT is not written: a row's place is its key",
        ));
    }
    if i.returning.is_some() {
        return Err(refused("RETURNING"));
    }
    let ti = match &i.table {
        TableObject::TableName(n) => table_index(catalog, n)?,
        _ => return Err(Error::new("an INSERT names a table")),
    };
    let table = &catalog.tables[ti];
    let columns: Vec<usize> = if i.columns.is_empty() {
        (0..table.columns.len()).collect()
    } else {
        i.columns
            .iter()
            .map(|n| column_index(table, n))
            .collect::<Result<_>>()?
    };
    for (k, c) in columns.iter().enumerate() {
        if columns[..k].contains(c) {
            return Err(Error::new(format!(
                "{} is named twice",
                table.columns[*c].name
            )));
        }
    }
    let q: &Query = i
        .source
        .as_deref()
        .ok_or_else(|| Error::new("an INSERT gives its rows"))?;
    if q.with.is_some() {
        return Err(refused("a write inside WITH"));
    }
    let source = match q.body.as_ref() {
        SetExpr::Values(v) => {
            let mut rows = Vec::with_capacity(v.rows.len());
            for r in &v.rows {
                if r.content.len() != columns.len() {
                    return Err(Error::new(format!(
                        "a row of the INSERT gives {} values for {} columns",
                        r.content.len(),
                        columns.len()
                    )));
                }
                let mut row = vec![Datum::Null; table.columns.len()];
                for (&c, e) in columns.iter().zip(&r.content) {
                    if matches!(uncast(e), Expr::Identifier(i) if i.value.eq_ignore_ascii_case("default"))
                    {
                        return Err(Error::new(
                            "DEFAULT is not written: a row gives every value it holds",
                        ));
                    }
                    row[c] = datum(e, table, c)?;
                }
                rows.push(row);
            }
            Source::Values(rows)
        }
        SetExpr::Select(s) => {
            if s.projection.len() != columns.len() {
                return Err(Error::new(format!(
                    "the INSERT's question returns {} columns for {}",
                    s.projection.len(),
                    columns.len()
                )));
            }
            let mut fill = vec![Fill::Literal(Datum::Null); table.columns.len()];
            let mut kept = Vec::new();
            for (&c, item) in columns.iter().zip(&s.projection) {
                let e = match item {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                    _ => return Err(Error::new("name each column the INSERT's question returns")),
                };
                if is_literal(e) {
                    fill[c] = Fill::Literal(datum(e, table, c)?);
                } else {
                    fill[c] = Fill::Answer(kept.len());
                    kept.push(item.clone());
                }
            }
            if kept.is_empty() {
                return Err(Error::new(
                    "an INSERT's question returns a column of the table it reads",
                ));
            }
            let mut reduced = q.clone();
            if let SetExpr::Select(s) = reduced.body.as_mut() {
                s.projection = kept;
            }
            Source::Select {
                question: reduced.to_string(),
                fill,
            }
        }
        _ => return Err(Error::new("an INSERT's rows are VALUES or a SELECT")),
    };
    Ok(Statement::Insert { table: ti, source })
}

pub fn parse(catalog: &Catalog, sql: &str) -> Result<Statement> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)?;
    let s = match statements.as_slice() {
        [s] => s,
        _ => return Err(Error::new("one statement at a time")),
    };
    match s {
        Sql::StartTransaction {
            modes, statements, ..
        } if modes.is_empty() && statements.is_empty() => Ok(Statement::Begin),
        Sql::Commit { chain: false, .. } => Ok(Statement::Commit),
        Sql::Rollback {
            chain: false,
            savepoint: None,
        } => Ok(Statement::Rollback),
        Sql::Insert(i) => insert(catalog, i),
        Sql::Update(u) => {
            if u.from.is_some() {
                return Err(refused("UPDATE … FROM"));
            }
            if u.returning.is_some() {
                return Err(refused("RETURNING"));
            }
            if !u.order_by.is_empty() || u.limit.is_some() {
                return Err(refused("an UPDATE with ORDER BY or LIMIT"));
            }
            let ti = one_table(catalog, &u.table)?;
            let table = &catalog.tables[ti];
            let mut sets = Vec::new();
            for a in &u.assignments {
                let c = match &a.target {
                    AssignmentTarget::ColumnName(n) => column_index(table, n)?,
                    AssignmentTarget::Tuple(_) => return Err(refused("an UPDATE of a tuple")),
                };
                if !is_literal(&a.value) {
                    return Err(Error::new(format!(
                        "{}: an UPDATE sets a value written out",
                        a.value
                    )));
                }
                sets.push((c, datum(&a.value, table, c)?));
            }
            Ok(Statement::Update {
                table: ti,
                find: finder(table, &u.table, u.selection.as_ref()),
                sets,
            })
        }
        Sql::Delete(d) => {
            if d.using.is_some() {
                return Err(refused("DELETE … USING"));
            }
            if d.returning.is_some() {
                return Err(refused("RETURNING"));
            }
            if !d.tables.is_empty() || !d.order_by.is_empty() || d.limit.is_some() {
                return Err(refused(
                    "a DELETE of several tables, or with ORDER BY or LIMIT",
                ));
            }
            let from = match &d.from {
                FromTable::WithFromKeyword(f) | FromTable::WithoutKeyword(f) => f,
            };
            let t = match from.as_slice() {
                [t] => t,
                _ => return Err(Error::new("a DELETE names one table")),
            };
            let ti = one_table(catalog, t)?;
            Ok(Statement::Delete {
                table: ti,
                find: finder(&catalog.tables[ti], t, d.selection.as_ref()),
            })
        }
        Sql::Query(_) => Err(Error::new("a SELECT is a question: ask it")),
        Sql::Truncate { .. } => Err(refused("TRUNCATE")),
        Sql::CreateTable(_) => Ok(Statement::Declare {
            ddl: s.to_string(),
            kind: "CREATE TABLE",
        }),
        Sql::CreateIndex(_) => Ok(Statement::Declare {
            ddl: s.to_string(),
            kind: "CREATE INDEX",
        }),
        Sql::AlterTable(_) => Ok(Statement::Declare {
            ddl: s.to_string(),
            kind: "ALTER TABLE",
        }),
        other => Err(Error::new(format!(
            "{other} is not a statement the snail writes"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b text, at timestamptz, PRIMARY KEY (a));
             CREATE TABLE s.u (a integer NOT NULL, n smallint, PRIMARY KEY (a));",
        )
        .unwrap()
    }

    #[test]
    fn each_write_is_read_with_its_rows_or_the_question_that_finds_them() {
        let c = catalog();
        match parse(&c, "INSERT INTO s.t (b, a) VALUES ('x', 1), (NULL, -2)").unwrap() {
            Statement::Insert {
                table: 0,
                source: Source::Values(rows),
            } => assert_eq!(
                rows,
                [
                    vec![Datum::Int(1), Datum::Text("x".into()), Datum::Null],
                    vec![Datum::Int(-2), Datum::Null, Datum::Null],
                ]
            ),
            other => panic!("{other:?}"),
        }
        match parse(
            &c,
            "INSERT INTO s.t (a, b, at) SELECT u.a, 'y', '2020-01-02 03:04:05+00' FROM s.u u WHERE u.n = 3",
        )
        .unwrap()
        {
            Statement::Insert {
                source: Source::Select { question, fill },
                ..
            } => {
                assert_eq!(question, "SELECT u.a FROM s.u u WHERE u.n = 3");
                assert!(matches!(fill[0], Fill::Answer(0)));
                assert!(matches!(&fill[1], Fill::Literal(Datum::Text(s)) if s == "y"));
                assert!(matches!(fill[2], Fill::Literal(Datum::Int(1_577_934_245_000_000))));
            }
            other => panic!("{other:?}"),
        }
        match parse(&c, "UPDATE s.t SET b = 'z' WHERE a = 1").unwrap() {
            Statement::Update { table, find, sets } => {
                assert_eq!(table, 0);
                assert_eq!(find, "SELECT \"a\", \"b\", \"at\" FROM s.t WHERE a = 1");
                assert_eq!(sets, [(1, Datum::Text("z".into()))]);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&c, "DELETE FROM s.u").unwrap(),
            Statement::Delete { table: 1, .. }
        ));
        for (sql, kind) in [
            ("BEGIN", "BEGIN"),
            ("COMMIT", "COMMIT"),
            ("ROLLBACK", "ROLLBACK"),
        ] {
            assert_eq!(parse(&c, sql).unwrap().kind(), kind);
        }
    }

    #[test]
    fn what_is_not_written_is_refused_with_its_reason() {
        let c = catalog();
        for (sql, why) in [
            (
                "UPDATE s.t SET b = 'z' FROM s.u WHERE s.u.a = s.t.a",
                "UPDATE … FROM",
            ),
            (
                "DELETE FROM s.t USING s.u WHERE s.u.a = s.t.a",
                "DELETE … USING",
            ),
            ("INSERT INTO s.t (a) VALUES (1) RETURNING a", "RETURNING"),
            (
                "INSERT INTO s.t (a) VALUES (1) ON CONFLICT DO NOTHING",
                "a row's place is its key",
            ),
            ("INSERT INTO s.t (a) VALUES (DEFAULT)", "DEFAULT"),
            ("UPDATE s.t SET b = b WHERE a = 1", "a value written out"),
            ("INSERT INTO s.u (n, a) VALUES (70000, 1)", "out of range"),
            ("TRUNCATE s.t", "TRUNCATE"),
            ("SELECT a FROM s.t", "ask it"),
            ("BEGIN; COMMIT", "one statement at a time"),
        ] {
            let e = parse(&c, sql).unwrap_err().to_string();
            assert!(e.contains(why), "{sql}: {e}");
        }
    }
}
