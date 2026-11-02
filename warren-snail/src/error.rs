use std::fmt;

/// Everything the snail refuses or fails on, as one sentence saying why.
#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }

    /// PostgreSQL's SQLSTATE, where the sentence is PostgreSQL's own failure of a transaction or of
    /// what it may do: that a retry may succeed, that it waited in a ring, that it holds what it may
    /// not take.
    pub fn sqlstate(&self) -> Option<&'static str> {
        let first = self.0.lines().next().unwrap_or_default();
        Some(if first.starts_with("could not serialize access") {
            "40001"
        } else if first.starts_with("deadlock detected") {
            "40P01"
        } else if first.starts_with("could not obtain lock on row") {
            "55P03"
        } else if first.starts_with("current transaction is aborted") {
            "25P02"
        } else if first.starts_with("cannot execute ")
            && first.ends_with(" in a read-only transaction")
        {
            "25006"
        } else if first.ends_with("must be called before any query")
            || first.ends_with("must be set before any query")
        {
            "25001"
        } else if first == "cannot use serializable mode in a hot standby" {
            "0A000"
        } else {
            return None;
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

macro_rules! from {
    ($($t:ty),*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Error(e.to_string())
            }
        }
    )*};
}

from!(
    std::io::Error,
    parquet::errors::ParquetError,
    arrow::error::ArrowError,
    sqlparser::parser::ParserError,
    sqlparser::tokenizer::TokenizerError
);
