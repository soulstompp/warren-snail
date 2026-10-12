use std::fmt;

/// Everything the snail refuses or fails on, as one sentence saying why.
#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
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
