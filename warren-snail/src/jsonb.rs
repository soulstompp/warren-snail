//! json and jsonb, as PostgreSQL reads, writes, compares and takes apart each. A json is its text
//! as written, once `json_in` has checked it; its operators read that text as `jsonfuncs.c` reads
//! it, the parser's callbacks finding what they are asked for. A jsonb crosses this module as its
//! normal text, the text `jsonb_out` writes, and is read again, whenever it is asked something,
//! into the tokens `JsonbIteratorNext` walks: its containers with their counts, its keys in the
//! order jsonb stores them, its scalars. Their functions, and json and jsonb built from values,
//! are in `funcs`.

use crate::numeric::Numeric;
use crate::places::varying_part;
use crate::{Error, Result};

mod funcs;
pub use funcs::*;

/// The longest string a jsonb holds, in bytes: `JENTRY_OFFLENMASK`.
pub const STRING_BYTES_MAX: usize = 0x0FFF_FFFF;

fn syntax() -> Error {
    Error::new("invalid input syntax for type json")
}

fn untranslatable() -> Error {
    Error::new("unsupported Unicode escape sequence")
}

fn int4_range() -> Error {
    Error::new("integer out of range")
}

/// A token of JSON's grammar, as `json_lex` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    ObjectStart,
    ObjectEnd,
    ArrayStart,
    ArrayEnd,
    Comma,
    Colon,
    String,
    Number,
    True,
    False,
    Null,
    End,
}

/// Whether `json_lex` takes the byte into a word it reports whole: `JSON_ALPHANUMERIC_CHAR`.
fn alphanumeric(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// A JSON text being read, as a `JsonLexContext` holds it: the token it stands on, where that token
/// starts (none at the end) and ends, where the token before it ended, how many objects and
/// arrays it is inside, and, where it de-escapes strings, the last string token's value.
#[derive(Clone)]
struct Lexer<'a> {
    text: &'a str,
    escapes: bool,
    token: Token,
    start: Option<usize>,
    end: usize,
    prev_end: usize,
    level: usize,
    value: String,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str, escapes: bool) -> Lexer<'a> {
        Lexer {
            text,
            escapes,
            token: Token::End,
            start: Some(0),
            end: 0,
            prev_end: 0,
            level: 0,
            value: String::new(),
        }
    }

    /// The text of the token it stands on.
    fn lexeme(&self) -> &'a str {
        &self.text[self.start.unwrap_or(self.end)..self.end]
    }

    /// Steps to the next token, as `json_lex` steps: blanks passed over, then a mark, a string, a
    /// number, or a word that must be `true`, `false` or `null`.
    fn next(&mut self) -> Result<()> {
        let b = self.text.as_bytes();
        let mut s = self.end;
        while s < b.len() && matches!(b[s], b' ' | b'\t' | b'\n' | b'\r') {
            s += 1;
        }
        self.start = Some(s);
        if s >= b.len() {
            self.start = None;
            self.prev_end = self.end;
            self.end = s;
            self.token = Token::End;
            return Ok(());
        }
        let mark = match b[s] {
            b'{' => Some(Token::ObjectStart),
            b'}' => Some(Token::ObjectEnd),
            b'[' => Some(Token::ArrayStart),
            b']' => Some(Token::ArrayEnd),
            b',' => Some(Token::Comma),
            b':' => Some(Token::Colon),
            _ => None,
        };
        if let Some(t) = mark {
            self.prev_end = self.end;
            self.end = s + 1;
            self.token = t;
            return Ok(());
        }
        match b[s] {
            b'"' => {
                self.string(s)?;
                self.token = Token::String;
            }
            b'-' => {
                self.number(s + 1)?;
                self.token = Token::Number;
            }
            b'0'..=b'9' => {
                self.number(s)?;
                self.token = Token::Number;
            }
            _ => {
                let mut p = s;
                while p < b.len() && alphanumeric(b[p]) {
                    p += 1;
                }
                self.prev_end = self.end;
                self.end = if p == s { s + 1 } else { p };
                self.token = match &b[s..p] {
                    b"true" => Token::True,
                    b"false" => Token::False,
                    b"null" => Token::Null,
                    _ => return Err(syntax()),
                };
            }
        }
        Ok(())
    }

    /// Reads the string token starting at the quote at `s`, as `json_lex_string` reads it: every
    /// control character refused unescaped, each escape one of JSON's, and where it de-escapes, each
    /// `\u` escape a character other than U+0000, a surrogate only in a pair.
    fn string(&mut self, mut s: usize) -> Result<()> {
        let b = self.text.as_bytes();
        let mut high: Option<u32> = None;
        if self.escapes {
            self.value.clear();
        }
        loop {
            s += 1;
            if s >= b.len() {
                return Err(syntax());
            }
            match b[s] {
                b'"' => break,
                b'\\' => {
                    s += 1;
                    if s >= b.len() {
                        return Err(syntax());
                    }
                    if b[s] == b'u' {
                        let mut ch: u32 = 0;
                        for _ in 0..4 {
                            s += 1;
                            let d = b.get(s).and_then(|&c| (c as char).to_digit(16));
                            ch = ch * 16 + d.ok_or_else(syntax)?;
                        }
                        if self.escapes {
                            if (0xD800..0xDC00).contains(&ch) {
                                if high.is_some() {
                                    return Err(syntax());
                                }
                                high = Some(ch);
                                continue;
                            } else if (0xDC00..0xE000).contains(&ch) {
                                let Some(h) = high.take() else {
                                    return Err(syntax());
                                };
                                ch = 0x10000 + ((h & 0x3FF) << 10) + (ch & 0x3FF);
                            }
                            if high.is_some() {
                                return Err(syntax());
                            }
                            if ch == 0 {
                                return Err(untranslatable());
                            }
                            self.value
                                .push(char::from_u32(ch).ok_or_else(untranslatable)?);
                        }
                    } else if self.escapes {
                        if high.is_some() {
                            return Err(syntax());
                        }
                        self.value.push(match b[s] {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'b' => '\u{8}',
                            b'f' => '\u{c}',
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            _ => return Err(syntax()),
                        });
                    } else if !b"\"\\/bfnrt".contains(&b[s]) {
                        return Err(syntax());
                    }
                }
                _ => {
                    if high.is_some() {
                        return Err(syntax());
                    }
                    let mut p = s;
                    while p < b.len() && b[p] != b'\\' && b[p] != b'"' {
                        if b[p] <= 31 {
                            return Err(syntax());
                        }
                        p += 1;
                    }
                    if self.escapes {
                        self.value.push_str(&self.text[s..p]);
                    }
                    s = p - 1;
                }
            }
        }
        if high.is_some() {
            return Err(syntax());
        }
        self.prev_end = self.end;
        self.end = s + 1;
        Ok(())
    }

    /// Reads a number's token from `s`, past any minus sign, as `json_lex_number` reads it: `0` or
    /// digits not led by `0`, then a point and digits, then `e` or `E`, a sign and digits; letters
    /// and digits run on make the whole word the refused token.
    fn number(&mut self, mut s: usize) -> Result<()> {
        let b = self.text.as_bytes();
        let digit = |i: usize| i < b.len() && b[i].is_ascii_digit();
        let mut error = false;
        if s < b.len() && b[s] == b'0' {
            s += 1;
        } else if s < b.len() && (b'1'..=b'9').contains(&b[s]) {
            while digit(s) {
                s += 1;
            }
        } else {
            error = true;
        }
        if s < b.len() && b[s] == b'.' {
            s += 1;
            if !digit(s) {
                error = true;
            }
            while digit(s) {
                s += 1;
            }
        }
        if s < b.len() && (b[s] == b'e' || b[s] == b'E') {
            s += 1;
            if s < b.len() && (b[s] == b'+' || b[s] == b'-') {
                s += 1;
            }
            if !digit(s) {
                error = true;
            }
            while digit(s) {
                s += 1;
            }
        }
        while s < b.len() && alphanumeric(b[s]) {
            s += 1;
            error = true;
        }
        self.prev_end = self.end;
        self.end = s;
        if error {
            Err(syntax())
        } else {
            Ok(())
        }
    }

    /// Steps past the token it stands on where that token is `t`, as `lex_expect` does.
    fn expect(&mut self, t: Token) -> Result<()> {
        if self.token == t {
            self.next()
        } else {
            Err(syntax())
        }
    }
}

/// What a reading does at each step of JSON's grammar: `JsonSemAction`'s callbacks, each called
/// where `pg_parse_json` calls it and seeing the lexer as it stands there.
trait Actions {
    fn object_start(&mut self, _: &Lexer) -> Result<()> {
        Ok(())
    }
    fn object_end(&mut self, _: &Lexer) -> Result<()> {
        Ok(())
    }
    fn array_start(&mut self, _: &Lexer) -> Result<()> {
        Ok(())
    }
    fn array_end(&mut self, _: &Lexer) -> Result<()> {
        Ok(())
    }
    fn field_start(&mut self, _: &Lexer, _name: &str, _isnull: bool) -> Result<()> {
        Ok(())
    }
    fn field_end(&mut self, _: &Lexer, _name: &str, _isnull: bool) -> Result<()> {
        Ok(())
    }
    fn element_start(&mut self, _: &Lexer, _isnull: bool) -> Result<()> {
        Ok(())
    }
    fn element_end(&mut self, _: &Lexer, _isnull: bool) -> Result<()> {
        Ok(())
    }
    fn scalar(&mut self, _: &Lexer, _value: &str, _token: Token) -> Result<()> {
        Ok(())
    }
}

/// A reading that only checks.
struct Check;

impl Actions for Check {}

/// An element or a field begun and not yet ended.
enum Open {
    Element(bool),
    Field(String, bool),
}

/// Reads the whole text as `pg_parse_json` reads it: one value, then the end.
fn parse(lex: &mut Lexer, act: &mut impl Actions) -> Result<()> {
    lex.next()?;
    parse_value(lex, act)?;
    lex.expect(Token::End)
}

/// Reads one value from the token the lexer stands on, calling the actions in the order
/// `parse_object`, `parse_array`, `parse_object_field`, `parse_array_element` and `parse_scalar`
/// call them, holding the elements and fields still open in a list rather than in calls.
fn parse_value(lex: &mut Lexer, act: &mut impl Actions) -> Result<()> {
    let mut open: Vec<Open> = Vec::new();
    loop {
        match lex.token {
            Token::ObjectStart => {
                act.object_start(lex)?;
                lex.level += 1;
                lex.next()?;
                match lex.token {
                    Token::String => {
                        begin_field(lex, act, &mut open)?;
                        continue;
                    }
                    Token::ObjectEnd => close_object(lex, act)?,
                    _ => return Err(syntax()),
                }
            }
            Token::ArrayStart => {
                act.array_start(lex)?;
                lex.level += 1;
                lex.next()?;
                if lex.token != Token::ArrayEnd {
                    begin_element(lex, act, &mut open)?;
                    continue;
                }
                close_array(lex, act)?;
            }
            _ => parse_scalar(lex, act)?,
        }
        loop {
            match open.pop() {
                None => return Ok(()),
                Some(Open::Element(isnull)) => {
                    act.element_end(lex, isnull)?;
                    if lex.token == Token::Comma {
                        lex.next()?;
                        begin_element(lex, act, &mut open)?;
                        break;
                    }
                    close_array(lex, act)?;
                }
                Some(Open::Field(name, isnull)) => {
                    act.field_end(lex, &name, isnull)?;
                    if lex.token == Token::Comma {
                        lex.next()?;
                        begin_field(lex, act, &mut open)?;
                        break;
                    }
                    close_object(lex, act)?;
                }
            }
        }
    }
}

fn begin_element(lex: &mut Lexer, act: &mut impl Actions, open: &mut Vec<Open>) -> Result<()> {
    let isnull = lex.token == Token::Null;
    act.element_start(lex, isnull)?;
    open.push(Open::Element(isnull));
    Ok(())
}

fn begin_field(lex: &mut Lexer, act: &mut impl Actions, open: &mut Vec<Open>) -> Result<()> {
    if lex.token != Token::String {
        return Err(syntax());
    }
    let name = lex.value.clone();
    lex.next()?;
    lex.expect(Token::Colon)?;
    let isnull = lex.token == Token::Null;
    act.field_start(lex, &name, isnull)?;
    open.push(Open::Field(name, isnull));
    Ok(())
}

fn close_object(lex: &mut Lexer, act: &mut impl Actions) -> Result<()> {
    lex.expect(Token::ObjectEnd)?;
    lex.level -= 1;
    act.object_end(lex)
}

fn close_array(lex: &mut Lexer, act: &mut impl Actions) -> Result<()> {
    lex.expect(Token::ArrayEnd)?;
    lex.level -= 1;
    act.array_end(lex)
}

fn parse_scalar(lex: &mut Lexer, act: &mut impl Actions) -> Result<()> {
    let t = lex.token;
    if !matches!(
        t,
        Token::String | Token::Number | Token::True | Token::False | Token::Null
    ) {
        return Err(syntax());
    }
    let value = if t == Token::String {
        lex.value.clone()
    } else {
        lex.lexeme().to_string()
    };
    lex.next()?;
    act.scalar(lex, &value, t)
}

/// How many elements the array the lexer stands on holds, as `json_count_array_elements` counts
/// them, reading ahead on a copy of the lexer.
fn count_elements(lex: &Lexer) -> Result<i64> {
    let mut copy = lex.clone();
    copy.escapes = false;
    copy.level += 1;
    copy.expect(Token::ArrayStart)?;
    let mut count = 0;
    if copy.token != Token::ArrayEnd {
        loop {
            count += 1;
            parse_value(&mut copy, &mut Check)?;
            if copy.token != Token::Comma {
                break;
            }
            copy.next()?;
        }
    }
    copy.expect(Token::ArrayEnd)?;
    Ok(count)
}

/// json's input check, as `json_in` makes it: the text read whole without de-escaping its strings,
/// so a `\u0000` or a lone surrogate escape passes and an escape JSON does not have is refused.
/// The text is kept as written.
pub fn json_check(s: &str) -> Result<()> {
    parse(&mut Lexer::new(s, false), &mut Check)
}

/// A jsonb as `JsonbIteratorNext` walks it: a container's start with its count (a raw scalar, a
/// scalar at the top, as an array of one), each key and scalar, and a container's end.
#[derive(Clone, Debug)]
enum Tok {
    Array { count: usize, raw: bool },
    Object { count: usize },
    Key(String),
    Null,
    Str(String),
    Num(Numeric),
    Bool(bool),
    EndArray,
    EndObject,
}

impl Tok {
    fn is_scalar(&self) -> bool {
        matches!(self, Tok::Null | Tok::Str(_) | Tok::Num(_) | Tok::Bool(_))
    }

    /// Its kind as `JsonbTypeName` names it.
    fn kind(&self) -> &'static str {
        match self {
            Tok::Null => "null",
            Tok::Str(_) => "string",
            Tok::Num(_) => "numeric",
            Tok::Bool(_) => "boolean",
            Tok::Array { .. } | Tok::EndArray => "array",
            Tok::Object { .. } | Tok::EndObject | Tok::Key(_) => "object",
        }
    }
}

/// The pairs an object keeps, in the order it stores them, as `uniqueifyJsonbObject` leaves them
/// after `lengthCompareJsonbPair`'s sort: keys by their length, then their bytes, and of a key
/// written more than once only its last pair.
fn kept(keys: &[&str]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&x, &y| {
        let (a, b) = (keys[x], keys[y]);
        a.len()
            .cmp(&b.len())
            .then_with(|| a.as_bytes().cmp(b.as_bytes()))
            .then(y.cmp(&x))
    });
    order.dedup_by(|later, earlier| keys[*later] == keys[*earlier]);
    order
}

fn check_string(s: &str) -> Result<()> {
    if s.len() > STRING_BYTES_MAX {
        return Err(Error::new("string too long to represent as jsonb string"));
    }
    Ok(())
}

/// A part of a container being read: a key, a scalar, or a container read whole.
enum Item {
    Key(String),
    Scalar(Tok),
    Node(usize),
}

/// A container being read, its parts in the order they were read; an object's in pairs.
struct Node {
    object: bool,
    items: Vec<Item>,
}

/// jsonb's reading, `jsonb_in`'s callbacks: each key and scalar as its token, a number through
/// `numeric_in`, an object's pairs put in jsonb's order as it closes, each container kept apart
/// until the whole is read and laid out in tokens.
#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    open: Vec<usize>,
    root: Option<Item>,
}

impl Builder {
    fn begin(&mut self, object: bool) {
        self.open.push(self.nodes.len());
        self.nodes.push(Node {
            object,
            items: Vec::new(),
        });
    }

    /// Adds a value to the container it lies in, or makes it the whole.
    fn add(&mut self, item: Item) {
        match self.open.last() {
            Some(&n) => self.nodes[n].items.push(item),
            None => self.root = Some(item),
        }
    }

    /// The whole, as `JsonbIteratorNext` walks it.
    fn tokens(mut self) -> Vec<Tok> {
        let mut toks = Vec::new();
        let root = match self.root.take() {
            Some(Item::Node(n)) => n,
            Some(Item::Scalar(t)) => {
                return vec![
                    Tok::Array {
                        count: 1,
                        raw: true,
                    },
                    t,
                    Tok::EndArray,
                ]
            }
            _ => unreachable!("a reading ends with its value"),
        };
        // Each container being laid out, and its parts still to lay out.
        let mut stack: Vec<(bool, std::vec::IntoIter<Item>)> = Vec::new();
        let enter = |n: usize, nodes: &mut Vec<Node>, toks: &mut Vec<Tok>| {
            let node = std::mem::replace(
                &mut nodes[n],
                Node {
                    object: false,
                    items: Vec::new(),
                },
            );
            toks.push(if node.object {
                Tok::Object {
                    count: node.items.len() / 2,
                }
            } else {
                Tok::Array {
                    count: node.items.len(),
                    raw: false,
                }
            });
            (node.object, node.items.into_iter())
        };
        stack.push(enter(root, &mut self.nodes, &mut toks));
        while let Some((object, items)) = stack.last_mut() {
            match items.next() {
                Some(Item::Key(k)) => toks.push(Tok::Key(k)),
                Some(Item::Scalar(t)) => toks.push(t),
                Some(Item::Node(n)) => {
                    let e = enter(n, &mut self.nodes, &mut toks);
                    stack.push(e);
                }
                None => {
                    toks.push(if *object {
                        Tok::EndObject
                    } else {
                        Tok::EndArray
                    });
                    stack.pop();
                }
            }
        }
        toks
    }
}

impl Actions for Builder {
    fn object_start(&mut self, _: &Lexer) -> Result<()> {
        self.begin(true);
        Ok(())
    }

    fn object_end(&mut self, _: &Lexer) -> Result<()> {
        let n = self.open.pop().expect("an object closes what it opened");
        let mut pairs: Vec<Option<(Item, Item)>> = Vec::new();
        let mut items = std::mem::take(&mut self.nodes[n].items).into_iter();
        while let (Some(k), Some(v)) = (items.next(), items.next()) {
            pairs.push(Some((k, v)));
        }
        let keys: Vec<&str> = pairs
            .iter()
            .map(|p| match p {
                Some((Item::Key(k), _)) => k.as_str(),
                _ => unreachable!("a pair starts at its key"),
            })
            .collect();
        let order = kept(&keys);
        let mut kept_items = Vec::with_capacity(2 * order.len());
        for i in order {
            let (k, v) = pairs[i].take().expect("a pair is kept once");
            kept_items.push(k);
            kept_items.push(v);
        }
        self.nodes[n].items = kept_items;
        self.add(Item::Node(n));
        Ok(())
    }

    fn array_start(&mut self, _: &Lexer) -> Result<()> {
        self.begin(false);
        Ok(())
    }

    fn array_end(&mut self, _: &Lexer) -> Result<()> {
        let n = self.open.pop().expect("an array closes what it opened");
        self.add(Item::Node(n));
        Ok(())
    }

    fn field_start(&mut self, _: &Lexer, name: &str, _: bool) -> Result<()> {
        check_string(name)?;
        self.add(Item::Key(name.to_string()));
        Ok(())
    }

    fn scalar(&mut self, _: &Lexer, value: &str, token: Token) -> Result<()> {
        let tok = match token {
            Token::String => {
                check_string(value)?;
                Tok::Str(value.to_string())
            }
            Token::Number => Tok::Num(Numeric::parse(value)?),
            Token::True => Tok::Bool(true),
            Token::False => Tok::Bool(false),
            _ => Tok::Null,
        };
        self.add(Item::Scalar(tok));
        Ok(())
    }
}

/// A jsonb read into its tokens, with where the value starting at each token ends.
struct Doc {
    toks: Vec<Tok>,
    end: Vec<usize>,
}

impl Doc {
    /// A jsonb's text read as `jsonb_in` reads it.
    fn read(text: &str) -> Result<Doc> {
        let mut b = Builder::default();
        parse(&mut Lexer::new(text, true), &mut b)?;
        Ok(Doc::of(b.tokens()))
    }

    fn of(toks: Vec<Tok>) -> Doc {
        let mut end = vec![0; toks.len()];
        let mut open = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            match t {
                Tok::Array { .. } | Tok::Object { .. } => open.push(i),
                Tok::EndArray | Tok::EndObject => {
                    let b = open.pop().expect("a container closes what it opened");
                    end[b] = i + 1;
                    end[i] = i + 1;
                }
                _ => end[i] = i + 1,
            }
        }
        Doc { toks, end }
    }

    fn is_object(&self) -> bool {
        matches!(self.toks[0], Tok::Object { .. })
    }

    fn is_raw(&self) -> bool {
        matches!(self.toks[0], Tok::Array { raw: true, .. })
    }

    fn count(&self, at: usize) -> usize {
        match self.toks[at] {
            Tok::Array { count, .. } | Tok::Object { count } => count,
            _ => 0,
        }
    }

    /// Where each element of the array at `at` starts; for an object, each key.
    fn children(&self, at: usize) -> Vec<usize> {
        let object = matches!(self.toks[at], Tok::Object { .. });
        let mut out = Vec::with_capacity(self.count(at));
        let mut i = at + 1;
        while !matches!(self.toks[i], Tok::EndArray | Tok::EndObject) {
            out.push(i);
            i = if object { self.end[i + 1] } else { self.end[i] };
        }
        out
    }

    /// The value of `key` in the object at `at`, as `getKeyJsonValueFromContainer` finds it.
    fn field(&self, at: usize, key: &str) -> Option<usize> {
        self.children(at)
            .into_iter()
            .find(|&k| matches!(&self.toks[k], Tok::Key(s) if s == key))
            .map(|k| k + 1)
    }

    /// The `i`th element of the array at `at`, as `getIthJsonbValueFromContainer` finds it.
    fn element(&self, at: usize, i: i64) -> Option<usize> {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.children(at).get(i).copied())
    }

    /// The value starting at `at` as a jsonb of its own, written as `jsonb_out` writes it.
    fn text(&self, at: usize) -> String {
        let mut out = String::new();
        if self.toks[at].is_scalar() {
            put_scalar(&mut out, &self.toks[at]);
        } else {
            write(&mut out, &self.toks[at..self.end[at]]);
        }
        out
    }

    /// The value starting at `at` as `JsonbValueAsText` writes it: a string as itself, a JSON null
    /// as none.
    fn as_text(&self, at: usize) -> Option<String> {
        match &self.toks[at] {
            Tok::Null => None,
            Tok::Str(s) => Some(s.clone()),
            _ => Some(self.text(at)),
        }
    }

    /// The whole of it, written as `jsonb_out` writes it.
    fn out(&self) -> String {
        self.text(0)
    }

    /// The tokens of the value at `at`.
    fn value(&self, at: usize) -> &[Tok] {
        &self.toks[at..self.end[at]]
    }
}

/// A string written as `escape_json` writes it: in quotes, `"`, `\` and the control characters
/// escaped, every other character as it is.
pub(crate) fn escape_json(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A scalar as `jsonb_put_escaped_value` writes it, a number as `numeric_out` writes it.
fn put_scalar(out: &mut String, t: &Tok) {
    match t {
        Tok::Null => out.push_str("null"),
        Tok::Bool(true) => out.push_str("true"),
        Tok::Bool(false) => out.push_str("false"),
        Tok::Num(n) => out.push_str(&n.to_string()),
        Tok::Str(s) => escape_json(out, s),
        _ => unreachable!("a scalar is written"),
    }
}

/// Tokens written as `JsonbToCString` writes them: `", "` between elements and pairs, `": "` after
/// a key, a raw scalar bare.
fn write(out: &mut String, toks: &[Tok]) {
    if let [Tok::Array { raw: true, .. }, t, Tok::EndArray] = toks {
        put_scalar(out, t);
        return;
    }
    let mut comma = false;
    for t in toks {
        match t {
            Tok::EndArray => out.push(']'),
            Tok::EndObject => out.push('}'),
            _ => {
                if comma {
                    out.push_str(", ");
                }
                match t {
                    Tok::Array { .. } => out.push('['),
                    Tok::Object { .. } => out.push('{'),
                    Tok::Key(k) => {
                        escape_json(out, k);
                        out.push_str(": ");
                    }
                    t => put_scalar(out, t),
                }
            }
        }
        comma = !matches!(t, Tok::Array { .. } | Tok::Object { .. } | Tok::Key(_));
    }
}

fn written(toks: &[Tok]) -> String {
    let mut out = String::new();
    write(&mut out, toks);
    out
}

/// jsonb read as `jsonb_in` reads it and written as `jsonb_out` writes it: whitespace dropped, an
/// object's keys by their length, then their bytes, a key written twice keeping its last value,
/// each number through `numeric_in` and `numeric_out`, each string de-escaped (`\u0000` and a lone
/// surrogate refused) and escaped again as `escape_json` escapes it, a scalar at the top bare.
pub fn jsonb_in(s: &str) -> Result<String> {
    Ok(Doc::read(s)?.out())
}

/// The key of a jsonb's class, from its normal text: bytes that sort as `jsonb_cmp` orders jsonb,
/// equal exactly where it gives 0. Each token `compareJsonbContainers` meets on its walk writes its
/// part, a container's end none: an array its code 0x10, its count in four bytes, then 0x00 for a
/// raw scalar or 0x01; an object 0x11 and its count; a key its bytes as a part of varying length;
/// a scalar its code (null 0x00, string 0x01, number 0x02, boolean 0x03), then a string's bytes or
/// a number's class key as a part of varying length, or a boolean's 0 or 1.
pub fn jsonb_key(text: &str) -> Vec<u8> {
    let doc = Doc::read(text).expect("a jsonb's normal text reads back");
    let mut k = Vec::with_capacity(text.len() + 8);
    for t in &doc.toks {
        match t {
            Tok::Array { count, raw } => {
                k.push(0x10);
                k.extend_from_slice(&(*count as u32).to_be_bytes());
                k.push(if *raw { 0 } else { 1 });
            }
            Tok::Object { count } => {
                k.push(0x11);
                k.extend_from_slice(&(*count as u32).to_be_bytes());
            }
            Tok::Key(s) => varying_part(&mut k, s.as_bytes()),
            Tok::Null => k.push(0x00),
            Tok::Str(s) => {
                k.push(0x01);
                varying_part(&mut k, s.as_bytes());
            }
            Tok::Num(n) => {
                k.push(0x02);
                varying_part(&mut k, n.class_key().as_bytes());
            }
            Tok::Bool(b) => {
                k.push(0x03);
                k.push(*b as u8);
            }
            Tok::EndArray | Tok::EndObject => {}
        }
    }
    k
}

/// A jsonb's normal text from the key of its class, as `jsonb_key` writes it, each number written
/// with the digits its class holds; none where it is no such key.
pub fn jsonb_of_key(key: &[u8]) -> Option<String> {
    fn part(key: &[u8], at: &mut usize) -> Option<Vec<u8>> {
        let rest = key.get(*at..)?;
        let (part, after) = crate::places::split_varying(rest);
        *at = key.len() - after.len();
        Some(part)
    }
    fn count(key: &[u8], at: &mut usize) -> Option<usize> {
        let n = u32::from_be_bytes(key.get(*at..*at + 4)?.try_into().ok()?);
        *at += 4;
        Some(n as usize)
    }
    fn value(key: &[u8], at: &mut usize, toks: &mut Vec<Tok>) -> Option<()> {
        let code = *key.get(*at)?;
        *at += 1;
        match code {
            0x10 => {
                let n = count(key, at)?;
                let raw = *key.get(*at)? == 0;
                *at += 1;
                toks.push(Tok::Array { count: n, raw });
                for _ in 0..n {
                    value(key, at, toks)?;
                }
                toks.push(Tok::EndArray);
            }
            0x11 => {
                let n = count(key, at)?;
                toks.push(Tok::Object { count: n });
                for _ in 0..n {
                    toks.push(Tok::Key(String::from_utf8(part(key, at)?).ok()?));
                    value(key, at, toks)?;
                }
                toks.push(Tok::EndObject);
            }
            0x00 => toks.push(Tok::Null),
            0x01 => toks.push(Tok::Str(String::from_utf8(part(key, at)?).ok()?)),
            0x02 => {
                let class = String::from_utf8(part(key, at)?).ok()?;
                let text = crate::numeric::text_of_class(&class)?;
                toks.push(Tok::Num(Numeric::parse(&text).ok()?));
            }
            0x03 => {
                toks.push(Tok::Bool(*key.get(*at)? != 0));
                *at += 1;
            }
            _ => return None,
        }
        Some(())
    }
    let mut toks = Vec::new();
    let mut at = 0;
    value(key, &mut at, &mut toks)?;
    (at == key.len()).then(|| Doc::of(toks).out())
}

/// The integer `strtoint` reads in base 10, where it reads the whole text (blanks first and a sign
/// allowed) and the integer fits 32 bits.
fn strtoint(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r') {
        i += 1;
    }
    let negative = b.get(i) == Some(&b'-');
    if matches!(b.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits = i;
    let mut v: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = (v * 10 + (b[i] - b'0') as i64).min(1 << 40);
        i += 1;
    }
    if i == digits || i != b.len() {
        return None;
    }
    i32::try_from(if negative { -v } else { v }).ok()
}

/// An index of an int4 argument.
fn int4(i: i64) -> Result<i32> {
    i32::try_from(i).map_err(|_| int4_range())
}

/// json's getters, `get_worker`'s callbacks: a field's or element's value at the end of a path,
/// sliced from the text as written, or a string de-escaped and a JSON null none where text is
/// asked for. Each array met at a level of the path turns a negative index there into a place
/// counted from its end, and keeps that place for the arrays after it.
struct Get<'p> {
    names: Option<&'p [String]>,
    indexes: Option<Vec<i32>>,
    npath: usize,
    as_text: bool,
    pathok: Vec<bool>,
    cur: Vec<i64>,
    result: Option<String>,
    start: Option<usize>,
    next_scalar: bool,
}

impl Get<'_> {
    fn begin(&mut self, lex: &Lexer, matched: bool, level: usize) {
        if !matched {
            return;
        }
        if level < self.npath {
            self.pathok[level] = true;
            return;
        }
        self.result = None;
        self.start = None;
        if self.as_text && lex.token == Token::String {
            self.next_scalar = true;
        } else {
            self.start = lex.start;
        }
    }

    fn finish(&mut self, lex: &Lexer, matched: bool, level: usize, isnull: bool) {
        if !matched {
            return;
        }
        if level < self.npath {
            self.pathok[level] = false;
            return;
        }
        if let Some(start) = self.start.take() {
            self.result = if isnull && self.as_text {
                None
            } else {
                Some(lex.text[start..lex.prev_end].to_string())
            };
        }
    }

    fn name_matches(&self, level: usize, name: &str) -> bool {
        match self.names {
            Some(names) => {
                level <= self.npath && self.pathok[level - 1] && names[level - 1] == name
            }
            None => false,
        }
    }

    fn index_matches(&self, level: usize) -> bool {
        match &self.indexes {
            Some(ix) => {
                level <= self.npath
                    && self.pathok[level - 1]
                    && self.cur[level - 1] == ix[level - 1] as i64
            }
            None => false,
        }
    }

    fn whole(&mut self, lex: &Lexer, starting: bool) {
        if lex.level == 0 && self.npath == 0 {
            if starting {
                self.start = lex.start;
            } else if let Some(start) = self.start {
                self.result = Some(lex.text[start..lex.prev_end].to_string());
            }
        }
    }
}

impl Actions for Get<'_> {
    fn object_start(&mut self, lex: &Lexer) -> Result<()> {
        self.whole(lex, true);
        Ok(())
    }

    fn object_end(&mut self, lex: &Lexer) -> Result<()> {
        self.whole(lex, false);
        Ok(())
    }

    fn array_start(&mut self, lex: &Lexer) -> Result<()> {
        let level = lex.level;
        if level < self.npath {
            if let Some(ix) = &self.indexes {
                self.cur[level] = -1;
                let i = ix[level];
                if i < 0 && i != i32::MIN {
                    let n = count_elements(lex)?;
                    if -(i as i64) <= n {
                        self.indexes.as_mut().unwrap()[level] = (i as i64 + n) as i32;
                    }
                }
            }
        } else {
            self.whole(lex, true);
        }
        Ok(())
    }

    fn array_end(&mut self, lex: &Lexer) -> Result<()> {
        self.whole(lex, false);
        Ok(())
    }

    fn field_start(&mut self, lex: &Lexer, name: &str, _: bool) -> Result<()> {
        let level = lex.level;
        let m = self.name_matches(level, name);
        self.begin(lex, m, level);
        Ok(())
    }

    fn field_end(&mut self, lex: &Lexer, name: &str, isnull: bool) -> Result<()> {
        let level = lex.level;
        let m = self.name_matches(level, name);
        self.finish(lex, m, level, isnull);
        Ok(())
    }

    fn element_start(&mut self, lex: &Lexer, _: bool) -> Result<()> {
        if self.indexes.is_none() {
            return Ok(());
        }
        let level = lex.level;
        if level <= self.npath {
            self.cur[level - 1] += 1;
        }
        let m = self.index_matches(level);
        self.begin(lex, m, level);
        Ok(())
    }

    fn element_end(&mut self, lex: &Lexer, isnull: bool) -> Result<()> {
        let level = lex.level;
        let m = self.index_matches(level);
        self.finish(lex, m, level, isnull);
        Ok(())
    }

    fn scalar(&mut self, lex: &Lexer, value: &str, token: Token) -> Result<()> {
        if lex.level == 0 && self.npath == 0 {
            if self.as_text && token == Token::String {
                self.next_scalar = true;
            } else if self.as_text && token == Token::Null {
                self.result = None;
            } else {
                self.result = Some(lex.text[..lex.prev_end].to_string());
            }
        }
        if self.next_scalar {
            self.result = Some(value.to_string());
            self.next_scalar = false;
        }
        Ok(())
    }
}

/// `get_worker`: the text read whole, de-escaping its strings, and what its callbacks found.
fn get(
    doc: &str,
    names: Option<&[String]>,
    indexes: Option<Vec<i32>>,
    as_text: bool,
) -> Result<Option<String>> {
    let npath = names
        .map(|n| n.len())
        .or(indexes.as_ref().map(|i| i.len()))
        .unwrap_or(0);
    let mut g = Get {
        names,
        indexes,
        npath,
        as_text,
        pathok: vec![false; npath],
        cur: vec![0; npath],
        result: None,
        start: None,
        next_scalar: false,
    };
    if npath > 0 {
        g.pathok[0] = true;
    }
    parse(&mut Lexer::new(doc, true), &mut g)?;
    Ok(g.result)
}

/// json `->` text, as `json_object_field` takes it: the value of the key in the object at the top,
/// as written; of a key written twice, the last.
pub fn json_field(doc: &str, key: &str) -> Result<Option<String>> {
    get(doc, Some(&[key.to_string()]), None, false)
}

/// json `->>` text, as `json_object_field_text` takes it: a string de-escaped, a JSON null none.
pub fn json_field_text(doc: &str, key: &str) -> Result<Option<String>> {
    get(doc, Some(&[key.to_string()]), None, true)
}

/// json `->` integer, as `json_array_element` takes it: the element of the array at the top, a
/// negative index counting from its end.
pub fn json_element(doc: &str, i: i64) -> Result<Option<String>> {
    get(doc, None, Some(vec![int4(i)?]), false)
}

/// json `->>` integer, as `json_array_element_text` takes it.
pub fn json_element_text(doc: &str, i: i64) -> Result<Option<String>> {
    get(doc, None, Some(vec![int4(i)?]), true)
}

fn json_get_path(doc: &str, path: &[Option<String>], as_text: bool) -> Result<Option<String>> {
    let Some(names) = path.iter().cloned().collect::<Option<Vec<String>>>() else {
        return Ok(None);
    };
    let indexes = names
        .iter()
        .map(|n| strtoint(n).unwrap_or(i32::MIN))
        .collect();
    get(doc, Some(&names), Some(indexes), as_text)
}

/// json `#>` text[], as `json_extract_path` takes it: each element of the path a key, or an index
/// where it reads as an integer; a path holding a NULL gives none.
pub fn json_path(doc: &str, path: &[Option<String>]) -> Result<Option<String>> {
    json_get_path(doc, path, false)
}

/// json `#>>` text[], as `json_extract_path_text` takes it.
pub fn json_path_text(doc: &str, path: &[Option<String>]) -> Result<Option<String>> {
    json_get_path(doc, path, true)
}

/// jsonb `->` text, as `jsonb_object_field` takes it: the key's value in the object at the top.
pub fn jsonb_field(doc: &str, key: &str) -> Result<Option<String>> {
    let d = Doc::read(doc)?;
    if !d.is_object() {
        return Ok(None);
    }
    Ok(d.field(0, key).map(|v| d.text(v)))
}

/// jsonb `->>` text, as `jsonb_object_field_text` takes it: a string as itself, a JSON null none.
pub fn jsonb_field_text(doc: &str, key: &str) -> Result<Option<String>> {
    let d = Doc::read(doc)?;
    if !d.is_object() {
        return Ok(None);
    }
    Ok(d.field(0, key).and_then(|v| d.as_text(v)))
}

fn jsonb_index(d: &Doc, i: i64) -> Result<Option<usize>> {
    let mut i = int4(i)? as i64;
    if d.is_object() {
        return Ok(None);
    }
    if i < 0 {
        let n = d.count(0) as i64;
        if -i > n {
            return Ok(None);
        }
        i += n;
    }
    Ok(d.element(0, i))
}

/// jsonb `->` integer, as `jsonb_array_element` takes it: the element of the array at the top (a
/// scalar there the one element of its raw array), a negative index counting from its end.
pub fn jsonb_element(doc: &str, i: i64) -> Result<Option<String>> {
    let d = Doc::read(doc)?;
    Ok(jsonb_index(&d, i)?.map(|v| d.text(v)))
}

/// jsonb `->>` integer, as `jsonb_array_element_text` takes it.
pub fn jsonb_element_text(doc: &str, i: i64) -> Result<Option<String>> {
    let d = Doc::read(doc)?;
    Ok(jsonb_index(&d, i)?.and_then(|v| d.as_text(v)))
}

/// `jsonb_get_element`: a path followed from the top, an element of the path a key in an object or
/// an integer `strtoint` reads in an array, a scalar met before its end giving none; an empty path
/// gives the whole.
fn jsonb_get(doc: &str, path: &[Option<String>], as_text: bool) -> Result<Option<String>> {
    let d = Doc::read(doc)?;
    let Some(path) = path.iter().cloned().collect::<Option<Vec<String>>>() else {
        return Ok(None);
    };
    let mut at = if d.is_raw() { 1 } else { 0 };
    if path.is_empty() && !d.is_raw() {
        return Ok(Some(d.out()));
    }
    for p in &path {
        let next = match d.toks[at] {
            Tok::Object { .. } => d.field(at, p),
            Tok::Array { .. } => {
                let Some(i) = strtoint(p) else {
                    return Ok(None);
                };
                let mut i = i as i64;
                if i < 0 {
                    let n = d.count(at) as i64;
                    if i == i32::MIN as i64 || -i > n {
                        return Ok(None);
                    }
                    i += n;
                }
                d.element(at, i)
            }
            _ => return Ok(None),
        };
        let Some(next) = next else {
            return Ok(None);
        };
        at = next;
    }
    Ok(if as_text {
        d.as_text(at)
    } else {
        Some(d.text(at))
    })
}

/// jsonb `#>` text[], as `jsonb_extract_path` takes it; a path holding a NULL gives none.
pub fn jsonb_path(doc: &str, path: &[Option<String>]) -> Result<Option<String>> {
    jsonb_get(doc, path, false)
}

/// jsonb `#>>` text[], as `jsonb_extract_path_text` takes it.
pub fn jsonb_path_text(doc: &str, path: &[Option<String>]) -> Result<Option<String>> {
    jsonb_get(doc, path, true)
}

/// Whether two scalars of one kind are equal, as `equalsJsonbScalarValue` finds them: strings by
/// their bytes, numbers by `numeric_eq`.
fn scalars_equal(a: &Tok, b: &Tok) -> bool {
    match (a, b) {
        (Tok::Null, Tok::Null) => true,
        (Tok::Str(x), Tok::Str(y)) => x == y,
        (Tok::Num(x), Tok::Num(y)) => x == y,
        (Tok::Bool(x), Tok::Bool(y)) => x == y,
        _ => false,
    }
}

/// A container of the left side being asked whether it holds one of the right's, as far as the
/// asking has got: an object at the right's next pair; an array at the right's next element, with
/// the left's containers and the one it is trying.
enum Asking {
    Object {
        left: usize,
        next: usize,
    },
    Array {
        left: usize,
        next: usize,
        conts: Option<Vec<usize>>,
        trying: usize,
    },
}

/// What one step of the asking does: answers for its level, asks a level below, or goes on.
enum Turn {
    Answer(bool),
    Ask(usize, usize),
    Go,
}

/// `JsonbDeepContains`: whether the left value holds the right one, every pair of a right object
/// found under its key on the left, every element of a right array among the left's elements, a
/// container matched by some container of the left's, down every level. The levels being asked
/// are held in a list rather than in calls.
fn deep_contains(a: &Doc, b: &Doc) -> bool {
    let open = |l: usize, r: usize| -> std::result::Result<Asking, bool> {
        match (&a.toks[l], &b.toks[r]) {
            (Tok::Object { count: nl }, Tok::Object { count: nr }) if nl >= nr => {
                Ok(Asking::Object {
                    left: l,
                    next: r + 1,
                })
            }
            (Tok::Array { raw: lr, .. }, Tok::Array { raw: rr, .. }) if !*lr || *rr => {
                Ok(Asking::Array {
                    left: l,
                    next: r + 1,
                    conts: None,
                    trying: 0,
                })
            }
            _ => Err(false),
        }
    };
    let mut stack: Vec<Asking> = Vec::new();
    let mut answer = match open(0, 0) {
        Ok(f) => {
            stack.push(f);
            None
        }
        Err(v) => Some(v),
    };
    loop {
        let Some(top) = stack.last_mut() else {
            return answer.expect("the outermost level answers");
        };
        let turn = match (top, answer.take()) {
            (Asking::Object { .. }, Some(v)) => {
                if v {
                    Turn::Go
                } else {
                    Turn::Answer(false)
                }
            }
            (Asking::Array { next, trying, .. }, Some(true)) => {
                *next = b.end[*next];
                *trying = 0;
                Turn::Go
            }
            (
                Asking::Array {
                    next,
                    conts,
                    trying,
                    ..
                },
                Some(false),
            ) => {
                *trying += 1;
                match conts.as_ref().and_then(|c| c.get(*trying)) {
                    Some(&cl) => Turn::Ask(cl, *next),
                    None => Turn::Answer(false),
                }
            }
            (Asking::Object { left, next }, None) => {
                let k = *next;
                match &b.toks[k] {
                    Tok::Key(key) => {
                        let rv = k + 1;
                        *next = b.end[rv];
                        match a.field(*left, key) {
                            None => Turn::Answer(false),
                            Some(lv) => {
                                let (lt, rt) = (&a.toks[lv], &b.toks[rv]);
                                if !lt.is_scalar() && !rt.is_scalar() {
                                    Turn::Ask(lv, rv)
                                } else if scalars_equal(lt, rt) {
                                    Turn::Go
                                } else {
                                    Turn::Answer(false)
                                }
                            }
                        }
                    }
                    _ => Turn::Answer(true),
                }
            }
            (
                Asking::Array {
                    left,
                    next,
                    conts,
                    trying,
                },
                None,
            ) => {
                let e = *next;
                let et = &b.toks[e];
                if matches!(et, Tok::EndArray) {
                    Turn::Answer(true)
                } else if et.is_scalar() {
                    if a.children(*left)
                        .iter()
                        .any(|&x| scalars_equal(&a.toks[x], et))
                    {
                        *next = e + 1;
                        Turn::Go
                    } else {
                        Turn::Answer(false)
                    }
                } else {
                    let left = *left;
                    let c = conts.get_or_insert_with(|| {
                        a.children(left)
                            .into_iter()
                            .filter(|&x| !a.toks[x].is_scalar())
                            .collect()
                    });
                    *trying = 0;
                    match c.first() {
                        Some(&cl) => Turn::Ask(cl, e),
                        None => Turn::Answer(false),
                    }
                }
            }
        };
        match turn {
            Turn::Go => {}
            Turn::Answer(v) => {
                stack.pop();
                answer = Some(v);
            }
            Turn::Ask(l, r) => match open(l, r) {
                Ok(f) => stack.push(f),
                Err(v) => answer = Some(v),
            },
        }
    }
}

/// jsonb `@>`, as `jsonb_contains` asks it: whether `a` holds `b`, an object only an object, an
/// array its elements in any order and any number of times, a scalar at the top of `b` held by an
/// array at the top of `a`. `<@` is it with its sides swapped.
pub fn jsonb_contains(a: &str, b: &str) -> Result<bool> {
    let (a, b) = (Doc::read(a)?, Doc::read(b)?);
    if a.is_object() != b.is_object() {
        return Ok(false);
    }
    Ok(deep_contains(&a, &b))
}

/// `findJsonbValueFromContainer` with a string at the top: a key of the object, or a string
/// element of the array.
fn has_key(d: &Doc, key: &str) -> bool {
    if d.is_object() {
        d.field(0, key).is_some()
    } else {
        d.children(0)
            .iter()
            .any(|&e| matches!(&d.toks[e], Tok::Str(s) if s == key))
    }
}

/// The bytes a key or a scalar of a jsonb is an element by, as `make_scalar_key` (jsonb_gin.c) keys it:
/// its kind's flag, then its text, a number normalized as `numeric_normalize` leaves it; a string
/// element of an array counting as a key. No long string is hashed.
fn element_bytes(t: &Tok, as_key: bool) -> Vec<u8> {
    let flagged = |flag: u8, s: &str| {
        let mut out = Vec::with_capacity(s.len() + 1);
        out.push(flag);
        out.extend_from_slice(s.as_bytes());
        out
    };
    match t {
        Tok::Key(s) => flagged(0x01, s),
        Tok::Null => flagged(0x02, ""),
        Tok::Bool(b) => flagged(0x03, if *b { "t" } else { "f" }),
        Tok::Num(n) => {
            let text = n.to_string();
            let bare = match text.contains('.') {
                true => text.trim_end_matches('0').trim_end_matches('.'),
                false => &text,
            };
            flagged(0x04, bare)
        }
        Tok::Str(s) => flagged(if as_key { 0x01 } else { 0x05 }, s),
        _ => unreachable!("a key or a scalar is an element"),
    }
}

/// A jsonb's elements under `jsonb_ops`, as `gin_extract_jsonb` walks them: every key and every
/// scalar, at every depth, a string element of an array as a key, each as many times as it stands.
pub fn gin_elements(doc: &str) -> Result<Vec<Vec<u8>>> {
    let d = Doc::read(doc)?;
    let mut out = Vec::new();
    let mut arrays = Vec::new();
    for t in &d.toks {
        match t {
            Tok::Array { .. } => arrays.push(true),
            Tok::Object { .. } => arrays.push(false),
            Tok::EndArray | Tok::EndObject => {
                arrays.pop();
            }
            t => out.push(element_bytes(t, arrays.last() == Some(&true))),
        }
    }
    Ok(out)
}

/// A jsonb's elements under `jsonb_path_ops`, as `gin_extract_jsonb_path` walks them: each scalar
/// with the keys on its way down, written out where PostgreSQL hashes them, an array adding nothing
/// to the path.
pub fn gin_path_elements(doc: &str) -> Result<Vec<Vec<u8>>> {
    let d = Doc::read(doc)?;
    let mut out = Vec::new();
    let mut paths: Vec<Vec<u8>> = Vec::new();
    let mut path: Vec<u8> = Vec::new();
    for t in &d.toks {
        match t {
            Tok::Array { .. } | Tok::Object { .. } => paths.push(path.clone()),
            Tok::EndArray | Tok::EndObject => {
                paths.pop();
                path = paths.last().cloned().unwrap_or_default();
            }
            Tok::Key(k) => {
                path.extend_from_slice(k.as_bytes());
                path.push(0);
            }
            t => {
                let mut whole = path.clone();
                whole.extend(element_bytes(t, false));
                out.push(whole);
                path = paths.last().cloned().unwrap_or_default();
            }
        }
    }
    Ok(out)
}

/// A text as the key `jsonb_ops` holds it under: what `?`, `?|` and `?&` ask for.
pub fn gin_key(key: &str) -> Vec<u8> {
    element_bytes(&Tok::Key(key.to_string()), true)
}

/// jsonb `?`, as `jsonb_exists` asks it: whether the key is a key of the object at the top, or a
/// string element of the array there.
pub fn jsonb_exists(doc: &str, key: &str) -> Result<bool> {
    Ok(has_key(&Doc::read(doc)?, key))
}

/// jsonb `?|`, as `jsonb_exists_any` asks it: whether any of the keys is there, a NULL passed over.
pub fn jsonb_exists_any(doc: &str, keys: &[Option<String>]) -> Result<bool> {
    let d = Doc::read(doc)?;
    Ok(keys.iter().flatten().any(|k| has_key(&d, k)))
}

/// jsonb `?&`, as `jsonb_exists_all` asks it: whether every key is there, a NULL passed over.
pub fn jsonb_exists_all(doc: &str, keys: &[Option<String>]) -> Result<bool> {
    let d = Doc::read(doc)?;
    Ok(keys.iter().flatten().all(|k| has_key(&d, k)))
}

/// An object of these pairs, keys and their values' tokens, in the order jsonb stores them.
fn object(pairs: &[(&str, &[Tok])]) -> Vec<Tok> {
    let keys: Vec<&str> = pairs.iter().map(|p| p.0).collect();
    let order = kept(&keys);
    let mut toks = vec![Tok::Object { count: order.len() }];
    for i in order {
        toks.push(Tok::Key(pairs[i].0.to_string()));
        toks.extend_from_slice(pairs[i].1);
    }
    toks.push(Tok::EndObject);
    toks
}

fn pairs(d: &Doc, at: usize) -> Vec<(&str, &[Tok])> {
    d.children(at)
        .into_iter()
        .map(|k| match &d.toks[k] {
            Tok::Key(s) => (s.as_str(), d.value(k + 1)),
            _ => unreachable!("an object's child is a key"),
        })
        .collect()
}

/// The tokens of the elements of the array at the top, a raw scalar's its scalar.
fn inner(d: &Doc) -> &[Tok] {
    &d.toks[1..d.toks.len() - 1]
}

/// jsonb `||`, as `jsonb_concat` and `IteratorConcat` join two: two objects one object, the right's
/// value kept for a key in both; two arrays (a scalar an array of one) one array; an object and an
/// array the object an element of the array, first or last.
pub fn jsonb_concat(a: &str, b: &str) -> Result<String> {
    let (a, b) = (Doc::read(a)?, Doc::read(b)?);
    if a.is_object() == b.is_object() {
        if a.count(0) == 0 && !b.is_raw() {
            return Ok(b.out());
        }
        if b.count(0) == 0 && !a.is_raw() {
            return Ok(a.out());
        }
    }
    let toks = match (a.is_object(), b.is_object()) {
        (true, true) => {
            let mut p = pairs(&a, 0);
            p.extend(pairs(&b, 0));
            object(&p)
        }
        (left_object, _) => {
            let (l, r): (&[Tok], &[Tok]) = match (left_object, b.is_object()) {
                (false, false) => (inner(&a), inner(&b)),
                (true, false) => (&a.toks, inner(&b)),
                _ => (inner(&a), &b.toks),
            };
            let count = if left_object { 1 } else { a.count(0) }
                + if b.is_object() { 1 } else { b.count(0) };
            let mut toks = vec![Tok::Array { count, raw: false }];
            toks.extend_from_slice(l);
            toks.extend_from_slice(r);
            toks.push(Tok::EndArray);
            toks
        }
    };
    Ok(written(&toks))
}

/// The document with the values at these places taken out of the container at `at`: the tokens
/// from each start to its end, the container's count less as many.
fn without(d: &Doc, at: usize, cut: &[(usize, usize)]) -> String {
    let mut toks = Vec::with_capacity(d.toks.len());
    let mut i = 0;
    for &(from, to) in cut {
        toks.extend_from_slice(&d.toks[i..from]);
        i = to;
    }
    toks.extend_from_slice(&d.toks[i..]);
    match &mut toks[at] {
        Tok::Array { count, .. } | Tok::Object { count } => *count -= cut.len(),
        _ => unreachable!("a container loses what is cut"),
    }
    written(&toks)
}

fn scalar_delete() -> Error {
    Error::new("cannot delete from scalar")
}

/// jsonb `-` text and `-` text[] at the top: an object's pairs with these keys, or an array's
/// string elements equal to one.
fn delete_strings(doc: &str, keys: &[&str]) -> Result<String> {
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Err(scalar_delete());
    }
    if d.count(0) == 0 || keys.is_empty() {
        return Ok(d.out());
    }
    let cut: Vec<(usize, usize)> = d
        .children(0)
        .into_iter()
        .filter_map(|c| match &d.toks[c] {
            Tok::Key(s) if keys.contains(&s.as_str()) => Some((c, d.end[c + 1])),
            Tok::Str(s) if keys.contains(&s.as_str()) => Some((c, c + 1)),
            _ => None,
        })
        .collect();
    Ok(without(&d, 0, &cut))
}

/// jsonb `-` text, as `jsonb_delete` takes it: from the object at the top the key's pair, from the
/// array there every string element equal to it; a scalar refused.
pub fn jsonb_delete_key(doc: &str, key: &str) -> Result<String> {
    delete_strings(doc, &[key])
}

/// jsonb `-` text[], as `jsonb_delete_array` takes it: each key as `-` text takes it, a NULL passed
/// over.
pub fn jsonb_delete_keys(doc: &str, keys: &[Option<String>]) -> Result<String> {
    let keys: Vec<&str> = keys.iter().flatten().map(|k| k.as_str()).collect();
    delete_strings(doc, &keys)
}

/// jsonb `-` integer, as `jsonb_delete_idx` takes it: the element of the array at the top, a
/// negative index counting from its end, one past either end taking nothing; a scalar and an
/// object refused.
pub fn jsonb_delete_index(doc: &str, i: i64) -> Result<String> {
    let i = int4(i)? as i64;
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Err(scalar_delete());
    }
    if d.is_object() {
        return Err(Error::new("cannot delete from object using integer index"));
    }
    let n = d.count(0) as i64;
    let i = if i < 0 {
        if -i > n {
            n
        } else {
            n + i
        }
    } else {
        i
    };
    match d.element(0, i) {
        Some(e) => Ok(without(&d, 0, &[(e, d.end[e])])),
        None => Ok(d.out()),
    }
}

/// jsonb `#-` text[], as `jsonb_delete_path` and `setPath` take it: the path followed down from
/// the top, a key in an object, an integer in an array (a negative one counting from its end), and
/// the value at its end taken out; a path that leaves the document takes nothing. A scalar at the
/// top is refused, a NULL in the path where the walk reaches it, and an element that is not an
/// integer where it meets an array.
pub fn jsonb_delete_path(doc: &str, path: &[Option<String>]) -> Result<String> {
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Err(Error::new("cannot delete path in scalar"));
    }
    if d.count(0) == 0 || path.is_empty() {
        return Ok(d.out());
    }
    let last = path.len() - 1;
    let mut at = 0;
    for (level, p) in path.iter().enumerate() {
        let Some(p) = p else {
            return Err(Error::new(format!(
                "path element at position {} is null",
                level + 1
            )));
        };
        let (cut, value) = match d.toks[at] {
            Tok::Array { count, .. } => {
                let mut i = strtoint(p).ok_or_else(|| {
                    Error::new(format!(
                        "path element at position {} is not an integer: \"{p}\"",
                        level + 1
                    ))
                })? as i64;
                let n = count as i64;
                if i < 0 {
                    i = if -i > n { i32::MIN as i64 } else { n + i };
                }
                match d.element(at, i) {
                    Some(e) => ((e, d.end[e]), e),
                    None => return Ok(d.out()),
                }
            }
            Tok::Object { .. } => match d.field(at, p) {
                Some(v) => ((v - 1, d.end[v]), v),
                None => return Ok(d.out()),
            },
            _ => return Ok(d.out()),
        };
        if level == last {
            return Ok(without(&d, at, &[cut]));
        }
        at = value;
    }
    Ok(d.out())
}

/// The scalar at the top, as `JsonbExtractScalar` finds it, or the kind of what is there instead.
fn top_scalar(d: &Doc) -> std::result::Result<&Tok, &'static str> {
    if d.is_raw() {
        Ok(&d.toks[1])
    } else {
        Err(d.toks[0].kind())
    }
}

fn cannot_cast(kind: &str, target: &str) -> Error {
    Error::new(format!("cannot cast jsonb {kind} to type {target}"))
}

/// jsonb cast to a number, as `jsonb_numeric`, `jsonb_int2`, `jsonb_int4`, `jsonb_int8`,
/// `jsonb_float4` and `jsonb_float8` cast it, `target` the type's name (`numeric`, `smallint`,
/// `integer`, `bigint`, `real`, `double precision`): only a number at the top, a JSON null there
/// none. An integer is the number rounded as `numeric_int4` rounds it, refused past its type; for
/// real and double precision the number itself, refused where `numeric_float4` or `numeric_float8`
/// refuses it.
pub fn jsonb_number(text: &str, target: &str) -> Result<Option<Numeric>> {
    if !matches!(
        target,
        "numeric" | "smallint" | "integer" | "bigint" | "real" | "double precision"
    ) {
        return Err(Error::new(format!("cannot cast type jsonb to {target}")));
    }
    let d = Doc::read(text)?;
    let n = match top_scalar(&d) {
        Ok(Tok::Null) => return Ok(None),
        Ok(Tok::Num(n)) => n,
        Ok(t) => return Err(cannot_cast(t.kind(), target)),
        Err(kind) => return Err(cannot_cast(kind, target)),
    };
    Ok(Some(match target {
        "smallint" => Numeric::of_int(n.to_integer(i16::MIN as i64, i16::MAX as i64, target)?),
        "integer" => Numeric::of_int(n.to_integer(i32::MIN as i64, i32::MAX as i64, target)?),
        "bigint" => Numeric::of_int(n.to_integer(i64::MIN, i64::MAX, target)?),
        "real" => {
            n.to_float(false)?;
            n.clone()
        }
        "double precision" => {
            n.to_float(true)?;
            n.clone()
        }
        _ => n.clone(),
    }))
}

/// jsonb cast to boolean, as `jsonb_bool` casts it: only a boolean at the top, a JSON null there
/// none.
pub fn jsonb_bool(text: &str) -> Result<Option<bool>> {
    let d = Doc::read(text)?;
    match top_scalar(&d) {
        Ok(Tok::Null) => Ok(None),
        Ok(Tok::Bool(b)) => Ok(Some(*b)),
        Ok(t) => Err(cannot_cast(t.kind(), "boolean")),
        Err(kind) => Err(cannot_cast(kind, "boolean")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    type Answer<'a> = std::result::Result<Option<&'a str>, &'a str>;
    type Checked<'a> = std::result::Result<(), &'a str>;
    type Text<'a> = std::result::Result<&'a str, &'a str>;

    fn path(p: &[Option<&str>]) -> Vec<Option<String>> {
        p.iter().map(|e| e.map(str::to_string)).collect()
    }

    fn got<T>(r: Result<T>) -> std::result::Result<T, String> {
        r.map_err(|e| e.to_string())
    }

    fn owned(w: Answer) -> std::result::Result<Option<String>, String> {
        w.map(|o| o.map(str::to_string)).map_err(str::to_string)
    }

    #[test]
    fn json_keeps_its_text_and_jsonb_writes_its_normal_text_as_postgresql_does() {
        // (input, json_in's answer, jsonb_in's and jsonb_out's), each PostgreSQL 19's; the first
        // ones the type table's values.
        let cases: &[(&str, Checked, Text)] = &[
            ("{\"a\":1}", Ok(()), Ok("{\"a\": 1}")),
            (
                "{\"b\":1, \"a\":2, \"a\":3}",
                Ok(()),
                Ok("{\"a\": 3, \"b\": 1}"),
            ),
            ("[1, 2 ,3]", Ok(()), Ok("[1, 2, 3]")),
            (" {\"a\" : 1} ", Ok(()), Ok("{\"a\": 1}")),
            ("1e1000000", Ok(()), Err("value overflows numeric format")),
            (
                "\"\\u0000\"",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "\"\\ud83d\"",
                Ok(()),
                Err("invalid input syntax for type json"),
            ),
            ("\"\\u0001\\n\\t\\/é \"", Ok(()), Ok("\"\\u0001\\n\\t/é \"")),
            ("null", Ok(()), Ok("null")),
            ("true", Ok(()), Ok("true")),
            ("false", Ok(()), Ok("false")),
            ("{}", Ok(()), Ok("{}")),
            ("[]", Ok(()), Ok("[]")),
            ("1.0", Ok(()), Ok("1.0")),
            ("1", Ok(()), Ok("1")),
            ("1.00", Ok(()), Ok("1.00")),
            ("0.0", Ok(()), Ok("0.0")),
            ("0", Ok(()), Ok("0")),
            ("[1.0]", Ok(()), Ok("[1.0]")),
            ("[1]", Ok(()), Ok("[1]")),
            ("{\"a\":[1.0,2]}", Ok(()), Ok("{\"a\": [1.0, 2]}")),
            ("{\"a\":[1,2.00]}", Ok(()), Ok("{\"a\": [1, 2.00]}")),
            ("\"a\"", Ok(()), Ok("\"a\"")),
            ("[[]]", Ok(()), Ok("[[]]")),
            ("{\"b\":1}", Ok(()), Ok("{\"b\": 1}")),
            ("{\"aa\":1}", Ok(()), Ok("{\"aa\": 1}")),
            ("{\"a\":1,\"b\":1}", Ok(()), Ok("{\"a\": 1, \"b\": 1}")),
            ("{\"c\":1}", Ok(()), Ok("{\"c\": 1}")),
            ("[1,2]", Ok(()), Ok("[1, 2]")),
            ("[3]", Ok(()), Ok("[3]")),
            ("[1,1]", Ok(()), Ok("[1, 1]")),
            ("[2]", Ok(()), Ok("[2]")),
            ("\"b\"", Ok(()), Ok("\"b\"")),
            ("\"aa\"", Ok(()), Ok("\"aa\"")),
            ("\"B\"", Ok(()), Ok("\"B\"")),
            ("\"é\"", Ok(()), Ok("\"é\"")),
            ("\"e\"", Ok(()), Ok("\"e\"")),
            ("\"NaN\"", Ok(()), Ok("\"NaN\"")),
            (
                "{\"aa\":1, \"b\":2, \"c\":{\"zz\":1,\"y\":2}}",
                Ok(()),
                Ok("{\"b\": 2, \"c\": {\"y\": 2, \"zz\": 1}, \"aa\": 1}"),
            ),
            ("1e2", Ok(()), Ok("100")),
            ("1.00e2", Ok(()), Ok("100")),
            ("-0", Ok(()), Ok("0")),
            ("-0.0", Ok(()), Ok("0.0")),
            ("0.10e1", Ok(()), Ok("1.0")),
            ("-1.5E-3", Ok(()), Ok("-0.0015")),
            ("1e-7", Ok(()), Ok("0.0000001")),
            ("1e131072", Ok(()), Err("value overflows numeric format")),
            ("1e-16384", Ok(()), Err("value overflows numeric format")),
            ("0.1e-16383", Ok(()), Err("value overflows numeric format")),
            (
                "TRUE",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "True",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "nul",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "nulls",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "truex",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "-",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "--1",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "01",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1.",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                ".1",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1e",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1e+",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1E-",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "+1",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "NaN",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "Infinity",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "-Infinity",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1_000",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1a",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "0x10",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[1,]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[,1]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[1 2]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{\"a\"}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{\"a\":}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{\"a\" 1}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{,}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{\"a\":1,}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{1:2}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{'a':1}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"abc",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"abc\\\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\x\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\u12\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\u12g4\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("\"😀\"", Ok(()), Ok("\"😀\"")),
            (
                "\"\\ude00\"",
                Ok(()),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\ud83d\\ud83d\"",
                Ok(()),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\ud83dx\"",
                Ok(()),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\ud83d\\n\"",
                Ok(()),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\ud83dA\"",
                Ok(()),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\\u0000x\"",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "\"x\\u0000\"",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "{\"\\u0000\":1}",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "{\"a\":\"\\u0000\"}",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "[\"\\u0000\", x]",
                Err("invalid input syntax for type json"),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "[1e-16384, x]",
                Err("invalid input syntax for type json"),
                Err("value overflows numeric format"),
            ),
            (
                "[1e-16384, \"\\u0000\"]",
                Ok(()),
                Err("value overflows numeric format"),
            ),
            (
                "[\"\\u0000\", 1e-16384]",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "[1e-16384 x]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("\"\u{2028}\"", Ok(()), Ok("\"\u{2028}\"")),
            ("\"\\/\"", Ok(()), Ok("\"/\"")),
            ("\"\\b\\f\\n\\r\\t\"", Ok(()), Ok("\"\\b\\f\\n\\r\\t\"")),
            (
                "\"\\u001f\\u007f\\u0080\"",
                Ok(()),
                Ok("\"\\u001f\u{7f}\u{80}\""),
            ),
            (
                "\"a\\u0000\"",
                Ok(()),
                Err("unsupported Unicode escape sequence"),
            ),
            (
                "[]]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("\"\"", Ok(()), Ok("\"\"")),
            (
                " ",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\t",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"a\"x",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"a\" \"b\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1 2",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "{} {}",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("[[[[[[[[[[1]]]]]]]]]]", Ok(()), Ok("[[[[[[[[[[1]]]]]]]]]]")),
            (
                "{\"a\":{\"a\":{\"a\":{\"a\":{}}}}}",
                Ok(()),
                Ok("{\"a\": {\"a\": {\"a\": {\"a\": {}}}}}"),
            ),
            ("{\"\":\"\"}", Ok(()), Ok("{\"\": \"\"}")),
            ("{\"\":1,\"\":2}", Ok(()), Ok("{\"\": 2}")),
            (
                "{\"a\":1,\"b\":2,\"a\":3,\"b\":4,\"a\":5}",
                Ok(()),
                Ok("{\"a\": 5, \"b\": 4}"),
            ),
            (
                "{\"aaa\":1,\"bb\":2,\"c\":3,\"\":4}",
                Ok(()),
                Ok("{\"\": 4, \"c\": 3, \"bb\": 2, \"aaa\": 1}"),
            ),
            (
                "{\"é\":1,\"e\":2,\"z\":3,\"ab\":4}",
                Ok(()),
                Ok("{\"e\": 2, \"z\": 3, \"ab\": 4, \"é\": 1}"),
            ),
            (
                "[null,true,false,1,\"a\",[],{}]",
                Ok(()),
                Ok("[null, true, false, 1, \"a\", [], {}]"),
            ),
            (
                "-12345678901234567890.12345678901234567890",
                Ok(()),
                Ok("-12345678901234567890.12345678901234567890"),
            ),
            (
                "0.0000000000000000000000001",
                Ok(()),
                Ok("0.0000000000000000000000001"),
            ),
            (
                "123456789012345678901234567890e-30",
                Ok(()),
                Ok("0.123456789012345678901234567890"),
            ),
            ("1E0", Ok(()), Ok("1")),
            ("1e-0", Ok(()), Ok("1")),
            ("-0e-0", Ok(()), Ok("0")),
            ("0e10", Ok(()), Ok("0")),
            ("0.0e10", Ok(()), Ok("0")),
            ("\"\u{ffff}\"", Ok(()), Ok("\"\u{ffff}\"")),
            ("\"\u{feff}\"", Ok(()), Ok("\"\u{feff}\"")),
            ("\"\u{10ffff}\"", Ok(()), Ok("\"\u{10ffff}\"")),
            ("\"\\\\u0000\"", Ok(()), Ok("\"\\\\u0000\"")),
            ("\"\\\\\"", Ok(()), Ok("\"\\\\\"")),
            (
                "{\"a\":1, \"b\":[1,2,{\"c\":null}], \"d\":\"x\\ny\"}",
                Ok(()),
                Ok("{\"a\": 1, \"b\": [1, 2, {\"c\": null}], \"d\": \"x\\ny\"}"),
            ),
            (
                "\"\t\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\"\u{1}\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "\t[1,",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "2]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[1,\u{c}2]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[1,\u{b}2]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("\"\u{a0}\"", Ok(()), Ok("\"\u{a0}\"")),
            (
                "\u{a0}[1]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "é",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "[é]",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            (
                "1é\"\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("\"abc\"", Ok(()), Ok("\"abc\"")),
            (
                "\"abc\ndef\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
            ("\"\\n\\\"\\\\\"", Ok(()), Ok("\"\\n\\\"\\\\\"")),
            (
                "\"\\v\"",
                Err("invalid input syntax for type json"),
                Err("invalid input syntax for type json"),
            ),
        ];
        for &(s, json, jsonb) in cases {
            assert_eq!(got(json_check(s)), json.map_err(str::to_string), "{s:?}");
            assert_eq!(
                got(jsonb_in(s)),
                jsonb.map(str::to_string).map_err(str::to_string),
                "{s:?}"
            );
            if let Ok(out) = jsonb {
                assert_eq!(jsonb_in(out).unwrap(), out, "{s:?}");
            }
        }
    }

    #[test]
    fn a_number_is_read_at_any_length_numeric_in_reads() {
        assert_eq!(jsonb_in("1e400").unwrap(), format!("1{}", "0".repeat(400)));
        assert_eq!(jsonb_in("[1e131071]").unwrap().len(), 131074);
        assert_eq!(
            jsonb_in("1e-16383").unwrap(),
            format!("0.{}1", "0".repeat(16382))
        );
        assert_eq!(
            got(jsonb_in("[1e131072]")),
            Err("value overflows numeric format".to_string())
        );
        // A refusal is met where the reading meets it: a number's after the token that follows it.
        assert_eq!(
            got(jsonb_in("[1e-16384 x]")),
            Err("invalid input syntax for type json".to_string())
        );
        assert_eq!(
            got(jsonb_in("[1e-16384, \"\\u0000\"]")),
            Err("value overflows numeric format".to_string())
        );
        assert_eq!(
            got(jsonb_in("[1e-16384 \"\\u0000\"]")),
            Err("unsupported Unicode escape sequence".to_string())
        );
    }

    #[test]
    fn jsonb_keys_sort_the_type_table_as_postgresql_orders_it() {
        // The type table's values in PostgreSQL 19's ORDER BY, the values of a group equal.
        let groups: &[&[&str]] = &[
            &["[]"],
            &["null"],
            &["\"B\""],
            &["\"NaN\""],
            &["\"a\""],
            &["\"aa\""],
            &["\"b\""],
            &["\"e\""],
            &["\"é\""],
            &["-1.5E-3"],
            &["0.0", "0", "-0", "-0.0"],
            &["1e-7"],
            &["1.0", "1", "1.00", "0.10e1"],
            &["1e2", "1.00e2"],
            &["1e400"],
            &["false"],
            &["true"],
            &["[1.0]", "[1]"],
            &["[2]"],
            &["[3]"],
            &["[[]]"],
            &["[1,1]"],
            &["[1,2]"],
            &["{}"],
            &["{\"a\":1}"],
            &["{\"a\":[1.0,2]}", "{\"a\":[1,2.00]}"],
            &["{\"aa\":1}"],
            &["{\"b\":1}"],
            &["{\"c\":1}"],
            &["{\"a\":1,\"b\":1}"],
            &["{\"b\":1, \"a\":2, \"a\":3}"],
            &["{\"aa\":1, \"b\":2, \"c\":{\"zz\":1,\"y\":2}}"],
        ];
        let keys: Vec<Vec<Vec<u8>>> = groups
            .iter()
            .map(|g| g.iter().map(|s| jsonb_key(&jsonb_in(s).unwrap())).collect())
            .collect();
        for (g, k) in groups.iter().zip(&keys) {
            assert!(k.windows(2).all(|w| w[0] == w[1]), "{g:?}");
        }
        for (g, k) in groups.windows(2).zip(keys.windows(2)) {
            assert!(k[0][0] < k[1][0], "{g:?}");
        }
    }

    /// `compareJsonbContainers` over two token walks, as jsonb_util.c writes it: tokens compared in
    /// turn; of two containers of one kind, an array's count overriding its raw-scalar test and an
    /// object's count; of two scalars of one kind, strings by `varstr_cmp` under a collation of
    /// byte order, numbers by `cmp_numerics`; of two kinds, their codes.
    fn walk_cmp(a: &[Tok], b: &[Tok]) -> Ordering {
        let code = |t: &Tok| match t {
            Tok::Null => 0,
            Tok::Str(_) => 1,
            Tok::Num(_) => 2,
            Tok::Bool(_) => 3,
            Tok::Array { .. } => 0x10,
            _ => 0x11,
        };
        for (x, y) in a.iter().zip(b) {
            let res = match (x, y) {
                (Tok::EndArray, Tok::EndArray) | (Tok::EndObject, Tok::EndObject) => continue,
                (Tok::Key(p), Tok::Key(q)) | (Tok::Str(p), Tok::Str(q)) => {
                    p.as_bytes().cmp(q.as_bytes())
                }
                (Tok::Num(p), Tok::Num(q)) => p.cmp(q),
                (Tok::Bool(p), Tok::Bool(q)) => p.cmp(q),
                (Tok::Null, Tok::Null) => Ordering::Equal,
                (Tok::Array { count: n, raw: r }, Tok::Array { count: m, raw: s }) => {
                    let mut res = Ordering::Equal;
                    if r != s {
                        res = if *r {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        };
                    }
                    if n != m {
                        res = n.cmp(m);
                    }
                    res
                }
                (Tok::Object { count: n }, Tok::Object { count: m }) => n.cmp(m),
                _ => code(x).cmp(&code(y)),
            };
            if res != Ordering::Equal {
                return res;
            }
        }
        Ordering::Equal
    }

    /// A seeded spread of jsonb texts over few values: numbers of one value at several scales, and
    /// strings and keys holding control characters, whose bytes lie below every code of a kind.
    fn spread(n: usize, mut x: u64) -> Vec<String> {
        const NUMS: &[&str] = &[
            "0", "0.0", "-0.00", "1", "1.0", "1.00", "2", "-1", "0.5", "0.50",
        ];
        const STRS: &[&str] = &[
            "\"\"",
            "\"a\"",
            "\"b\"",
            "\"aa\"",
            "\"é\"",
            "\"1\"",
            "\"a\\u0001\"",
            "\"\\u0002x\"",
        ];
        const KEYS: &[&str] = &[
            "\"a\"",
            "\"b\"",
            "\"aa\"",
            "\"\"",
            "\"a\\u0001\"",
            "\"\\u0001\"",
            "\"\\u0011\"",
        ];
        let mut next = move |m: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % m as u64) as usize
        };
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            // Each value written by a list of what is still to write.
            let mut s = String::new();
            let mut todo: Vec<(usize, Option<&str>)> = vec![(3, None)];
            while let Some((depth, text)) = todo.pop() {
                if let Some(t) = text {
                    s.push_str(t);
                    continue;
                }
                match next(if depth == 0 { 6 } else { 9 }) {
                    0 => s.push_str("null"),
                    1 => s.push_str(["true", "false"][next(2)]),
                    2 | 3 => s.push_str(NUMS[next(NUMS.len())]),
                    4 | 5 => s.push_str(STRS[next(STRS.len())]),
                    k => {
                        let object = k == 8;
                        s.push(if object { '{' } else { '[' });
                        todo.push((0, Some(if object { "}" } else { "]" })));
                        let m = next(3);
                        for i in (0..m).rev() {
                            todo.push((depth - 1, None));
                            if object {
                                todo.push((0, Some(":")));
                                todo.push((0, Some(KEYS[next(KEYS.len())])));
                            }
                            if i > 0 {
                                todo.push((0, Some(",")));
                            }
                        }
                    }
                }
            }
            out.push(s);
        }
        out
    }

    #[test]
    fn a_jsonb_key_sorts_as_the_walk_compares_over_every_pair_of_a_seeded_spread() {
        let texts: Vec<String> = spread(900, 0x2545_f491_4f6c_dd1d)
            .iter()
            .map(|s| jsonb_in(s).unwrap())
            .collect();
        let docs: Vec<Doc> = texts.iter().map(|t| Doc::read(t).unwrap()).collect();
        let keys: Vec<Vec<u8>> = texts.iter().map(|t| jsonb_key(t)).collect();
        let mut ties = 0;
        for i in 0..texts.len() {
            for j in 0..texts.len() {
                let walk = walk_cmp(&docs[i].toks, &docs[j].toks);
                assert_eq!(keys[i].cmp(&keys[j]), walk, "{} {}", texts[i], texts[j]);
                if walk == Ordering::Equal && texts[i] != texts[j] {
                    ties += 1;
                }
            }
        }
        // Values equal and written differently are among them.
        assert!(ties > 100, "{ties}");
    }

    #[test]
    fn the_json_getters_answer_as_postgresql_s() {
        type Getter = fn(&str, &str) -> Result<Option<String>>;
        let by_key: &[(Getter, &str, &str, Answer)] = &[
            (json_field, "{\"a\":1,\"a\":2}", "a", Ok(Some("2"))),
            (
                json_field,
                "{\"a\" : [1, 2] , \"b\":1}",
                "a",
                Ok(Some("[1, 2]")),
            ),
            (json_field, "{\"a\":null}", "a", Ok(Some("null"))),
            (json_field_text, "{\"a\":null}", "a", Ok(None)),
            (json_field_text, "{\"a\":\"x\\ny\"}", "a", Ok(Some("x\ny"))),
            (json_field, "{\"a\":\"é\"}", "a", Ok(Some("\"é\""))),
            (json_field_text, "{\"a\":\"é\"}", "a", Ok(Some("é"))),
            (json_field, "[1]", "a", Ok(None)),
            (
                json_field,
                "{\"a\":1,\"b\":\"\\u0000\"}",
                "a",
                Err("unsupported Unicode escape sequence"),
            ),
            (
                json_field,
                "{\"a\":1,\"b\":\"\\ud800\"}",
                "a",
                Err("invalid input syntax for type json"),
            ),
            (json_field, "\"a\"", "a", Ok(None)),
            (json_field, "{\"a\":{\"b\":1}}", "b", Ok(None)),
            (json_field, "{\"a\":1e2}", "a", Ok(Some("1e2"))),
            (json_field_text, "{\"a\":1e2}", "a", Ok(Some("1e2"))),
            (
                json_field_text,
                "{\"a\":[1, \"x\"]}",
                "a",
                Ok(Some("[1, \"x\"]")),
            ),
            (jsonb_field, "{\"a\": null}", "a", Ok(Some("null"))),
            (jsonb_field_text, "{\"a\": null}", "a", Ok(None)),
            (jsonb_field, "[1]", "a", Ok(None)),
            (jsonb_field, "\"a\"", "a", Ok(None)),
            (jsonb_field_text, "{\"a\": \"é\\n\"}", "a", Ok(Some("é\n"))),
            (
                jsonb_field_text,
                "{\"a\": [1, \"x\"]}",
                "a",
                Ok(Some("[1, \"x\"]")),
            ),
            (jsonb_field, "{\"a\": 1.50}", "a", Ok(Some("1.50"))),
            (
                json_field,
                "{\"a\":\"\\u00e9\", \"b\": \"\\ud83d\\ude00\"}",
                "a",
                Ok(Some("\"\\u00e9\"")),
            ),
            (
                json_field_text,
                "{\"a\":\"\\u00e9\", \"b\": \"\\ud83d\\ude00\"}",
                "b",
                Ok(Some("😀")),
            ),
            (json_field, "{\"\\u0061\":1, \"a\":2}", "a", Ok(Some("2"))),
        ];
        for &(f, doc, key, want) in by_key {
            assert_eq!(got(f(doc, key)), owned(want), "{doc:?} {key:?}");
        }
        type ByIndex = fn(&str, i64) -> Result<Option<String>>;
        let by_index: &[(ByIndex, &str, i64, Answer)] = &[
            (json_element, "[1, [2, 3] ,4]", 1, Ok(Some("[2, 3]"))),
            (json_element, "[1, [2, 3] ,4]", -1, Ok(Some("4"))),
            (json_element, "[1, [2, 3] ,4]", -3, Ok(Some("1"))),
            (json_element, "[1, [2, 3] ,4]", -4, Ok(None)),
            (json_element, "[1, [2, 3] ,4]", 3, Ok(None)),
            (json_element, "{\"0\":1}", 0, Ok(None)),
            (json_element, "[null]", 0, Ok(Some("null"))),
            (json_element_text, "[null]", 0, Ok(None)),
            (json_element_text, "[\"a\\tb\"]", 0, Ok(Some("a\tb"))),
            (json_element, "1", 0, Ok(None)),
            (json_element, "[[1, 2], [3]]", -1, Ok(Some("[3]"))),
            (jsonb_element, "\"a\"", 0, Ok(Some("\"a\""))),
            (jsonb_element, "\"a\"", -1, Ok(Some("\"a\""))),
            (jsonb_element_text, "\"a\"", 0, Ok(Some("a"))),
            (jsonb_element, "\"a\"", 1, Ok(None)),
            (jsonb_element, "[1, [2]]", -1, Ok(Some("[2]"))),
            (jsonb_element_text, "[1, [2]]", -1, Ok(Some("[2]"))),
            (jsonb_element, "[1, [2]]", -3, Ok(None)),
            (jsonb_element, "{\"a\": 1}", 0, Ok(None)),
            (jsonb_element_text, "[null]", 0, Ok(None)),
            (jsonb_element, "[null]", 0, Ok(Some("null"))),
            (json_element, "[1]", 1 << 40, Err("integer out of range")),
            (
                jsonb_element,
                "[1]",
                -(1 << 40),
                Err("integer out of range"),
            ),
        ];
        for &(f, doc, i, want) in by_index {
            assert_eq!(got(f(doc, i)), owned(want), "{doc:?} {i}");
        }
        type ByPath = fn(&str, &[Option<String>]) -> Result<Option<String>>;
        let by_path: &[(ByPath, &str, &[Option<&str>], Answer)] = &[
            (json_path, "  \"a\"  ", &[], Ok(Some("  \"a\""))),
            (json_path_text, "  \"a\"  ", &[], Ok(Some("a"))),
            (json_path, "  12  ", &[], Ok(Some("  12"))),
            (json_path, " [1, 2] ", &[], Ok(Some("[1, 2]"))),
            (json_path_text, "null", &[], Ok(None)),
            (
                json_path,
                "{\"x\":[1,2,3],\"a\":[4,5]}",
                &[Some("a"), Some("-1")],
                Ok(None),
            ),
            (
                json_path,
                "{\"a\":[4,5]}",
                &[Some("a"), Some("-1")],
                Ok(Some("5")),
            ),
            (
                json_path,
                "{\"a\":[4,5],\"x\":[1,2,3]}",
                &[Some("a"), Some("-1")],
                Ok(Some("5")),
            ),
            (
                json_path,
                "{\"a\":{\"b\":[1,{\"c\":true}]}}",
                &[Some("a"), Some("b"), Some("1"), Some("c")],
                Ok(Some("true")),
            ),
            (
                json_path,
                "{\"a\":{\"b\":[1,{\"c\":true}]}}",
                &[Some("a"), None],
                Ok(None),
            ),
            (
                json_path,
                "[{\"1\":\"x\"}]",
                &[Some("0"), Some("1")],
                Ok(Some("\"x\"")),
            ),
            (
                json_path,
                "{\"1\":[7]}",
                &[Some("1"), Some("0")],
                Ok(Some("7")),
            ),
            (json_path, "[5,6]", &[Some(" 1")], Ok(Some("6"))),
            (json_path, "[5,6]", &[Some("+1")], Ok(Some("6"))),
            (json_path, "[5,6]", &[Some("1 ")], Ok(None)),
            (json_path, "[5,6]", &[Some("")], Ok(None)),
            (
                json_path_text,
                "{\"a\":{\"b\":\"\\\"q\\\"\"}}",
                &[Some("a"), Some("b")],
                Ok(Some("\"q\"")),
            ),
            (
                json_path_text,
                "{\"a\":{\"b\":null}}",
                &[Some("a"), Some("b")],
                Ok(None),
            ),
            (
                json_path,
                "{\"a\":{\"b\":null}}",
                &[Some("a"), Some("b")],
                Ok(Some("null")),
            ),
            (jsonb_path, "\"a\"", &[], Ok(Some("\"a\""))),
            (jsonb_path_text, "\"a\"", &[], Ok(Some("a"))),
            (jsonb_path_text, "null", &[], Ok(None)),
            (jsonb_path, "\"a\"", &[Some("0")], Ok(None)),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), Some("1"), Some("b")],
                Ok(Some("2")),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), Some("-1"), Some("b")],
                Ok(Some("2")),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), Some("-3")],
                Ok(None),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), Some("x")],
                Ok(None),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), Some(" 1"), Some("b")],
                Ok(Some("2")),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), Some("1 ")],
                Ok(None),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[Some("a"), None],
                Ok(None),
            ),
            (
                jsonb_path,
                "{\"a\": [1, {\"b\": 2}]}",
                &[],
                Ok(Some("{\"a\": [1, {\"b\": 2}]}")),
            ),
            (
                jsonb_path_text,
                "{\"a\": [1, {\"b\": 2}]}",
                &[],
                Ok(Some("{\"a\": [1, {\"b\": 2}]}")),
            ),
            (
                jsonb_path_text,
                "{\"a\": [1, {\"b\": \"x\"}]}",
                &[Some("a"), Some("1"), Some("b")],
                Ok(Some("x")),
            ),
            (jsonb_path, "{\"a\": 1}", &[Some("a"), Some("b")], Ok(None)),
        ];
        for &(f, doc, p, want) in by_path {
            assert_eq!(got(f(doc, &path(p))), owned(want), "{doc:?} {p:?}");
        }
    }

    #[test]
    fn the_jsonb_tests_answer_as_postgresql_s() {
        type Test = fn(&str, &str) -> Result<bool>;
        let pairs: &[(Test, &str, &str, bool)] = &[
            (jsonb_contains, "[1, 2, [3, 4]]", "[[4]]", true),
            (jsonb_contains, "[1, 2]", "1", true),
            (jsonb_contains, "1", "[1]", false),
            (jsonb_contains, "{\"a\": 1}", "{}", true),
            (jsonb_contains, "[]", "[]", true),
            (jsonb_contains, "[1]", "[]", true),
            (
                jsonb_contains,
                "{\"a\": [1, 2]}",
                "{\"a\": [2, 2, 1]}",
                true,
            ),
            (jsonb_contains, "{\"a\": {\"b\": 1}}", "{\"a\": []}", false),
            (
                jsonb_contains,
                "[{\"a\": 1}, {\"b\": 2}]",
                "[{\"a\": 1, \"b\": 2}]",
                false,
            ),
            (
                jsonb_contains,
                "[{\"a\": 1, \"b\": 2}]",
                "[{\"b\": 2}]",
                true,
            ),
            (jsonb_contains, "\"a\"", "\"a\"", true),
            (jsonb_contains, "{\"a\": 1}", "[]", false),
            (jsonb_contains, "[1.0]", "[1]", true),
            (jsonb_contains, "[[1]]", "[1]", false),
            (jsonb_contains, "[1]", "[[1]]", false),
            (
                jsonb_contains,
                "{\"a\": 1, \"b\": {\"c\": [1, {\"d\": null}]}}",
                "{\"b\": {\"c\": [{}]}}",
                true,
            ),
            (jsonb_contains, "{\"a\": 1}", "{\"a\": 1, \"b\": 2}", false),
            (jsonb_exists, "\"foo\"", "foo", true),
            (jsonb_exists, "[1, \"1\"]", "1", true),
            (jsonb_exists, "{\"a\": {\"b\": 1}}", "b", false),
            (jsonb_exists, "{\"a\": null}", "a", true),
        ];
        for &(f, a, b, want) in pairs {
            assert_eq!(f(a, b).unwrap(), want, "{a:?} {b:?}");
        }
        type TestKeys = fn(&str, &[Option<String>]) -> Result<bool>;
        let by_keys: &[(TestKeys, &str, &[Option<&str>], bool)] = &[
            (jsonb_exists_any, "[\"a\"]", &[None, Some("a")], true),
            (jsonb_exists_all, "[\"a\"]", &[None], true),
            (jsonb_exists_all, "[\"a\"]", &[], true),
            (jsonb_exists_any, "[\"a\"]", &[], false),
            (
                jsonb_exists_all,
                "{\"a\": 1, \"b\": 2}",
                &[Some("a"), Some("b")],
                true,
            ),
            (
                jsonb_exists_all,
                "{\"a\": 1, \"b\": 2}",
                &[Some("a"), Some("c")],
                false,
            ),
        ];
        for &(f, doc, keys, want) in by_keys {
            assert_eq!(f(doc, &path(keys)).unwrap(), want, "{doc:?} {keys:?}");
        }
    }

    #[test]
    fn the_jsonb_edits_answer_as_postgresql_s() {
        type Edit = fn(&str, &str) -> Result<String>;
        let pairs: &[(Edit, &str, &str, Text)] = &[
            (jsonb_concat, "1", "2", Ok("[1, 2]")),
            (jsonb_concat, "[]", "1", Ok("[1]")),
            (jsonb_concat, "1", "[]", Ok("[1]")),
            (jsonb_concat, "{}", "[]", Ok("[{}]")),
            (jsonb_concat, "[]", "{}", Ok("[{}]")),
            (
                jsonb_concat,
                "{\"a\": 1, \"b\": 2}",
                "{\"a\": 3}",
                Ok("{\"a\": 3, \"b\": 2}"),
            ),
            (jsonb_concat, "{\"a\": 1}", "[1]", Ok("[{\"a\": 1}, 1]")),
            (
                jsonb_concat,
                "\"x\"",
                "{\"a\": 1}",
                Ok("[\"x\", {\"a\": 1}]"),
            ),
            (jsonb_concat, "[1]", "[[2]]", Ok("[1, [2]]")),
            (jsonb_concat, "{}", "{}", Ok("{}")),
            (jsonb_concat, "[]", "[]", Ok("[]")),
            (
                jsonb_concat,
                "{\"b\": 1}",
                "{\"aa\": 2, \"a\": 3}",
                Ok("{\"a\": 3, \"b\": 1, \"aa\": 2}"),
            ),
            (
                jsonb_delete_key,
                "\"a\"",
                "a",
                Err("cannot delete from scalar"),
            ),
            (jsonb_delete_key, "[\"a\", 1, \"a\"]", "a", Ok("[1]")),
            (
                jsonb_delete_key,
                "{\"a\": 1, \"b\": 2}",
                "a",
                Ok("{\"b\": 2}"),
            ),
            (jsonb_delete_key, "[]", "a", Ok("[]")),
            (jsonb_delete_key, "{\"a\": {\"a\": 1}}", "a", Ok("{}")),
            (jsonb_delete_key, "[[\"a\"]]", "a", Ok("[[\"a\"]]")),
        ];
        for &(f, a, b, want) in pairs {
            assert_eq!(
                got(f(a, b)),
                want.map(str::to_string).map_err(str::to_string),
                "{a:?} {b:?}"
            );
        }
        type EditIndex = fn(&str, i64) -> Result<String>;
        let by_index: &[(EditIndex, &str, i64, Text)] = &[
            (
                jsonb_delete_index,
                "\"a\"",
                0,
                Err("cannot delete from scalar"),
            ),
            (
                jsonb_delete_index,
                "{\"a\": 1}",
                0,
                Err("cannot delete from object using integer index"),
            ),
            (jsonb_delete_index, "[1, 2, 3]", -1, Ok("[1, 2]")),
            (jsonb_delete_index, "[1, 2, 3]", -3, Ok("[2, 3]")),
            (jsonb_delete_index, "[1, 2, 3]", -4, Ok("[1, 2, 3]")),
            (jsonb_delete_index, "[1, 2, 3]", 3, Ok("[1, 2, 3]")),
            (jsonb_delete_index, "[]", 0, Ok("[]")),
            (
                jsonb_delete_index,
                "{}",
                0,
                Err("cannot delete from object using integer index"),
            ),
        ];
        for &(f, doc, i, want) in by_index {
            assert_eq!(
                got(f(doc, i)),
                want.map(str::to_string).map_err(str::to_string),
                "{doc:?} {i}"
            );
        }
        type EditPath = fn(&str, &[Option<String>]) -> Result<String>;
        let by_path: &[(EditPath, &str, &[Option<&str>], Text)] = &[
            (
                jsonb_delete_keys,
                "{\"a\": 1, \"b\": 2, \"c\": 3}",
                &[Some("a"), None, Some("c")],
                Ok("{\"b\": 2}"),
            ),
            (
                jsonb_delete_keys,
                "\"a\"",
                &[],
                Err("cannot delete from scalar"),
            ),
            (jsonb_delete_keys, "[]", &[None], Ok("[]")),
            (
                jsonb_delete_keys,
                "[\"a\", \"b\"]",
                &[],
                Ok("[\"a\", \"b\"]"),
            ),
            (
                jsonb_delete_path,
                "1",
                &[],
                Err("cannot delete path in scalar"),
            ),
            (
                jsonb_delete_path,
                "{\"a\": 1}",
                &[Some("a"), None],
                Err("path element at position 2 is null"),
            ),
            (
                jsonb_delete_path,
                "{\"a\": 1}",
                &[Some("b"), None],
                Ok("{\"a\": 1}"),
            ),
            (
                jsonb_delete_path,
                "{\"a\": []}",
                &[Some("a"), Some("x")],
                Err("path element at position 2 is not an integer: \"x\""),
            ),
            (jsonb_delete_path, "{}", &[None], Ok("{}")),
            (jsonb_delete_path, "[]", &[Some("x")], Ok("[]")),
            (jsonb_delete_path, "{\"a\": 1}", &[], Ok("{\"a\": 1}")),
            (
                jsonb_delete_path,
                "{\"a\": 1}",
                &[None],
                Err("path element at position 1 is null"),
            ),
            (
                jsonb_delete_path,
                "[1, [2, 3]]",
                &[Some("1"), Some("-1")],
                Ok("[1, [2]]"),
            ),
            (jsonb_delete_path, "[1, 2]", &[Some("-3")], Ok("[1, 2]")),
            (
                jsonb_delete_path,
                "{\"a\": {\"b\": {\"c\": 1}}}",
                &[Some("a"), Some("b"), Some("c")],
                Ok("{\"a\": {\"b\": {}}}"),
            ),
            (
                jsonb_delete_path,
                "[1, 2]",
                &[Some("x")],
                Err("path element at position 1 is not an integer: \"x\""),
            ),
            (
                jsonb_delete_path,
                "{\"a\": [1]}",
                &[Some("a"), Some("0"), Some("b")],
                Ok("{\"a\": [1]}"),
            ),
            (
                jsonb_delete_path,
                "{\"a\": [1]}",
                &[Some("a"), Some("0"), None],
                Err("path element at position 3 is null"),
            ),
            (jsonb_delete_path, "[1, 2]", &[Some("-1")], Ok("[1]")),
            (jsonb_delete_path, "[1, 2]", &[Some(" -1")], Ok("[1]")),
        ];
        for &(f, doc, p, want) in by_path {
            assert_eq!(
                got(f(doc, &path(p))),
                want.map(str::to_string).map_err(str::to_string),
                "{doc:?} {p:?}"
            );
        }
    }

    #[test]
    fn a_jsonb_casts_to_a_number_or_a_boolean_as_postgresql_s_casts_do() {
        let n = |text: &str, target: &str| {
            got(jsonb_number(text, target)).map(|n| n.map(|n| n.to_string()))
        };
        let all = [
            "numeric",
            "smallint",
            "integer",
            "bigint",
            "real",
            "double precision",
        ];
        for t in all {
            assert_eq!(n("null", t), Ok(None));
            for (text, kind) in [
                ("\"1\"", "string"),
                ("[]", "array"),
                ("{}", "object"),
                ("true", "boolean"),
            ] {
                assert_eq!(
                    n(text, t),
                    Err(format!("cannot cast jsonb {kind} to type {t}"))
                );
            }
        }
        // (text, numeric, smallint, integer, bigint): an integer rounded half away from zero.
        let integers: &[(&str, &str, &str, &str, &str)] = &[
            ("1.5", "1.5", "2", "2", "2"),
            ("-2.5", "-2.5", "-3", "-3", "-3"),
            ("32767.4", "32767.4", "32767", "32767", "32767"),
            ("32768", "32768", "!smallint", "32768", "32768"),
            ("-32768.5", "-32768.5", "!smallint", "-32769", "-32769"),
            (
                "2147483648",
                "2147483648",
                "!smallint",
                "!integer",
                "2147483648",
            ),
            (
                "-2147483648.5",
                "-2147483648.5",
                "!smallint",
                "!integer",
                "-2147483649",
            ),
            (
                "9223372036854775808",
                "9223372036854775808",
                "!smallint",
                "!integer",
                "!bigint",
            ),
        ];
        for &(text, num, small, int, big) in integers {
            for (t, want) in all.iter().zip([num, small, int, big]) {
                let want = match want.strip_prefix('!') {
                    Some(name) => Err(format!("{name} out of range")),
                    None => Ok(Some(want.to_string())),
                };
                assert_eq!(n(text, t), want, "{text} {t}");
            }
        }
        // A float is the number itself, refused where float4in or float8in refuses its digits.
        assert_eq!(
            n("3.5e38", "double precision"),
            Ok(Some("350000000000000000000000000000000000000".into()))
        );
        assert_eq!(
            n("3.5e38", "real"),
            Err("\"350000000000000000000000000000000000000\" is out of range for type real".into())
        );
        assert_eq!(
            n("1e-46", "real"),
            Err("\"0.0000000000000000000000000000000000000000000001\" is out of range for type real".into())
        );
        assert!(n("1.8e308", "double precision").is_err());
        assert!(n("1e-400", "double precision").is_err());
        assert_eq!(n("1", "text"), Err("cannot cast type jsonb to text".into()));
        let b = |text: &str| got(jsonb_bool(text));
        assert_eq!(b("null"), Ok(None));
        assert_eq!(b("true"), Ok(Some(true)));
        assert_eq!(b("false"), Ok(Some(false)));
        assert_eq!(
            b("1"),
            Err("cannot cast jsonb numeric to type boolean".into())
        );
        assert_eq!(
            b("\"true\""),
            Err("cannot cast jsonb string to type boolean".into())
        );
        assert_eq!(
            b("[true]"),
            Err("cannot cast jsonb array to type boolean".into())
        );
        assert_eq!(
            b("{}"),
            Err("cannot cast jsonb object to type boolean".into())
        );
    }

    #[test]
    fn nesting_is_read_to_any_depth_in_a_list_rather_than_in_calls() {
        // PostgreSQL reads 16,000 levels under its default stack; read here as there, and far past.
        for n in [16_000, 200_000] {
            let unsorted = format!("{}1{}", "{\"b\":".repeat(n), ",\"a\":1}".repeat(n));
            let out = jsonb_in(&unsorted).unwrap();
            if n == 16_000 {
                assert!(out.starts_with("{\"a\": 1, \"b\": {\"a\": 1, \"b\": "));
                assert!(out.ends_with(&format!(": 1{}", "}".repeat(n))));
            }
            assert!(!jsonb_key(&out).is_empty());
            assert!(jsonb_contains(&out, &out).unwrap());
            let arrays = format!("{}1{}", "[".repeat(n), "]".repeat(n));
            json_check(&arrays).unwrap();
            assert_eq!(
                json_path(&arrays, &path(&[Some("0"), Some("-1")]))
                    .unwrap()
                    .map(|s| s.len()),
                Some(2 * n - 3)
            );
        }
    }
}
