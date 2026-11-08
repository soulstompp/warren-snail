use std::fmt;

/// Everything the snail refuses or fails on, as one sentence saying why.
#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }

    /// PostgreSQL's SQLSTATE, where the sentence is PostgreSQL's own failure: of a transaction or of
    /// what it may do (that a retry may succeed, that it waited in a ring, that it holds what it may
    /// not take), of a constraint, of a value, or of a name, each in PostgreSQL's words.
    pub fn sqlstate(&self) -> Option<&'static str> {
        let first = self.0.lines().next().unwrap_or_default();
        if let Some(code) = said(first) {
            return Some(code);
        }
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

/// How a refusal's first line is told by its words.
enum Words {
    /// It is these.
    Is(&'static str),
    /// It starts with these.
    Starts(&'static str),
    /// It starts with the first, and holds the second after it.
    Holds(&'static str, &'static str),
}

/// Each failure of a constraint, a value or a name by PostgreSQL's words for it, and its SQLSTATE;
/// read in order, a RESTRICT before the foreign key's other refusal.
const SAID: &[(Words, &str)] = &[
    (Words::Starts("duplicate key value violates unique constraint "), "23505"),
    (Words::Holds("new row for relation ", " violates check constraint "), "23514"),
    (Words::Holds("check constraint ", " is violated by some row"), "23514"),
    (Words::Holds("value for domain ", " violates check constraint "), "23514"),
    (Words::Holds("null value in column ", " violates not-null constraint"), "23502"),
    (Words::Holds("column ", " contains null values"), "23502"),
    (Words::Holds("domain ", " does not allow null values"), "23502"),
    (
        Words::Holds("update or delete on table ", " violates RESTRICT setting of foreign key "),
        "23001",
    ),
    (Words::Holds("insert or update on table ", " violates foreign key constraint "), "23503"),
    (Words::Holds("update or delete on table ", " violates foreign key constraint "), "23503"),
    (Words::Starts("conflicting key value violates exclusion constraint "), "23P01"),
    (Words::Starts("new row violates check option for view "), "44000"),
    (Words::Is("smallint out of range"), "22003"),
    (Words::Is("integer out of range"), "22003"),
    (Words::Is("bigint out of range"), "22003"),
    (Words::Starts("numeric field overflow"), "22003"),
    (Words::Holds("value \"", "\" is out of range for type "), "22003"),
    (Words::Holds("\"", "\" is out of range for type "), "22003"),
    (Words::Starts("value out of range: "), "22003"),
    (Words::Starts("value too long for type "), "22001"),
    (Words::Is("division by zero"), "22012"),
    (Words::Starts("nextval: reached maximum value of sequence "), "2200H"),
    (Words::Starts("nextval: reached minimum value of sequence "), "2200H"),
    (Words::Holds("MAXVALUE (", ") is out of range for sequence data type "), "22023"),
    (Words::Holds("MINVALUE (", ") is out of range for sequence data type "), "22023"),
    (Words::Holds("MINVALUE (", ") must be less than MAXVALUE ("), "22023"),
    (Words::Starts("START value ("), "22023"),
    (Words::Starts("RESTART value ("), "22023"),
    (Words::Is("INCREMENT must not be zero"), "22023"),
    (Words::Holds("CACHE (", ") must be greater than zero"), "22023"),
    (Words::Is("conflicting or redundant options"), "42601"),
    (Words::Holds("column \"", "\" is a generated column"), "42601"),
    (Words::Holds("column \"", "\" is an identity column"), "42601"),
    (Words::Starts("cannot cast type "), "42846"),
    (Words::Is("ON CONFLICT DO UPDATE command cannot affect row a second time"), "21000"),
    (Words::Is("more than one row returned by a subquery used as an expression"), "21000"),
    (Words::Starts("invalid input syntax for type date:"), "22007"),
    (Words::Starts("invalid input syntax for type time:"), "22007"),
    (Words::Starts("invalid input syntax for type time with time zone:"), "22007"),
    (Words::Starts("invalid input syntax for type timestamp:"), "22007"),
    (Words::Starts("invalid input syntax for type timestamp with time zone:"), "22007"),
    (Words::Starts("invalid input syntax for type interval:"), "22007"),
    (Words::Starts("invalid input syntax for type "), "22P02"),
    (Words::Starts("date/time field value out of range"), "22008"),
    (Words::Holds("relation \"", "\" does not exist"), "42P01"),
    (Words::Starts("missing FROM-clause entry for table "), "42P01"),
    (Words::Starts("invalid reference to FROM-clause entry for table "), "42P01"),
    (Words::Holds("relation \"", "\" already exists"), "42P07"),
    (Words::Holds("constraint \"", "\" already exists"), "42710"),
    (Words::Holds("column \"", "\" does not exist"), "42703"),
    (Words::Starts("operator does not exist: "), "42883"),
    (Words::Holds("function ", " does not exist"), "42883"),
    (Words::Starts("operator is not unique: "), "42725"),
    (Words::Starts("could not determine data type of parameter "), "42P18"),
    (Words::Holds("cannot drop ", " because other objects depend on it"), "2BP01"),
    (Words::Is("cannot truncate a table referenced in a foreign key constraint"), "0A000"),
    (Words::Starts("nondeterministic collations are not supported for "), "0A000"),
    (Words::Holds("WITH query \"", "\" does not have a RETURNING clause"), "0A000"),
    (Words::Starts("sql parser error: "), "42601"),
];

/// The SQLSTATE PostgreSQL raises a failure with, where `first` is its words.
fn said(first: &str) -> Option<&'static str> {
    SAID.iter().find_map(|(w, code)| {
        let held = match w {
            Words::Is(s) => first == *s,
            Words::Starts(s) => first.starts_with(s),
            Words::Holds(s, t) => first.strip_prefix(s).is_some_and(|rest| rest.contains(t)),
        };
        held.then_some(*code)
    })
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
