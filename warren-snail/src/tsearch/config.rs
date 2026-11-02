//! Text search's configurations as PostgreSQL 19 runs them: the default parser's tokens, each of
//! a type the configuration maps through its dictionary, `simple` or `english_stem`, into
//! lexemes at positions (`ts_parse.c`, `dict_simple.c`, `dict_snowball.c`); and from them a
//! tsvector (`to_tsany.c`), a tsquery (`tsquery.c`'s parser with `pushval_morph`, cleaned of stop
//! words by `tsquery_cleanup.c`), `ts_parse`, `ts_token_type`, `ts_lexize` and `ts_debug`
//! (`wparser.c`, `dict.c`, `system_functions.sql`). Only `simple` and `english` (and the
//! dictionaries and parser they use) are read; any other is refused by name.

use super::funcs::{flat, Kind, Node};
use super::parser::{self, TOKEN_TYPES};
use super::snowball::english_stem;
use super::{
    data_bytes, infix, is_operator, is_space, token_as, too_long, vector_text, Entry, Item, Op,
    Reader, Tok, ALLOC_MAX, HEADER, ITEM, LEXEME_MAX, POSITIONS_MAX, POSITION_MAX, STACK_DEPTH,
    STRING_MAX,
};
use crate::catalog::Casing;
use crate::{Error, Result};

/// A configuration the snail reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Config {
    Simple,
    English,
}

/// What a file records of `default_text_search_config`: the configuration a call naming none reads,
/// `pg_catalog.simple` unless the file names another, PostgreSQL's built-in default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextSearch {
    #[default]
    Simple,
    English,
}

impl TextSearch {
    /// The configuration named, as PostgreSQL takes it for the setting; refused by name where the
    /// snail reads no such configuration.
    pub fn open(name: &str) -> Result<TextSearch> {
        Ok(match config_of(name)? {
            Config::Simple => TextSearch::Simple,
            Config::English => TextSearch::English,
        })
    }

    /// Its name as the setting prints it.
    pub fn name(self) -> &'static str {
        match self {
            TextSearch::Simple => "pg_catalog.simple",
            TextSearch::English => "pg_catalog.english",
        }
    }

    /// Its name as `regconfigout` prints it.
    pub fn config(self) -> &'static str {
        match self {
            TextSearch::Simple => "simple",
            TextSearch::English => "english",
        }
    }
}

/// A dictionary the snail reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dict {
    Simple,
    EnglishStem,
}

impl Dict {
    fn name(self) -> &'static str {
        match self {
            Dict::Simple => "simple",
            Dict::EnglishStem => "english_stem",
        }
    }
}

/// An object's name as `stringToQualifiedNameList` reads it: each part lower-cased unless quoted,
/// a schema of `pg_catalog` dropped; none where it names another schema.
fn object_name(name: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut it = name.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if it.peek() == Some(&'"') {
            it.next();
            loop {
                match it.next() {
                    Some('"') if it.peek() == Some(&'"') => {
                        it.next();
                        part.push('"');
                    }
                    Some('"') | None => break,
                    Some(c) => part.push(c),
                }
            }
        } else {
            while let Some(&c) = it.peek() {
                if c == '.' {
                    break;
                }
                part.push(c.to_ascii_lowercase());
                it.next();
            }
        }
        parts.push(part);
        match it.next() {
            Some('.') => continue,
            _ => break,
        }
    }
    match parts.as_slice() {
        [n] => Some(n.clone()),
        [s, n] if s == "pg_catalog" => Some(n.clone()),
        _ => None,
    }
}

fn not_read(what: &str, name: &str) -> Error {
    Error::new(format!("text search {what} \"{name}\" is not read"))
}

/// A configuration by its name: `simple` or `english`, in `pg_catalog`.
pub(crate) fn config_of(name: &str) -> Result<Config> {
    match object_name(name).as_deref() {
        Some("simple") => Ok(Config::Simple),
        Some("english") => Ok(Config::English),
        _ => Err(not_read("configuration", name)),
    }
}

/// A dictionary by its name: `simple` or `english_stem`, in `pg_catalog`.
pub(crate) fn dict_of(name: &str) -> Result<Dict> {
    match object_name(name).as_deref() {
        Some("simple") => Ok(Dict::Simple),
        Some("english_stem") => Ok(Dict::EnglishStem),
        _ => Err(not_read("dictionary", name)),
    }
}

/// The parser by its name: `default`, in `pg_catalog`.
fn parser_of(name: &str) -> Result<()> {
    match object_name(name).as_deref() {
        Some("default") => Ok(()),
        _ => Err(not_read("parser", name)),
    }
}

/// The dictionary a configuration maps a token type to, or none where it passes the type over
/// (`pg_ts_config_map`): `english` stems words of letters and their parts, and leaves the rest to
/// `simple`; `simple` takes every type it maps itself. Neither maps blanks, tags, protocols or
/// entities.
pub(super) fn map(c: Config, tokid: i32) -> Option<Dict> {
    match tokid {
        1 | 2 | 10 | 11 | 16 | 17 => Some(match c {
            Config::Simple => Dict::Simple,
            Config::English => Dict::EnglishStem,
        }),
        3..=9 | 15 | 18..=22 => Some(Dict::Simple),
        _ => None,
    }
}

/// The stop words of `english_stem`: `src/backend/snowball/stopwords/english.stop`, sorted.
const ENGLISH_STOP: [&str; 127] = [
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "did",
    "do",
    "does",
    "doing",
    "don",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "me",
    "more",
    "most",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "s",
    "same",
    "she",
    "should",
    "so",
    "some",
    "such",
    "t",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

/// A text lower-cased as `str_tolower` does under the database's collation, glibc's C.UTF-8.
pub(crate) fn lower(s: &str) -> Result<String> {
    crate::strings::case(s, false, Casing::Libc)
}

/// A token's lexemes from a dictionary: none where it does not know the token, an empty list for
/// a stop word: `dsimple_lexize` (the simple dictionary accepts every token) and
/// `dsnowball_lexize` (a token past 1000 bytes lower-cased alone, else stemmed).
pub(super) fn lexize(d: Dict, token: &str) -> Result<Option<Vec<String>>> {
    let txt = lower(token)?;
    Ok(Some(match d {
        Dict::Simple if txt.is_empty() => vec![],
        Dict::Simple => vec![txt],
        Dict::EnglishStem if token.len() > 1000 => vec![txt],
        Dict::EnglishStem
            if txt.is_empty() || ENGLISH_STOP.binary_search(&txt.as_str()).is_ok() =>
        {
            vec![]
        }
        Dict::EnglishStem => vec![english_stem(&txt)],
    }))
}

/// A lexeme of a parsed text at its position: `ParsedWord`.
#[derive(Clone, Debug)]
struct Word {
    word: String,
    pos: u32,
}

/// The lexemes a configuration finds in a text, each at its position: `parsetext` (ts_parse.c).
/// Every token a dictionary answers takes a position, a stop word among them; a token or a lexeme
/// past 2047 bytes is passed over.
fn parsetext(c: Config, text: &str) -> Result<Vec<Word>> {
    let mut words = Vec::new();
    let mut pos: u32 = 0;
    for (tokid, token) in parser::parse(text) {
        if token.len() > LEXEME_MAX {
            continue;
        }
        let Some(d) = map(c, tokid) else {
            continue;
        };
        let Some(lexemes) = lexize(d, &token)? else {
            continue;
        };
        pos += 1;
        for word in lexemes {
            if word.len() > LEXEME_MAX {
                continue;
            }
            words.push(Word {
                word,
                pos: pos.min(POSITION_MAX as u32),
            });
        }
    }
    Ok(words)
}

/// A tsvector from a text's lexemes: `make_tsvector` (to_tsany.c), each lexeme's positions sorted,
/// made unique, and at most 255 of them, ending at the first 16383 (`uniqueWORD`).
fn make_tsvector(mut words: Vec<Word>) -> Result<Vec<Entry>> {
    words.sort_by(|a, b| {
        a.word
            .as_bytes()
            .cmp(b.word.as_bytes())
            .then(a.pos.cmp(&b.pos))
    });
    let mut out: Vec<Entry> = Vec::new();
    for w in words {
        let p = w.pos as u16;
        match out.last_mut() {
            Some(e) if e.word == w.word => {
                let last = *e.pos.last().expect("a position");
                if e.pos.len() < POSITIONS_MAX - 1 && last != POSITION_MAX && last != p {
                    e.pos.push(p);
                }
            }
            _ => out.push(Entry {
                word: w.word,
                pos: vec![p],
            }),
        }
    }
    let n = data_bytes(&out);
    if n > STRING_MAX {
        return Err(too_long(n));
    }
    Ok(out)
}

/// `to_tsvector(regconfig, text)`: the text's lexemes at their positions (`to_tsvector_byid`).
pub fn to_tsvector(config: &str, text: &str) -> Result<String> {
    let c = config_of(config)?;
    Ok(vector_text(&make_tsvector(parsetext(c, text)?)?))
}

/// `ts_lexize(regdictionary, text)`: the lexemes the dictionary gives the token, an empty array
/// for a stop word, NULL where it does not know it (`ts_lexize`, dict.c).
pub fn ts_lexize(dictionary: &str, token: &str) -> Result<Option<Vec<String>>> {
    lexize(dict_of(dictionary)?, token)
}

/// `ts_parse(parser, text)`: the parser's tokens, each with its type's id (`ts_parse_byname`,
/// wparser.c).
pub fn ts_parse(parser_name: &str, text: &str) -> Result<Vec<(i32, String)>> {
    parser_of(parser_name)?;
    Ok(parser::parse(text))
}

/// `ts_token_type(parser)`: the parser's token types, their aliases and descriptions
/// (`ts_token_type_byname`, wparser.c).
pub fn ts_token_type(parser_name: &str) -> Result<Vec<(i32, String, String)>> {
    parser_of(parser_name)?;
    Ok(TOKEN_TYPES
        .iter()
        .map(|(id, alias, descr)| (*id, alias.to_string(), descr.to_string()))
        .collect())
}

/// A row of `ts_debug`: a token's type's alias and description, the token, the dictionaries its
/// type maps to, the one that answered it, and its lexemes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Debugged {
    pub alias: String,
    pub description: String,
    pub token: String,
    pub dictionaries: Vec<String>,
    pub dictionary: Option<String>,
    pub lexemes: Option<Vec<String>>,
}

/// `ts_debug(regconfig, text)`: a row for each of the parser's tokens, in order, as the SQL
/// function `ts_debug` (system_functions.sql) gives them.
pub fn ts_debug(config: &str, text: &str) -> Result<Vec<Debugged>> {
    let c = config_of(config)?;
    let mut rows = Vec::new();
    for (tokid, token) in parser::parse(text) {
        let (_, alias, description) = TOKEN_TYPES
            .iter()
            .find(|t| t.0 == tokid)
            .expect("a token type the parser gives");
        let d = map(c, tokid);
        let lexemes = match d {
            Some(d) => lexize(d, &token)?,
            None => None,
        };
        rows.push(Debugged {
            alias: alias.to_string(),
            description: description.to_string(),
            token,
            dictionaries: d.iter().map(|d| d.name().to_string()).collect(),
            dictionary: d
                .filter(|_| lexemes.is_some())
                .map(|d| d.name().to_string()),
            lexemes,
        });
    }
    Ok(rows)
}

/// An item of a tsquery being read, in the order `makepol` pushes them: an operand, a stop word's
/// place, or an operator.
enum Pol {
    Val {
        word: String,
        weight: u8,
        prefix: bool,
    },
    Stop,
    Opr(Op, u16),
}

/// What `parse_tsquery` (tsquery.c) is reading: the query's text, the items pushed, and the bytes
/// of the operands pushed.
struct State<'a> {
    buffer: &'a str,
    pol: Vec<Pol>,
    sumlen: usize,
}

impl State<'_> {
    /// An operand pushed: `pushValue`.
    fn value(&mut self, word: &str, weight: u8, prefix: bool) -> Result<()> {
        if word.len() > LEXEME_MAX {
            return Err(Error::new(format!(
                "word is too long in tsquery: \"{}\"",
                self.buffer
            )));
        }
        if self.sumlen > STRING_MAX {
            return Err(Error::new(format!(
                "value is too big in tsquery: \"{}\"",
                self.buffer
            )));
        }
        self.sumlen += word.len() + 1;
        self.pol.push(Pol::Val {
            word: word.to_string(),
            weight,
            prefix,
        });
        Ok(())
    }

    /// An operator pushed: `pushOperator`, a distance kept for a phrase alone.
    fn operator(&mut self, op: Op, distance: u16) {
        self.pol
            .push(Pol::Opr(op, if op == Op::Phrase { distance } else { 0 }));
    }
}

/// A token of a tsquery's text being read: a token of `gettoken_query_standard`,
/// `gettoken_query_websearch` or `gettoken_query_plain`, or the end after a stop word's place
/// pushed.
enum QTok {
    Val {
        word: String,
        weight: u8,
        prefix: bool,
    },
    Opr(Op, u16),
    Open,
    Close,
    End,
}

/// How a tsquery's text is read: as `to_tsquery` reads it, as `websearch_to_tsquery` does, or
/// whole, as `plainto_tsquery` and `phraseto_tsquery` do.
enum Lexer<'a> {
    Standard(Reader<'a>),
    Web(Web<'a>),
    Plain(Option<&'a str>),
}

impl Lexer<'_> {
    fn next(&mut self, state: &mut State) -> Result<QTok> {
        match self {
            Lexer::Standard(r) => Ok(match r.next()? {
                Tok::End => QTok::End,
                Tok::Val {
                    word,
                    weight,
                    prefix,
                } => QTok::Val {
                    word,
                    weight,
                    prefix,
                },
                Tok::Opr(op, d) => QTok::Opr(op, d),
                Tok::Open => QTok::Open,
                Tok::Close => QTok::Close,
            }),
            Lexer::Web(w) => w.next(state),
            Lexer::Plain(p) => Ok(match p.take() {
                Some(s) if !s.is_empty() => QTok::Val {
                    word: s.to_string(),
                    weight: 0,
                    prefix: false,
                },
                _ => QTok::End,
            }),
        }
    }
}

/// Where `gettoken_query_websearch` is: before the first operand, before another, or after one.
#[derive(Clone, Copy, PartialEq)]
enum WebWait {
    First,
    Operand,
    Operator,
}

/// The reader of `websearch_to_tsquery`'s text.
struct Web<'a> {
    s: &'a str,
    i: usize,
    wait: WebWait,
}

impl Web<'_> {
    /// Whether an `or` starts at the reader, a separate word with something after it:
    /// `parse_or_operator`.
    fn or_operator(&mut self) -> bool {
        let b = self.s.as_bytes();
        if !(b.len() >= self.i + 2 && b[self.i..self.i + 2].eq_ignore_ascii_case(b"or")) {
            return false;
        }
        let mut p = self.i + 2;
        if p >= b.len() {
            return false;
        }
        let next = self.s[p..].chars().next().expect("a character");
        if next == '-' || next == '_' || parser::is_alnum(next) {
            return false;
        }
        loop {
            p += next_len(b, p);
            if p >= b.len() {
                return false;
            }
            if !is_space(b[p]) {
                break;
            }
        }
        self.i += 2;
        true
    }

    /// The next token: `gettoken_query_websearch`.
    fn next(&mut self, state: &mut State) -> Result<QTok> {
        let b = self.s.as_bytes();
        loop {
            let c = b.get(self.i).copied().unwrap_or(0);
            match self.wait {
                WebWait::First | WebWait::Operand => {
                    if c == b'-' {
                        self.i += 1;
                        self.wait = WebWait::Operand;
                        return Ok(QTok::Opr(Op::Not, 0));
                    } else if c == b'"' {
                        self.i += 1;
                        let start = self.i;
                        while self.i < b.len() && b[self.i] != b'"' {
                            self.i += 1;
                        }
                        let word = self.s[start..self.i].to_string();
                        if self.i < b.len() {
                            self.i += 1;
                        }
                        self.wait = WebWait::Operator;
                        return Ok(QTok::Val {
                            word,
                            weight: 0,
                            prefix: false,
                        });
                    } else if is_operator(c) {
                        self.i += 1;
                        self.wait = WebWait::Operand;
                        continue;
                    } else if !is_space(c) {
                        match token_as(self.s, self.i, true, true)? {
                            Some(t) => {
                                self.i = t.end;
                                self.wait = WebWait::Operator;
                                return Ok(QTok::Val {
                                    word: t.word,
                                    weight: 0,
                                    prefix: false,
                                });
                            }
                            None if self.wait == WebWait::First => return Ok(QTok::End),
                            None => {
                                state.pol.push(Pol::Stop);
                                return Ok(QTok::End);
                            }
                        }
                    }
                }
                WebWait::Operator => {
                    if c == 0 {
                        return Ok(QTok::End);
                    } else if self.or_operator() {
                        self.wait = WebWait::Operand;
                        return Ok(QTok::Opr(Op::Or, 0));
                    } else if is_operator(c) {
                        self.i += 1;
                        continue;
                    } else if !is_space(c) {
                        self.wait = WebWait::Operand;
                        return Ok(QTok::Opr(Op::And, 0));
                    }
                }
            }
            self.i += next_len(b, self.i);
        }
    }
}

/// The bytes of the character at `i`, as `pg_mblen` reads them; one past the end.
fn next_len(b: &[u8], i: usize) -> usize {
    b.get(i).map_or(1, |&c| super::char_len(c))
}

/// An operand's text through the configuration: each of its lexemes pushed, those of one token
/// at one position joined by `qop` (the phrase operator, or AND), a stop word's place pushed
/// where one left a gap, and one alone where none is found: `pushval_morph` (to_tsany.c).
fn pushval_morph(
    c: Config,
    state: &mut State,
    text: &str,
    weight: u8,
    prefix: bool,
    qop: Op,
) -> Result<()> {
    let words = parsetext(c, text)?;
    if words.is_empty() {
        state.pol.push(Pol::Stop);
        return Ok(());
    }
    let (mut pos, mut cntpos, mut count) = (0u32, 0, 0);
    while count < words.len() {
        if pos > 0 && pos + 1 < words[count].pos {
            while pos + 1 < words[count].pos {
                state.pol.push(Pol::Stop);
                if cntpos > 0 {
                    state.operator(qop, 1);
                }
                cntpos += 1;
                pos += 1;
            }
        }
        pos = words[count].pos;
        let mut cntvar = 0;
        while count < words.len() && pos == words[count].pos {
            let mut cnt = 0;
            while count < words.len() && pos == words[count].pos {
                state.value(&words[count].word, weight, prefix)?;
                if cnt > 0 {
                    state.operator(Op::And, 0);
                }
                cnt += 1;
                count += 1;
            }
            if cntvar > 0 {
                state.operator(Op::Or, 0);
            }
            cntvar += 1;
        }
        if cntpos > 0 {
            state.operator(qop, 1);
        }
        cntpos += 1;
    }
    Ok(())
}

/// The operators of one level of parentheses pushed, from the last, while they bind at least as
/// tightly as `op` (a NOT only more tightly than another NOT): `cleanOpStack`.
fn clean_stack(stack: &mut Vec<(Op, u16)>, state: &mut State, op: Op) {
    while let Some(&(top, d)) = stack.last() {
        if (op != Op::Not && op.priority() > top.priority())
            || (op == Op::Not && op.priority() >= top.priority())
        {
            break;
        }
        stack.pop();
        state.operator(top, d);
    }
}

/// A tsquery's text read into items: `makepol`, each level of parentheses with its own stack of
/// operators.
fn makepol(
    lexer: &mut Lexer,
    state: &mut State,
    push: &mut dyn FnMut(&mut State, &str, u8, bool) -> Result<()>,
) -> Result<()> {
    let mut levels: Vec<Vec<(Op, u16)>> = vec![Vec::new()];
    loop {
        match lexer.next(state)? {
            QTok::End => break,
            QTok::Val {
                word,
                weight,
                prefix,
            } => push(state, &word, weight, prefix)?,
            QTok::Opr(op, d) => {
                let stack = levels.last_mut().expect("a level");
                clean_stack(stack, state, op);
                if stack.len() >= STACK_DEPTH {
                    return Err(Error::new("tsquery is too complex"));
                }
                stack.push((op, d));
            }
            QTok::Open => levels.push(Vec::new()),
            QTok::Close => {
                let mut stack = levels.pop().expect("a level");
                clean_stack(&mut stack, state, Op::Or);
                if levels.is_empty() {
                    levels.push(Vec::new());
                }
            }
        }
    }
    while let Some(mut stack) = levels.pop() {
        clean_stack(&mut stack, state, Op::Or);
    }
    Ok(())
}

/// The tree of items pushed in `makepol`'s order: `findoprnd`, each operator's right operand the
/// one pushed last.
fn pol_tree(pol: Vec<Pol>) -> Node {
    let mut stack: Vec<Node> = Vec::new();
    for p in pol {
        match p {
            Pol::Val {
                word,
                weight,
                prefix,
            } => stack.push(Node::val(word, weight, prefix)),
            Pol::Stop => stack.push(Node::stop()),
            Pol::Opr(Op::Not, _) => {
                let x = stack.pop().expect("an operand");
                stack.push(Node::opr(Op::Not, 0, vec![x]));
            }
            Pol::Opr(op, d) => {
                let r = stack.pop().expect("a right operand");
                let l = stack.pop().expect("a left operand");
                stack.push(Node::opr(op, d, vec![r, l]));
            }
        }
    }
    stack.pop().expect("a tree")
}

/// A tree with its stop words' places taken out, an operator losing an operand left with the
/// other, a phrase's distance carried to the phrase above it: `clean_stopword_intree`
/// (tsquery_cleanup.c). Its distances to add left and right come with it.
fn clean_stop(node: Node) -> (Option<Node>, i32, i32) {
    let op = match &node.kind {
        Kind::Val { .. } => return (Some(node), 0, 0),
        Kind::Stop => return (None, 0, 0),
        Kind::Opr { op, .. } => *op,
    };
    let distance = match node.kind {
        Kind::Opr { distance, .. } => distance,
        _ => 0,
    };
    let mut child = node.child;
    if op == Op::Not {
        let (r, ladd, radd) = clean_stop(child.pop().expect("an operand"));
        return match r {
            None => (None, ladd, radd),
            Some(r) => (Some(Node::opr(Op::Not, 0, vec![r])), ladd, radd),
        };
    }
    let lnode = child.pop().expect("a left operand");
    let rnode = child.pop().expect("a right operand");
    let (l, lladd, lradd) = clean_stop(lnode);
    let (r, rladd, rradd) = clean_stop(rnode);
    let isphrase = op == Op::Phrase;
    let nd = if isphrase { distance as i32 } else { 0 };
    match (l, r) {
        (None, None) => {
            let add = if isphrase {
                lladd + nd + rladd
            } else {
                lladd.max(rladd)
            };
            (None, add, add)
        }
        (None, Some(r)) => {
            if isphrase {
                (Some(r), lladd + nd + rladd, rradd)
            } else {
                (Some(r), rladd, rradd)
            }
        }
        (Some(l), None) => {
            if isphrase {
                (Some(l), lladd, lradd + nd + rradd)
            } else {
                (Some(l), lladd, lradd)
            }
        }
        (Some(l), Some(r)) => {
            if isphrase {
                let d = (distance as i32 + lradd + rladd) as u16;
                (Some(Node::opr(op, d, vec![r, l])), lladd, rradd)
            } else {
                (Some(Node::opr(op, distance, vec![r, l])), 0, 0)
            }
        }
    }
}

/// Whether a tree holds a stop word's place.
fn has_stop(n: &Node) -> bool {
    n.kind == Kind::Stop || n.child.iter().any(has_stop)
}

/// A tsquery read from text by `parse_tsquery` (tsquery.c) with `pushval_morph`, its stop words
/// cleaned out, as the snail holds it.
fn parse_morph(c: Config, text: &str, mut lexer: Lexer, qop: Op) -> Result<String> {
    let text = super::cstring(text);
    let mut state = State {
        buffer: text,
        pol: Vec::new(),
        sumlen: 0,
    };
    let mut push = |state: &mut State, word: &str, weight: u8, prefix: bool| {
        pushval_morph(c, state, word, weight, prefix, qop)
    };
    makepol(&mut lexer, &mut state, &mut push)?;
    if state.pol.is_empty() {
        return Ok(String::new());
    }
    let n = state.pol.len();
    if state.sumlen > ALLOC_MAX - HEADER || n > (ALLOC_MAX - HEADER - state.sumlen) / ITEM {
        return Err(Error::new("tsquery is too large"));
    }
    let deep = n;
    super::on_stack(deep, move || {
        let root = pol_tree(state.pol);
        let root = if has_stop(&root) {
            match clean_stop(root).0 {
                None => return Ok(String::new()),
                Some(r) => r,
            }
        } else {
            root
        };
        let mut items: Vec<Item> = Vec::new();
        flat(&root, &mut items);
        Ok(infix(&items, true))
    })
}

/// `to_tsquery(regconfig, text)`: a tsquery's text, each operand through the configuration
/// (`to_tsquery_byid`): the lexemes of one operand joined by the phrase operator.
pub fn to_tsquery(config: &str, text: &str) -> Result<String> {
    let c = config_of(config)?;
    let s = super::cstring(text);
    let reader = Reader {
        s,
        i: 0,
        wait: super::Wait::First,
        count: 0,
    };
    parse_morph(c, s, Lexer::Standard(reader), Op::Phrase)
}

/// `plainto_tsquery(regconfig, text)`: the text's lexemes joined by AND
/// (`plainto_tsquery_byid`).
pub fn plainto_tsquery(config: &str, text: &str) -> Result<String> {
    let c = config_of(config)?;
    let s = super::cstring(text);
    parse_morph(c, s, Lexer::Plain(Some(s)), Op::And)
}

/// `phraseto_tsquery(regconfig, text)`: the text's lexemes joined by the phrase operator, a stop
/// word's place widening the distance (`phraseto_tsquery_byid`).
pub fn phraseto_tsquery(config: &str, text: &str) -> Result<String> {
    let c = config_of(config)?;
    let s = super::cstring(text);
    parse_morph(c, s, Lexer::Plain(Some(s)), Op::Phrase)
}

/// `websearch_to_tsquery(regconfig, text)`: the text read as a web search reads it, quoted words
/// a phrase, `-` NOT, `or` OR, and AND between the rest (`websearch_to_tsquery_byid`).
pub fn websearch_to_tsquery(config: &str, text: &str) -> Result<String> {
    let c = config_of(config)?;
    let s = super::cstring(text);
    let web = Web {
        s,
        i: 0,
        wait: WebWait::First,
    };
    parse_morph(c, s, Lexer::Web(web), Op::Phrase)
}

#[cfg(test)]
mod tests {
    use super::super::tsquery_out;
    use super::*;

    // Every expected value below is PostgreSQL 19's, read on its server.

    #[test]
    fn a_text_is_lexed_as_the_configuration_maps_its_tokens() {
        assert_eq!(
            to_tsvector("english", "foo-bar state-of-the-art 123-abc").unwrap(),
            "'123':9 'abc':10 'art':8 'bar':3 'foo':2 'foo-bar':1 'state':5 'state-of-the-art':4"
        );
        assert_eq!(
            to_tsvector("simple", "The cat, the CAT").unwrap(),
            "'cat':2,4 'the':1,3"
        );
        assert_eq!(
            to_tsvector("english", "The cat, the CAT").unwrap(),
            "'cat':2,4"
        );
        let cats = "cat ".repeat(300);
        let v = to_tsvector("english", &cats).unwrap();
        assert_eq!(v.matches(',').count(), 254);
        assert_eq!(to_tsvector("simple", &"a".repeat(2048)).unwrap(), "");
        assert_eq!(ts_lexize("english_stem", "").unwrap(), Some(vec![]));
        assert_eq!(ts_lexize("english_stem", "The").unwrap(), Some(vec![]));
        assert_eq!(
            ts_lexize("pg_catalog.english_stem", "Running").unwrap(),
            Some(vec!["run".to_string()])
        );
        let edge = format!("{}s", "ab".repeat(500));
        assert_eq!(
            ts_lexize("english_stem", &edge).unwrap(),
            Some(vec![edge.clone()])
        );
        let long = "ab".repeat(501);
        assert_eq!(ts_lexize("english_stem", &long).unwrap(), Some(vec![long]));
        assert_eq!(
            ts_lexize("french_stem", "x").unwrap_err().0,
            "text search dictionary \"french_stem\" is not read"
        );
        assert_eq!(
            to_tsvector("french", "x").unwrap_err().0,
            "text search configuration \"french\" is not read"
        );
        assert_eq!(to_tsvector("English", "Cats").unwrap(), "'cat':1");
        let rows = ts_debug("english", "foo-bar ").unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].alias, "asciihword");
        assert_eq!(rows[0].description, "Hyphenated word, all ASCII");
        assert_eq!(rows[2].dictionaries, Vec::<String>::new());
        assert_eq!(rows[2].dictionary, None);
        assert_eq!(rows[3].lexemes, Some(vec!["bar".to_string()]));
    }

    #[test]
    fn a_query_text_is_read_and_cleaned_of_stop_words() {
        let q = |r: Result<String>| tsquery_out(&r.unwrap());
        assert_eq!(
            q(to_tsquery("english", "(the|and&(i&1))&fghj")),
            "'1' & 'fghj'"
        );
        assert_eq!(
            q(to_tsquery("english", "foo <-> (a <-> bar)")),
            "'foo' <2> 'bar'"
        );
        assert_eq!(
            q(to_tsquery("english", "foo <-> a <-> the <-> bar")),
            "'foo' <3> 'bar'"
        );
        assert_eq!(
            q(to_tsquery("english", "foo-bar")),
            "'foo-bar' <-> 'foo' <-> 'bar'"
        );
        assert_eq!(
            q(to_tsquery("english", "cats:* & running:A")),
            "'cat':* & 'run':A"
        );
        assert_eq!(q(to_tsquery("english", "!the")), "");
        assert_eq!(
            q(phraseto_tsquery("english", "the cat the the sat on a mat")),
            "'cat' <3> 'sat' <3> 'mat'"
        );
        assert_eq!(
            q(plainto_tsquery("english", "the and z 1))& fghj")),
            "'z' & '1' & 'fghj'"
        );
        assert_eq!(
            q(websearch_to_tsquery(
                "english",
                "My brand \"new -smartphone\""
            )),
            "'brand' & 'new' <-> 'smartphon'"
        );
        assert_eq!(
            q(websearch_to_tsquery("simple", "fat or(rat")),
            "'fat' | 'rat'"
        );
        assert_eq!(
            q(websearch_to_tsquery("simple", "abc OR1234")),
            "'abc' & 'or1234'"
        );
        assert_eq!(q(websearch_to_tsquery("english", "\"\" -")), "");
        assert_eq!(
            to_tsquery("english", "(a").unwrap_err().0,
            "syntax error in tsquery: \"(a\""
        );
    }
}
