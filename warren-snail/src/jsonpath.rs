//! jsonpath, as PostgreSQL reads and writes it: read as `jsonpath_in` reads it (the scanner of
//! jsonpath_scan.l, the grammar of jsonpath_gram.y, and the checks `flattenJsonPathParseItem`
//! makes), and written as `jsonpath_out` writes it (`printJsonPathItem`). A jsonpath has no order
//! and no equality of its own: two spellings are one value where they write the same text.

use crate::numeric::Numeric;
use crate::{Error, Result};

mod exec;
mod gin;

pub use gin::{gin_asks, Asks};

pub use exec::{
    jsonb_path_exists, jsonb_path_exists_op, jsonb_path_match, jsonb_path_match_op,
    jsonb_path_query, jsonb_path_query_array, jsonb_path_query_first,
};

/// A jsonpath as `jsonpath_in` reads it, written back as `jsonpath_out` writes it: `lax` dropped,
/// `strict` kept, every key and variable quoted, each operation in parentheses where its priority
/// asks for them, each number as `numeric_out` writes it. Refused with PostgreSQL's own words.
pub fn jsonpath_in(s: &str) -> Result<String> {
    let (lax, path) = read(s)?;
    let mut out = Vec::with_capacity(s.len() + 8);
    if !lax {
        out.extend_from_slice(b"strict ");
    }
    print(&mut out, &path, true);
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A jsonpath as `jsonpath_in` reads it: whether it is lax, and its items.
fn read(s: &str) -> Result<(bool, Chain)> {
    let mut p = Parser {
        lex: Lexer {
            b: s.as_bytes(),
            at: 0,
        },
        ahead: None,
        depth: 0,
    };
    let Some((lax, path)) = p.path()? else {
        return Err(Error::new(format!(
            "invalid input syntax for type jsonpath: \"{s}\""
        )));
    };
    check(&path, 0, false)?;
    Ok((lax, path))
}

// ---- The scanner: jsonpath_scan.l ----

/// The words the scanner knows, `checkKeyword`'s table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kw {
    Is,
    To,
    Abs,
    Lax,
    Date,
    Flag,
    Last,
    Null,
    Size,
    Time,
    True,
    Type,
    With,
    Btrim,
    False,
    Floor,
    Lower,
    Ltrim,
    Rtrim,
    Upper,
    Bigint,
    Double,
    Exists,
    Number,
    Starts,
    Strict,
    String,
    Boolean,
    Ceiling,
    Decimal,
    Initcap,
    Integer,
    Replace,
    TimeTz,
    Unknown,
    Datetime,
    KeyValue,
    Timestamp,
    LikeRegex,
    SplitPart,
    TimestampTz,
}

const KEYWORDS: [(&str, Kw); 41] = [
    ("is", Kw::Is),
    ("to", Kw::To),
    ("abs", Kw::Abs),
    ("lax", Kw::Lax),
    ("date", Kw::Date),
    ("flag", Kw::Flag),
    ("last", Kw::Last),
    ("null", Kw::Null),
    ("size", Kw::Size),
    ("time", Kw::Time),
    ("true", Kw::True),
    ("type", Kw::Type),
    ("with", Kw::With),
    ("btrim", Kw::Btrim),
    ("false", Kw::False),
    ("floor", Kw::Floor),
    ("lower", Kw::Lower),
    ("ltrim", Kw::Ltrim),
    ("rtrim", Kw::Rtrim),
    ("upper", Kw::Upper),
    ("bigint", Kw::Bigint),
    ("double", Kw::Double),
    ("exists", Kw::Exists),
    ("number", Kw::Number),
    ("starts", Kw::Starts),
    ("strict", Kw::Strict),
    ("string", Kw::String),
    ("boolean", Kw::Boolean),
    ("ceiling", Kw::Ceiling),
    ("decimal", Kw::Decimal),
    ("initcap", Kw::Initcap),
    ("integer", Kw::Integer),
    ("replace", Kw::Replace),
    ("time_tz", Kw::TimeTz),
    ("unknown", Kw::Unknown),
    ("datetime", Kw::Datetime),
    ("keyvalue", Kw::KeyValue),
    ("timestamp", Kw::Timestamp),
    ("like_regex", Kw::LikeRegex),
    ("split_part", Kw::SplitPart),
    ("timestamp_tz", Kw::TimestampTz),
];

/// A word's keyword, as `checkKeyword` finds it: any case, but `null`, `true` and `false` only in
/// lower case.
fn keyword(w: &[u8]) -> Option<Kw> {
    KEYWORDS.iter().find_map(|&(name, k)| {
        let lower_only = matches!(k, Kw::Null | Kw::True | Kw::False);
        let same = if lower_only {
            w == name.as_bytes()
        } else {
            w.eq_ignore_ascii_case(name.as_bytes())
        };
        same.then_some(k)
    })
}

/// What the scanner returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok {
    End,
    /// One of the characters the scanner calls special, returned as itself.
    Char(u8),
    And,
    Or,
    Not,
    /// `**`
    AnyPath,
    Less,
    LessEqual,
    Equal,
    NotEqual,
    GreaterEqual,
    Greater,
    Variable,
    Str,
    Numeric,
    Int,
    Ident,
    Kw(Kw),
}

struct Token {
    tok: Tok,
    /// A string's characters, a variable's or a word's name, a number's text.
    val: Vec<u8>,
    /// The text the scanner last matched, which a refusal quotes; none where it quotes the end.
    near: Option<String>,
}

fn is_special(c: u8) -> bool {
    matches!(
        c,
        b'?' | b'%'
            | b'$'
            | b'.'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'('
            | b')'
            | b'|'
            | b'&'
            | b'!'
            | b'='
            | b'<'
            | b'>'
            | b'@'
            | b'#'
            | b','
            | b'*'
            | b':'
            | b'-'
            | b'+'
            | b'/'
    )
}

fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// What the scanner calls other: neither special, blank, a backslash nor a double quote.
fn is_other(c: u8) -> bool {
    !is_special(c) && !is_blank(c) && c != b'\\' && c != b'"'
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn near(msg: &str, text: &[u8]) -> Error {
    Error::new(format!(
        "{msg} at or near \"{}\" of jsonpath input",
        String::from_utf8_lossy(text)
    ))
}

fn at_end(msg: &str) -> Error {
    Error::new(format!("{msg} at end of jsonpath input"))
}

/// The refusal `jsonpath_yyerror` makes at a token the grammar cannot take.
fn syntax(t: &Token) -> Error {
    match &t.near {
        Some(text) => near("syntax error", text.as_bytes()),
        None => at_end("syntax error"),
    }
}

/// The end of the digits `d(_?d)*` from `i`, where a digit starts them.
fn dec_digits(b: &[u8], i: usize) -> Option<usize> {
    run_of(b, i, |c| c.is_ascii_digit())
}

/// The end of the digits `d(_?d)*` from `i` that `digit` takes, an underscore only between two.
fn run_of(b: &[u8], mut i: usize, digit: impl Fn(u8) -> bool) -> Option<usize> {
    if !b.get(i).is_some_and(|&c| digit(c)) {
        return None;
    }
    i += 1;
    loop {
        match b.get(i) {
            Some(&c) if digit(c) => i += 1,
            Some(b'_') if b.get(i + 1).is_some_and(|&c| digit(c)) => i += 2,
            _ => return Some(i),
        }
    }
}

/// The end of an integer in base 10 from `i`: `0`, or a digit from 1 and digits after it.
fn dec_integer(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i) {
        Some(b'0') => Some(i + 1),
        Some(b'1'..=b'9') => dec_digits(b, i),
        _ => None,
    }
}

/// The end of an integer in base 16, 8 or 2 from `i`, after `0` and one of `letters`.
fn radix_integer(b: &[u8], i: usize, letters: &[u8], digit: fn(u8) -> bool) -> Option<usize> {
    if b.get(i) == Some(&b'0') && b.get(i + 1).is_some_and(|c| letters.contains(c)) {
        run_of(b, i + 2, digit)
    } else {
        None
    }
}

/// What the scanner does with the longest text one of its number rules takes.
#[derive(Clone, Copy)]
enum NumberRule {
    Numeric,
    Int,
    Invalid,
    Junk,
    Word,
}

struct Lexer<'a> {
    b: &'a [u8],
    at: usize,
}

impl Lexer<'_> {
    fn token(&mut self, tok: Tok, len: usize) -> Token {
        let text = &self.b[self.at..self.at + len];
        self.at += len;
        Token {
            tok,
            val: text.to_vec(),
            near: Some(String::from_utf8_lossy(text).into_owned()),
        }
    }

    /// The next token, as `jsonpath_yylex` returns it.
    fn next(&mut self) -> Result<Token> {
        let b = self.b;
        loop {
            let Some(&c) = b.get(self.at) else {
                return Ok(Token {
                    tok: Tok::End,
                    val: Vec::new(),
                    near: None,
                });
            };
            let c2 = b.get(self.at + 1).copied();
            let t = match (c, c2) {
                _ if is_blank(c) => {
                    self.at += 1;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    self.at += 2;
                    self.comment()?;
                    continue;
                }
                (b'&', Some(b'&')) => self.token(Tok::And, 2),
                (b'|', Some(b'|')) => self.token(Tok::Or, 2),
                (b'*', Some(b'*')) => self.token(Tok::AnyPath, 2),
                (b'<', Some(b'=')) => self.token(Tok::LessEqual, 2),
                (b'<', Some(b'>')) => self.token(Tok::NotEqual, 2),
                (b'<', _) => self.token(Tok::Less, 1),
                (b'=', Some(b'=')) => self.token(Tok::Equal, 2),
                (b'!', Some(b'=')) => self.token(Tok::NotEqual, 2),
                (b'!', _) => self.token(Tok::Not, 1),
                (b'>', Some(b'=')) => self.token(Tok::GreaterEqual, 2),
                (b'>', _) => self.token(Tok::Greater, 1),
                (b'$', Some(d)) if is_other(d) => {
                    let end = self.other_run(self.at + 1);
                    let mut t = self.token(Tok::Variable, end - self.at);
                    t.val.remove(0);
                    t
                }
                (b'$', Some(b'"')) => {
                    self.at += 2;
                    return self.quoted(Tok::Variable);
                }
                (b'"', _) => {
                    self.at += 1;
                    return self.quoted(Tok::Str);
                }
                (b'\\', _) => match self.word(Vec::new())? {
                    Some(t) => t,
                    None => continue,
                },
                (b'0'..=b'9', _) | (b'.', Some(b'0'..=b'9')) => match self.number()? {
                    Some(t) => t,
                    None => continue,
                },
                _ if is_special(c) => self.token(Tok::Char(c), 1),
                _ => {
                    let end = self.other_run(self.at);
                    let start = b[self.at..end].to_vec();
                    self.at = end;
                    match self.word(start)? {
                        Some(t) => t,
                        None => continue,
                    }
                }
            };
            return Ok(t);
        }
    }

    fn other_run(&self, mut i: usize) -> usize {
        while self.b.get(i).is_some_and(|&c| is_other(c)) {
            i += 1;
        }
        i
    }

    /// A comment's body after its `/*`, to its `*/`.
    fn comment(&mut self) -> Result<()> {
        match self.b[self.at..].windows(2).position(|w| w == b"*/") {
            Some(p) => {
                self.at += p + 2;
                Ok(())
            }
            None => Err(at_end("unexpected end of comment")),
        }
    }

    /// A number, as the scanner's number rules take it: the longest text any of them matches,
    /// the first rule on a tie. A rule's text that runs on into a word is a word.
    fn number(&mut self) -> Result<Option<Token>> {
        let b = self.b;
        let at = self.at;
        let int = dec_integer(b, at);
        let decimal = match int {
            Some(e) if b.get(e) == Some(&b'.') => Some(dec_digits(b, e + 1).unwrap_or(e + 1)),
            _ if b[at] == b'.' => dec_digits(b, at + 1),
            _ => None,
        };
        let (real, real_fail) = match decimal.or(int) {
            Some(e) if matches!(b.get(e), Some(b'e' | b'E')) => {
                let signed = matches!(b.get(e + 1), Some(b'+' | b'-'));
                let digits = if signed { e + 2 } else { e + 1 };
                (dec_digits(b, digits), signed.then_some(e + 2))
            }
            _ => (None, None),
        };
        let junk = |end: Option<usize>| {
            end.filter(|&e| b.get(e).is_some_and(|&c| is_other(c)))
                .map(|e| e + 1)
        };
        let word = b[at].is_ascii_digit().then(|| self.other_run(at));
        let rules = [
            (real, NumberRule::Numeric),
            (decimal, NumberRule::Numeric),
            (int, NumberRule::Int),
            (radix_integer(b, at, b"xX", is_hex), NumberRule::Int),
            (
                radix_integer(b, at, b"oO", |c| (b'0'..=b'7').contains(&c)),
                NumberRule::Int,
            ),
            (
                radix_integer(b, at, b"bB", |c| c == b'0' || c == b'1'),
                NumberRule::Int,
            ),
            (real_fail, NumberRule::Invalid),
            (junk(int), NumberRule::Junk),
            (junk(decimal), NumberRule::Junk),
            (junk(real), NumberRule::Junk),
            (word, NumberRule::Word),
        ];
        let (end, rule) = rules
            .iter()
            .filter_map(|&(end, rule)| end.map(|e| (e, rule)))
            .fold(
                None,
                |best: Option<(usize, NumberRule)>, (e, r)| match best {
                    Some((be, _)) if be >= e => best,
                    _ => Some((e, r)),
                },
            )
            .expect("a digit is a number");
        let text = &b[at..end];
        match rule {
            NumberRule::Numeric => Ok(Some(self.token(Tok::Numeric, end - at))),
            NumberRule::Int => Ok(Some(self.token(Tok::Int, end - at))),
            NumberRule::Invalid => Err(near("invalid numeric literal", text)),
            NumberRule::Junk => Err(near("trailing junk after numeric literal", text)),
            NumberRule::Word => {
                self.at = end;
                self.word(text.to_vec())
            }
        }
    }

    /// A word not in quotes, from what the scanner has taken of it: the scanner's `xnq` state.
    /// It ends at a blank, a special character, a double quote or the end; a comment starting in
    /// it drops it, and none is returned.
    fn word(&mut self, mut val: Vec<u8>) -> Result<Option<Token>> {
        let b = self.b;
        loop {
            let Some(&c) = b.get(self.at) else {
                return Ok(Some(word_token(val, None)));
            };
            if is_blank(c) {
                let start = self.at;
                while b.get(self.at).is_some_and(|&c| is_blank(c)) {
                    self.at += 1;
                }
                let blanks = String::from_utf8_lossy(&b[start..self.at]).into_owned();
                return Ok(Some(word_token(val, Some(blanks))));
            }
            if c == b'/' && b.get(self.at + 1) == Some(&b'*') {
                self.at += 2;
                self.comment()?;
                return Ok(None);
            }
            if is_special(c) || c == b'"' {
                return Ok(Some(word_token(val, None)));
            }
            if c == b'\\' {
                self.escape(&mut val)?;
                continue;
            }
            let end = self.other_run(self.at);
            val.extend_from_slice(&b[self.at..end]);
            self.at = end;
        }
    }

    /// A string or a variable's name in double quotes, after its opening quote.
    fn quoted(&mut self, tok: Tok) -> Result<Token> {
        let b = self.b;
        let mut val = Vec::new();
        loop {
            match b.get(self.at) {
                None => return Err(at_end("unterminated quoted string")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(Token {
                        tok,
                        val,
                        near: Some("\"".into()),
                    });
                }
                Some(b'\\') => self.escape(&mut val)?,
                Some(_) => {
                    let start = self.at;
                    while b.get(self.at).is_some_and(|&c| c != b'\\' && c != b'"') {
                        self.at += 1;
                    }
                    val.extend_from_slice(&b[start..self.at]);
                }
            }
        }
    }

    /// An escape at a backslash: the six letters of control characters, `\x` and two hex digits,
    /// `\u` and four hex digits or up to six in braces (a run of them read together, a UTF-16
    /// surrogate pair among them one character), and a backslash before any other character that
    /// character.
    fn escape(&mut self, val: &mut Vec<u8>) -> Result<()> {
        let b = self.b;
        let at = self.at;
        match b.get(at + 1) {
            Some(b'u') => {
                let mut i = at;
                let mut chars = Vec::new();
                while let Some((ch, end)) = unicode_escape(b, i) {
                    chars.push(ch);
                    i = end;
                }
                if b.get(i) == Some(&b'\\') && b.get(i + 1) == Some(&b'u') {
                    let mut end = i + 2;
                    let max = if b.get(end) == Some(&b'{') {
                        end += 1;
                        6
                    } else {
                        3
                    };
                    let mut n = 0;
                    while n < max && b.get(end).is_some_and(|&c| is_hex(c)) {
                        end += 1;
                        n += 1;
                    }
                    return Err(near("invalid Unicode escape sequence", &b[at..end]));
                }
                self.at = i;
                add_unicode(val, &chars)
            }
            Some(b'x') => {
                let hi = b.get(at + 2).copied().filter(|&c| is_hex(c));
                let lo = b.get(at + 3).copied().filter(|&c| is_hex(c));
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        self.at = at + 4;
                        add_char(val, hex_value(h) << 4 | hex_value(l))
                    }
                    _ => {
                        let end = at + if hi.is_some() { 3 } else { 2 };
                        Err(near("invalid hexadecimal character sequence", &b[at..end]))
                    }
                }
            }
            Some(&c) if c != b'\n' => {
                val.push(match c {
                    b'b' => b'\x08',
                    b'f' => b'\x0c',
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => b'\x0b',
                    _ => c,
                });
                self.at = at + 2;
                Ok(())
            }
            _ => Err(near("unexpected end after backslash", b"\\")),
        }
    }
}

fn word_token(val: Vec<u8>, near: Option<String>) -> Token {
    Token {
        tok: keyword(&val).map_or(Tok::Ident, Tok::Kw),
        val,
        near,
    }
}

fn hex_value(c: u8) -> u32 {
    (c as char).to_digit(16).unwrap_or(0)
}

/// The character of one `\u` escape at `i`, and where it ends.
fn unicode_escape(b: &[u8], i: usize) -> Option<(u32, usize)> {
    if b.get(i) != Some(&b'\\') || b.get(i + 1) != Some(&b'u') {
        return None;
    }
    let hex_run = |from: usize, max: usize| {
        let mut n = 0;
        while n < max && b.get(from + n).is_some_and(|&c| is_hex(c)) {
            n += 1;
        }
        n
    };
    let value = |from: usize, n: usize| {
        b[from..from + n]
            .iter()
            .fold(0, |v, &c| v << 4 | hex_value(c))
    };
    if b.get(i + 2) == Some(&b'{') {
        let n = hex_run(i + 3, 6);
        if n == 0 || b.get(i + 3 + n) != Some(&b'}') {
            return None;
        }
        Some((value(i + 3, n), i + 4 + n))
    } else if hex_run(i + 2, 4) == 4 {
        Some((value(i + 2, 4), i + 6))
    } else {
        None
    }
}

/// The characters of a run of `\u` escapes, as `parseUnicode` adds them: a high surrogate then a
/// low one make one character, and a surrogate alone is refused.
fn add_unicode(val: &mut Vec<u8>, chars: &[u32]) -> Result<()> {
    let lone = || Error::new("invalid input syntax for type jsonpath");
    let mut high: Option<u32> = None;
    for &ch in chars {
        let ch = if (0xD800..=0xDBFF).contains(&ch) {
            if high.is_some() {
                return Err(lone());
            }
            high = Some(ch);
            continue;
        } else if (0xDC00..=0xDFFF).contains(&ch) {
            let Some(h) = high.take() else {
                return Err(lone());
            };
            ((h & 0x3FF) << 10) + 0x10000 + (ch & 0x3FF)
        } else if high.is_some() {
            return Err(lone());
        } else {
            ch
        };
        add_char(val, ch)?;
    }
    if high.is_some() {
        return Err(lone());
    }
    Ok(())
}

/// One character by its code point, as `addUnicodeChar` adds it in UTF-8.
fn add_char(val: &mut Vec<u8>, ch: u32) -> Result<()> {
    if ch == 0 {
        return Err(Error::new("unsupported Unicode escape sequence"));
    }
    let c = char::from_u32(ch).ok_or_else(|| Error::new("invalid Unicode code point"))?;
    let mut buf = [0u8; 4];
    val.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    Ok(())
}

// ---- The items: jsonpath_gram.y's JsonPathParseItem ----

/// The operations that take two items, in `jspOperationName`'s words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    And,
    Or,
    Equal,
    NotEqual,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    StartsWith,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::And => "&&",
            Op::Or => "||",
            Op::Equal => "==",
            Op::NotEqual => "!=",
            Op::Less => "<",
            Op::Greater => ">",
            Op::LessOrEqual => "<=",
            Op::GreaterOrEqual => ">=",
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Mod => "%",
            Op::StartsWith => "starts with",
        }
    }

    /// Its priority, as `operationPriority` gives it.
    fn priority(self) -> u8 {
        match self {
            Op::Or => 0,
            Op::And => 1,
            Op::Equal
            | Op::NotEqual
            | Op::Less
            | Op::Greater
            | Op::LessOrEqual
            | Op::GreaterOrEqual
            | Op::StartsWith => 2,
            Op::Add | Op::Sub => 3,
            Op::Mul | Op::Div | Op::Mod => 4,
        }
    }
}

/// One path item.
#[derive(Debug)]
enum Item {
    Null,
    Str(Vec<u8>),
    Num(Numeric),
    Bool(bool),
    Binary(Op, Chain, Chain),
    Not(Chain),
    IsUnknown(Chain),
    Plus(Chain),
    Minus(Chain),
    Filter(Chain),
    Exists(Chain),
    AnyArray,
    AnyKey,
    Index(Vec<(Chain, Option<Chain>)>),
    /// `**` with its first and last level, `u32::MAX` for `last`.
    Any(u32, u32),
    Key(Vec<u8>),
    Current,
    Root,
    Variable(Vec<u8>),
    Last,
    /// A method without an argument, by its name.
    Method(&'static str),
    /// A method with one argument it may go without.
    Call(&'static str, Option<Chain>),
    /// `.decimal()` with its precision and scale.
    Decimal(Option<Chain>, Option<Chain>),
    /// A method with two arguments.
    Call2(&'static str, Chain, Chain),
    LikeRegex {
        expr: Chain,
        pattern: Vec<u8>,
        flags: u32,
    },
}

/// An item and the items after it, each applied to what the one before gives.
type Chain = Vec<Item>;

impl Item {
    /// The chains it holds, taken out of it.
    fn take_chains(&mut self, into: &mut Vec<Chain>) {
        match self {
            Item::Binary(_, l, r) | Item::Call2(_, l, r) => {
                into.push(std::mem::take(l));
                into.push(std::mem::take(r));
            }
            Item::Decimal(l, r) => into.extend([l.take(), r.take()].into_iter().flatten()),
            Item::Call(_, a) => into.extend(a.take()),
            Item::Not(a)
            | Item::IsUnknown(a)
            | Item::Plus(a)
            | Item::Minus(a)
            | Item::Filter(a)
            | Item::Exists(a)
            | Item::LikeRegex { expr: a, .. } => into.push(std::mem::take(a)),
            Item::Index(elems) => {
                for (from, to) in elems.drain(..) {
                    into.push(from);
                    into.extend(to);
                }
            }
            _ => {}
        }
    }
}

impl Drop for Item {
    /// Taken apart in a loop, so a long run of operations is not dropped by deep recursion.
    fn drop(&mut self) {
        let mut chains = Vec::new();
        self.take_chains(&mut chains);
        while let Some(mut chain) = chains.pop() {
            for item in &mut chain {
                item.take_chains(&mut chains);
            }
        }
    }
}

/// How many levels deep the grammar is read before a jsonpath is refused, in PostgreSQL's words
/// for a statement too deep for its stack: each parenthesis, filter, `!`, `exists`, subscript and
/// sign takes a level or two. PostgreSQL reads deeper, to its parser's own limit.
const NEST_MAX: usize = 100;

const REGEX_ICASE: u32 = 0x01;
const REGEX_DOTALL: u32 = 0x02;
const REGEX_MLINE: u32 = 0x04;
const REGEX_WSPACE: u32 = 0x08;
const REGEX_QUOTE: u32 = 0x10;

fn method_name(k: Kw) -> Option<&'static str> {
    Some(match k {
        Kw::Abs => "abs",
        Kw::Size => "size",
        Kw::Type => "type",
        Kw::Floor => "floor",
        Kw::Double => "double",
        Kw::Ceiling => "ceiling",
        Kw::KeyValue => "keyvalue",
        Kw::Bigint => "bigint",
        Kw::Boolean => "boolean",
        Kw::Date => "date",
        Kw::Integer => "integer",
        Kw::Number => "number",
        Kw::String => "string",
        Kw::Lower => "lower",
        Kw::Upper => "upper",
        Kw::Initcap => "initcap",
        _ => return None,
    })
}

fn takes_call(k: Kw) -> bool {
    method_name(k).is_some()
        || matches!(
            k,
            Kw::Decimal
                | Kw::Datetime
                | Kw::Time
                | Kw::TimeTz
                | Kw::Timestamp
                | Kw::TimestampTz
                | Kw::Replace
                | Kw::SplitPart
                | Kw::Ltrim
                | Kw::Rtrim
                | Kw::Btrim
        )
}

/// A number token's value, as `makeItemNumeric` reads it with `numeric_in`.
fn numeric(t: &Token) -> Result<Numeric> {
    Numeric::parse(&String::from_utf8_lossy(&t.val))
}

/// `+` or `-` before an item, as `makeItemUnary` makes it: a number alone takes the sign itself.
fn unary_item(minus: bool, arg: Chain) -> Chain {
    if let [Item::Num(n)] = arg.as_slice() {
        return vec![Item::Num(if minus { n.neg() } else { n.clone() })];
    }
    vec![if minus {
        Item::Minus(arg)
    } else {
        Item::Plus(arg)
    }]
}

/// A level of `**`, as `pg_strtoint32` reads it.
fn any_level(t: &Token) -> Result<u32> {
    let text = String::from_utf8_lossy(&t.val);
    let digits: String = text.chars().filter(|&c| c != '_').collect();
    let (radix, body) = match digits.get(..2) {
        Some("0x" | "0X") => (16, &digits[2..]),
        Some("0o" | "0O") => (8, &digits[2..]),
        Some("0b" | "0B") => (2, &digits[2..]),
        _ => (10, &digits[..]),
    };
    match i32::from_str_radix(body, radix) {
        Ok(v) => Ok(v as u32),
        Err(_) => Err(Error::new(format!(
            "value \"{text}\" is out of range for type integer"
        ))),
    }
}

/// The flags of LIKE_REGEX, as `makeItemLikeRegex` reads them and `jspConvertRegexFlags` takes
/// them, and its pattern checked as `pg_regcomp` would compile it under them.
fn like_regex_flags(flags: Option<&[u8]>, pattern: &[u8]) -> Result<u32> {
    let mut bits = 0;
    for &c in flags.unwrap_or_default() {
        bits |= match c {
            b'i' => REGEX_ICASE,
            b's' => REGEX_DOTALL,
            b'm' => REGEX_MLINE,
            b'x' => REGEX_WSPACE,
            b'q' => REGEX_QUOTE,
            _ => return Err(Error::new("invalid input syntax for type jsonpath")),
        };
    }
    if bits & REGEX_QUOTE == 0 && bits & REGEX_WSPACE != 0 {
        return Err(Error::new(
            "XQuery \"x\" flag (expanded regular expressions) is not implemented",
        ));
    }
    regex::check(pattern, bits & REGEX_QUOTE != 0)?;
    Ok(bits)
}

// ---- The grammar: jsonpath_gram.y ----

/// What an item is to the grammar: an expression gives values, a predicate gives a truth.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Expr,
    Pred,
}

/// Where an item is read: where the grammar takes an expression only, or either.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Any,
    Expr,
}

/// The grammar read by descent, refusing at the first token no jsonpath can go on with, as the
/// parser bison makes of jsonpath_gram.y refuses; each token scanned only when the grammar needs
/// it, so a refusal the scanner or a rule makes comes in the parser's order.
struct Parser<'a> {
    lex: Lexer<'a>,
    ahead: Option<Token>,
    /// How many levels deep the reading is.
    depth: usize,
}

impl Parser<'_> {
    fn peek(&mut self) -> Result<Tok> {
        if self.ahead.is_none() {
            self.ahead = Some(self.lex.next()?);
        }
        Ok(self.ahead.as_ref().map_or(Tok::End, |t| t.tok))
    }

    fn bump(&mut self) -> Result<Token> {
        self.peek()?;
        Ok(self.ahead.take().expect("a token ahead"))
    }

    /// The refusal at the token ahead.
    fn fail<T>(&mut self) -> Result<T> {
        self.peek()?;
        Err(syntax(self.ahead.as_ref().expect("a token ahead")))
    }

    fn eat(&mut self, t: Tok) -> Result<bool> {
        if self.peek()? == t {
            self.bump()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, t: Tok) -> Result<Token> {
        if self.peek()? == t {
            self.bump()
        } else {
            self.fail()
        }
    }

    /// `result`: the mode, then an expression or a predicate; none for an input with no token.
    fn path(&mut self) -> Result<Option<(bool, Chain)>> {
        let lax = match self.peek()? {
            Tok::End => return Ok(None),
            Tok::Kw(Kw::Strict) => {
                self.bump()?;
                false
            }
            Tok::Kw(Kw::Lax) => {
                self.bump()?;
                true
            }
            _ => true,
        };
        let (path, _) = self.binary(0, Ctx::Any)?;
        if self.peek()? != Tok::End {
            return self.fail();
        }
        Ok(Some((lax, path)))
    }

    /// A predicate where the grammar takes only one.
    fn predicate(&mut self) -> Result<Chain> {
        let (p, kind) = self.binary(0, Ctx::Any)?;
        if kind == Kind::Expr {
            return self.fail();
        }
        Ok(p)
    }

    /// An expression where the grammar takes only one.
    fn expr(&mut self) -> Result<Chain> {
        Ok(self.binary(Op::Add.priority(), Ctx::Expr)?.0)
    }

    /// One level deeper into the grammar, refused past `NEST_MAX` levels.
    fn nest(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > NEST_MAX {
            return Err(Error::new("stack depth limit exceeded"));
        }
        Ok(())
    }

    /// The operations of priority `min` and above, each run of one priority read from the left,
    /// as the grammar's precedences bind them: `||`, `&&`, the comparisons (`starts with` and
    /// `like_regex` among them), `+ -`, `* / %`. `||` and `&&` join predicates, a comparison
    /// makes a predicate of two expressions, and the others join expressions.
    fn binary(&mut self, min: u8, ctx: Ctx) -> Result<(Chain, Kind)> {
        self.nest()?;
        let (mut left, mut kind) = match (self.peek()?, ctx) {
            (Tok::Not, Ctx::Any) => (vec![self.not()?], Kind::Pred),
            (Tok::Kw(Kw::Exists), Ctx::Any) => (vec![self.exists()?], Kind::Pred),
            _ => self.unary(ctx)?,
        };
        loop {
            let op = match self.peek()? {
                Tok::Or => Op::Or,
                Tok::And => Op::And,
                Tok::Equal => Op::Equal,
                Tok::NotEqual => Op::NotEqual,
                Tok::Less => Op::Less,
                Tok::Greater => Op::Greater,
                Tok::LessEqual => Op::LessOrEqual,
                Tok::GreaterEqual => Op::GreaterOrEqual,
                Tok::Kw(Kw::Starts) => Op::StartsWith,
                // like_regex binds as the comparisons do, and is told apart below
                Tok::Kw(Kw::LikeRegex) => Op::Equal,
                Tok::Char(b'+') => Op::Add,
                Tok::Char(b'-') => Op::Sub,
                Tok::Char(b'*') => Op::Mul,
                Tok::Char(b'/') => Op::Div,
                Tok::Char(b'%') => Op::Mod,
                _ => break,
            };
            if op.priority() < min {
                break;
            }
            let joins_predicates = matches!(op, Op::Or | Op::And);
            if (kind == Kind::Pred) != joins_predicates {
                return self.fail();
            }
            let t = self.bump()?;
            let (item, k) = match op {
                Op::Or | Op::And => {
                    let (right, k) = self.binary(op.priority() + 1, Ctx::Any)?;
                    if k == Kind::Expr {
                        return self.fail();
                    }
                    (Item::Binary(op, left, right), Kind::Pred)
                }
                _ if t.tok == Tok::Kw(Kw::LikeRegex) => (self.like_regex(left)?, Kind::Pred),
                Op::StartsWith => {
                    self.expect(Tok::Kw(Kw::With))?;
                    let right = match self.peek()? {
                        Tok::Str => Item::Str(self.bump()?.val),
                        Tok::Variable => Item::Variable(self.bump()?.val),
                        _ => return self.fail(),
                    };
                    (Item::Binary(op, left, vec![right]), Kind::Pred)
                }
                Op::Mul | Op::Div | Op::Mod => {
                    let right = self.unary(Ctx::Expr)?.0;
                    (Item::Binary(op, left, right), Kind::Expr)
                }
                Op::Add | Op::Sub => {
                    let right = self.binary(Op::Mul.priority(), Ctx::Expr)?.0;
                    (Item::Binary(op, left, right), Kind::Expr)
                }
                _ => {
                    let right = self.expr()?;
                    (Item::Binary(op, left, right), Kind::Pred)
                }
            };
            left = vec![item];
            kind = k;
        }
        self.depth -= 1;
        Ok((left, kind))
    }

    /// `!` and the predicate in parentheses or the `exists` after it.
    fn not(&mut self) -> Result<Item> {
        self.bump()?;
        let p = match self.peek()? {
            Tok::Char(b'(') => {
                self.bump()?;
                let p = self.predicate()?;
                self.expect(Tok::Char(b')'))?;
                p
            }
            Tok::Kw(Kw::Exists) => vec![self.exists()?],
            _ => return self.fail(),
        };
        Ok(Item::Not(p))
    }

    fn exists(&mut self) -> Result<Item> {
        self.bump()?;
        self.expect(Tok::Char(b'('))?;
        let e = self.expr()?;
        self.expect(Tok::Char(b')'))?;
        Ok(Item::Exists(e))
    }

    /// `like_regex` after its expression: the pattern and its flags, checked as soon as they
    /// are read.
    fn like_regex(&mut self, expr: Chain) -> Result<Item> {
        let pattern = self.expect(Tok::Str)?.val;
        let flags = if self.eat(Tok::Kw(Kw::Flag))? {
            Some(self.expect(Tok::Str)?.val)
        } else {
            None
        };
        let flags = like_regex_flags(flags.as_deref(), &pattern)?;
        Ok(Item::LikeRegex {
            expr,
            pattern,
            flags,
        })
    }

    fn unary(&mut self, ctx: Ctx) -> Result<(Chain, Kind)> {
        let minus = match self.peek()? {
            Tok::Char(b'+') => false,
            Tok::Char(b'-') => true,
            _ => return self.postfix(ctx),
        };
        self.bump()?;
        self.nest()?;
        let arg = self.unary(Ctx::Expr)?.0;
        self.depth -= 1;
        Ok((unary_item(minus, arg), Kind::Expr))
    }

    /// `path_primary` or a parenthesised expression or predicate, and the accessors after it.
    fn postfix(&mut self, ctx: Ctx) -> Result<(Chain, Kind)> {
        let first = match self.peek()? {
            Tok::Str => Item::Str(self.bump()?.val),
            Tok::Kw(Kw::Null) => {
                self.bump()?;
                Item::Null
            }
            Tok::Kw(Kw::True) => {
                self.bump()?;
                Item::Bool(true)
            }
            Tok::Kw(Kw::False) => {
                self.bump()?;
                Item::Bool(false)
            }
            Tok::Numeric | Tok::Int => Item::Num(numeric(&self.bump()?)?),
            Tok::Variable => Item::Variable(self.bump()?.val),
            Tok::Char(b'$') => {
                self.bump()?;
                Item::Root
            }
            Tok::Char(b'@') => {
                self.bump()?;
                Item::Current
            }
            Tok::Kw(Kw::Last) => {
                self.bump()?;
                Item::Last
            }
            Tok::Char(b'(') => {
                self.bump()?;
                let (mut inner, kind) = self.binary(0, Ctx::Any)?;
                self.expect(Tok::Char(b')'))?;
                if self.at_accessor()? {
                    self.accessors(&mut inner)?;
                    return Ok((inner, Kind::Expr));
                }
                return match (kind, ctx) {
                    (Kind::Expr, _) => Ok((inner, Kind::Expr)),
                    (Kind::Pred, Ctx::Any) => {
                        if self.eat(Tok::Kw(Kw::Is))? {
                            self.expect(Tok::Kw(Kw::Unknown))?;
                            Ok((vec![Item::IsUnknown(inner)], Kind::Pred))
                        } else {
                            Ok((inner, Kind::Pred))
                        }
                    }
                    (Kind::Pred, Ctx::Expr) => self.fail(),
                };
            }
            _ => return self.fail(),
        };
        let mut chain = vec![first];
        self.accessors(&mut chain)?;
        Ok((chain, Kind::Expr))
    }

    fn at_accessor(&mut self) -> Result<bool> {
        Ok(matches!(self.peek()?, Tok::Char(b'.' | b'[' | b'?')))
    }

    fn accessors(&mut self, chain: &mut Chain) -> Result<()> {
        while self.at_accessor()? {
            let item = match self.bump()?.tok {
                Tok::Char(b'.') => self.after_dot()?,
                Tok::Char(b'[') => self.subscripts()?,
                _ => {
                    self.expect(Tok::Char(b'('))?;
                    let p = self.predicate()?;
                    self.expect(Tok::Char(b')'))?;
                    Item::Filter(p)
                }
            };
            chain.push(item);
        }
        Ok(())
    }

    /// What follows a `.`: a key, `*`, `**` with its levels, or a method with its arguments.
    fn after_dot(&mut self) -> Result<Item> {
        match self.peek()? {
            Tok::Char(b'*') => {
                self.bump()?;
                Ok(Item::AnyKey)
            }
            Tok::AnyPath => {
                self.bump()?;
                self.any_path()
            }
            Tok::Ident | Tok::Str => Ok(Item::Key(self.bump()?.val)),
            Tok::Kw(k) => {
                let word = self.bump()?;
                if takes_call(k) && self.peek()? == Tok::Char(b'(') {
                    self.bump()?;
                    return self.call(k);
                }
                Ok(Item::Key(word.val))
            }
            _ => self.fail(),
        }
    }

    /// A method's arguments after its `(`, and its `)`.
    fn call(&mut self, k: Kw) -> Result<Item> {
        let close = Tok::Char(b')');
        if let Some(name) = method_name(k) {
            self.expect(close)?;
            return Ok(Item::Method(name));
        }
        let item = match k {
            Kw::Decimal => {
                let mut args = Vec::new();
                if self.peek()? != close {
                    loop {
                        args.push(self.int_elem()?);
                        if !self.eat(Tok::Char(b','))? {
                            break;
                        }
                    }
                }
                self.expect(close)?;
                if args.len() > 2 {
                    return Err(Error::new("invalid input syntax for type jsonpath"));
                }
                let mut args = args.into_iter();
                return Ok(Item::Decimal(args.next(), args.next()));
            }
            Kw::Datetime | Kw::Ltrim | Kw::Rtrim | Kw::Btrim => {
                let arg = if self.peek()? == Tok::Str {
                    Some(vec![Item::Str(self.bump()?.val)])
                } else {
                    None
                };
                let name = match k {
                    Kw::Datetime => "datetime",
                    Kw::Ltrim => "ltrim",
                    Kw::Rtrim => "rtrim",
                    _ => "btrim",
                };
                Item::Call(name, arg)
            }
            Kw::Time | Kw::TimeTz | Kw::Timestamp | Kw::TimestampTz => {
                let arg = if self.peek()? == Tok::Int {
                    Some(vec![Item::Num(numeric(&self.bump()?)?)])
                } else {
                    None
                };
                let name = match k {
                    Kw::Time => "time",
                    Kw::TimeTz => "time_tz",
                    Kw::Timestamp => "timestamp",
                    _ => "timestamp_tz",
                };
                Item::Call(name, arg)
            }
            _ => {
                let first = vec![Item::Str(self.expect(Tok::Str)?.val)];
                self.expect(Tok::Char(b','))?;
                if k == Kw::Replace {
                    let second = vec![Item::Str(self.expect(Tok::Str)?.val)];
                    Item::Call2("replace", first, second)
                } else {
                    Item::Call2("split_part", first, self.int_elem()?)
                }
            }
        };
        self.expect(close)?;
        Ok(item)
    }

    /// `int_elem`: an integer, `+` or `-` before it taken into it.
    fn int_elem(&mut self) -> Result<Chain> {
        let minus = match self.peek()? {
            Tok::Char(b'+') => Some(false),
            Tok::Char(b'-') => Some(true),
            _ => None,
        };
        if minus.is_some() {
            self.bump()?;
        }
        if self.peek()? != Tok::Int {
            return self.fail();
        }
        let n = numeric(&self.bump()?)?;
        Ok(vec![Item::Num(if minus == Some(true) {
            n.neg()
        } else {
            n
        })])
    }

    /// What follows a `[`: `*`, or subscripts and ranges, and the `]`.
    fn subscripts(&mut self) -> Result<Item> {
        if self.eat(Tok::Char(b'*'))? {
            self.expect(Tok::Char(b']'))?;
            return Ok(Item::AnyArray);
        }
        let mut elems = Vec::new();
        loop {
            let from = self.expr()?;
            let to = if self.eat(Tok::Kw(Kw::To))? {
                Some(self.expr()?)
            } else {
                None
            };
            elems.push((from, to));
            if !self.eat(Tok::Char(b','))? {
                break;
            }
        }
        self.expect(Tok::Char(b']'))?;
        Ok(Item::Index(elems))
    }

    /// What follows `**`: nothing, or its levels in braces.
    fn any_path(&mut self) -> Result<Item> {
        if !self.eat(Tok::Char(b'{'))? {
            return Ok(Item::Any(0, u32::MAX));
        }
        let first = self.level()?;
        let last = if self.eat(Tok::Kw(Kw::To))? {
            self.level()?
        } else {
            first
        };
        self.expect(Tok::Char(b'}'))?;
        Ok(Item::Any(first, last))
    }

    fn level(&mut self) -> Result<u32> {
        match self.peek()? {
            Tok::Int => any_level(&self.bump()?),
            Tok::Kw(Kw::Last) => {
                self.bump()?;
                Ok(u32::MAX)
            }
            _ => self.fail(),
        }
    }
}

// ---- The checks of flattenJsonPathParseItem ----

/// The checks `flattenJsonPathParseItem` makes, item by item in its order: `@` only inside a
/// filter, `last` only inside an array's subscripts.
fn check(chain: &[Item], nesting: u32, subscript: bool) -> Result<()> {
    for item in chain {
        match item {
            Item::Current if nesting == 0 => {
                return Err(Error::new("@ is not allowed in root expressions"));
            }
            Item::Last if !subscript => {
                return Err(Error::new("LAST is allowed only in array subscripts"));
            }
            Item::Binary(..) => {
                let (bottom, run) = left_run(item);
                check(bottom, nesting, subscript)?;
                for (_, r, _) in run.iter().rev() {
                    check(r, nesting, subscript)?;
                }
            }
            Item::Call2(_, l, r) => {
                check(l, nesting, subscript)?;
                check(r, nesting, subscript)?;
            }
            Item::Decimal(l, r) => {
                for a in [l, r].into_iter().flatten() {
                    check(a, nesting, subscript)?;
                }
            }
            Item::Filter(a) => check(a, nesting + 1, subscript)?,
            Item::Not(a)
            | Item::IsUnknown(a)
            | Item::Plus(a)
            | Item::Minus(a)
            | Item::Exists(a)
            | Item::Call(_, Some(a))
            | Item::LikeRegex { expr: a, .. } => check(a, nesting, subscript)?,
            Item::Index(elems) => {
                for (from, to) in elems {
                    check(from, nesting, true)?;
                    if let Some(to) = to {
                        check(to, nesting, true)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The operations down the left of `item`, an operation, from `item` down: each with its right
/// operand and whether its left operand is written in parentheses; and the operand they end at.
/// Walked in a loop, so a long run of one operation (`a || b || c ...`) needs no deep recursion.
fn left_run(item: &Item) -> (&[Item], Vec<(Op, &[Item], bool)>) {
    let mut run = Vec::new();
    let mut at = item;
    loop {
        let Item::Binary(op, l, r) = at else {
            unreachable!("a run is of operations");
        };
        run.push((*op, r.as_slice(), priority(l) <= op.priority()));
        match l.as_slice() {
            [next @ Item::Binary(..)] => at = next,
            _ => return (l, run),
        }
    }
}

// ---- The writer: printJsonPathItem ----

/// An item's priority, as `operationPriority` gives it.
fn priority(chain: &[Item]) -> u8 {
    match chain.first() {
        Some(Item::Binary(op, _, _)) => op.priority(),
        Some(Item::Plus(_) | Item::Minus(_)) => 5,
        _ => 6,
    }
}

/// A chain as `printJsonPathItem` writes it: its first item, in parentheses where `brackets`
/// asks for them, then each item after it.
fn print(out: &mut Vec<u8>, chain: &[Item], brackets: bool) {
    for (i, item) in chain.iter().enumerate() {
        let has_next = i + 1 < chain.len();
        if i == 0 {
            print_item(out, item, has_next, false, brackets);
        } else {
            print_item(out, item, has_next, true, true);
        }
    }
}

fn print_item(out: &mut Vec<u8>, item: &Item, has_next: bool, in_key: bool, brackets: bool) {
    let open = |out: &mut Vec<u8>| {
        if brackets {
            out.push(b'(');
        }
    };
    let close = |out: &mut Vec<u8>| {
        if brackets {
            out.push(b')');
        }
    };
    match item {
        Item::Null => out.extend_from_slice(b"null"),
        Item::Str(s) => escape_json(out, s),
        Item::Num(n) => {
            if has_next {
                out.push(b'(');
            }
            out.extend_from_slice(n.to_string().as_bytes());
            if has_next {
                out.push(b')');
            }
        }
        Item::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Item::Binary(..) => {
            // The operations down the left, each in parentheses where the one above it asks for
            // them, written from the innermost operand out.
            let (bottom, run) = left_run(item);
            let parens: Vec<bool> = std::iter::once(brackets)
                .chain(run.iter().map(|&(_, _, p)| p))
                .collect();
            for &p in &parens[..run.len()] {
                if p {
                    out.push(b'(');
                }
            }
            print(out, bottom, parens[run.len()]);
            for (i, &(op, r, _)) in run.iter().enumerate().rev() {
                out.push(b' ');
                out.extend_from_slice(op.name().as_bytes());
                out.push(b' ');
                print(out, r, priority(r) <= op.priority());
                if parens[i] {
                    out.push(b')');
                }
            }
        }
        Item::Not(a) => {
            out.extend_from_slice(b"!(");
            print(out, a, false);
            out.push(b')');
        }
        Item::IsUnknown(a) => {
            out.push(b'(');
            print(out, a, false);
            out.extend_from_slice(b") is unknown");
        }
        Item::Plus(a) | Item::Minus(a) => {
            open(out);
            out.push(if matches!(item, Item::Plus(_)) {
                b'+'
            } else {
                b'-'
            });
            print(out, a, priority(a) <= 5);
            close(out);
        }
        Item::AnyArray => out.extend_from_slice(b"[*]"),
        Item::AnyKey => {
            if in_key {
                out.push(b'.');
            }
            out.push(b'*');
        }
        Item::Index(elems) => {
            out.push(b'[');
            for (i, (from, to)) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                print(out, from, false);
                if let Some(to) = to {
                    out.extend_from_slice(b" to ");
                    print(out, to, false);
                }
            }
            out.push(b']');
        }
        Item::Any(first, last) => {
            if in_key {
                out.push(b'.');
            }
            let level = |v: u32| {
                if v == u32::MAX {
                    "last".to_string()
                } else {
                    v.to_string()
                }
            };
            let s = if *first == 0 && *last == u32::MAX {
                "**".to_string()
            } else if first == last {
                format!("**{{{}}}", level(*first))
            } else {
                format!("**{{{} to {}}}", level(*first), level(*last))
            };
            out.extend_from_slice(s.as_bytes());
        }
        Item::Key(k) => {
            if in_key {
                out.push(b'.');
            }
            escape_json(out, k);
        }
        Item::Current => out.push(b'@'),
        Item::Root => out.push(b'$'),
        Item::Variable(v) => {
            out.push(b'$');
            escape_json(out, v);
        }
        Item::Filter(a) => {
            out.extend_from_slice(b"?(");
            print(out, a, false);
            out.push(b')');
        }
        Item::Exists(a) => {
            out.extend_from_slice(b"exists (");
            print(out, a, false);
            out.push(b')');
        }
        Item::Last => out.extend_from_slice(b"last"),
        Item::Method(name) => {
            out.push(b'.');
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b"()");
        }
        Item::Call(name, arg) => {
            out.push(b'.');
            out.extend_from_slice(name.as_bytes());
            out.push(b'(');
            if let Some(a) = arg {
                print(out, a, false);
            }
            out.push(b')');
        }
        Item::Decimal(l, r) => {
            out.extend_from_slice(b".decimal(");
            if let Some(l) = l {
                print(out, l, false);
            }
            if let Some(r) = r {
                out.push(b',');
                print(out, r, false);
            }
            out.push(b')');
        }
        Item::Call2(name, l, r) => {
            out.push(b'.');
            out.extend_from_slice(name.as_bytes());
            out.push(b'(');
            print(out, l, false);
            out.push(b',');
            print(out, r, false);
            out.push(b')');
        }
        Item::LikeRegex {
            expr,
            pattern,
            flags,
        } => {
            open(out);
            print(out, expr, priority(expr) <= 6);
            out.extend_from_slice(b" like_regex ");
            escape_json(out, pattern);
            if *flags != 0 {
                out.extend_from_slice(b" flag \"");
                for (bit, c) in [
                    (REGEX_ICASE, b'i'),
                    (REGEX_DOTALL, b's'),
                    (REGEX_MLINE, b'm'),
                    (REGEX_WSPACE, b'x'),
                    (REGEX_QUOTE, b'q'),
                ] {
                    if flags & bit != 0 {
                        out.push(c);
                    }
                }
                out.push(b'"');
            }
            close(out);
        }
    }
}

/// A string as `escape_json_with_len` writes it: in double quotes, the six control characters
/// JSON names by a letter, `"` and `\` escaped, every other control character as `\u` and four
/// hex digits.
fn escape_json(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ if c < b' ' => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// The checks `pg_regcomp` makes of a pattern as it reads it: its scanner (regc_lex.c) and its
/// parser (regcomp.c's `parse`, `parseqatom`, `bracket`, `brackpart`, and regc_locale.c's names),
/// refusing with `pg_regerror`'s words. What it refuses only while it builds the automaton, a
/// pattern too big for it, is not checked here.
mod regex {
    use crate::{Error, Result};

    const EXTENDED: u32 = 0o1;
    const ADVF: u32 = 0o2;
    const ADVANCED: u32 = 0o3;
    const QUOTE: u32 = 0o4;
    const ICASE: u32 = 0o10;
    const EXPANDED: u32 = 0o40;
    const NLSTOP: u32 = 0o100;
    const NLANCH: u32 = 0o200;
    const NEWLINE: u32 = 0o300;

    /// The most a bound counts, `DUPMAX`; one more stands for no bound.
    const DUPMAX: u32 = 255;
    const DUPINF: u32 = DUPMAX + 1;

    /// The largest character the scanner takes from an escape, `CHR_MAX`.
    const CHR_MAX: u32 = 0x7fff_fffe;

    /// The refusals, with `pg_regerror`'s words.
    #[derive(Clone, Copy, Debug)]
    enum Code {
        BadPat,
        ECollate,
        ECtype,
        EEscape,
        ESubreg,
        EBrack,
        EParen,
        EBrace,
        BadBr,
        ERange,
        BadRpt,
        Assert,
        BadOpt,
    }

    impl Code {
        fn message(self) -> &'static str {
            match self {
                Code::BadPat => "invalid regexp (reg version 0.8)",
                Code::ECollate => "invalid collating element",
                Code::ECtype => "invalid character class",
                Code::EEscape => "invalid escape \\ sequence",
                Code::ESubreg => "invalid backreference number",
                Code::EBrack => "brackets [] not balanced",
                Code::EParen => "parentheses () not balanced",
                Code::EBrace => "braces {} not balanced",
                Code::BadBr => "invalid repetition count(s)",
                Code::ERange => "invalid character range",
                Code::BadRpt => "quantifier operand invalid",
                Code::Assert => "\"cannot happen\" -- you found a bug",
                Code::BadOpt => "invalid embedded option",
            }
        }
    }

    type R<T> = std::result::Result<T, Code>;

    /// A LIKE_REGEX pattern checked as `pg_regcomp` reads it: an advanced expression, or under
    /// the `q` flag a literal string.
    pub(super) fn check(pattern: &[u8], quote: bool) -> Result<()> {
        let s: Vec<u32> = String::from_utf8_lossy(pattern)
            .chars()
            .map(u32::from)
            .collect();
        let cflags = if quote { QUOTE } else { ADVANCED };
        let mut v = Vars {
            s,
            now: 0,
            cflags,
            lexcon: Con::Q,
            nexttype: T::Empty,
            nextvalue: 0,
            lasttype: T::Empty,
            nsubexp: 0,
            closed: Vec::new(),
        };
        v.compile()
            .map_err(|c| Error::new(format!("invalid regular expression: {}", c.message())))
    }

    /// The scanner's contexts.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Con {
        Ere,
        Bre,
        Q,
        Ebnd,
        Bbnd,
        Brack,
        Cel,
        Ecl,
        Ccl,
    }

    /// The scanner's tokens.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum T {
        Empty,
        Eos,
        Plain,
        Digit,
        Backref,
        Collel,
        Eclass,
        Cclass,
        End,
        Range,
        Lacon,
        Wbdry,
        Nwbdry,
        Sbegin,
        Send,
        Cclasss,
        Cclassc,
        /// An operator, by its character.
        Op(u8),
    }

    /// The scanner's and parser's state, `struct vars`.
    struct Vars {
        s: Vec<u32>,
        now: usize,
        cflags: u32,
        lexcon: Con,
        nexttype: T,
        nextvalue: u32,
        lasttype: T,
        nsubexp: u32,
        /// Which capturing groups are closed, by number.
        closed: Vec<bool>,
    }

    fn ch(c: u8) -> u32 {
        c as u32
    }

    fn is_digit(c: u32) -> bool {
        (ch(b'0')..=ch(b'9')).contains(&c)
    }

    fn is_alpha(c: u32) -> bool {
        char::from_u32(c).is_some_and(char::is_alphabetic)
    }

    fn is_space(c: u32) -> bool {
        char::from_u32(c).is_some_and(char::is_whitespace)
    }

    /// The ASCII names of characters `element` knows, regc_locale.c's `cnames`.
    const CNAMES: [(&str, u8); 95] = [
        ("NUL", 0),
        ("SOH", 1),
        ("STX", 2),
        ("ETX", 3),
        ("EOT", 4),
        ("ENQ", 5),
        ("ACK", 6),
        ("BEL", 7),
        ("alert", 7),
        ("BS", 8),
        ("backspace", 8),
        ("HT", 9),
        ("tab", 9),
        ("LF", 10),
        ("newline", 10),
        ("VT", 11),
        ("vertical-tab", 11),
        ("FF", 12),
        ("form-feed", 12),
        ("CR", 13),
        ("carriage-return", 13),
        ("SO", 14),
        ("SI", 15),
        ("DLE", 16),
        ("DC1", 17),
        ("DC2", 18),
        ("DC3", 19),
        ("DC4", 20),
        ("NAK", 21),
        ("SYN", 22),
        ("ETB", 23),
        ("CAN", 24),
        ("EM", 25),
        ("SUB", 26),
        ("ESC", 27),
        ("IS4", 28),
        ("FS", 28),
        ("IS3", 29),
        ("GS", 29),
        ("IS2", 30),
        ("RS", 30),
        ("IS1", 31),
        ("US", 31),
        ("space", b' '),
        ("exclamation-mark", b'!'),
        ("quotation-mark", b'"'),
        ("number-sign", b'#'),
        ("dollar-sign", b'$'),
        ("percent-sign", b'%'),
        ("ampersand", b'&'),
        ("apostrophe", b'\''),
        ("left-parenthesis", b'('),
        ("right-parenthesis", b')'),
        ("asterisk", b'*'),
        ("plus-sign", b'+'),
        ("comma", b','),
        ("hyphen", b'-'),
        ("hyphen-minus", b'-'),
        ("period", b'.'),
        ("full-stop", b'.'),
        ("slash", b'/'),
        ("solidus", b'/'),
        ("zero", b'0'),
        ("one", b'1'),
        ("two", b'2'),
        ("three", b'3'),
        ("four", b'4'),
        ("five", b'5'),
        ("six", b'6'),
        ("seven", b'7'),
        ("eight", b'8'),
        ("nine", b'9'),
        ("colon", b':'),
        ("semicolon", b';'),
        ("less-than-sign", b'<'),
        ("equals-sign", b'='),
        ("greater-than-sign", b'>'),
        ("question-mark", b'?'),
        ("commercial-at", b'@'),
        ("left-square-bracket", b'['),
        ("backslash", b'\\'),
        ("reverse-solidus", b'\\'),
        ("right-square-bracket", b']'),
        ("circumflex", b'^'),
        ("circumflex-accent", b'^'),
        ("underscore", b'_'),
        ("low-line", b'_'),
        ("grave-accent", b'`'),
        ("left-brace", b'{'),
        ("left-curly-bracket", b'{'),
        ("vertical-line", b'|'),
        ("right-brace", b'}'),
        ("right-curly-bracket", b'}'),
        ("tilde", b'~'),
        ("DEL", 127),
    ];

    /// The names of the character classes, regc_locale.c's `classNames`.
    const CLASSES: [&str; 14] = [
        "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
        "space", "upper", "xdigit", "word",
    ];

    fn same_name(name: &str, s: &[u32]) -> bool {
        name.len() == s.len() && name.bytes().zip(s).all(|(a, &b)| ch(a) == b)
    }

    impl Vars {
        fn ateos(&self) -> bool {
            self.now >= self.s.len()
        }

        fn at(&self, i: usize) -> Option<u32> {
            self.s.get(self.now + i).copied()
        }

        fn next1(&self, c: u8) -> bool {
            self.at(0) == Some(ch(c))
        }

        fn see(&self, t: T) -> bool {
            self.nexttype == t
        }

        fn eat(&mut self, t: T) -> R<bool> {
            if self.see(t) {
                self.next()?;
                Ok(true)
            } else {
                Ok(false)
            }
        }

        fn ret(&mut self, t: T, value: u32) -> R<()> {
            self.nexttype = t;
            self.nextvalue = value;
            Ok(())
        }

        /// `pg_regcomp`'s reading: `lexstart`, then `parse` to the end.
        fn compile(&mut self) -> R<()> {
            self.prefixes()?;
            self.lexcon = if self.cflags & QUOTE != 0 {
                Con::Q
            } else if self.cflags & EXTENDED != 0 {
                Con::Ere
            } else {
                Con::Bre
            };
            self.nexttype = T::Empty;
            self.next()?;
            self.parse(T::Eos, false)
        }

        /// `prefixes`: a leading `***` director and an ARE's embedded options.
        fn prefixes(&mut self) -> R<()> {
            if self.cflags & QUOTE != 0 {
                return Ok(());
            }
            let star = Some(ch(b'*'));
            if self.s.len() >= 4 && self.at(0) == star && self.at(1) == star && self.at(2) == star {
                match self.at(3).and_then(char::from_u32) {
                    Some('?') => return Err(Code::BadPat),
                    Some('=') => {
                        self.cflags |= QUOTE;
                        self.cflags &= !(ADVANCED | EXPANDED | NEWLINE);
                        self.now += 4;
                        return Ok(());
                    }
                    Some(':') => {
                        self.cflags |= ADVANCED;
                        self.now += 4;
                    }
                    _ => return Err(Code::BadRpt),
                }
            }
            if self.cflags & ADVANCED != ADVANCED {
                return Ok(());
            }
            if self.s.len() >= self.now + 3
                && self.next1(b'(')
                && self.at(1) == Some(ch(b'?'))
                && self.at(2).is_some_and(is_alpha)
            {
                self.now += 2;
                while let Some(c) = self.at(0).filter(|&c| is_alpha(c)) {
                    match char::from_u32(c) {
                        Some('b') => self.cflags &= !(ADVANCED | QUOTE),
                        Some('c') => self.cflags &= !ICASE,
                        Some('e') => {
                            self.cflags |= EXTENDED;
                            self.cflags &= !(ADVF | QUOTE);
                        }
                        Some('i') => self.cflags |= ICASE,
                        Some('m' | 'n') => self.cflags |= NEWLINE,
                        Some('p') => {
                            self.cflags |= NLSTOP;
                            self.cflags &= !NLANCH;
                        }
                        Some('q') => {
                            self.cflags |= QUOTE;
                            self.cflags &= !ADVANCED;
                        }
                        Some('s') => self.cflags &= !NEWLINE,
                        Some('t') => self.cflags &= !EXPANDED,
                        Some('w') => {
                            self.cflags &= !NLSTOP;
                            self.cflags |= NLANCH;
                        }
                        Some('x') => self.cflags |= EXPANDED,
                        _ => return Err(Code::BadOpt),
                    }
                    self.now += 1;
                }
                if !self.next1(b')') {
                    return Err(Code::BadOpt);
                }
                self.now += 1;
                if self.cflags & QUOTE != 0 {
                    self.cflags &= !(EXPANDED | NEWLINE);
                }
            }
            Ok(())
        }

        /// `next`: the next token.
        fn next(&mut self) -> R<()> {
            loop {
                self.lasttype = self.nexttype;
                if self.cflags & EXPANDED != 0
                    && matches!(self.lexcon, Con::Ere | Con::Bre | Con::Ebnd | Con::Bbnd)
                {
                    self.skip();
                }
                if self.ateos() {
                    return match self.lexcon {
                        Con::Ere | Con::Bre | Con::Q => self.ret(T::Eos, 0),
                        Con::Ebnd | Con::Bbnd => Err(Code::EBrace),
                        _ => Err(Code::EBrack),
                    };
                }
                let c = self.s[self.now];
                self.now += 1;
                let cc = char::from_u32(c).unwrap_or('\0');
                match self.lexcon {
                    Con::Bre => return self.brenext(c),
                    Con::Ere => {}
                    Con::Q => return self.ret(T::Plain, c),
                    Con::Bbnd | Con::Ebnd => {
                        return match cc {
                            '0'..='9' => self.ret(T::Digit, c - ch(b'0')),
                            ',' => self.ret(T::Op(b','), 0),
                            '}' if self.lexcon == Con::Ebnd => {
                                self.lexcon = Con::Ere;
                                if self.cflags & ADVF != 0 && self.next1(b'?') {
                                    self.now += 1;
                                    return self.ret(T::Op(b'}'), 0);
                                }
                                self.ret(T::Op(b'}'), 1)
                            }
                            '\\' if self.lexcon == Con::Bbnd && self.next1(b'}') => {
                                self.now += 1;
                                self.lexcon = Con::Bre;
                                self.ret(T::Op(b'}'), 1)
                            }
                            _ => Err(Code::BadBr),
                        };
                    }
                    Con::Brack => {
                        return match cc {
                            ']' if self.lasttype == T::Op(b'[') => self.ret(T::Plain, c),
                            ']' => {
                                self.lexcon = if self.cflags & EXTENDED != 0 {
                                    Con::Ere
                                } else {
                                    Con::Bre
                                };
                                self.ret(T::Op(b']'), 0)
                            }
                            '\\' => {
                                if self.cflags & ADVF == 0 {
                                    return self.ret(T::Plain, c);
                                }
                                if self.ateos() {
                                    return Err(Code::EEscape);
                                }
                                self.lexescape()?;
                                match self.nexttype {
                                    T::Plain | T::Cclasss | T::Cclassc => Ok(()),
                                    _ => Err(Code::EEscape),
                                }
                            }
                            '-' => {
                                if self.lasttype == T::Op(b'[') || self.next1(b']') {
                                    self.ret(T::Plain, c)
                                } else {
                                    self.ret(T::Range, c)
                                }
                            }
                            '[' => {
                                let Some(d) = self.at(0) else {
                                    return Err(Code::EBrack);
                                };
                                self.now += 1;
                                match char::from_u32(d) {
                                    Some('.') => {
                                        self.lexcon = Con::Cel;
                                        self.ret(T::Collel, 0)
                                    }
                                    Some('=') => {
                                        self.lexcon = Con::Ecl;
                                        self.ret(T::Eclass, 0)
                                    }
                                    Some(':') => {
                                        self.lexcon = Con::Ccl;
                                        self.ret(T::Cclass, 0)
                                    }
                                    _ => {
                                        self.now -= 1;
                                        self.ret(T::Plain, c)
                                    }
                                }
                            }
                            _ => self.ret(T::Plain, c),
                        };
                    }
                    Con::Cel | Con::Ecl | Con::Ccl => {
                        let end = match self.lexcon {
                            Con::Cel => b'.',
                            Con::Ecl => b'=',
                            _ => b':',
                        };
                        if c == ch(end) && self.next1(b']') {
                            self.now += 1;
                            self.lexcon = Con::Brack;
                            return self.ret(T::End, c);
                        }
                        return self.ret(T::Plain, c);
                    }
                }
                // An ERE's or an ARE's operators; a backslash below.
                match cc {
                    '|' => return self.ret(T::Op(b'|'), 0),
                    '*' | '+' | '?' => {
                        if self.cflags & ADVF != 0 && self.next1(b'?') {
                            self.now += 1;
                            return self.ret(T::Op(cc as u8), 0);
                        }
                        return self.ret(T::Op(cc as u8), 1);
                    }
                    '{' => {
                        if self.cflags & EXPANDED != 0 {
                            self.skip();
                        }
                        if !self.at(0).is_some_and(is_digit) {
                            return self.ret(T::Plain, c);
                        }
                        self.lexcon = Con::Ebnd;
                        return self.ret(T::Op(b'{'), 0);
                    }
                    '(' => {
                        if self.cflags & ADVF != 0 && self.next1(b'?') {
                            self.now += 1;
                            let Some(d) = self.at(0) else {
                                return Err(Code::BadRpt);
                            };
                            self.now += 1;
                            match char::from_u32(d) {
                                Some(':') => return self.ret(T::Op(b'('), 0),
                                Some('#') => {
                                    while self.at(0).is_some_and(|c| c != ch(b')')) {
                                        self.now += 1;
                                    }
                                    if !self.ateos() {
                                        self.now += 1;
                                    }
                                    continue;
                                }
                                Some('=' | '!') => return self.ret(T::Lacon, 0),
                                Some('<') => {
                                    let Some(e) = self.at(0) else {
                                        return Err(Code::BadRpt);
                                    };
                                    self.now += 1;
                                    if e == ch(b'=') || e == ch(b'!') {
                                        return self.ret(T::Lacon, 0);
                                    }
                                    return Err(Code::BadRpt);
                                }
                                _ => return Err(Code::BadRpt),
                            }
                        }
                        return self.ret(T::Op(b'('), 1);
                    }
                    ')' => return self.ret(T::Op(b')'), c),
                    '[' => return self.open_bracket(),
                    '.' | '^' | '$' => return self.ret(T::Op(cc as u8), 0),
                    '\\' => {
                        if self.ateos() {
                            return Err(Code::EEscape);
                        }
                    }
                    _ => return self.ret(T::Plain, c),
                }
                if self.cflags & ADVF == 0 {
                    let d = self.s[self.now];
                    self.now += 1;
                    return self.ret(T::Plain, d);
                }
                return self.lexescape();
            }
        }

        /// A `[` read: `[[:<:]]` and `[[:>:]]`, or a bracket expression's start.
        fn open_bracket(&mut self) -> R<()> {
            let word_end = |v: &Self| {
                let at = |i| v.at(i).and_then(char::from_u32);
                if at(0) == Some('[')
                    && at(1) == Some(':')
                    && matches!(at(2), Some('<' | '>'))
                    && at(3) == Some(':')
                    && at(4) == Some(']')
                    && at(5) == Some(']')
                {
                    at(2)
                } else {
                    None
                }
            };
            if let Some(c) = word_end(self) {
                self.now += 6;
                return self.ret(T::Op(c as u8), 0);
            }
            self.lexcon = Con::Brack;
            if self.next1(b'^') {
                self.now += 1;
                return self.ret(T::Op(b'['), 0);
            }
            self.ret(T::Op(b'['), 1)
        }

        /// `lexescape`: an ARE's escape after its backslash.
        fn lexescape(&mut self) -> R<()> {
            let c = self.s[self.now];
            self.now += 1;
            let Some(cc) = char::from_u32(c).filter(char::is_ascii_alphanumeric) else {
                return self.ret(T::Plain, c);
            };
            let plain = |v: &mut Self, x: u8| v.ret(T::Plain, ch(x));
            match cc {
                'a' => plain(self, 7),
                'A' => self.ret(T::Sbegin, 0),
                'b' => plain(self, 8),
                'B' => plain(self, b'\\'),
                'c' => {
                    let Some(d) = self.at(0) else {
                        return Err(Code::EEscape);
                    };
                    self.now += 1;
                    self.ret(T::Plain, d & 0o37)
                }
                'd' | 's' | 'w' => self.ret(T::Cclasss, 0),
                'D' | 'S' | 'W' => self.ret(T::Cclassc, 0),
                'e' => plain(self, 27),
                'f' => plain(self, 12),
                'm' => self.ret(T::Op(b'<'), 0),
                'M' => self.ret(T::Op(b'>'), 0),
                'n' => plain(self, 10),
                'r' => plain(self, 13),
                't' => plain(self, 9),
                'v' => plain(self, 11),
                'u' | 'U' | 'x' => {
                    let (min, max) = match cc {
                        'u' => (4, 4),
                        'U' => (8, 8),
                        _ => (1, 255),
                    };
                    let v = self.lexdigits(16, min, max)?;
                    if v > CHR_MAX {
                        return Err(Code::EEscape);
                    }
                    self.ret(T::Plain, v)
                }
                'y' => self.ret(T::Wbdry, 0),
                'Y' => self.ret(T::Nwbdry, 0),
                'Z' => self.ret(T::Send, 0),
                '1'..='9' => {
                    let save = self.now;
                    self.now -= 1;
                    let n = self.lexdigits(10, 1, 255)?;
                    if self.now == save || (n > 0 && n <= self.nsubexp) {
                        return self.ret(T::Backref, n);
                    }
                    self.now = save;
                    self.octal()
                }
                '0' => self.octal(),
                _ => Err(Code::EEscape),
            }
        }

        /// An octal escape from the digit before `now`.
        fn octal(&mut self) -> R<()> {
            self.now -= 1;
            let mut n = self.lexdigits(8, 1, 3)?;
            if n > 0xff {
                self.now -= 1;
                n >>= 3;
            }
            self.ret(T::Plain, n)
        }

        /// `lexdigits`: from `min` to `max` digits in `base`, their value wrapping as an unsigned
        /// 32 bits.
        fn lexdigits(&mut self, base: u32, min: usize, max: usize) -> R<u32> {
            let mut n: u32 = 0;
            let mut len = 0;
            while len < max {
                let Some(d) = self
                    .at(0)
                    .and_then(char::from_u32)
                    .and_then(|c| c.to_digit(16))
                else {
                    break;
                };
                if d >= base {
                    break;
                }
                self.now += 1;
                n = n.wrapping_mul(base).wrapping_add(d);
                len += 1;
            }
            if len < min {
                return Err(Code::EEscape);
            }
            Ok(n)
        }

        /// `brenext`: a BRE's next token.
        fn brenext(&mut self, c: u32) -> R<()> {
            match char::from_u32(c) {
                Some('*') => {
                    if matches!(self.lasttype, T::Empty | T::Op(b'(') | T::Op(b'^')) {
                        return self.ret(T::Plain, c);
                    }
                    return self.ret(T::Op(b'*'), 1);
                }
                Some('[') => return self.open_bracket(),
                Some('.') => return self.ret(T::Op(b'.'), 0),
                Some('^') => {
                    if matches!(self.lasttype, T::Empty | T::Op(b'(')) {
                        return self.ret(T::Op(b'^'), 0);
                    }
                    return self.ret(T::Plain, c);
                }
                Some('$') => {
                    if self.cflags & EXPANDED != 0 {
                        self.skip();
                    }
                    if self.ateos() || (self.next1(b'\\') && self.at(1) == Some(ch(b')'))) {
                        return self.ret(T::Op(b'$'), 0);
                    }
                    return self.ret(T::Plain, c);
                }
                Some('\\') => {}
                _ => return self.ret(T::Plain, c),
            }
            let Some(d) = self.at(0) else {
                return Err(Code::EEscape);
            };
            self.now += 1;
            match char::from_u32(d) {
                Some('{') => {
                    self.lexcon = Con::Bbnd;
                    self.ret(T::Op(b'{'), 0)
                }
                Some('(') => self.ret(T::Op(b'('), 1),
                Some(')') => self.ret(T::Op(b')'), d),
                Some('<') => self.ret(T::Op(b'<'), 0),
                Some('>') => self.ret(T::Op(b'>'), 0),
                Some('1'..='9') => self.ret(T::Backref, d - ch(b'0')),
                _ => self.ret(T::Plain, d),
            }
        }

        /// `skip`: white space and `#` comments in an expanded expression.
        fn skip(&mut self) {
            loop {
                while self.at(0).is_some_and(is_space) {
                    self.now += 1;
                }
                if !self.next1(b'#') {
                    break;
                }
                while self.at(0).is_some_and(|c| c != ch(b'\n')) {
                    self.now += 1;
                }
            }
        }

        /// `parse`: branches joined by `|`, up to `stopper`.
        fn parse(&mut self, stopper: T, lacon: bool) -> R<()> {
            loop {
                while !self.see(T::Op(b'|')) && !self.see(stopper) && !self.see(T::Eos) {
                    self.parseqatom(lacon)?;
                }
                if !self.eat(T::Op(b'|'))? {
                    break;
                }
            }
            if !self.see(stopper) {
                return Err(Code::EParen);
            }
            Ok(())
        }

        /// `parseqatom`: an atom or a constraint, and the quantifier after an atom.
        fn parseqatom(&mut self, lacon: bool) -> R<()> {
            match self.nexttype {
                T::Op(b'^' | b'$' | b'<' | b'>') | T::Sbegin | T::Send | T::Wbdry | T::Nwbdry => {
                    return self.next();
                }
                T::Lacon => {
                    self.next()?;
                    self.parse(T::Op(b')'), true)?;
                    return self.next();
                }
                T::Op(b'*' | b'+' | b'?' | b'{') => return Err(Code::BadRpt),
                T::Op(b')') => {
                    if self.cflags & ADVANCED != EXTENDED {
                        return Err(Code::EParen);
                    }
                    self.next()?;
                }
                T::Plain | T::Cclasss | T::Cclassc | T::Op(b'.') => self.next()?,
                T::Op(b'[') => {
                    self.next()?;
                    while !self.see(T::Op(b']')) && !self.see(T::Eos) {
                        self.brackpart()?;
                    }
                    self.next()?;
                }
                T::Op(b'(') => {
                    let group = (!lacon && self.nextvalue != 0).then(|| {
                        self.nsubexp += 1;
                        self.nsubexp as usize
                    });
                    self.next()?;
                    self.parse(T::Op(b')'), lacon)?;
                    self.next()?;
                    if let Some(n) = group {
                        if self.closed.len() <= n {
                            self.closed.resize(n + 1, false);
                        }
                        self.closed[n] = true;
                    }
                }
                T::Backref => {
                    let n = self.nextvalue as usize;
                    if lacon || !self.closed.get(n).copied().unwrap_or(false) {
                        return Err(Code::ESubreg);
                    }
                    self.next()?;
                }
                _ => return Err(Code::Assert),
            }
            match self.nexttype {
                T::Op(b'*' | b'+' | b'?') => self.next()?,
                T::Op(b'{') => {
                    self.next()?;
                    let m = self.scannum()?;
                    if self.eat(T::Op(b','))? {
                        let n = if self.see(T::Digit) {
                            self.scannum()?
                        } else {
                            DUPINF
                        };
                        if m > n {
                            return Err(Code::BadBr);
                        }
                    }
                    if !self.see(T::Op(b'}')) {
                        return Err(Code::BadBr);
                    }
                    self.next()?;
                }
                _ => {}
            }
            Ok(())
        }

        /// `scannum`: a bound's count.
        fn scannum(&mut self) -> R<u32> {
            let mut n = 0;
            while self.see(T::Digit) && n < DUPMAX {
                n = n * 10 + self.nextvalue;
                self.next()?;
            }
            if self.see(T::Digit) || n > DUPMAX {
                return Err(Code::BadBr);
            }
            Ok(n)
        }

        /// `brackpart`: one item or range of a bracket expression.
        fn brackpart(&mut self) -> R<()> {
            let start = match self.nexttype {
                T::Range => return Err(Code::ERange),
                T::Plain => {
                    let c = self.nextvalue;
                    self.next()?;
                    if !self.see(T::Range) {
                        return Ok(());
                    }
                    c
                }
                T::Collel => {
                    let name = self.scanplain(Code::ECollate)?;
                    element(&name)?
                }
                T::Eclass => {
                    let name = self.scanplain(Code::ECollate)?;
                    element(&name)?;
                    return Ok(());
                }
                T::Cclass => {
                    let name = self.scanplain(Code::ECtype)?;
                    if !CLASSES.iter().any(|c| same_name(c, &name)) {
                        return Err(Code::ECtype);
                    }
                    return Ok(());
                }
                T::Cclasss | T::Cclassc => return self.next(),
                _ => return Err(Code::Assert),
            };
            let end = if self.see(T::Range) {
                self.next()?;
                match self.nexttype {
                    T::Plain | T::Range => {
                        let c = self.nextvalue;
                        self.next()?;
                        c
                    }
                    T::Collel => {
                        let name = self.scanplain(Code::ECollate)?;
                        element(&name)?
                    }
                    _ => return Err(Code::ERange),
                }
            } else {
                start
            };
            if start != end && start > end {
                return Err(Code::ERange);
            }
            Ok(())
        }

        /// `scanplain`: the name inside `[.`, `[=` or `[:`, refused with `empty` where it has
        /// none.
        fn scanplain(&mut self, empty: Code) -> R<Vec<u32>> {
            let start = self.now;
            self.next()?;
            let mut end = self.now;
            while self.see(T::Plain) {
                end = self.now;
                self.next()?;
            }
            self.next()?;
            if start >= end {
                return Err(empty);
            }
            Ok(self.s[start..end].to_vec())
        }
    }

    /// `element`: a collating element's character by its name.
    fn element(name: &[u32]) -> R<u32> {
        if let [c] = name {
            return Ok(*c);
        }
        CNAMES
            .iter()
            .find(|(n, _)| same_name(n, name))
            .map(|&(_, c)| ch(c))
            .ok_or(Code::ECollate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An input, and what PostgreSQL 19 gave for it: the text `jsonpath_out` wrote of what
    /// `jsonpath_in` read, or the refusal.
    type Case = (
        &'static str,
        std::result::Result<&'static str, &'static str>,
    );

    fn reads_as_postgresql(cases: &[Case]) {
        for &(input, want) in cases {
            let got = jsonpath_in(input).map_err(|e| e.to_string());
            assert_eq!(got.as_deref().map_err(String::as_str), want, "{input:?}");
        }
    }

    /// Every jsonpath literal of PostgreSQL's regression file jsonpath.sql.
    const REGRESSION: &[Case] = &[
        ("", Err("invalid input syntax for type jsonpath: \"\"")),
        ("$", Ok("$")),
        ("strict $", Ok("strict $")),
        ("lax $", Ok("$")),
        ("$.a", Ok("$.\"a\"")),
        ("$.a.v", Ok("$.\"a\".\"v\"")),
        ("$.a.*", Ok("$.\"a\".*")),
        ("$.*[*]", Ok("$.*[*]")),
        ("$.a[*]", Ok("$.\"a\"[*]")),
        ("$.a[*][*]", Ok("$.\"a\"[*][*]")),
        ("$[*]", Ok("$[*]")),
        ("$[0]", Ok("$[0]")),
        ("$[*][0]", Ok("$[*][0]")),
        ("$[*].a", Ok("$[*].\"a\"")),
        ("$[*][0].a.b", Ok("$[*][0].\"a\".\"b\"")),
        ("$.a.**.b", Ok("$.\"a\".**.\"b\"")),
        ("$.a.**{2}.b", Ok("$.\"a\".**{2}.\"b\"")),
        ("$.a.**{2 to 2}.b", Ok("$.\"a\".**{2}.\"b\"")),
        ("$.a.**{2 to 5}.b", Ok("$.\"a\".**{2 to 5}.\"b\"")),
        ("$.a.**{0 to 5}.b", Ok("$.\"a\".**{0 to 5}.\"b\"")),
        ("$.a.**{5 to last}.b", Ok("$.\"a\".**{5 to last}.\"b\"")),
        ("$.a.**{last}.b", Ok("$.\"a\".**{last}.\"b\"")),
        ("$.a.**{last to 5}.b", Ok("$.\"a\".**{last to 5}.\"b\"")),
        ("$+1", Ok("($ + 1)")),
        ("$-1", Ok("($ - 1)")),
        ("$--+1", Ok("($ - -1)")),
        ("$.a/+-1", Ok("($.\"a\" / -1)")),
        ("1 * 2 + 4 % -3 != false", Ok("(1 * 2 + 4 % -3 != false)")),
        (
            "\"\\b\\f\\r\\n\\t\\v\\\"\\'\\\\\"",
            Ok("\"\\b\\f\\r\\n\\t\\u000b\\\"'\\\\\""),
        ),
        (
            "\"\\x50\\u0067\\u{53}\\u{051}\\u{00004C}\"",
            Ok("\"PgSQL\""),
        ),
        (
            "$.foo\\x50\\u0067\\u{53}\\u{051}\\u{00004C}\\t\\\"bar",
            Ok("$.\"fooPgSQL\\t\\\"bar\""),
        ),
        ("\"\\z\"", Ok("\"z\"")),
        ("$.g ? ($.a == 1)", Ok("$.\"g\"?($.\"a\" == 1)")),
        ("$.g ? (@ == 1)", Ok("$.\"g\"?(@ == 1)")),
        ("$.g ? (@.a == 1)", Ok("$.\"g\"?(@.\"a\" == 1)")),
        (
            "$.g ? (@.a == 1 || @.a == 4)",
            Ok("$.\"g\"?(@.\"a\" == 1 || @.\"a\" == 4)"),
        ),
        (
            "$.g ? (@.a == 1 && @.a == 4)",
            Ok("$.\"g\"?(@.\"a\" == 1 && @.\"a\" == 4)"),
        ),
        (
            "$.g ? (@.a == 1 || @.a == 4 && @.b == 7)",
            Ok("$.\"g\"?(@.\"a\" == 1 || @.\"a\" == 4 && @.\"b\" == 7)"),
        ),
        (
            "$.g ? (@.a == 1 || !(@.a == 4) && @.b == 7)",
            Ok("$.\"g\"?(@.\"a\" == 1 || !(@.\"a\" == 4) && @.\"b\" == 7)"),
        ),
        (
            "$.g ? (@.a == 1 || !(@.x >= 123 || @.a == 4) && @.b == 7)",
            Ok("$.\"g\"?(@.\"a\" == 1 || !(@.\"x\" >= 123 || @.\"a\" == 4) && @.\"b\" == 7)"),
        ),
        (
            "$.g ? (@.x >= @[*]?(@.a > \"abc\"))",
            Ok("$.\"g\"?(@.\"x\" >= @[*]?(@.\"a\" > \"abc\"))"),
        ),
        (
            "$.g ? ((@.x >= 123 || @.a == 4) is unknown)",
            Ok("$.\"g\"?((@.\"x\" >= 123 || @.\"a\" == 4) is unknown)"),
        ),
        ("$.g ? (exists (@.x))", Ok("$.\"g\"?(exists (@.\"x\"))")),
        (
            "$.g ? (exists (@.x ? (@ == 14)))",
            Ok("$.\"g\"?(exists (@.\"x\"?(@ == 14)))"),
        ),
        (
            "$.g ? ((@.x >= 123 || @.a == 4) && exists (@.x ? (@ == 14)))",
            Ok("$.\"g\"?((@.\"x\" >= 123 || @.\"a\" == 4) && exists (@.\"x\"?(@ == 14)))"),
        ),
        (
            "$.g ? (+@.x >= +-(+@.a + 2))",
            Ok("$.\"g\"?(+@.\"x\" >= +(-(+@.\"a\" + 2)))"),
        ),
        ("$a", Ok("$\"a\"")),
        ("$a.b", Ok("$\"a\".\"b\"")),
        ("$a[*]", Ok("$\"a\"[*]")),
        (
            "$.g ? (@.zip == $zip)",
            Ok("$.\"g\"?(@.\"zip\" == $\"zip\")"),
        ),
        ("$.a[1,2, 3 to 16]", Ok("$.\"a\"[1,2,3 to 16]")),
        (
            "$.a[$a + 1, ($b[*]) to -($[0] * 2)]",
            Ok("$.\"a\"[$\"a\" + 1,$\"b\"[*] to -($[0] * 2)]"),
        ),
        ("$.a[$.a.size() - 3]", Ok("$.\"a\"[$.\"a\".size() - 3]")),
        ("last", Err("LAST is allowed only in array subscripts")),
        ("\"last\"", Ok("\"last\"")),
        ("$.last", Ok("$.\"last\"")),
        (
            "$ ? (last > 0)",
            Err("LAST is allowed only in array subscripts"),
        ),
        ("$[last]", Ok("$[last]")),
        ("$[$[0] ? (last > 0)]", Ok("$[$[0]?(last > 0)]")),
        ("null.type()", Ok("null.type()")),
        (
            "1.type()",
            Err("trailing junk after numeric literal at or near \"1.t\" of jsonpath input"),
        ),
        ("(1).type()", Ok("(1).type()")),
        ("1.2.type()", Ok("(1.2).type()")),
        ("\"aaa\".type()", Ok("\"aaa\".type()")),
        ("true.type()", Ok("true.type()")),
        (
            "$.double().floor().ceiling().abs()",
            Ok("$.double().floor().ceiling().abs()"),
        ),
        ("$.keyvalue().key", Ok("$.keyvalue().\"key\"")),
        ("$.datetime()", Ok("$.datetime()")),
        (
            "$.datetime(\"datetime template\")",
            Ok("$.datetime(\"datetime template\")"),
        ),
        (
            "$.bigint().integer().number().decimal()",
            Ok("$.bigint().integer().number().decimal()"),
        ),
        ("$.boolean()", Ok("$.boolean()")),
        ("$.date()", Ok("$.date()")),
        ("$.decimal(4,2)", Ok("$.decimal(4,2)")),
        ("$.string()", Ok("$.string()")),
        (
            "$.replace(\"hello\",\"bye\")",
            Ok("$.replace(\"hello\",\"bye\")"),
        ),
        ("$.lower()", Ok("$.lower()")),
        ("$.upper()", Ok("$.upper()")),
        (
            "$.lower().upper().lower().replace(\"hello\",\"bye\")",
            Ok("$.lower().upper().lower().replace(\"hello\",\"bye\")"),
        ),
        ("$.ltrim()", Ok("$.ltrim()")),
        ("$.ltrim(\"xyz\")", Ok("$.ltrim(\"xyz\")")),
        ("$.rtrim()", Ok("$.rtrim()")),
        ("$.rtrim(\"xyz\")", Ok("$.rtrim(\"xyz\")")),
        ("$.btrim()", Ok("$.btrim()")),
        ("$.btrim(\"xyz\")", Ok("$.btrim(\"xyz\")")),
        ("$.initcap()", Ok("$.initcap()")),
        ("$.split_part(\"~@~\", 2)", Ok("$.split_part(\"~@~\",2)")),
        (
            "$.replace(\"hello\")",
            Err("syntax error at or near \")\" of jsonpath input"),
        ),
        (
            "$.replace()",
            Err("syntax error at or near \")\" of jsonpath input"),
        ),
        (
            "$.replace(\"hello\",\"bye\",\"extra\")",
            Err("syntax error at or near \",\" of jsonpath input"),
        ),
        (
            "$.split_part(\"~@~\")",
            Err("syntax error at or near \")\" of jsonpath input"),
        ),
        (
            "$.split_part()",
            Err("syntax error at or near \")\" of jsonpath input"),
        ),
        (
            "$.split_part(\"~@~\", \"hi\")",
            Err("syntax error at or near \"\"\" of jsonpath input"),
        ),
        (
            "$.split_part(\"~@~\", 2, \"extra\")",
            Err("syntax error at or near \",\" of jsonpath input"),
        ),
        (
            "$.lower(\"hi\")",
            Err("syntax error at or near \"\"\" of jsonpath input"),
        ),
        (
            "$.upper(\"hi\")",
            Err("syntax error at or near \"\"\" of jsonpath input"),
        ),
        (
            "$.initcap(\"hi\")",
            Err("syntax error at or near \"\"\" of jsonpath input"),
        ),
        (
            "$.ltrim(42)",
            Err("syntax error at or near \"42\" of jsonpath input"),
        ),
        (
            "$.ltrim(\"x\", \"y\")",
            Err("syntax error at or near \",\" of jsonpath input"),
        ),
        (
            "$.rtrim(42)",
            Err("syntax error at or near \"42\" of jsonpath input"),
        ),
        (
            "$.rtrim(\"x\", \"y\")",
            Err("syntax error at or near \",\" of jsonpath input"),
        ),
        (
            "$.trim(42)",
            Err("syntax error at or near \"(\" of jsonpath input"),
        ),
        (
            "$.trim(\"x\", \"y\")",
            Err("syntax error at or near \"(\" of jsonpath input"),
        ),
        ("$.lower", Ok("$.\"lower\"")),
        ("$.upper", Ok("$.\"upper\"")),
        ("$.initcap", Ok("$.\"initcap\"")),
        ("$.replace", Ok("$.\"replace\"")),
        ("$.split_part", Ok("$.\"split_part\"")),
        ("$.ltrim", Ok("$.\"ltrim\"")),
        ("$.rtrim", Ok("$.\"rtrim\"")),
        ("$.btrim", Ok("$.\"btrim\"")),
        ("$.time()", Ok("$.time()")),
        ("$.time(6)", Ok("$.time(6)")),
        ("$.time_tz()", Ok("$.time_tz()")),
        ("$.time_tz(4)", Ok("$.time_tz(4)")),
        ("$.timestamp()", Ok("$.timestamp()")),
        ("$.timestamp(2)", Ok("$.timestamp(2)")),
        ("$.timestamp_tz()", Ok("$.timestamp_tz()")),
        ("$.timestamp_tz(0)", Ok("$.timestamp_tz(0)")),
        (
            "$ ? (@ starts with \"abc\")",
            Ok("$?(@ starts with \"abc\")"),
        ),
        ("$ ? (@ starts with $var)", Ok("$?(@ starts with $\"var\")")),
        (
            "$ ? (@ like_regex \"(invalid pattern\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        (
            "$ ? (@ like_regex \"pattern\")",
            Ok("$?(@ like_regex \"pattern\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"\")",
            Ok("$?(@ like_regex \"pattern\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"i\")",
            Ok("$?(@ like_regex \"pattern\" flag \"i\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"is\")",
            Ok("$?(@ like_regex \"pattern\" flag \"is\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"isim\")",
            Ok("$?(@ like_regex \"pattern\" flag \"ism\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"xsms\")",
            Err("XQuery \"x\" flag (expanded regular expressions) is not implemented"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"q\")",
            Ok("$?(@ like_regex \"pattern\" flag \"q\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"iq\")",
            Ok("$?(@ like_regex \"pattern\" flag \"iq\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"smixq\")",
            Ok("$?(@ like_regex \"pattern\" flag \"ismxq\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"a\")",
            Err("invalid input syntax for type jsonpath"),
        ),
        ("$ < 1", Ok("($ < 1)")),
        (
            "($ < 1) || $.a.b <= $x",
            Ok("($ < 1 || $.\"a\".\"b\" <= $\"x\")"),
        ),
        ("@ + 1", Err("@ is not allowed in root expressions")),
        ("($).a.b", Ok("$.\"a\".\"b\"")),
        ("($.a.b).c.d", Ok("$.\"a\".\"b\".\"c\".\"d\"")),
        (
            "($.a.b + -$.x.y).c.d",
            Ok("($.\"a\".\"b\" + -$.\"x\".\"y\").\"c\".\"d\""),
        ),
        ("(-+$.a.b).c.d", Ok("(-(+$.\"a\".\"b\")).\"c\".\"d\"")),
        (
            "1 + ($.a.b + 2).c.d",
            Ok("(1 + ($.\"a\".\"b\" + 2).\"c\".\"d\")"),
        ),
        (
            "1 + ($.a.b > 2).c.d",
            Ok("(1 + ($.\"a\".\"b\" > 2).\"c\".\"d\")"),
        ),
        ("($)", Ok("$")),
        ("(($))", Ok("$")),
        (
            "((($ + 1)).a + ((2)).b ? ((((@ > 1)) || (exists(@.c)))))",
            Ok("(($ + 1).\"a\" + (2).\"b\"?(@ > 1 || exists (@.\"c\")))"),
        ),
        ("$ ? (@.a < 1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < -1)", Ok("$?(@.\"a\" < -1)")),
        ("$ ? (@.a < +1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < .1)", Ok("$?(@.\"a\" < 0.1)")),
        ("$ ? (@.a < -.1)", Ok("$?(@.\"a\" < -0.1)")),
        ("$ ? (@.a < +.1)", Ok("$?(@.\"a\" < 0.1)")),
        ("$ ? (@.a < 0.1)", Ok("$?(@.\"a\" < 0.1)")),
        ("$ ? (@.a < -0.1)", Ok("$?(@.\"a\" < -0.1)")),
        ("$ ? (@.a < +0.1)", Ok("$?(@.\"a\" < 0.1)")),
        ("$ ? (@.a < 10.1)", Ok("$?(@.\"a\" < 10.1)")),
        ("$ ? (@.a < -10.1)", Ok("$?(@.\"a\" < -10.1)")),
        ("$ ? (@.a < +10.1)", Ok("$?(@.\"a\" < 10.1)")),
        ("$ ? (@.a < 1e1)", Ok("$?(@.\"a\" < 10)")),
        ("$ ? (@.a < -1e1)", Ok("$?(@.\"a\" < -10)")),
        ("$ ? (@.a < +1e1)", Ok("$?(@.\"a\" < 10)")),
        ("$ ? (@.a < .1e1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < -.1e1)", Ok("$?(@.\"a\" < -1)")),
        ("$ ? (@.a < +.1e1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < 0.1e1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < -0.1e1)", Ok("$?(@.\"a\" < -1)")),
        ("$ ? (@.a < +0.1e1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < 10.1e1)", Ok("$?(@.\"a\" < 101)")),
        ("$ ? (@.a < -10.1e1)", Ok("$?(@.\"a\" < -101)")),
        ("$ ? (@.a < +10.1e1)", Ok("$?(@.\"a\" < 101)")),
        ("$ ? (@.a < 1e-1)", Ok("$?(@.\"a\" < 0.1)")),
        ("$ ? (@.a < -1e-1)", Ok("$?(@.\"a\" < -0.1)")),
        ("$ ? (@.a < +1e-1)", Ok("$?(@.\"a\" < 0.1)")),
        ("$ ? (@.a < .1e-1)", Ok("$?(@.\"a\" < 0.01)")),
        ("$ ? (@.a < -.1e-1)", Ok("$?(@.\"a\" < -0.01)")),
        ("$ ? (@.a < +.1e-1)", Ok("$?(@.\"a\" < 0.01)")),
        ("$ ? (@.a < 0.1e-1)", Ok("$?(@.\"a\" < 0.01)")),
        ("$ ? (@.a < -0.1e-1)", Ok("$?(@.\"a\" < -0.01)")),
        ("$ ? (@.a < +0.1e-1)", Ok("$?(@.\"a\" < 0.01)")),
        ("$ ? (@.a < 10.1e-1)", Ok("$?(@.\"a\" < 1.01)")),
        ("$ ? (@.a < -10.1e-1)", Ok("$?(@.\"a\" < -1.01)")),
        ("$ ? (@.a < +10.1e-1)", Ok("$?(@.\"a\" < 1.01)")),
        ("$ ? (@.a < 1e+1)", Ok("$?(@.\"a\" < 10)")),
        ("$ ? (@.a < -1e+1)", Ok("$?(@.\"a\" < -10)")),
        ("$ ? (@.a < +1e+1)", Ok("$?(@.\"a\" < 10)")),
        ("$ ? (@.a < .1e+1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < -.1e+1)", Ok("$?(@.\"a\" < -1)")),
        ("$ ? (@.a < +.1e+1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < 0.1e+1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < -0.1e+1)", Ok("$?(@.\"a\" < -1)")),
        ("$ ? (@.a < +0.1e+1)", Ok("$?(@.\"a\" < 1)")),
        ("$ ? (@.a < 10.1e+1)", Ok("$?(@.\"a\" < 101)")),
        ("$ ? (@.a < -10.1e+1)", Ok("$?(@.\"a\" < -101)")),
        ("$ ? (@.a < +10.1e+1)", Ok("$?(@.\"a\" < 101)")),
        ("0", Ok("0")),
        (
            "00",
            Err("trailing junk after numeric literal at or near \"00\" of jsonpath input"),
        ),
        ("0755", Err("syntax error at end of jsonpath input")),
        ("0.0", Ok("0.0")),
        ("0.000", Ok("0.000")),
        ("0.000e1", Ok("0.00")),
        ("0.000e2", Ok("0.0")),
        ("0.000e3", Ok("0")),
        ("0.0010", Ok("0.0010")),
        ("0.0010e-1", Ok("0.00010")),
        ("0.0010e+1", Ok("0.010")),
        ("0.0010e+2", Ok("0.10")),
        (".001", Ok("0.001")),
        (".001e1", Ok("0.01")),
        ("1.", Ok("1")),
        ("1.e1", Ok("10")),
        (
            "1a",
            Err("trailing junk after numeric literal at or near \"1a\" of jsonpath input"),
        ),
        (
            "1e",
            Err("trailing junk after numeric literal at or near \"1e\" of jsonpath input"),
        ),
        (
            "1.e",
            Err("trailing junk after numeric literal at or near \"1.e\" of jsonpath input"),
        ),
        (
            "1.2a",
            Err("trailing junk after numeric literal at or near \"1.2a\" of jsonpath input"),
        ),
        (
            "1.2e",
            Err("trailing junk after numeric literal at or near \"1.2e\" of jsonpath input"),
        ),
        ("1.2.e", Ok("(1.2).\"e\"")),
        ("(1.2).e", Ok("(1.2).\"e\"")),
        ("1e3", Ok("1000")),
        ("1.e3", Ok("1000")),
        ("1.e3.e", Ok("(1000).\"e\"")),
        ("1.e3.e4", Ok("(1000).\"e4\"")),
        ("1.2e3", Ok("1200")),
        (
            "1.2e3a",
            Err("trailing junk after numeric literal at or near \"1.2e3a\" of jsonpath input"),
        ),
        ("1.2.e3", Ok("(1.2).\"e3\"")),
        ("(1.2).e3", Ok("(1.2).\"e3\"")),
        ("1..e", Ok("(1).\"e\"")),
        ("1..e3", Ok("(1).\"e3\"")),
        ("(1.).e", Ok("(1).\"e\"")),
        ("(1.).e3", Ok("(1).\"e3\"")),
        ("1?(2>3)", Ok("(1)?(2 > 3)")),
        ("0b100101", Ok("37")),
        ("0o273", Ok("187")),
        ("0x42F", Ok("1071")),
        (
            "0b",
            Err("trailing junk after numeric literal at or near \"0b\" of jsonpath input"),
        ),
        (
            "1b",
            Err("trailing junk after numeric literal at or near \"1b\" of jsonpath input"),
        ),
        ("0b0x", Err("syntax error at end of jsonpath input")),
        (
            "0o",
            Err("trailing junk after numeric literal at or near \"0o\" of jsonpath input"),
        ),
        (
            "1o",
            Err("trailing junk after numeric literal at or near \"1o\" of jsonpath input"),
        ),
        ("0o0x", Err("syntax error at end of jsonpath input")),
        (
            "0x",
            Err("trailing junk after numeric literal at or near \"0x\" of jsonpath input"),
        ),
        (
            "1x",
            Err("trailing junk after numeric literal at or near \"1x\" of jsonpath input"),
        ),
        ("0x0y", Err("syntax error at end of jsonpath input")),
        ("1_000_000", Ok("1000000")),
        ("1_2_3", Ok("123")),
        ("0x1EEE_FFFF", Ok("518979583")),
        ("0o2_73", Ok("187")),
        ("0b10_0101", Ok("37")),
        ("1_000.000_005", Ok("1000.000005")),
        ("1_000.", Ok("1000")),
        (".000_005", Ok("0.000005")),
        ("1_000.5e0_1", Ok("10005")),
        ("_100", Err("syntax error at end of jsonpath input")),
        (
            "100_",
            Err("trailing junk after numeric literal at or near \"100_\" of jsonpath input"),
        ),
        ("100__000", Err("syntax error at end of jsonpath input")),
        ("_1_000.5", Err("syntax error at end of jsonpath input")),
        (
            "1_000_.5",
            Err("trailing junk after numeric literal at or near \"1_000_\" of jsonpath input"),
        ),
        (
            "1_000._5",
            Err("trailing junk after numeric literal at or near \"1_000._\" of jsonpath input"),
        ),
        (
            "1_000.5_",
            Err("trailing junk after numeric literal at or near \"1_000.5_\" of jsonpath input"),
        ),
        (
            "1_000.5e_1",
            Err("trailing junk after numeric literal at or near \"1_000.5e\" of jsonpath input"),
        ),
        ("0b_10_0101", Err("syntax error at end of jsonpath input")),
        ("0o_273", Err("syntax error at end of jsonpath input")),
        ("0x_42F", Err("syntax error at end of jsonpath input")),
        (
            "$ ? (@ like_regex \"pattern\" flag \"smixq\")",
            Ok("$?(@ like_regex \"pattern\" flag \"ismxq\")"),
        ),
        (
            "$ ? (@ like_regex \"pattern\" flag \"a\")",
            Err("invalid input syntax for type jsonpath"),
        ),
        ("@ + 1", Err("@ is not allowed in root expressions")),
        (
            "00",
            Err("trailing junk after numeric literal at or near \"00\" of jsonpath input"),
        ),
        (
            "1a",
            Err("trailing junk after numeric literal at or near \"1a\" of jsonpath input"),
        ),
    ];

    /// The values of the type table, types.jsonpath.
    const TYPE_TABLE: &[Case] = &[
        ("$.\"a\"", Ok("$.\"a\"")),
        ("$", Ok("$")),
        ("strict $.\"a\"[*]?(@ > 1)", Ok("strict $.\"a\"[*]?(@ > 1)")),
        ("($.\"a\" + 1.50)", Ok("($.\"a\" + 1.50)")),
        ("$.datetime()", Ok("$.datetime()")),
        ("$.\"a\".integer()", Ok("$.\"a\".integer()")),
        ("$.a", Ok("$.\"a\"")),
        ("$ . a", Ok("$.\"a\"")),
        ("lax $.a", Ok("$.\"a\"")),
        ("strict $.a[*] ? (@ > 1)", Ok("strict $.\"a\"[*]?(@ > 1)")),
        ("$.a + 1.50", Ok("($.\"a\" + 1.50)")),
        ("$.a == 1e2", Ok("($.\"a\" == 100)")),
    ];

    /// The scanner's and the grammar's edges: numbers and the words they run into, escapes,
    /// comments, keywords in any case, every method with and without its arguments, where `@`
    /// and `last` may stand, operations and their parentheses, and the token each refusal names.
    const EDGES: &[Case] = &[
        ("1e3a", Err("trailing junk after numeric literal at or near \"1e3a\" of jsonpath input")),
        ("1e3ab", Err("syntax error at end of jsonpath input")),
        ("1e+", Err("invalid numeric literal at or near \"1e+\" of jsonpath input")),
        ("1e+a", Err("invalid numeric literal at or near \"1e+\" of jsonpath input")),
        ("1e-5", Ok("0.00001")),
        ("1E5", Ok("100000")),
        ("0x", Err("trailing junk after numeric literal at or near \"0x\" of jsonpath input")),
        ("0X1f", Ok("31")),
        ("0o17", Ok("15")),
        ("0O17", Ok("15")),
        ("0B101", Ok("5")),
        ("0b102", Err("syntax error at end of jsonpath input")),
        ("0x1g", Err("syntax error at end of jsonpath input")),
        ("0xg", Err("syntax error at end of jsonpath input")),
        ("1__0", Err("syntax error at end of jsonpath input")),
        ("1_", Err("trailing junk after numeric literal at or near \"1_\" of jsonpath input")),
        (".5", Ok("0.5")),
        (".5e1", Ok("5")),
        (".e1", Err("syntax error at or near \".\" of jsonpath input")),
        ("5.", Ok("5")),
        ("5..", Err("syntax error at end of jsonpath input")),
        ("5.e", Err("trailing junk after numeric literal at or near \"5.e\" of jsonpath input")),
        ("5.e+", Err("invalid numeric literal at or near \"5.e+\" of jsonpath input")),
        ("5.e+1", Ok("50")),
        ("0.", Ok("0")),
        ("00.5", Err("trailing junk after numeric literal at or near \"00\" of jsonpath input")),
        ("01", Err("trailing junk after numeric literal at or near \"01\" of jsonpath input")),
        ("1.5.5", Err("syntax error at or near \".5\" of jsonpath input")),
        ("$.5", Err("syntax error at or near \".5\" of jsonpath input")),
        ("$[.5]", Ok("$[0.5]")),
        ("$[1.5e1]", Ok("$[15]")),
        ("1e1000000", Err("value overflows numeric format")),
        ("1e-1000000", Err("value overflows numeric format")),
        ("1e-16383", Ok("0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001")),
        ("1e-16384", Err("value overflows numeric format")),
        ("0.0000000000000000000000001", Ok("0.0000000000000000000000001")),
        ("-0", Ok("0")),
        ("-0.0", Ok("0.0")),
        ("+-0", Ok("0")),
        ("--1", Ok("1")),
        ("-+-1", Ok("1")),
        ("- 1", Ok("-1")),
        ("$[-1]", Ok("$[-1]")),
        ("9999999999999999999999999999999", Ok("9999999999999999999999999999999")),
        ("1e100", Ok("10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000")),
        ("1.000e-3", Ok("0.001000")),
        ("0e5", Ok("0")),
        ("0.e5", Ok("0")),
        ("0_0", Err("syntax error at end of jsonpath input")),
        ("1_0e1_0", Ok("100000000000")),
        ("1e1_", Err("trailing junk after numeric literal at or near \"1e1_\" of jsonpath input")),
        ("1e_1", Err("syntax error at end of jsonpath input")),
        ("1.5e", Err("trailing junk after numeric literal at or near \"1.5e\" of jsonpath input")),
        ("1.5e-", Err("invalid numeric literal at or near \"1.5e-\" of jsonpath input")),
        ("1.5e-x", Err("invalid numeric literal at or near \"1.5e-\" of jsonpath input")),
        ("1.5e-1x", Err("trailing junk after numeric literal at or near \"1.5e-1x\" of jsonpath input")),
        ("1.5e-1xy", Err("trailing junk after numeric literal at or near \"1.5e-1x\" of jsonpath input")),
        ("0x1F_", Err("syntax error at end of jsonpath input")),
        ("0x_1", Err("syntax error at end of jsonpath input")),
        ("0x1__2", Err("syntax error at end of jsonpath input")),
        ("0o8", Err("syntax error at end of jsonpath input")),
        ("0b2", Err("syntax error at end of jsonpath input")),
        ("0x1.5", Err("syntax error at or near \".5\" of jsonpath input")),
        ("1x1", Err("syntax error at end of jsonpath input")),
        ("$a", Ok("$\"a\"")),
        ("$\"a b\"", Ok("$\"a b\"")),
        ("$\"a\\u0041\"", Ok("$\"aA\"")),
        ("$a\\u0041", Err("syntax error at end of jsonpath input")),
        ("$\"", Err("unterminated quoted string at end of jsonpath input")),
        ("$\"abc", Err("unterminated quoted string at end of jsonpath input")),
        ("$", Ok("$")),
        ("$$", Err("syntax error at or near \"$\" of jsonpath input")),
        ("$_", Ok("$\"_\"")),
        ("$1", Ok("$\"1\"")),
        ("$é", Ok("$\"é\"")),
        ("$a.b", Ok("$\"a\".\"b\"")),
        ("$a[0]", Ok("$\"a\"[0]")),
        ("$\"a\".\"b\"", Ok("$\"a\".\"b\"")),
        ("$\"a\\\"b\"", Ok("$\"a\\\"b\"")),
        ("$\"\\x41\"", Ok("$\"A\"")),
        ("\"a\\\"b\"", Ok("\"a\\\"b\"")),
        ("\"\\\\\"", Ok("\"\\\\\"")),
        ("\"\\/\"", Ok("\"/\"")),
        ("\"\\'\"", Ok("\"'\"")),
        ("\"é\"", Ok("\"é\"")),
        ("\"\\é\"", Ok("\"é\"")),
        ("\"\\u00e9\"", Ok("\"é\"")),
        ("\"\\x41\\x42\"", Ok("\"AB\"")),
        ("\"\\x0a\"", Ok("\"\\n\"")),
        ("\"\\x01\"", Ok("\"\\u0001\"")),
        ("\"\\u\"", Err("invalid Unicode escape sequence at or near \"\\u\" of jsonpath input")),
        ("\"\\x\"", Err("invalid hexadecimal character sequence at or near \"\\x\" of jsonpath input")),
        ("\"abc\\", Err("unexpected end after backslash at or near \"\\\" of jsonpath input")),
        ("\"\\", Err("unexpected end after backslash at or near \"\\\" of jsonpath input")),
        ("\\", Err("unexpected end after backslash at or near \"\\\" of jsonpath input")),
        ("$.a\\nb", Ok("$.\"a\\nb\"")),
        ("$.\\x41", Ok("$.\"A\"")),
        ("$.\\u0041bc", Ok("$.\"Abc\"")),
        ("$.a\\ b", Ok("$.\"a b\"")),
        ("$.a\\", Err("unexpected end after backslash at or near \"\\\" of jsonpath input")),
        ("$.\\", Err("unexpected end after backslash at or near \"\\\" of jsonpath input")),
        ("\\x41", Err("syntax error at end of jsonpath input")),
        ("abc", Err("syntax error at end of jsonpath input")),
        ("a b", Err("syntax error at or near \" \" of jsonpath input")),
        ("true", Ok("true")),
        ("TRUE", Err("syntax error at end of jsonpath input")),
        ("$.TRUE", Ok("$.\"TRUE\"")),
        ("$.True", Ok("$.\"True\"")),
        ("$.null", Ok("$.\"null\"")),
        ("$.Null", Ok("$.\"Null\"")),
        ("STRICT $", Ok("strict $")),
        ("$.SIZE()", Ok("$.size()")),
        ("$.Type()", Ok("$.type()")),
        ("$ ? (@ STARTS WITH \"a\")", Ok("$?(@ starts with \"a\")")),
        ("$ ? (@ Like_Regex \"a\")", Ok("$?(@ like_regex \"a\")")),
        ("$.**{LAST}", Ok("$.**{last}")),
        ("$[LAST]", Ok("$[last]")),
        ("$ ? (EXISTS(@))", Ok("$?(exists (@))")),
        ("$ ? ((@ > 1) IS UNKNOWN)", Ok("$?((@ > 1) is unknown)")),
        ("$.DateTime(\"x\")", Ok("$.datetime(\"x\")")),
        ("$.Decimal(1)", Ok("$.decimal(1)")),
        ("/**/$", Ok("$")),
        ("$/**/.a", Ok("$.\"a\"")),
        ("$./*c*/a", Ok("$.\"a\"")),
        ("$.a/*c*/", Err("syntax error at end of jsonpath input")),
        ("$.a/*c*/.b", Err("syntax error at or near \".\" of jsonpath input")),
        ("$.a/*c*/b", Ok("$.\"b\"")),
        ("/*", Err("unexpected end of comment at end of jsonpath input")),
        ("$ /* unterminated", Err("unexpected end of comment at end of jsonpath input")),
        ("/* a */ /* b */ $", Ok("$")),
        ("$.a/*", Err("unexpected end of comment at end of jsonpath input")),
        ("$ /*/ $ */", Ok("$")),
        ("$/*a**/", Ok("$")),
        ("$.a /* x */ .b", Ok("$.\"a\".\"b\"")),
        ("$ # 1", Err("syntax error at or near \"#\" of jsonpath input")),
        ("$ : 1", Err("syntax error at or near \":\" of jsonpath input")),
        ("$ = 1", Err("syntax error at or near \"=\" of jsonpath input")),
        ("$ & 1", Err("syntax error at or near \"&\" of jsonpath input")),
        ("$ | 1", Err("syntax error at or near \"|\" of jsonpath input")),
        ("$ <> 1", Ok("($ != 1)")),
        ("$ ! 1", Err("syntax error at or near \"!\" of jsonpath input")),
        ("$ !== 1", Err("syntax error at or near \"=\" of jsonpath input")),
        ("$ === 1", Err("syntax error at or near \"=\" of jsonpath input")),
        ("$ <== 1", Err("syntax error at or near \"=\" of jsonpath input")),
        ("$ >== 1", Err("syntax error at or near \"=\" of jsonpath input")),
        ("$ <>= 1", Err("syntax error at or near \"=\" of jsonpath input")),
        ("$ ** 1", Err("syntax error at or near \"**\" of jsonpath input")),
        ("$ *** 1", Err("syntax error at or near \"**\" of jsonpath input")),
        ("$.***", Err("syntax error at end of jsonpath input")),
        ("$.a**", Err("syntax error at or near \"**\" of jsonpath input")),
        ("$ ? (@ == 1 == 2)", Err("syntax error at or near \"==\" of jsonpath input")),
        ("{", Err("syntax error at or near \"{\" of jsonpath input")),
        ("}", Err("syntax error at or near \"}\" of jsonpath input")),
        ("$ {", Err("syntax error at or near \"{\" of jsonpath input")),
        ("$.**{", Err("syntax error at end of jsonpath input")),
        ("$.**{1", Err("syntax error at end of jsonpath input")),
        ("$.**{1 to", Err("syntax error at end of jsonpath input")),
        ("$.**{1 to 2", Err("syntax error at end of jsonpath input")),
        ("$.**{1 to 2}", Ok("$.**{1 to 2}")),
        ("$.**{2 to 1}", Ok("$.**{2 to 1}")),
        ("$.**{-1}", Err("syntax error at or near \"-\" of jsonpath input")),
        ("$.**{0x10}", Ok("$.**{16}")),
        ("$.**{1_0}", Ok("$.**{10}")),
        ("$.**{0b11 to 0o7}", Ok("$.**{3 to 7}")),
        ("$.**{last to last}", Ok("$.**{last}")),
        ("$.**{0 to last}", Ok("$.**")),
        ("$.**{last to 0}", Ok("$.**{last to 0}")),
        ("$.**{2147483647}", Ok("$.**{2147483647}")),
        ("$.**{2147483648}", Err("value \"2147483648\" is out of range for type integer")),
        ("$.**{4294967295}", Err("value \"4294967295\" is out of range for type integer")),
        ("$.**{99999999999999999999}", Err("value \"99999999999999999999\" is out of range for type integer")),
        ("$.**{1.5}", Err("syntax error at or near \"1.5\" of jsonpath input")),
        ("$.**{\"a\"}", Err("syntax error at or near \"\"\" of jsonpath input")),
        ("$.**.a", Ok("$.**.\"a\"")),
        ("$.a.**", Ok("$.\"a\".**")),
        ("$.**[*]", Ok("$.**[*]")),
        ("$**", Err("syntax error at or near \"**\" of jsonpath input")),
        ("**", Err("syntax error at or near \"**\" of jsonpath input")),
        ("$.**{1}.b", Ok("$.**{1}.\"b\"")),
        ("$.**{ 1 }", Ok("$.**{1}")),
        ("$.**{0}", Ok("$.**{0}")),
        ("$.**{0 to 0}", Ok("$.**{0}")),
        ("$.**{2147483647 to last}", Ok("$.**{2147483647 to last}")),
        ("($)", Ok("$")),
        ("(($))", Ok("$")),
        ("((1))", Ok("1")),
        ("(1)", Ok("1")),
        ("(1).a", Ok("(1).\"a\"")),
        ("((1)).a", Ok("(1).\"a\"")),
        ("(-1).a", Ok("(-1).\"a\"")),
        ("-(1).a", Ok("(-(1).\"a\")")),
        ("-1.5", Ok("-1.5")),
        ("-(1.5)", Ok("-1.5")),
        ("+(1)", Ok("1")),
        ("-(-1)", Ok("1")),
        ("-(-(1))", Ok("1")),
        ("- -1", Ok("1")),
        ("-$", Ok("(-$)")),
        ("+$", Ok("(+$)")),
        ("-(-$)", Ok("(-(-$))")),
        ("-$.a", Ok("(-$.\"a\")")),
        ("-$.a.b", Ok("(-$.\"a\".\"b\")")),
        ("(-$).a", Ok("(-$).\"a\"")),
        ("$ - -$", Ok("($ - -$)")),
        ("$ * -$", Ok("($ * -$)")),
        ("-$ * $", Ok("(-$ * $)")),
        ("-($ * $)", Ok("(-($ * $))")),
        ("$ + $ * $", Ok("($ + $ * $)")),
        ("($ + $) * $", Ok("(($ + $) * $)")),
        ("$ * ($ + $)", Ok("($ * ($ + $))")),
        ("$ - ($ - $)", Ok("($ - ($ - $))")),
        ("($ - $) - $", Ok("(($ - $) - $)")),
        ("$ / $ / $", Ok("(($ / $) / $)")),
        ("$ / ($ / $)", Ok("($ / ($ / $))")),
        ("$ % $ * $", Ok("(($ % $) * $)")),
        ("1 + 2 * 3 - 4 / 5 % 6", Ok("((1 + 2 * 3) - (4 / 5) % 6)")),
        ("2 + ($ * 1).a", Ok("(2 + $ * 1.\"a\")")),
        ("2 * ($ + 1).a", Ok("(2 * ($ + 1).\"a\")")),
        ("($ * 1).a + 2", Ok("($ * 1.\"a\" + 2)")),
        ("-($ * 1).a", Ok("(-($ * 1).\"a\")")),
        ("-(-$).a", Ok("(-(-$).\"a\")")),
        ("(+$).a", Ok("(+$).\"a\"")),
        ("(1 + 2).type()", Ok("(1 + 2).type()")),
        ("(1).type().size()", Ok("(1).type().size()")),
        ("$ == 1", Ok("($ == 1)")),
        ("$ ? ($ == 1)", Ok("$?($ == 1)")),
        ("$ ? (@ == 1 && @ == 2 || @ == 3)", Ok("$?(@ == 1 && @ == 2 || @ == 3)")),
        ("$ ? (@ == 1 || @ == 2 && @ == 3)", Ok("$?(@ == 1 || @ == 2 && @ == 3)")),
        ("$ ? ((@ == 1 || @ == 2) && @ == 3)", Ok("$?((@ == 1 || @ == 2) && @ == 3)")),
        ("$ ? (@ == 1 && (@ == 2 || @ == 3))", Ok("$?(@ == 1 && (@ == 2 || @ == 3))")),
        ("$ ? (!(@ == 1))", Ok("$?(!(@ == 1))")),
        ("$ ? (!exists(@))", Ok("$?(!(exists (@)))")),
        ("$ ? (! exists (@.a))", Ok("$?(!(exists (@.\"a\")))")),
        ("$ ? (!!(@ == 1))", Err("syntax error at or near \"!\" of jsonpath input")),
        ("$ ? (!@ == 1)", Err("syntax error at or near \"@\" of jsonpath input")),
        ("$ ? (!(1))", Err("syntax error at or near \")\" of jsonpath input")),
        ("$ ? (!((@ == 1)))", Ok("$?(!(@ == 1))")),
        ("$ ? (!(@ == 1) is unknown)", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ ? ((@ == 1) is unknown is unknown)", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ ? (((@ == 1) is unknown) is unknown)", Ok("$?(((@ == 1) is unknown) is unknown)")),
        ("$ ? ((@ == 1) is unknown && @ == 2)", Ok("$?((@ == 1) is unknown && @ == 2)")),
        ("$ ? ((@ == 1) is known)", Err("syntax error at end of jsonpath input")),
        ("$ ? ((@ == 1) is)", Err("syntax error at or near \")\" of jsonpath input")),
        ("$ ? (@ is unknown)", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ ? ((1) is unknown)", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ ? (exists(@) is unknown)", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ ? ((exists(@)) is unknown)", Ok("$?((exists (@)) is unknown)")),
        ("$ ? (exists(@ == 1))", Err("syntax error at or near \"==\" of jsonpath input")),
        ("$ ? (exists((@ == 1)))", Err("syntax error at or near \")\" of jsonpath input")),
        ("$ ? (exists((@ == 1).a))", Ok("$?(exists (@ == 1.\"a\"))")),
        ("$ ? (exists(@).a)", Err("syntax error at or near \".\" of jsonpath input")),
        ("$ ? (exists @)", Err("syntax error at or near \"@\" of jsonpath input")),
        ("$ ? (exists)", Err("syntax error at or near \")\" of jsonpath input")),
        ("exists(@)", Err("@ is not allowed in root expressions")),
        ("exists($)", Ok("exists ($)")),
        ("exists($) && exists($)", Ok("(exists ($) && exists ($))")),
        ("$ == 1 && $ == 2", Ok("($ == 1 && $ == 2)")),
        ("$ && $", Err("syntax error at or near \"&&\" of jsonpath input")),
        ("$ == 1 && $", Err("syntax error at end of jsonpath input")),
        ("$ || $ == 1", Err("syntax error at or near \"||\" of jsonpath input")),
        ("($ == 1) + 1", Err("syntax error at or near \"+\" of jsonpath input")),
        ("1 + ($ == 1)", Err("syntax error at end of jsonpath input")),
        ("1 + ($ == 1).a", Ok("(1 + ($ == 1).\"a\")")),
        ("($ == 1).a", Ok("($ == 1).\"a\"")),
        ("(($ == 1)).a", Ok("($ == 1).\"a\"")),
        ("($ == 1) == 1", Err("syntax error at or near \"==\" of jsonpath input")),
        ("1 == ($ == 1)", Err("syntax error at end of jsonpath input")),
        ("($ == 1) starts with \"a\"", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ starts with \"a\"", Ok("($ starts with \"a\")")),
        ("$ starts with $a", Ok("($ starts with $\"a\")")),
        ("$ starts with $\"a\"", Ok("($ starts with $\"a\")")),
        ("$ starts with 1", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$ starts \"a\"", Err("syntax error at or near \"\"\" of jsonpath input")),
        ("$ starts with", Err("syntax error at end of jsonpath input")),
        ("$ like_regex \"a\"", Ok("($ like_regex \"a\")")),
        ("$ like_regex \"a\" flag \"i\"", Ok("($ like_regex \"a\" flag \"i\")")),
        ("$ like_regex", Err("syntax error at end of jsonpath input")),
        ("$ like_regex 1", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$ like_regex \"a\" flag", Err("syntax error at end of jsonpath input")),
        ("$ like_regex \"a\" flag 1", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$ like_regex \"a\" flag \"z\"", Err("invalid input syntax for type jsonpath")),
        ("$ like_regex \"a\" flag \"xq\"", Ok("($ like_regex \"a\" flag \"xq\")")),
        ("$ like_regex \"a\" flag \"qx\"", Ok("($ like_regex \"a\" flag \"xq\")")),
        ("$ like_regex \"a\" flag \"x\"", Err("XQuery \"x\" flag (expanded regular expressions) is not implemented")),
        ("$ like_regex \"a\" flag \"ii\"", Ok("($ like_regex \"a\" flag \"i\")")),
        ("$ like_regex \"a\" flag \"mq\"", Ok("($ like_regex \"a\" flag \"mq\")")),
        ("$ like_regex \"a\" flag \"xz\"", Err("invalid input syntax for type jsonpath")),
        ("$ like_regex \"a\" flag \"zx\"", Err("invalid input syntax for type jsonpath")),
        ("($ like_regex \"a\")", Ok("($ like_regex \"a\")")),
        ("($ like_regex \"a\").a", Ok("($ like_regex \"a\").\"a\"")),
        ("$ + 1 like_regex \"a\"", Ok("(($ + 1) like_regex \"a\")")),
        ("-$ like_regex \"a\"", Ok("((-$) like_regex \"a\")")),
        ("(1).a like_regex \"a\"", Ok("((1).\"a\" like_regex \"a\")")),
        ("$ like_regex \"a\" && $ like_regex \"b\"", Ok("($ like_regex \"a\" && $ like_regex \"b\")")),
        ("!($ like_regex \"a\")", Ok("!($ like_regex \"a\")")),
        ("$ like_regex \"a\" == 1", Err("syntax error at or near \"==\" of jsonpath input")),
        ("($ == 1) like_regex \"a\"", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ ? (@ like_regex \"a\" flag \"i\" && @ starts with \"b\")", Ok("$?(@ like_regex \"a\" flag \"i\" && @ starts with \"b\")")),
        ("$ == $ starts with \"a\"", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ like_regex \"a\" 1a", Err("trailing junk after numeric literal at or near \"1a\" of jsonpath input")),
        ("$ like_regex \"a\" flag \"i\" 1a", Err("trailing junk after numeric literal at or near \"1a\" of jsonpath input")),
        ("$ ? (@)", Err("syntax error at or near \")\" of jsonpath input")),
        ("$ ? ()", Err("syntax error at or near \")\" of jsonpath input")),
        ("$ ? (@ > 1", Err("syntax error at end of jsonpath input")),
        ("$ ?(@ > 1)", Ok("$?(@ > 1)")),
        ("$ ? @ > 1", Err("syntax error at or near \"@\" of jsonpath input")),
        ("$ ? (@ > 1) ? (@ < 5)", Ok("$?(@ > 1)?(@ < 5)")),
        ("$ ? (@.a ? (@ > 1) == 1)", Ok("$?(@.\"a\"?(@ > 1) == 1)")),
        ("$ ? (@[*] > 1)", Ok("$?(@[*] > 1)")),
        ("$.a ? (@ > 1).b", Ok("$.\"a\"?(@ > 1).\"b\"")),
        ("$ ? ((@ > 1))", Ok("$?(@ > 1)")),
        ("$ ? (((@ > 1)))", Ok("$?(@ > 1)")),
        ("$ ? (@ > 1).type()", Ok("$?(@ > 1).type()")),
        ("@", Err("@ is not allowed in root expressions")),
        ("@.a", Err("@ is not allowed in root expressions")),
        ("$ ? (@ > 1) + @", Err("@ is not allowed in root expressions")),
        ("$[@]", Err("@ is not allowed in root expressions")),
        ("$ ? ($[@] > 1)", Ok("$?($[@] > 1)")),
        ("$ ? (@[last] > 1)", Ok("$?(@[last] > 1)")),
        ("$[last]", Ok("$[last]")),
        ("$[last - 1]", Ok("$[last - 1]")),
        ("$[1 to last]", Ok("$[1 to last]")),
        ("$[last to 1]", Ok("$[last to 1]")),
        ("last", Err("LAST is allowed only in array subscripts")),
        ("$ + last", Err("LAST is allowed only in array subscripts")),
        ("$[$[last]]", Ok("$[$[last]]")),
        ("$[(last)]", Ok("$[last]")),
        ("$[$ ? (last > 1)]", Ok("$[$?(last > 1)]")),
        ("$[0].a ? (last > 1)", Err("LAST is allowed only in array subscripts")),
        ("$[0 ? (last > 1)]", Ok("$[(0)?(last > 1)]")),
        ("$[0 ? (@ > 1)]", Ok("$[(0)?(@ > 1)]")),
        ("$[]", Err("syntax error at or near \"]\" of jsonpath input")),
        ("$[*]", Ok("$[*]")),
        ("$[* ]", Ok("$[*]")),
        ("$[*,1]", Err("syntax error at or near \",\" of jsonpath input")),
        ("$[1,*]", Err("syntax error at or near \"*\" of jsonpath input")),
        ("$[1,]", Err("syntax error at or near \"]\" of jsonpath input")),
        ("$[,1]", Err("syntax error at or near \",\" of jsonpath input")),
        ("$[1 to]", Err("syntax error at or near \"]\" of jsonpath input")),
        ("$[to 1]", Err("syntax error at or near \" \" of jsonpath input")),
        ("$[1 to 2 to 3]", Err("syntax error at or near \" \" of jsonpath input")),
        ("$[1,2,3]", Ok("$[1,2,3]")),
        ("$[1 to 2, 3 to 4]", Ok("$[1 to 2,3 to 4]")),
        ("$[\"a\"]", Ok("$[\"a\"]")),
        ("$[$]", Ok("$[$]")),
        ("$[$.a]", Ok("$[$.\"a\"]")),
        ("$[1 + 2]", Ok("$[1 + 2]")),
        ("$[(1)]", Ok("$[1]")),
        ("$[(1 == 1)]", Err("syntax error at or near \"]\" of jsonpath input")),
        ("$[1 == 1]", Err("syntax error at or near \"==\" of jsonpath input")),
        ("$[exists(@)]", Err("syntax error at end of jsonpath input")),
        ("$[!(@ == 1)]", Err("syntax error at or near \"!\" of jsonpath input")),
        ("$[1.5]", Ok("$[1.5]")),
        ("$[null]", Ok("$[null]")),
        ("$[true]", Ok("$[true]")),
        ("$[$a]", Ok("$[$\"a\"]")),
        ("$[1", Err("syntax error at end of jsonpath input")),
        ("$[1,", Err("syntax error at end of jsonpath input")),
        ("$[", Err("syntax error at end of jsonpath input")),
        ("$]", Err("syntax error at or near \"]\" of jsonpath input")),
        ("$[[1]]", Err("syntax error at or near \"[\" of jsonpath input")),
        ("$[1][2]", Ok("$[1][2]")),
        ("$[*][*]", Ok("$[*][*]")),
        ("$.a[1].b[2]", Ok("$.\"a\"[1].\"b\"[2]")),
        ("$.type", Ok("$.\"type\"")),
        ("$.type(", Err("syntax error at end of jsonpath input")),
        ("$.type(1)", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$.type()()", Err("syntax error at or near \"(\" of jsonpath input")),
        ("$.size()", Ok("$.size()")),
        ("$.abs()", Ok("$.abs()")),
        ("$.floor()", Ok("$.floor()")),
        ("$.ceiling()", Ok("$.ceiling()")),
        ("$.double()", Ok("$.double()")),
        ("$.keyvalue()", Ok("$.keyvalue()")),
        ("$.bigint()", Ok("$.bigint()")),
        ("$.boolean()", Ok("$.boolean()")),
        ("$.date()", Ok("$.date()")),
        ("$.integer()", Ok("$.integer()")),
        ("$.number()", Ok("$.number()")),
        ("$.string()", Ok("$.string()")),
        ("$.lower()", Ok("$.lower()")),
        ("$.upper()", Ok("$.upper()")),
        ("$.initcap()", Ok("$.initcap()")),
        ("$.datetime()", Ok("$.datetime()")),
        ("$.datetime(\"a\")", Ok("$.datetime(\"a\")")),
        ("$.datetime(1)", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$.datetime(\"a\", \"b\")", Err("syntax error at or near \",\" of jsonpath input")),
        ("$.datetime($a)", Err("syntax error at or near \"$a\" of jsonpath input")),
        ("$.time()", Ok("$.time()")),
        ("$.time(1)", Ok("$.time(1)")),
        ("$.time(-1)", Err("syntax error at or near \"-\" of jsonpath input")),
        ("$.time(+1)", Err("syntax error at or near \"+\" of jsonpath input")),
        ("$.time(1.5)", Err("syntax error at or near \"1.5\" of jsonpath input")),
        ("$.time(\"a\")", Err("syntax error at or near \"\"\" of jsonpath input")),
        ("$.time(0x10)", Ok("$.time(16)")),
        ("$.time(1_0)", Ok("$.time(10)")),
        ("$.time_tz(2)", Ok("$.time_tz(2)")),
        ("$.timestamp(3)", Ok("$.timestamp(3)")),
        ("$.timestamp_tz(4)", Ok("$.timestamp_tz(4)")),
        ("$.time(99999999999999999999)", Ok("$.time(99999999999999999999)")),
        ("$.decimal()", Ok("$.decimal()")),
        ("$.decimal(1)", Ok("$.decimal(1)")),
        ("$.decimal(1,2)", Ok("$.decimal(1,2)")),
        ("$.decimal(1,2,3)", Err("invalid input syntax for type jsonpath")),
        ("$.decimal(-1,+2)", Ok("$.decimal(-1,2)")),
        ("$.decimal(+1)", Ok("$.decimal(1)")),
        ("$.decimal(1.5)", Err("syntax error at or near \"1.5\" of jsonpath input")),
        ("$.decimal(1,)", Err("syntax error at or near \")\" of jsonpath input")),
        ("$.decimal(,1)", Err("syntax error at or near \",\" of jsonpath input")),
        ("$.decimal(\"a\")", Err("syntax error at or near \"\"\" of jsonpath input")),
        ("$.decimal(0x1F, 0b1)", Ok("$.decimal(31,1)")),
        ("$.decimal(1,2,3,4)", Err("invalid input syntax for type jsonpath")),
        ("$.decimal(1,2,3 $", Err("syntax error at or near \"$\" of jsonpath input")),
        ("$.decimal(1,2,3) 1a", Err("invalid input syntax for type jsonpath")),
        ("$.decimal(1,2,3", Err("syntax error at end of jsonpath input")),
        ("$.decimal(--1)", Err("syntax error at or near \"-\" of jsonpath input")),
        ("$.decimal(- 1)", Ok("$.decimal(-1)")),
        ("$.decimal(-0)", Ok("$.decimal(0)")),
        ("$.decimal(1e1)", Err("syntax error at or near \"1e1\" of jsonpath input")),
        ("$.replace(\"a\",\"b\")", Ok("$.replace(\"a\",\"b\")")),
        ("$.replace(\"a\",1)", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$.split_part(\"a\",1)", Ok("$.split_part(\"a\",1)")),
        ("$.split_part(\"a\",-1)", Ok("$.split_part(\"a\",-1)")),
        ("$.split_part(\"a\",+1)", Ok("$.split_part(\"a\",1)")),
        ("$.split_part(\"a\",1.5)", Err("syntax error at or near \"1.5\" of jsonpath input")),
        ("$.split_part(1,1)", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$.ltrim()", Ok("$.ltrim()")),
        ("$.ltrim(\"a\")", Ok("$.ltrim(\"a\")")),
        ("$.rtrim(\"a\")", Ok("$.rtrim(\"a\")")),
        ("$.btrim(\"a\")", Ok("$.btrim(\"a\")")),
        ("$.trim()", Err("syntax error at or near \"(\" of jsonpath input")),
        ("$.foo()", Err("syntax error at or near \"(\" of jsonpath input")),
        ("$.lower(", Err("syntax error at end of jsonpath input")),
        ("$.datetime", Ok("$.\"datetime\"")),
        ("$.decimal", Ok("$.\"decimal\"")),
        ("$.time", Ok("$.\"time\"")),
        ("$.replace", Ok("$.\"replace\"")),
        ("$.split_part", Ok("$.\"split_part\"")),
        ("$.decimal.a", Ok("$.\"decimal\".\"a\"")),
        ("$.datetime.type()", Ok("$.\"datetime\".type()")),
        ("$.\"type\"()", Err("syntax error at or near \"(\" of jsonpath input")),
        ("$.\"type\"", Ok("$.\"type\"")),
        ("$.a.type().b", Ok("$.\"a\".type().\"b\"")),
        ("\"a\".type()", Ok("\"a\".type()")),
        ("null.size()", Ok("null.size()")),
        ("true.abs()", Ok("true.abs()")),
        ("$a.type()", Ok("$\"a\".type()")),
        ("@.type()", Err("@ is not allowed in root expressions")),
        ("$[last.type()]", Ok("$[last.type()]")),
        ("$.'a'", Ok("$.\"'a'\"")),
        ("$.é", Ok("$.\"é\"")),
        ("$.a-b", Err("syntax error at end of jsonpath input")),
        ("$.a - $.b", Ok("($.\"a\" - $.\"b\")")),
        ("$.1", Err("syntax error at or near \".1\" of jsonpath input")),
        ("$.1a", Err("trailing junk after numeric literal at or near \".1a\" of jsonpath input")),
        ("$.a1", Ok("$.\"a1\"")),
        ("$._", Ok("$.\"_\"")),
        ("$.$", Err("syntax error at or near \"$\" of jsonpath input")),
        ("$.@", Err("syntax error at or near \"@\" of jsonpath input")),
        ("$.\"a b\"", Ok("$.\"a b\"")),
        ("$.\"a\\\"b\"", Ok("$.\"a\\\"b\"")),
        ("$.\"\"", Ok("$.\"\"")),
        ("$.a.\"\"", Ok("$.\"a\".\"\"")),
        ("$.to", Ok("$.\"to\"")),
        ("$.is", Ok("$.\"is\"")),
        ("$.with", Ok("$.\"with\"")),
        ("$.flag", Ok("$.\"flag\"")),
        ("$.strict", Ok("$.\"strict\"")),
        ("$.lax", Ok("$.\"lax\"")),
        ("$.starts", Ok("$.\"starts\"")),
        ("$.like_regex", Ok("$.\"like_regex\"")),
        ("$.exists", Ok("$.\"exists\"")),
        ("$.unknown", Ok("$.\"unknown\"")),
        ("$.last", Ok("$.\"last\"")),
        ("$.true", Ok("$.\"true\"")),
        ("$.false", Ok("$.\"false\"")),
        ("$.null", Ok("$.\"null\"")),
        ("$.abs", Ok("$.\"abs\"")),
        ("$.keyvalue", Ok("$.\"keyvalue\"")),
        ("$.*", Ok("$.*")),
        ("$.*.*", Ok("$.*.*")),
        ("$.* .a", Ok("$.*.\"a\"")),
        ("$. a", Ok("$.\"a\"")),
        ("$ .a", Ok("$.\"a\"")),
        ("$ . a", Ok("$.\"a\"")),
        ("$.", Err("syntax error at end of jsonpath input")),
        ("$..a", Err("syntax error at or near \".\" of jsonpath input")),
        ("$.[*]", Err("syntax error at or near \"[\" of jsonpath input")),
        ("$.?(@ > 1)", Err("syntax error at or near \"?\" of jsonpath input")),
        ("$.(a)", Err("syntax error at or near \"(\" of jsonpath input")),
        ("$.a.b.c.d.e", Ok("$.\"a\".\"b\".\"c\".\"d\".\"e\"")),
        ("strict", Err("syntax error at end of jsonpath input")),
        ("lax", Err("syntax error at end of jsonpath input")),
        ("strict strict $", Err("syntax error at or near \" \" of jsonpath input")),
        ("strict lax $", Err("syntax error at or near \" \" of jsonpath input")),
        ("$ strict", Err("syntax error at end of jsonpath input")),
        ("lax $.a", Ok("$.\"a\"")),
        ("strict $.a", Ok("strict $.\"a\"")),
        ("STRICT", Err("syntax error at end of jsonpath input")),
        ("strict(1)", Ok("strict 1")),
        ("strict -1", Ok("strict -1")),
        ("strict 1 + 2", Ok("strict (1 + 2)")),
        ("strict $ ? (@ > 1)", Ok("strict $?(@ > 1)")),
        ("", Err("invalid input syntax for type jsonpath: \"\"")),
        (" ", Err("invalid input syntax for type jsonpath: \" \"")),
        ("/**/", Err("invalid input syntax for type jsonpath: \"/**/\"")),
        ("null", Ok("null")),
        ("true", Ok("true")),
        ("false", Ok("false")),
        ("\"a\"", Ok("\"a\"")),
        ("1", Ok("1")),
        ("$ $", Err("syntax error at or near \"$\" of jsonpath input")),
        ("1 1", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$ 1", Err("syntax error at or near \"1\" of jsonpath input")),
        ("$ \"a\"", Err("syntax error at or near \"\"\" of jsonpath input")),
        ("$ a", Err("syntax error at end of jsonpath input")),
        ("$ a b", Err("syntax error at or near \" \" of jsonpath input")),
        ("$a b", Err("syntax error at end of jsonpath input")),
        ("\"\\u0041\"", Ok("\"A\"")),
        ("\"\\u{41}\"", Ok("\"A\"")),
        ("\"\\u{0041}\"", Ok("\"A\"")),
        ("\"\\u{000041}\"", Ok("\"A\"")),
        ("\"\\u{0000041}\"", Err("invalid Unicode escape sequence at or near \"\\u{000004\" of jsonpath input")),
        ("\"\\u{1F600}\"", Ok("\"😀\"")),
        ("\"\\u{10FFFF}\"", Ok("\"\u{10ffff}\"")),
        ("\"\\u{110000}\"", Err("invalid Unicode code point")),
        ("\"\\u{FFFFFF}\"", Err("invalid Unicode code point")),
        ("\"\\uD83D\\uDE00\"", Ok("\"😀\"")),
        ("\"\\u{D83D}\\u{DE00}\"", Ok("\"😀\"")),
        ("\"\\uD83D\"", Err("invalid input syntax for type jsonpath")),
        ("\"\\uDE00\"", Err("invalid input syntax for type jsonpath")),
        ("\"\\uD83D\\uD83D\\uDE00\"", Err("invalid input syntax for type jsonpath")),
        ("\"\\uD83Dx\"", Err("invalid input syntax for type jsonpath")),
        ("\"\\uD83D\\n\"", Err("invalid input syntax for type jsonpath")),
        ("\"\\u0000\"", Err("unsupported Unicode escape sequence")),
        ("\"\\u{0}\"", Err("unsupported Unicode escape sequence")),
        ("\"\\u0001\"", Ok("\"\\u0001\"")),
        ("\"\\u001f\"", Ok("\"\\u001f\"")),
        ("\"\\u007f\"", Ok("\"\u{7f}\"")),
        ("\"\\u0080\"", Ok("\"\u{80}\"")),
        ("\"\\u00A0\"", Ok("\"\u{a0}\"")),
        ("\"\\uFFFF\"", Ok("\"\u{ffff}\"")),
        ("\"\\uFFFE\"", Ok("\"\u{fffe}\"")),
        ("\"\\u004\"", Err("invalid Unicode escape sequence at or near \"\\u004\" of jsonpath input")),
        ("\"\\u004g\"", Err("invalid Unicode escape sequence at or near \"\\u004\" of jsonpath input")),
        ("\"\\u{\"", Err("invalid Unicode escape sequence at or near \"\\u{\" of jsonpath input")),
        ("\"\\u{}\"", Err("invalid Unicode escape sequence at or near \"\\u{\" of jsonpath input")),
        ("\"\\u{g}\"", Err("invalid Unicode escape sequence at or near \"\\u{\" of jsonpath input")),
        ("\"\\u{41\"", Err("invalid Unicode escape sequence at or near \"\\u{41\" of jsonpath input")),
        ("\"\\u{41g}\"", Err("invalid Unicode escape sequence at or near \"\\u{41\" of jsonpath input")),
        ("\"\\u0041\\u\"", Err("invalid Unicode escape sequence at or near \"\\u0041\\u\" of jsonpath input")),
        ("\"\\u0041\\x\"", Err("invalid hexadecimal character sequence at or near \"\\x\" of jsonpath input")),
        ("\"\\u0041\\x4\"", Err("invalid hexadecimal character sequence at or near \"\\x4\" of jsonpath input")),
        ("$.\\u0041", Ok("$.\"A\"")),
        ("$.a\\u0041", Ok("$.\"aA\"")),
        ("$.\\u0041 b", Err("syntax error at end of jsonpath input")),
        ("\\u0041", Err("syntax error at end of jsonpath input")),
        ("$\"\\u0041\"", Ok("$\"A\"")),
        ("$\\u0041", Err("syntax error at end of jsonpath input")),
        ("\"\\x00\"", Err("unsupported Unicode escape sequence")),
        ("\"\\xff\"", Ok("\"ÿ\"")),
        ("\"\\xFf\"", Ok("\"ÿ\"")),
        ("\"\\x7\"", Err("invalid hexadecimal character sequence at or near \"\\x7\" of jsonpath input")),
        ("\"\\x7g\"", Err("invalid hexadecimal character sequence at or near \"\\x7\" of jsonpath input")),
        ("\"\\xg\"", Err("invalid hexadecimal character sequence at or near \"\\x\" of jsonpath input")),
        ("\"\\b\\f\\n\\r\\t\\v\"", Ok("\"\\b\\f\\n\\r\\t\\u000b\"")),
        ("\"\\a\"", Ok("\"a\"")),
        ("\"\\z\"", Ok("\"z\"")),
        ("\"\\0\"", Ok("\"0\"")),
        ("\"\\1\"", Ok("\"1\"")),
        ("\"\\\\x41\"", Ok("\"\\\\x41\"")),
        ("\"\\\\\\u0041\"", Ok("\"\\\\A\"")),
        ("$ ? (@.a == \"x\" && $.b[1 to last] == $var || !(@.c like_regex \"^a.*$\" flag \"is\"))", Ok("$?(@.\"a\" == \"x\" && $.\"b\"[1 to last] == $\"var\" || !(@.\"c\" like_regex \"^a.*$\" flag \"is\"))")),
        ("$.a[*] ? (@.b > 10 && @.c < 20).d.e.size()", Ok("$.\"a\"[*]?(@.\"b\" > 10 && @.\"c\" < 20).\"d\".\"e\".size()")),
        ("$.**{1 to 3} ? (@.type() == \"number\")", Ok("$.**{1 to 3}?(@.type() == \"number\")")),
        ("$.datetime(\"YYYY-MM-DD\").date()", Ok("$.datetime(\"YYYY-MM-DD\").date()")),
        ("$.a ? (exists (@.b ? (@ > $min && @ < $max)))", Ok("$.\"a\"?(exists (@.\"b\"?(@ > $\"min\" && @ < $\"max\")))")),
    ];

    /// LIKE_REGEX patterns, each checked as `pg_regcomp` reads it.
    const PATTERNS: &[Case] = &[
        ("$ ? (@ like_regex \"a\")", Ok("$?(@ like_regex \"a\")")),
        (
            "$ ? (@ like_regex \"(a\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        (
            "$ ? (@ like_regex \"a)\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        (
            "$ ? (@ like_regex \"(a))\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        (
            "$ ? (@ like_regex \"((a)\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        ("$ ? (@ like_regex \"()\")", Ok("$?(@ like_regex \"()\")")),
        (
            "$ ? (@ like_regex \"(?:a)\")",
            Ok("$?(@ like_regex \"(?:a)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?:a\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        (
            "$ ? (@ like_regex \"(?=a)\")",
            Ok("$?(@ like_regex \"(?=a)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?!a)\")",
            Ok("$?(@ like_regex \"(?!a)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?<=a)\")",
            Ok("$?(@ like_regex \"(?<=a)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?<!a)\")",
            Ok("$?(@ like_regex \"(?<!a)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?<a)\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"(?<\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"(?\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"(?a)\")",
            Err("invalid regular expression: invalid embedded option"),
        ),
        (
            "$ ? (@ like_regex \"(?#comment)a\")",
            Ok("$?(@ like_regex \"(?#comment)a\")"),
        ),
        (
            "$ ? (@ like_regex \"(?#comment\")",
            Ok("$?(@ like_regex \"(?#comment\")"),
        ),
        (
            "$ ? (@ like_regex \"(?i)a\")",
            Ok("$?(@ like_regex \"(?i)a\")"),
        ),
        (
            "$ ? (@ like_regex \"(?ix)a b\")",
            Ok("$?(@ like_regex \"(?ix)a b\")"),
        ),
        (
            "$ ? (@ like_regex \"(?z)a\")",
            Err("invalid regular expression: invalid embedded option"),
        ),
        (
            "$ ? (@ like_regex \"(?i\")",
            Err("invalid regular expression: invalid embedded option"),
        ),
        (
            "$ ? (@ like_regex \"(?i)(?i)a\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"(?b)a\\\\(b\\\\)\")",
            Ok("$?(@ like_regex \"(?b)a\\\\(b\\\\)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?b)\\\\(a\")",
            Err("invalid regular expression: parentheses () not balanced"),
        ),
        (
            "$ ? (@ like_regex \"(?b)a\\\\{2\\\\}\")",
            Ok("$?(@ like_regex \"(?b)a\\\\{2\\\\}\")"),
        ),
        (
            "$ ? (@ like_regex \"(?b)a\\\\{2}\")",
            Err("invalid regular expression: invalid repetition count(s)"),
        ),
        (
            "$ ? (@ like_regex \"(?b)a{2}\")",
            Ok("$?(@ like_regex \"(?b)a{2}\")"),
        ),
        (
            "$ ? (@ like_regex \"(?b)*a\")",
            Ok("$?(@ like_regex \"(?b)*a\")"),
        ),
        (
            "$ ? (@ like_regex \"(?b)^*a\")",
            Ok("$?(@ like_regex \"(?b)^*a\")"),
        ),
        (
            "$ ? (@ like_regex \"(?b)a\\\\\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"(?b)\\\\1\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(?b)\\\\(a\\\\)\\\\1\")",
            Ok("$?(@ like_regex \"(?b)\\\\(a\\\\)\\\\1\")"),
        ),
        (
            "$ ? (@ like_regex \"(?e)a)\")",
            Ok("$?(@ like_regex \"(?e)a)\")"),
        ),
        (
            "$ ? (@ like_regex \"(?e)\\\\d\")",
            Ok("$?(@ like_regex \"(?e)\\\\d\")"),
        ),
        (
            "$ ? (@ like_regex \"(?e)a{2,1}\")",
            Err("invalid regular expression: invalid repetition count(s)"),
        ),
        (
            "$ ? (@ like_regex \"(?q)(((\")",
            Ok("$?(@ like_regex \"(?q)(((\")"),
        ),
        (
            "$ ? (@ like_regex \"(?x)a  b # comment\")",
            Ok("$?(@ like_regex \"(?x)a  b # comment\")"),
        ),
        (
            "$ ? (@ like_regex \"(?x)a { 2 }\")",
            Ok("$?(@ like_regex \"(?x)a { 2 }\")"),
        ),
        (
            "$ ? (@ like_regex \"***:a\")",
            Ok("$?(@ like_regex \"***:a\")"),
        ),
        (
            "$ ? (@ like_regex \"***=(((\")",
            Ok("$?(@ like_regex \"***=(((\")"),
        ),
        (
            "$ ? (@ like_regex \"***?\")",
            Err("invalid regular expression: invalid regexp (reg version 0.8)"),
        ),
        (
            "$ ? (@ like_regex \"***a\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"***\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"**\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"*a\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"a**\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        ("$ ? (@ like_regex \"a*?\")", Ok("$?(@ like_regex \"a*?\")")),
        ("$ ? (@ like_regex \"a+?\")", Ok("$?(@ like_regex \"a+?\")")),
        ("$ ? (@ like_regex \"a??\")", Ok("$?(@ like_regex \"a??\")")),
        (
            "$ ? (@ like_regex \"a{2}?\")",
            Ok("$?(@ like_regex \"a{2}?\")"),
        ),
        (
            "$ ? (@ like_regex \"a{2}{3}\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"a{2,1}\")",
            Err("invalid regular expression: invalid repetition count(s)"),
        ),
        (
            "$ ? (@ like_regex \"a{1,2}\")",
            Ok("$?(@ like_regex \"a{1,2}\")"),
        ),
        (
            "$ ? (@ like_regex \"a{,2}\")",
            Ok("$?(@ like_regex \"a{,2}\")"),
        ),
        (
            "$ ? (@ like_regex \"a{2,}\")",
            Ok("$?(@ like_regex \"a{2,}\")"),
        ),
        (
            "$ ? (@ like_regex \"a{256}\")",
            Err("invalid regular expression: invalid repetition count(s)"),
        ),
        (
            "$ ? (@ like_regex \"a{255}\")",
            Ok("$?(@ like_regex \"a{255}\")"),
        ),
        (
            "$ ? (@ like_regex \"a{1,256}\")",
            Err("invalid regular expression: invalid repetition count(s)"),
        ),
        (
            "$ ? (@ like_regex \"a{1\")",
            Err("invalid regular expression: braces {} not balanced"),
        ),
        (
            "$ ? (@ like_regex \"a{1,\")",
            Err("invalid regular expression: braces {} not balanced"),
        ),
        (
            "$ ? (@ like_regex \"a{1x}\")",
            Err("invalid regular expression: invalid repetition count(s)"),
        ),
        ("$ ? (@ like_regex \"a{\")", Ok("$?(@ like_regex \"a{\")")),
        ("$ ? (@ like_regex \"{\")", Ok("$?(@ like_regex \"{\")")),
        ("$ ? (@ like_regex \"}\")", Ok("$?(@ like_regex \"}\")")),
        ("$ ? (@ like_regex \"a}\")", Ok("$?(@ like_regex \"a}\")")),
        (
            "$ ? (@ like_regex \"a{2}}\")",
            Ok("$?(@ like_regex \"a{2}}\")"),
        ),
        (
            "$ ? (@ like_regex \"^*\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"$*\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"\\\\A*\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        (
            "$ ? (@ like_regex \"a|*\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
        ("$ ? (@ like_regex \"|a\")", Ok("$?(@ like_regex \"|a\")")),
        ("$ ? (@ like_regex \"a|\")", Ok("$?(@ like_regex \"a|\")")),
        (
            "$ ? (@ like_regex \"a||b\")",
            Ok("$?(@ like_regex \"a||b\")"),
        ),
        (
            "$ ? (@ like_regex \"[a\")",
            Err("invalid regular expression: brackets [] not balanced"),
        ),
        (
            "$ ? (@ like_regex \"[]\")",
            Err("invalid regular expression: brackets [] not balanced"),
        ),
        (
            "$ ? (@ like_regex \"[]a]\")",
            Ok("$?(@ like_regex \"[]a]\")"),
        ),
        (
            "$ ? (@ like_regex \"[^]a]\")",
            Ok("$?(@ like_regex \"[^]a]\")"),
        ),
        (
            "$ ? (@ like_regex \"[a-]\")",
            Ok("$?(@ like_regex \"[a-]\")"),
        ),
        (
            "$ ? (@ like_regex \"[-a]\")",
            Ok("$?(@ like_regex \"[-a]\")"),
        ),
        (
            "$ ? (@ like_regex \"[a-b-c]\")",
            Err("invalid regular expression: invalid character range"),
        ),
        (
            "$ ? (@ like_regex \"[z-a]\")",
            Err("invalid regular expression: invalid character range"),
        ),
        (
            "$ ? (@ like_regex \"[a-a]\")",
            Ok("$?(@ like_regex \"[a-a]\")"),
        ),
        (
            "$ ? (@ like_regex \"[--a]\")",
            Ok("$?(@ like_regex \"[--a]\")"),
        ),
        (
            "$ ? (@ like_regex \"[!--]\")",
            Ok("$?(@ like_regex \"[!--]\")"),
        ),
        (
            "$ ? (@ like_regex \"[a\\\\]\")",
            Err("invalid regular expression: brackets [] not balanced"),
        ),
        (
            "$ ? (@ like_regex \"[\\\\d]\")",
            Ok("$?(@ like_regex \"[\\\\d]\")"),
        ),
        (
            "$ ? (@ like_regex \"[\\\\A]\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"[\\\\1]\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"[\\\\x41-\\\\x40]\")",
            Err("invalid regular expression: invalid character range"),
        ),
        (
            "$ ? (@ like_regex \"[[:alpha:]]\")",
            Ok("$?(@ like_regex \"[[:alpha:]]\")"),
        ),
        (
            "$ ? (@ like_regex \"[[:alpha]\")",
            Err("invalid regular expression: brackets [] not balanced"),
        ),
        (
            "$ ? (@ like_regex \"[[:foo:]]\")",
            Err("invalid regular expression: invalid character class"),
        ),
        (
            "$ ? (@ like_regex \"[[:]]\")",
            Err("invalid regular expression: brackets [] not balanced"),
        ),
        (
            "$ ? (@ like_regex \"[[.a.]]\")",
            Ok("$?(@ like_regex \"[[.a.]]\")"),
        ),
        (
            "$ ? (@ like_regex \"[[.ab.]]\")",
            Err("invalid regular expression: invalid collating element"),
        ),
        (
            "$ ? (@ like_regex \"[[.space.]]\")",
            Ok("$?(@ like_regex \"[[.space.]]\")"),
        ),
        (
            "$ ? (@ like_regex \"[[.space.]-a]\")",
            Ok("$?(@ like_regex \"[[.space.]-a]\")"),
        ),
        (
            "$ ? (@ like_regex \"[[..]]\")",
            Err("invalid regular expression: invalid collating element"),
        ),
        (
            "$ ? (@ like_regex \"[[.]]\")",
            Err("invalid regular expression: brackets [] not balanced"),
        ),
        (
            "$ ? (@ like_regex \"[[=a=]]\")",
            Ok("$?(@ like_regex \"[[=a=]]\")"),
        ),
        (
            "$ ? (@ like_regex \"[[=ab=]]\")",
            Err("invalid regular expression: invalid collating element"),
        ),
        (
            "$ ? (@ like_regex \"[[:<:]]a\")",
            Ok("$?(@ like_regex \"[[:<:]]a\")"),
        ),
        (
            "$ ? (@ like_regex \"a[[:>:]]\")",
            Ok("$?(@ like_regex \"a[[:>:]]\")"),
        ),
        (
            "$ ? (@ like_regex \"[a-[.z.]]\")",
            Ok("$?(@ like_regex \"[a-[.z.]]\")"),
        ),
        (
            "$ ? (@ like_regex \"[a-[:alpha:]]\")",
            Err("invalid regular expression: invalid character range"),
        ),
        (
            "$ ? (@ like_regex \"[[:alpha:]-z]\")",
            Err("invalid regular expression: invalid character range"),
        ),
        (
            "$ ? (@ like_regex \"\\\\\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"a\\\\\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\d\\\\D\\\\s\\\\S\\\\w\\\\W\")",
            Ok("$?(@ like_regex \"\\\\d\\\\D\\\\s\\\\S\\\\w\\\\W\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\y\\\\Y\\\\m\\\\M\\\\A\\\\Z\")",
            Ok("$?(@ like_regex \"\\\\y\\\\Y\\\\m\\\\M\\\\A\\\\Z\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\a\\\\b\\\\B\\\\e\\\\f\\\\n\\\\r\\\\t\\\\v\")",
            Ok("$?(@ like_regex \"\\\\a\\\\b\\\\B\\\\e\\\\f\\\\n\\\\r\\\\t\\\\v\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\cA\")",
            Ok("$?(@ like_regex \"\\\\cA\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\c\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\g\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\k\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\z\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\C\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\x41\")",
            Ok("$?(@ like_regex \"\\\\x41\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\x\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\xg\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\x0123456789abcdef0123\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\U0010FFFF\")",
            Ok("$?(@ like_regex \"\\\\U0010FFFF\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\U7fffffff\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\U12345\")",
            Err("invalid regular expression: invalid escape \\ sequence"),
        ),
        (
            "$ ? (@ like_regex \"\\\\0\")",
            Ok("$?(@ like_regex \"\\\\0\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\07\")",
            Ok("$?(@ like_regex \"\\\\07\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\077\")",
            Ok("$?(@ like_regex \"\\\\077\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\0777\")",
            Ok("$?(@ like_regex \"\\\\0777\")"),
        ),
        (
            "$ ? (@ like_regex \"\\\\8\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"\\\\1\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(a)\\\\1\")",
            Ok("$?(@ like_regex \"(a)\\\\1\")"),
        ),
        (
            "$ ? (@ like_regex \"(a)\\\\2\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(a\\\\1)\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(a)(b)\\\\2\")",
            Ok("$?(@ like_regex \"(a)(b)\\\\2\")"),
        ),
        (
            "$ ? (@ like_regex \"(a)\\\\10\")",
            Ok("$?(@ like_regex \"(a)\\\\10\")"),
        ),
        (
            "$ ? (@ like_regex \"(?:a)\\\\1\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(?=(a))\\\\1\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(?=a\\\\1)\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        (
            "$ ? (@ like_regex \"(?=(a)\\\\1)\")",
            Err("invalid regular expression: invalid backreference number"),
        ),
        ("$ ? (@ like_regex \".*\")", Ok("$?(@ like_regex \".*\")")),
        ("$ ? (@ like_regex \"a.b\")", Ok("$?(@ like_regex \"a.b\")")),
        ("$ ? (@ like_regex \"^a$\")", Ok("$?(@ like_regex \"^a$\")")),
        (
            "$ ? (@ like_regex \"a{0}\")",
            Ok("$?(@ like_regex \"a{0}\")"),
        ),
        (
            "$ ? (@ like_regex \"(a){0}\\\\1\")",
            Ok("$?(@ like_regex \"(a){0}\\\\1\")"),
        ),
        (
            "$ ? (@ like_regex \"x{1}{2}\")",
            Err("invalid regular expression: quantifier operand invalid"),
        ),
    ];

    /// Paths whose text PostgreSQL does not read back to itself: an operation, `!`, `exists` or
    /// `is unknown` with accessors after it, written where its parentheses are left off. Each
    /// input, its text, and what reading that text gives.
    const NOT_READ_BACK: &[(&str, &str, &str)] = &[
        (
            "(!($ == 1)).a",
            "!($ == 1).\"a\"",
            "syntax error at or near \".\" of jsonpath input",
        ),
        (
            "(($ == 1) is unknown).a",
            "($ == 1) is unknown.\"a\"",
            "syntax error at or near \".\" of jsonpath input",
        ),
        (
            "(exists($)).a",
            "exists ($).\"a\"",
            "syntax error at or near \".\" of jsonpath input",
        ),
        (
            "$[($ + 1).a]",
            "$[$ + 1.\"a\"]",
            "syntax error at or near \"\"\" of jsonpath input",
        ),
        (
            "2 + ($ * 1).a",
            "(2 + $ * 1.\"a\")",
            "syntax error at or near \"\"\" of jsonpath input",
        ),
        (
            "($ * 1).a + 2",
            "($ * 1.\"a\" + 2)",
            "syntax error at or near \"\"\" of jsonpath input",
        ),
        (
            "$ ? (exists((@ == 1).a))",
            "$?(exists (@ == 1.\"a\"))",
            "syntax error at or near \"==\" of jsonpath input",
        ),
    ];

    #[test]
    fn reads_the_regression_file_as_postgresql_does() {
        reads_as_postgresql(REGRESSION);
    }

    #[test]
    fn reads_the_type_table_as_postgresql_does() {
        reads_as_postgresql(TYPE_TABLE);
    }

    #[test]
    fn reads_the_edges_as_postgresql_does() {
        reads_as_postgresql(EDGES);
    }

    #[test]
    fn checks_like_regex_patterns_as_pg_regcomp_does() {
        reads_as_postgresql(PATTERNS);
    }

    #[test]
    fn reads_the_text_it_writes_back_to_itself() {
        let texts = REGRESSION
            .iter()
            .chain(TYPE_TABLE)
            .chain(EDGES)
            .chain(PATTERNS)
            .filter_map(|&(_, want)| want.ok())
            .filter(|t| !NOT_READ_BACK.iter().any(|&(_, n, _)| n == *t));
        for text in texts {
            assert_eq!(jsonpath_in(text).unwrap(), text);
        }
    }

    #[test]
    fn writes_what_postgresql_writes_where_its_text_does_not_read_back() {
        for &(input, text, refusal) in NOT_READ_BACK {
            assert_eq!(jsonpath_in(input).unwrap(), text, "{input:?}");
            assert_eq!(
                jsonpath_in(text).unwrap_err().to_string(),
                refusal,
                "{text:?}"
            );
        }
    }

    #[test]
    fn takes_long_runs_and_refuses_nesting_past_its_bound() {
        // As PostgreSQL takes them: a run of one operation and a chain of keys, 10000 long.
        let sum = jsonpath_in(&format!("1{}", "+1".repeat(10_000))).unwrap();
        assert!(sum.starts_with("((((") && sum.ends_with(" + 1) + 1)"));
        let keys = jsonpath_in(&format!("${}", ".a".repeat(10_000))).unwrap();
        assert_eq!(keys, format!("${}", ".\"a\"".repeat(10_000)));
        let any: Vec<String> = (0..10_000).map(|i| format!("@ == {i}")).collect();
        let filter = jsonpath_in(&format!("$ ? ({})", any.join(" || "))).unwrap();
        assert!(filter.ends_with(" || @ == 9998) || @ == 9999)"));
        // Nesting, to its bound and past it.
        let parens = |n: usize| format!("{}${}", "(".repeat(n), ")".repeat(n));
        assert_eq!(jsonpath_in(&parens(NEST_MAX - 1)).unwrap(), "$");
        let deep = jsonpath_in(&parens(NEST_MAX)).unwrap_err();
        assert_eq!(deep.to_string(), "stack depth limit exceeded");
        let signs = |n: usize| format!("{}$", "-".repeat(n));
        assert!(jsonpath_in(&signs(NEST_MAX - 1)).is_ok());
        assert!(jsonpath_in(&signs(NEST_MAX)).is_err());
    }
}
