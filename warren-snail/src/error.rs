use std::fmt;

/// Everything the snail refuses or fails on, as one sentence saying why, and PostgreSQL's SQLSTATE
/// where its words alone do not tell it.
#[derive(Debug)]
pub struct Error(pub String, Option<&'static str>);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into(), None)
    }

    /// A failure PostgreSQL raises with `code`, where the same words are raised with another code
    /// elsewhere.
    pub fn coded(code: &'static str, msg: impl Into<String>) -> Self {
        Error(msg.into(), Some(code))
    }

    /// PostgreSQL's SQLSTATE, where the sentence is PostgreSQL's own failure: of a transaction or of
    /// what it may do (that a retry may succeed, that it waited in a ring, that it holds what it may
    /// not take), of a constraint, of a value, or of a name, each in PostgreSQL's words.
    pub fn sqlstate(&self) -> Option<&'static str> {
        if self.1.is_some() {
            return self.1;
        }
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
    (Words::Starts("cannot insert a non-DEFAULT value into column \""), "428C9"),
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
    (Words::Holds("setval: value ", " is out of bounds for sequence "), "22003"),
    (Words::Holds("currval of sequence \"", "\" is not yet defined in this session"), "55000"),
    (Words::Is("lastval is not yet defined in this session"), "55000"),
    (Words::Starts("cannot open relation \""), "42809"),
    (Words::Starts("could not open relation with OID "), "XX000"),
    (Words::Starts("improper relation name (too many dotted names): "), "42601"),
    (Words::Starts("cross-database references are not implemented: "), "0A000"),
    (Words::Is("invalid name syntax"), "42602"),
    (Words::Holds("MAXVALUE (", ") is out of range for sequence data type "), "22023"),
    (Words::Holds("MINVALUE (", ") is out of range for sequence data type "), "22023"),
    (Words::Holds("MINVALUE (", ") must be less than MAXVALUE ("), "22023"),
    (Words::Starts("START value ("), "22023"),
    (Words::Starts("RESTART value ("), "22023"),
    (Words::Is("INCREMENT must not be zero"), "22023"),
    (Words::Holds("CACHE (", ") must be greater than zero"), "22023"),
    (Words::Starts("invalid value for parameter \""), "22023"),
    (Words::Holds("time zone \"", "\" appears to use leap seconds"), "22023"),
    (Words::Holds("time zone \"", "\" not recognized"), "22023"),
    (Words::Starts("step size cannot "), "22023"),
    (Words::Starts("start value cannot be "), "22023"),
    (Words::Starts("stop value cannot be "), "22023"),
    (Words::Is("timestamp out of range"), "22008"),
    (Words::Is("canceling statement due to statement timeout"), "57014"),
    (Words::Holds("cannot convert value from ", " without time zone usage"), "0A000"),
    (Words::Is("time zone interval must be HOUR or HOUR TO MINUTE"), "42601"),
    (Words::Starts("syntax error at or near \""), "42601"),
    (Words::Is("syntax error at end of input"), "42601"),
    (Words::Holds("", " is outside the valid range for parameter \""), "22023"),
    (Words::Holds("parameter \"", "\" requires a Boolean value"), "22023"),
    (Words::Holds("SET ", " takes only one argument"), "22023"),
    (Words::Starts("unrecognized configuration parameter \""), "42704"),
    (Words::Holds("parameter \"", "\" cannot be changed"), "55P02"),
    (Words::Is("conflicting or redundant options"), "42601"),
    (Words::Holds("column \"", "\" is a generated column"), "42601"),
    (Words::Holds("column \"", "\" is an identity column"), "42601"),
    (Words::Holds("column \"", " but expression is of type "), "42804"),
    (Words::Holds("argument of ", " must be type boolean, not type "), "42804"),
    (Words::Is("array subscript must have type integer"), "42804"),
    (Words::Starts("collations are not supported by type "), "42804"),
    (Words::Starts("aggregate functions are not allowed in "), "42803"),
    (
        Words::Holds("column \"", "\" must appear in the GROUP BY clause or be used in an aggregate function"),
        "42803",
    ),
    (Words::Starts("window functions are not allowed in "), "42P20"),
    (
        Words::Is("for SELECT DISTINCT, ORDER BY expressions must appear in select list"),
        "42P10",
    ),
    (
        Words::Is("SELECT DISTINCT ON expressions must match initial ORDER BY expressions"),
        "42P10",
    ),
    (Words::Holds("ORDER BY position ", " is not in select list"), "42P10"),
    (Words::Holds("GROUP BY position ", " is not in select list"), "42P10"),
    (Words::Holds("DISTINCT ON position ", " is not in select list"), "42P10"),
    (Words::Starts("non-integer constant in "), "42601"),
    (Words::Holds("ORDER BY \"", "\" is ambiguous"), "42702"),
    (Words::Holds("GROUP BY \"", "\" is ambiguous"), "42702"),
    (Words::Holds("DISTINCT ON \"", "\" is ambiguous"), "42702"),
    (Words::Holds("column reference \"", "\" is ambiguous"), "42702"),
    (Words::Holds("", " columns available but "), "42P10"),
    (Words::Starts("could not identify an equality operator for type "), "42883"),
    (Words::Starts("could not identify an ordering operator for type "), "42883"),
    (Words::Is("unequal number of entries in row expressions"), "42601"),
    (Words::Is("cannot compare rows of zero length"), "0A000"),
    (Words::Is("input of anonymous composite types is not implemented"), "0A000"),
    (Words::Starts("cannot cast type "), "42846"),
    (Words::Is("ON CONFLICT DO UPDATE command cannot affect row a second time"), "21000"),
    (Words::Is("more than one row returned by a subquery used as an expression"), "21000"),
    (Words::Is("subquery must return only one column"), "42601"),
    (
        Words::Is("FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"),
        "0A000",
    ),
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
    (Words::Holds("relation \"", " clause not found in FROM clause"), "42P01"),
    (Words::Holds("FOR ", " is not allowed with "), "0A000"),
    (Words::Holds("FOR ", " cannot be applied to the nullable side of an outer join"), "0A000"),
    (Words::Is("LIMIT must not be negative"), "2201W"),
    (Words::Is("OFFSET must not be negative"), "2201X"),
    (Words::Holds("", " can only be used in transaction blocks"), "25P01"),
    (Words::Holds("column \"", "\" specified more than once"), "42701"),
    (Words::Holds("column name \"", "\" appears more than once in USING clause"), "42701"),
    (Words::Holds("common column name \"", "\" appears more than once in "), "42702"),
    (Words::Holds("table name \"", "\" specified more than once"), "42712"),
    (Words::Holds("WITH query name \"", "\" specified more than once"), "42712"),
    (Words::Starts("multiple assignments to same column \""), "42601"),
    (Words::Holds("relation \"", "\" already exists"), "42P07"),
    (Words::Holds("schema \"", "\" already exists"), "42P06"),
    (Words::Holds("table \"", "\" does not exist"), "42P01"),
    (Words::Holds("view \"", "\" does not exist"), "42P01"),
    (Words::Holds("sequence \"", "\" does not exist"), "42P01"),
    (Words::Holds("index \"", "\" does not exist"), "42704"),
    (Words::Holds("\"", "\" is not a table"), "42809"),
    (Words::Holds("\"", "\" is not a view"), "42809"),
    (Words::Holds("\"", "\" is not a sequence"), "42809"),
    (Words::Holds("\"", "\" is not an index"), "42809"),
    (Words::Holds("cannot drop ", " requires it"), "2BP01"),
    (Words::Starts("DROP INDEX CONCURRENTLY does not support "), "0A000"),
    (Words::Holds("permission denied: \"", "\" is a system catalog"), "42501"),
    (Words::Holds("schema \"", "\" does not exist"), "3F000"),
    (Words::Is("no schema has been selected to create in"), "3F000"),
    (Words::Holds("type \"", "\" does not exist"), "42704"),
    (Words::Starts("unacceptable schema name \""), "42939"),
    (Words::Holds("cannot drop ", " because it is required by the database system"), "2BP01"),
    (
        Words::Starts("cannot drop desired object(s) because other objects depend on them"),
        "2BP01",
    ),
    (Words::Holds("constraint \"", "\" already exists"), "42710"),
    (Words::Holds("type \"", "\" already exists"), "42710"),
    (Words::Holds("function \"", "\" already exists with same argument types"), "42723"),
    (Words::Holds("column \"", "\" does not exist"), "42703"),
    (Words::Holds("column ", " does not exist"), "42703"),
    (Words::Starts("operator does not exist: "), "42883"),
    (Words::Holds("function ", " does not exist"), "42883"),
    (Words::Starts("operator is not unique: "), "42725"),
    (Words::Holds("function ", " is not unique"), "42725"),
    (Words::Starts("could not determine data type of parameter "), "42P18"),
    (Words::Starts("inconsistent types deduced for parameter "), "42P08"),
    (Words::Starts("there is no parameter $"), "42P02"),
    (Words::Holds("prepared statement \"", "\" already exists"), "42P05"),
    (Words::Holds("prepared statement \"", "\" does not exist"), "26000"),
    (Words::Starts("wrong number of parameters for prepared statement \""), "42601"),
    (Words::Holds("parameter $", " cannot be coerced to the expected type "), "42804"),
    (Words::Is("cannot use subquery in EXECUTE parameter"), "0A000"),
    (Words::Holds("cannot drop ", " because other objects depend on it"), "2BP01"),
    (Words::Is("cannot truncate a table referenced in a foreign key constraint"), "0A000"),
    (Words::Starts("nondeterministic collations are not supported for "), "0A000"),
    (Words::Holds("WITH query \"", "\" does not have a RETURNING clause"), "0A000"),
    (Words::Is("WITH CHECK OPTION is supported only on automatically updatable views"), "0A000"),
    (Words::Starts("cannot insert into column \""), "0A000"),
    (Words::Starts("cannot update column \""), "0A000"),
    (Words::Starts("cannot insert into view \""), "55000"),
    (Words::Starts("cannot update view \""), "55000"),
    (Words::Starts("cannot delete from view \""), "55000"),
    (Words::Is("VALUES lists must all be the same length"), "42601"),
    (Words::Is("INSERT has more expressions than target columns"), "42601"),
    (Words::Is("INSERT has more target columns than expressions"), "42601"),
    (
        Words::Is("there is no unique or exclusion constraint matching the ON CONFLICT specification"),
        "42P10",
    ),
    (
        Words::Is("ON CONFLICT DO UPDATE requires inference specification or constraint name"),
        "42601",
    ),
    (Words::Holds("constraint \"", "\" for table \""), "42704"),
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
                Error(e.to_string(), None)
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
