//! TABLESAMPLE: a table's rows drawn by its counts before anything else reads them, SYSTEM a block at
//! a time and BERNOULLI a row at a time, each with the chance its argument gives in percent, by a hash
//! of its place and the clause's seed: REPEATABLE's, or one the statement drew as it was read. What
//! PostgreSQL refuses is refused in its words.

use super::spelled::SAMPLED;
use crate::{Error, Result};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectNamePart, TableSampleKind,
    TableSampleModifier, UnaryOperator, Value,
};

/// What a sample draws at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// A block of the table's order.
    System,
    /// A row.
    Bernoulli,
}

/// A TABLESAMPLE clause read: its method, the chance of each draw out of 2^32, and its seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub method: Method,
    cutoff: u64,
    seed: u64,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A seed for a statement's clause that names none.
pub(crate) fn drawn_seed() -> u32 {
    use std::io::Read;
    let mut b = [0u8; 4];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    u32::from_le_bytes(b)
}

impl Sample {
    /// Whether the block or row at `place` is drawn.
    fn draws(&self, place: u64) -> bool {
        mix(self.seed ^ mix(place)) >> 32 < self.cutoff
    }

    /// Of blocks holding `live` rows each, in the order a whole read reads them, the live rows drawn,
    /// each counted from the first block's first.
    pub(super) fn picks(&self, live: &[u64]) -> Vec<u64> {
        let mut out = Vec::new();
        let mut before = 0u64;
        for (block, &rows) in live.iter().enumerate() {
            match self.method {
                Method::System if self.draws(block as u64) => out.extend(before..before + rows),
                Method::System => {}
                Method::Bernoulli => out.extend((before..before + rows).filter(|&r| self.draws(r))),
            }
            before += rows;
        }
        out
    }
}

/// A constant argument's value: none where it is NULL.
fn constant(e: &Expr, ty: &str) -> Result<Option<f64>> {
    match e {
        Expr::Nested(x) => constant(x, ty),
        Expr::UnaryOp { op, expr } if matches!(op, UnaryOperator::Minus | UnaryOperator::Plus) => {
            Ok(constant(expr, ty)?.map(|v| match op {
                UnaryOperator::Minus => -v,
                _ => v,
            }))
        }
        Expr::Value(v) => match &v.value {
            Value::Null => Ok(None),
            Value::Number(n, _) => n
                .parse::<f64>()
                .map(Some)
                .map_err(|_| Error::new(format!("{n} is not a number"))),
            Value::SingleQuotedString(s) => match s.trim().parse::<f64>() {
                Ok(v) => Ok(Some(v)),
                Err(_) => Err(Error::new(format!(
                    "invalid input syntax for type {ty}: \"{s}\""
                ))),
            },
            _ => Err(Error::new(format!("TABLESAMPLE {e}: its argument is not a number"))),
        },
        _ => Err(Error::new(format!(
            "TABLESAMPLE {e}: an argument other than a constant is not read"
        ))),
    }
}

/// A table's TABLESAMPLE clause, as the scanner carries it in a call's name (`spelled::SAMPLED`), read
/// as PostgreSQL's parser and its sample scan read it.
pub(super) fn read(k: &TableSampleKind) -> Result<Sample> {
    let TableSampleKind::AfterTableAlias(ts) = k else {
        return Err(Error::new("a TABLESAMPLE before a table's alias is not read"));
    };
    if ts.modifier == TableSampleModifier::Sample {
        return Err(Error::new("syntax error at or near \"SAMPLE\""));
    }
    let call = match &ts.quantity {
        Some(q) if ts.name.is_none() && ts.seed.is_none() && ts.bucket.is_none() && ts.offset.is_none() => {
            &q.value
        }
        _ => return Err(Error::new(format!("TABLESAMPLE {ts}: this form is not read"))),
    };
    let Expr::Function(f) = call else {
        return Err(Error::new(format!("TABLESAMPLE {ts}: this form is not read")));
    };
    let carried = match f.name.0.as_slice() {
        [ObjectNamePart::Identifier(i)] => i.value.strip_prefix(SAMPLED),
        _ => None,
    };
    let Some((kind, method)) = carried.and_then(|c| {
        let mut parts = c.splitn(2, SAMPLED);
        Some((parts.next()?, parts.next()?))
    }) else {
        return Err(Error::new(format!("TABLESAMPLE {ts}: this form is not read")));
    };
    let mut args: Vec<&Expr> = match &f.args {
        FunctionArguments::List(l) => l
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                other => Err(Error::new(format!("TABLESAMPLE {other} is not read"))),
            })
            .collect::<Result<_>>()?,
        _ => Vec::new(),
    };
    let method = match method {
        "system" | "pg_catalog.system" => Method::System,
        "bernoulli" | "pg_catalog.bernoulli" => Method::Bernoulli,
        other => {
            return Err(Error::new(format!(
                "tablesample method {other} does not exist"
            )))
        }
    };
    let name = match method {
        Method::System => "system",
        Method::Bernoulli => "bernoulli",
    };
    let seed = match kind {
        "r" | "s" => args.pop(),
        _ => None,
    };
    if args.len() != 1 {
        return Err(Error::new(format!(
            "tablesample method {name} requires 1 argument, not {}",
            args.len()
        )));
    }
    let percent = constant(args[0], "real")?;
    let seed = match (kind, seed) {
        ("r", Some(e)) => match constant(e, "double precision")? {
            Some(v) => mix(v.to_bits()),
            None => return Err(Error::new("TABLESAMPLE REPEATABLE parameter cannot be null")),
        },
        ("s", Some(e)) => match constant(e, "bigint")? {
            Some(v) => mix(v as u64),
            None => mix(u64::from(drawn_seed())),
        },
        _ => mix(u64::from(drawn_seed())),
    };
    let Some(percent) = percent else {
        return Err(Error::new("TABLESAMPLE parameter cannot be null"));
    };
    // Its argument is a real.
    let percent = f64::from(percent as f32);
    if !(0.0..=100.0).contains(&percent) {
        return Err(Error::new("sample percentage must be between 0 and 100"));
    }
    Ok(Sample {
        method,
        cutoff: ((u32::MAX as f64 + 1.0) * percent / 100.0).round_ties_even() as u64,
        seed,
    })
}
