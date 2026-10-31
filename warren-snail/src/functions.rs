//! The functions PostgreSQL defines from 14 to 19, each by its signature, as the snail classes them
//! (`functions.txt`, drawn from each release's catalog): a type's, which the snail answers once its
//! family is built, or the server's own, what only a running server has, which the snail refuses by
//! name as the part of the server it reads.

use crate::{Error, Result};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Whose a function is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Class {
    /// A type's: what it returns depends only on its arguments, the statement's clock, the settings
    /// the snail answers under, what the file holds and a value the statement draws.
    Type,
    /// The server's own: it reads or changes what only a running server has.
    Server,
}

/// One function of the list.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Function {
    /// Its signature as `regprocedure` prints it: `round(numeric,integer)`.
    pub signature: &'static str,
    /// Its name, unquoted.
    pub name: &'static str,
    pub class: Class,
    /// A type's family, as the build takes them in turn; or the part of the server the server's own
    /// reads.
    pub family: &'static str,
    /// The releases that define it, a bit a release from 14.
    releases: u8,
}

impl Function {
    /// Whether the release defines it.
    pub fn in_release(&self, release: u8) -> bool {
        release
            .checked_sub(*crate::regs::RELEASES.start())
            .is_some_and(|b| b < 8 && self.releases & (1 << b) != 0)
    }

    /// Whether the snail answers it.
    pub fn held(&self) -> bool {
        held().contains(self.signature)
    }
}

/// The functions of a type the snail answers, by signature: the numbers', time's, then the aggregates
/// past those the types carry and the window functions, then strings'.
const HELD: &[&str] = &[
    "abs(bigint)",
    "abs(double precision)",
    "abs(integer)",
    "abs(numeric)",
    "abs(real)",
    "abs(smallint)",
    "acos(double precision)",
    "acosd(double precision)",
    "acosh(double precision)",
    "asin(double precision)",
    "asind(double precision)",
    "asinh(double precision)",
    "atan(double precision)",
    "atan2(double precision,double precision)",
    "atan2d(double precision,double precision)",
    "atand(double precision)",
    "atanh(double precision)",
    "cbrt(double precision)",
    "ceil(double precision)",
    "ceil(numeric)",
    "ceiling(double precision)",
    "ceiling(numeric)",
    "cos(double precision)",
    "cosd(double precision)",
    "cosh(double precision)",
    "cot(double precision)",
    "cotd(double precision)",
    "degrees(double precision)",
    "dexp(double precision)",
    "div(numeric,numeric)",
    "dlog1(double precision)",
    "dlog10(double precision)",
    "dround(double precision)",
    "dtrunc(double precision)",
    "erf(double precision)",
    "erfc(double precision)",
    "exp(double precision)",
    "exp(numeric)",
    "factorial(bigint)",
    "floor(double precision)",
    "floor(numeric)",
    "gamma(double precision)",
    "gcd(bigint,bigint)",
    "gcd(integer,integer)",
    "gcd(numeric,numeric)",
    "lcm(bigint,bigint)",
    "lcm(integer,integer)",
    "lcm(numeric,numeric)",
    "lgamma(double precision)",
    "ln(double precision)",
    "ln(numeric)",
    "log(double precision)",
    "log(numeric)",
    "log(numeric,numeric)",
    "log10(double precision)",
    "log10(numeric)",
    "min_scale(numeric)",
    "mod(bigint,bigint)",
    "mod(integer,integer)",
    "mod(numeric,numeric)",
    "mod(smallint,smallint)",
    "num_nonnulls(\"any\")",
    "num_nulls(\"any\")",
    "numeric_div_trunc(numeric,numeric)",
    "numeric_exp(numeric)",
    "numeric_ln(numeric)",
    "numeric_log(numeric,numeric)",
    "numeric_sqrt(numeric)",
    "pi()",
    "pow(double precision,double precision)",
    "pow(numeric,numeric)",
    "power(double precision,double precision)",
    "power(numeric,numeric)",
    "radians(double precision)",
    "round(double precision)",
    "round(numeric)",
    "round(numeric,integer)",
    "scale(numeric)",
    "sign(double precision)",
    "sign(numeric)",
    "sin(double precision)",
    "sind(double precision)",
    "sinh(double precision)",
    "sqrt(double precision)",
    "sqrt(numeric)",
    "tan(double precision)",
    "tand(double precision)",
    "tanh(double precision)",
    "to_bin(bigint)",
    "to_bin(integer)",
    "to_hex(bigint)",
    "to_hex(integer)",
    "to_oct(bigint)",
    "to_oct(integer)",
    "trim_scale(numeric)",
    "trunc(double precision)",
    "trunc(numeric)",
    "trunc(numeric,integer)",
    "width_bucket(double precision,double precision,double precision,integer)",
    "width_bucket(numeric,numeric,numeric,integer)",
    "\"extract\"(text,date)",
    "\"extract\"(text,interval)",
    "\"extract\"(text,time with time zone)",
    "\"extract\"(text,time without time zone)",
    "\"extract\"(text,timestamp with time zone)",
    "\"extract\"(text,timestamp without time zone)",
    "\"overlaps\"(time with time zone,time with time zone,time with time zone,time with time zone)",
    "\"overlaps\"(time without time zone,interval,time without time zone,interval)",
    "\"overlaps\"(time without time zone,interval,time without time zone,time without time zone)",
    "\"overlaps\"(time without time zone,time without time zone,time without time zone,interval)",
    "\"overlaps\"(time without time zone,time without time zone,time without time zone,time without time zone)",
    "\"overlaps\"(timestamp with time zone,interval,timestamp with time zone,interval)",
    "\"overlaps\"(timestamp with time zone,interval,timestamp with time zone,timestamp with time zone)",
    "\"overlaps\"(timestamp with time zone,timestamp with time zone,timestamp with time zone,interval)",
    "\"overlaps\"(timestamp with time zone,timestamp with time zone,timestamp with time zone,timestamp with time zone)",
    "\"overlaps\"(timestamp without time zone,interval,timestamp without time zone,interval)",
    "\"overlaps\"(timestamp without time zone,interval,timestamp without time zone,timestamp without time zone)",
    "\"overlaps\"(timestamp without time zone,timestamp without time zone,timestamp without time zone,interval)",
    "\"overlaps\"(timestamp without time zone,timestamp without time zone,timestamp without time zone,timestamp without time zone)",
    "\"timestamp\"(date,time without time zone)",
    "age(timestamp with time zone)",
    "age(timestamp with time zone,timestamp with time zone)",
    "age(timestamp without time zone)",
    "age(timestamp without time zone,timestamp without time zone)",
    "clock_timestamp()",
    "date_add(timestamp with time zone,interval)",
    "date_bin(interval,timestamp with time zone,timestamp with time zone)",
    "date_bin(interval,timestamp without time zone,timestamp without time zone)",
    "date_part(text,date)",
    "date_part(text,interval)",
    "date_part(text,time with time zone)",
    "date_part(text,time without time zone)",
    "date_part(text,timestamp with time zone)",
    "date_part(text,timestamp without time zone)",
    "date_subtract(timestamp with time zone,interval)",
    "date_trunc(text,interval)",
    "date_trunc(text,timestamp with time zone)",
    "date_trunc(text,timestamp with time zone,text)",
    "date_trunc(text,timestamp without time zone)",
    "isfinite(date)",
    "isfinite(interval)",
    "isfinite(timestamp with time zone)",
    "isfinite(timestamp without time zone)",
    "justify_days(interval)",
    "justify_hours(interval)",
    "justify_interval(interval)",
    "make_date(integer,integer,integer)",
    "make_interval(integer,integer,integer,integer,integer,integer,double precision)",
    "make_time(integer,integer,double precision)",
    "make_timestamp(integer,integer,integer,integer,integer,double precision)",
    "make_timestamptz(integer,integer,integer,integer,integer,double precision)",
    "make_timestamptz(integer,integer,integer,integer,integer,double precision,text)",
    "now()",
    "statement_timestamp()",
    "timeofday()",
    "timestamptz(date,time with time zone)",
    "timestamptz(date,time without time zone)",
    "timezone(text,timestamp with time zone)",
    "timezone(text,timestamp without time zone)",
    "to_timestamp(double precision)",
    "transaction_timestamp()",
    "any_value(anyelement)",
    "bit_and(bigint)",
    "bit_and(bit)",
    "bit_and(integer)",
    "bit_and(smallint)",
    "bit_or(bigint)",
    "bit_or(bit)",
    "bit_or(integer)",
    "bit_or(smallint)",
    "bit_xor(bigint)",
    "bit_xor(bit)",
    "bit_xor(integer)",
    "bit_xor(smallint)",
    "corr(double precision,double precision)",
    "covar_pop(double precision,double precision)",
    "covar_samp(double precision,double precision)",
    "cume_dist(\"any\")",
    "cume_dist()",
    "dense_rank(\"any\")",
    "dense_rank()",
    "first_value(anyelement)",
    "lag(anycompatible,integer,anycompatible)",
    "lag(anyelement)",
    "lag(anyelement,integer)",
    "last_value(anyelement)",
    "lead(anycompatible,integer,anycompatible)",
    "lead(anyelement)",
    "lead(anyelement,integer)",
    "mode(anyelement)",
    "nth_value(anyelement,integer)",
    "ntile(integer)",
    "percent_rank(\"any\")",
    "percent_rank()",
    "percentile_cont(double precision,double precision)",
    "percentile_cont(double precision,interval)",
    "percentile_disc(double precision,anyelement)",
    "rank(\"any\")",
    "rank()",
    "regr_avgx(double precision,double precision)",
    "regr_avgy(double precision,double precision)",
    "regr_count(double precision,double precision)",
    "regr_intercept(double precision,double precision)",
    "regr_r2(double precision,double precision)",
    "regr_slope(double precision,double precision)",
    "regr_sxx(double precision,double precision)",
    "regr_sxy(double precision,double precision)",
    "regr_syy(double precision,double precision)",
    "row_number()",
    "stddev(bigint)",
    "stddev(double precision)",
    "stddev(integer)",
    "stddev(numeric)",
    "stddev(real)",
    "stddev(smallint)",
    "stddev_pop(bigint)",
    "stddev_pop(double precision)",
    "stddev_pop(integer)",
    "stddev_pop(numeric)",
    "stddev_pop(real)",
    "stddev_pop(smallint)",
    "stddev_samp(bigint)",
    "stddev_samp(double precision)",
    "stddev_samp(integer)",
    "stddev_samp(numeric)",
    "stddev_samp(real)",
    "stddev_samp(smallint)",
    "var_pop(bigint)",
    "var_pop(double precision)",
    "var_pop(integer)",
    "var_pop(numeric)",
    "var_pop(real)",
    "var_pop(smallint)",
    "var_samp(bigint)",
    "var_samp(double precision)",
    "var_samp(integer)",
    "var_samp(numeric)",
    "var_samp(real)",
    "var_samp(smallint)",
    "variance(bigint)",
    "variance(double precision)",
    "variance(integer)",
    "variance(numeric)",
    "variance(real)",
    "variance(smallint)",
    "every(boolean)",
    "\"left\"(text,integer)",
    "\"like\"(text,text)",
    "\"overlay\"(bytea,bytea,integer)",
    "\"overlay\"(bytea,bytea,integer,integer)",
    "\"overlay\"(text,text,integer)",
    "\"overlay\"(text,text,integer,integer)",
    "\"position\"(bytea,bytea)",
    "\"position\"(text,text)",
    "\"right\"(text,integer)",
    "\"substring\"(bytea,integer)",
    "\"substring\"(bytea,integer,integer)",
    "\"substring\"(text,integer)",
    "\"substring\"(text,integer,integer)",
    "\"substring\"(text,text)",
    "ascii(text)",
    "bit_length(bytea)",
    "bit_length(text)",
    "btrim(bytea,bytea)",
    "btrim(text)",
    "btrim(text,text)",
    "casefold(text)",
    "char_length(character)",
    "char_length(text)",
    "character_length(character)",
    "character_length(text)",
    "chr(integer)",
    "concat(\"any\")",
    "concat_ws(text,\"any\")",
    "format(text)",
    "format(text,\"any\")",
    "initcap(text)",
    "length(bytea)",
    "length(character)",
    "length(text)",
    "lower(text)",
    "lpad(text,integer)",
    "lpad(text,integer,text)",
    "ltrim(bytea,bytea)",
    "ltrim(text)",
    "ltrim(text,text)",
    "notlike(text,text)",
    "octet_length(bytea)",
    "octet_length(character)",
    "octet_length(text)",
    "quote_ident(text)",
    "quote_literal(anyelement)",
    "quote_literal(text)",
    "quote_nullable(anyelement)",
    "quote_nullable(text)",
    "repeat(text,integer)",
    "replace(text,text,text)",
    "reverse(bytea)",
    "reverse(text)",
    "rpad(text,integer)",
    "rpad(text,integer,text)",
    "rtrim(bytea,bytea)",
    "rtrim(text)",
    "rtrim(text,text)",
    "split_part(text,text,integer)",
    "string_agg(bytea,bytea)",
    "string_agg(text,text)",
    "strpos(text,text)",
    "substr(bytea,integer)",
    "substr(bytea,integer,integer)",
    "substr(text,integer)",
    "substr(text,integer,integer)",
    "textlen(text)",
    "translate(text,text,text)",
    "unistr(text)",
    "upper(text)",
    "bit_count(bytea)",
    "convert(bytea,name,name)",
    "convert_from(bytea,name)",
    "convert_to(text,name)",
    "crc32(bytea)",
    "crc32c(bytea)",
    "decode(text,text)",
    "encode(bytea,text)",
    "get_bit(bytea,bigint)",
    "get_byte(bytea,integer)",
    "md5(bytea)",
    "md5(text)",
    "pg_char_to_encoding(name)",
    "pg_encoding_max_length(integer)",
    "pg_encoding_to_char(integer)",
    "set_bit(bytea,bigint,integer)",
    "set_byte(bytea,integer,integer)",
    "sha224(bytea)",
    "sha256(bytea)",
    "sha384(bytea)",
    "sha512(bytea)",
    "to_ascii(text)",
    "to_ascii(text,name)",
    "to_char(bigint,text)",
    "to_char(double precision,text)",
    "to_char(integer,text)",
    "to_char(interval,text)",
    "to_char(numeric,text)",
    "to_char(real,text)",
    "to_char(timestamp with time zone,text)",
    "to_char(timestamp without time zone,text)",
    "to_date(text,text)",
    "to_number(text,text)",
    "to_timestamp(text,text)",
];

fn held() -> &'static std::collections::HashSet<&'static str> {
    static HELD_SET: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    HELD_SET.get_or_init(|| HELD.iter().copied().collect())
}

/// Every function of the list, by signature.
pub fn all() -> &'static [Function] {
    static ALL: OnceLock<Vec<Function>> = OnceLock::new();
    ALL.get_or_init(|| {
        include_str!("functions.txt")
            .lines()
            .map(|line| {
                let mut f = line.split('\t');
                let signature = f.next().expect("a row of functions.txt names a signature");
                let releases = f.next().filter(|s| !s.is_empty());
                let class = match f.next() {
                    Some("server") => Class::Server,
                    _ => Class::Type,
                };
                let family = f.next().expect("a row of functions.txt names a family");
                let name = signature.split('(').next().unwrap_or_default();
                Function {
                    signature,
                    name: name.trim_matches('"'),
                    class,
                    family,
                    releases: crate::regs::releases_of(releases),
                }
            })
            .collect()
    })
}

/// The functions of each name.
fn by_name() -> &'static HashMap<&'static str, Vec<&'static Function>> {
    static BY_NAME: OnceLock<HashMap<&'static str, Vec<&'static Function>>> = OnceLock::new();
    BY_NAME.get_or_init(|| {
        let mut m: HashMap<&'static str, Vec<&'static Function>> = HashMap::new();
        for f in all() {
            m.entry(f.name).or_default().push(f);
        }
        m
    })
}

/// The functions of a name a release defines.
pub fn named(name: &str, release: u8) -> impl Iterator<Item = &'static Function> {
    by_name()
        .get(name)
        .into_iter()
        .flatten()
        .copied()
        .filter(move |f| f.in_release(release))
}

/// The part of the server a function reads, where every function of its name the release defines
/// is the server's own.
pub fn server_family(name: &str, release: u8) -> Option<&'static str> {
    let mut of = named(name, release).peekable();
    of.peek()?;
    let mut family = None;
    for f in of {
        if f.class != Class::Server {
            return None;
        }
        family.get_or_insert(f.family);
    }
    family
}

/// Why the snail does not answer a function of the server's own.
pub fn refusal(name: &str, family: &str) -> Error {
    Error::new(format!(
        "{name} is the server's own ({family}): a snail answers only what its file holds"
    ))
}

/// A function the server owns, refused by name as its class, where the call names one.
pub(crate) fn refuse_server(name: &str, release: u8) -> Result<()> {
    match server_family(name, release) {
        Some(family) => Err(refusal(name, family)),
        None => Ok(()),
    }
}
