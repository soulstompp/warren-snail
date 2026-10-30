//! xml, read and written as PostgreSQL built with libxml reads and writes it under `xmloption`
//! content: `xml_in` keeps the text as written once `xml_parse` takes it, and `xml_out` writes it
//! back with its XML declaration reduced as `xml_out_internal` reduces it. A value whose content
//! holds a DOCTYPE is read as a whole document, its internal subset's declarations and entities
//! among it.
//!
//! What the server takes and this reading refuses, each refused by name: an encoding declared
//! before a DOCTYPE that is not in [`ENCODINGS`], a parameter entity whose text holds `%`, a value
//! whose entities expand past [`EXPANDED_BYTES`] bytes or [`EXPANDED_REFERENCES`] references, a
//! start tag with attributes or a DOCTYPE ending past byte [`LOOKUP_BYTES`], and a name past
//! [`NAME_BYTES`] bytes whose part after its first colon is not a plain local part.

use crate::{Error, Result};
use std::collections::{HashMap, HashSet};

/// The refusal of a value that is not well-formed, as `xml_parse` words it.
const INVALID: &str = "invalid XML content";

/// The refusal of a declaration `parse_xml_decl` does not read.
const INVALID_DECL: &str = "invalid XML content: invalid XML declaration";

/// The most bytes a name holds, a QName's prefix and local part each.
pub const NAME_BYTES: usize = 50_000;

/// The most bytes a comment, a processing instruction's data or a CDATA section holds.
pub const SECTION_BYTES: usize = 10_000_000;

/// The byte a start tag with attributes, and a DOCTYPE, end by at the latest.
pub const LOOKUP_BYTES: usize = 10_000_000;

/// The most elements open at once in content; a document's root is one more.
pub const ELEMENT_DEPTH: usize = 256;

/// The most groups open at once in an element declaration's content model.
pub const MODEL_DEPTH: usize = 128;

/// The deepest entities nest: an entity read as content counts two, one read in an attribute one.
pub const ENTITY_DEPTH: usize = 40;

/// The most bytes of entity text one value reads, every expansion counted.
pub const EXPANDED_BYTES: usize = 1_000_000;

/// The most entity references one value expands.
pub const EXPANDED_REFERENCES: usize = 100_000;

/// The encodings a declaration may name before a DOCTYPE, upper-cased: the names the server was
/// seen to know.
pub const ENCODINGS: &[&str] = &[
    "ANSI_X3.4-1968",
    "ARABIC",
    "ARMSCII-8",
    "ASCII",
    "ASMO-708",
    "BIG5",
    "BIG5-HKSCS",
    "CESU-8",
    "CP1250",
    "CP1251",
    "CP1252",
    "CP1253",
    "CP1254",
    "CP1255",
    "CP1256",
    "CP1257",
    "CP1258",
    "CP437",
    "CP819",
    "CP850",
    "CP852",
    "CP866",
    "CP874",
    "CP932",
    "CP936",
    "CP949",
    "CP950",
    "CSISOLATIN1",
    "CYRILLIC",
    "EBCDIC-US",
    "ECMA-114",
    "EUC-CN",
    "EUC-JP",
    "EUCJP",
    "EUC-KR",
    "EUC-TW",
    "GB18030",
    "GB2312",
    "GBK",
    "GEORGIAN-PS",
    "GREEK",
    "HEBREW",
    "HTML",
    "HZ",
    "IBM037",
    "IBM437",
    "IBM819",
    "IBM850",
    "ISO-10646-UCS-2",
    "ISO-10646-UCS-4",
    "ISO-2022-CN",
    "ISO-2022-JP",
    "ISO-2022-KR",
    "ISO-8859-1",
    "ISO_8859-1",
    "ISO8859-1",
    "ISO88591",
    "ISO-8859-10",
    "ISO-8859-11",
    "ISO-8859-13",
    "ISO-8859-14",
    "ISO-8859-15",
    "ISO_8859-15",
    "ISO-8859-16",
    "ISO-8859-2",
    "ISO-8859-3",
    "ISO-8859-4",
    "ISO-8859-5",
    "ISO-8859-6",
    "ISO-8859-7",
    "ISO-8859-8",
    "ISO-8859-9",
    "ISO-IR-100",
    "JOHAB",
    "KOI8-R",
    "KOI8-T",
    "KOI8-U",
    "L1",
    "LATIN1",
    "LATIN10",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "MAC",
    "MACINTOSH",
    "MS-ANSI",
    "PT154",
    "SHIFT-JIS",
    "SHIFT_JIS",
    "SHIFT_JISX0213",
    "SJIS",
    "TCVN",
    "TIS-620",
    "UCS-2",
    "UCS2",
    "UCS-2BE",
    "UCS-2LE",
    "UCS-4",
    "UCS4",
    "UCS-4BE",
    "UCS-4LE",
    "UHC",
    "UNICODE",
    "UNICODEBIG",
    "UNICODELITTLE",
    "US-ASCII",
    "UTF-16",
    "UTF16",
    "UTF-16BE",
    "UTF-16LE",
    "UTF-32",
    "UTF32",
    "UTF-32BE",
    "UTF-32LE",
    "UTF-7",
    "UTF7",
    "UTF-8",
    "UTF8",
    "VISCII",
    "WINDOWS-1250",
    "WINDOWS-1251",
    "WINDOWS-1252",
    "WINDOWS-1253",
    "WINDOWS-1254",
    "WINDOWS-1255",
    "WINDOWS-1256",
    "WINDOWS-1257",
    "WINDOWS-1258",
    "WINDOWS-874",
    "X-MAC-ROMAN",
];

/// The namespace the prefix `xml` is bound to.
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The namespace of namespace declarations.
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

/// The five entities every value knows.
const PREDEFINED: [&str; 5] = ["lt", "gt", "amp", "apos", "quot"];

/// xml read as `xml_in` reads it under `xmloption` content: an XML declaration first, read by
/// `parse_xml_decl`; then the rest as content, or, where `xml_doctype_in_content` finds a DOCTYPE
/// in it, the whole value as a document. The value is kept as written.
pub fn xml_in(s: &str) -> Result<String> {
    let decl = parse_xml_decl(s).ok_or_else(|| Error::new(INVALID_DECL))?;
    let rest = &s[decl.len..];
    let mut reader = Reader::default();
    if doctype_in_content(rest.as_bytes()) {
        reader.document(s, &decl)?;
    } else if !rest.is_empty() {
        let mut sc = Scan { s: rest, i: 0 };
        reader.content(&mut sc, false, Some(decl.len))?;
    }
    Ok(s.to_string())
}

/// xml written as `xml_out` writes a stored value: `xml_out_internal` rereads its declaration and
/// writes one, as `print_xml_decl` does, only where its version is not 1.0 or it gives standalone,
/// never with its encoding; where it writes none, one newline after the declaration is dropped.
pub fn xml_out(stored: &str) -> String {
    let Some(decl) = parse_xml_decl(stored) else {
        return stored.to_string();
    };
    let mut out = String::with_capacity(stored.len());
    let mut len = decl.len;
    if decl.version.is_some_and(|v| v != "1.0") || decl.standalone.is_some() {
        out.push_str("<?xml version=\"");
        out.push_str(decl.version.unwrap_or("1.0"));
        out.push('"');
        match decl.standalone {
            Some(true) => out.push_str(" standalone=\"yes\""),
            Some(false) => out.push_str(" standalone=\"no\""),
            None => {}
        }
        out.push_str("?>");
    } else if stored.as_bytes().get(len) == Some(&b'\n') {
        len += 1;
    }
    out.push_str(&stored[len..]);
    out
}

/// An XML declaration as `parse_xml_decl` reads one: its length (0 where there is none), its
/// version, its encoding and its standalone.
struct Decl<'a> {
    len: usize,
    version: Option<&'a str>,
    encoding: Option<&'a str>,
    standalone: Option<bool>,
}

fn blank(b: Option<&u8>) -> bool {
    matches!(b, Some(b' ' | b'\t' | b'\n' | b'\r'))
}

fn skip_blanks(b: &[u8], mut p: usize) -> usize {
    while blank(b.get(p)) {
        p += 1;
    }
    p
}

/// A character `parse_xml_decl` takes for a name's after `<?xml`, making it a processing
/// instruction: a letter, digit, `.`, `-`, `_`, `:`, the middle dot, or one of Latin-1's letters
/// or anything past them.
fn decl_name_char(c: char) -> bool {
    matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' | ':' | '\u{B7}'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}')
        || c >= '\u{F8}'
}

/// The text between the quotes at `p`, and the place after the closing one.
fn quoted(s: &str, p: usize) -> Option<(&str, usize)> {
    let q = *s.as_bytes().get(p).filter(|&&q| q == b'"' || q == b'\'')?;
    let end = p + 1 + s.as_bytes()[p + 1..].iter().position(|&b| b == q)?;
    Some((&s[p + 1..end], end + 1))
}

/// An XML declaration read as `parse_xml_decl` reads one; none where its bytes are refused.
fn parse_xml_decl(s: &str) -> Option<Decl<'_>> {
    let b = s.as_bytes();
    let mut decl = Decl {
        len: 0,
        version: None,
        encoding: None,
        standalone: None,
    };
    if !b.starts_with(b"<?xml") || s[5..].chars().next().is_some_and(decl_name_char) {
        return Some(decl);
    }
    let mut p = 5;
    if !blank(b.get(p)) {
        return None;
    }
    p = skip_blanks(b, p);
    if !b[p..].starts_with(b"version") {
        return None;
    }
    p = skip_blanks(b, p + 7);
    if b.get(p) != Some(&b'=') {
        return None;
    }
    let (version, after) = quoted(s, skip_blanks(b, p + 1))?;
    decl.version = Some(version);
    p = after;
    let save = p;
    p = skip_blanks(b, p);
    if b[p..].starts_with(b"encoding") {
        if !blank(b.get(save)) {
            return None;
        }
        p = skip_blanks(b, p + 8);
        if b.get(p) != Some(&b'=') {
            return None;
        }
        let (encoding, after) = quoted(s, skip_blanks(b, p + 1))?;
        decl.encoding = Some(encoding);
        p = after;
    } else {
        p = save;
    }
    let save = p;
    p = skip_blanks(b, p);
    if b[p..].starts_with(b"standalone") {
        if !blank(b.get(save)) {
            return None;
        }
        p = skip_blanks(b, p + 10);
        if b.get(p) != Some(&b'=') {
            return None;
        }
        p = skip_blanks(b, p + 1);
        let r = &b[p..];
        if r.starts_with(b"'yes'") || r.starts_with(b"\"yes\"") {
            decl.standalone = Some(true);
            p += 5;
        } else if r.starts_with(b"'no'") || r.starts_with(b"\"no\"") {
            decl.standalone = Some(false);
            p += 4;
        } else {
            return None;
        }
    } else {
        p = save;
    }
    p = skip_blanks(b, p);
    if !b[p..].starts_with(b"?>") {
        return None;
    }
    p += 2;
    if !b[..p].is_ascii() {
        return None;
    }
    decl.len = p;
    Some(decl)
}

fn find(b: &[u8], from: usize, lit: &[u8]) -> Option<usize> {
    b.get(from..)?
        .windows(lit.len())
        .position(|w| w == lit)
        .map(|k| from + k)
}

/// Whether content holds a DOCTYPE, as `xml_doctype_in_content` finds one: after white space,
/// comments and processing instructions only.
fn doctype_in_content(b: &[u8]) -> bool {
    let mut p = 0;
    loop {
        p = skip_blanks(b, p);
        if b.get(p) != Some(&b'<') {
            return false;
        }
        p += 1;
        if b.get(p) == Some(&b'!') {
            p += 1;
            if b[p..].starts_with(b"DOCTYPE") {
                return true;
            }
            if !b[p..].starts_with(b"--") {
                return false;
            }
            match find(b, p + 2, b"--") {
                Some(q) if b.get(q + 2) == Some(&b'>') => p = q + 3,
                _ => return false,
            }
            continue;
        }
        if b.get(p) != Some(&b'?') {
            return false;
        }
        match find(b, p + 1, b"?>") {
            Some(e) => p = e + 2,
            None => return false,
        }
    }
}

/// A character XML allows.
fn is_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{FFFE}' && c != '\u{FFFF}')
}

/// A character a name may start with, as XML 1.0's fifth edition says.
fn name_start(c: char) -> bool {
    matches!(c, ':' | 'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

/// A character a name may hold past its first.
fn name_char(c: char) -> bool {
    name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}'
            | '\u{203F}'..='\u{2040}')
}

/// A character a URI holds as itself: unreserved, a sub-delimiter, or `%` opening an encoding.
fn uri_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=%".contains(&c)
}

/// A character of a URI's path segment.
fn pchar(c: u8) -> bool {
    uri_char(c) || c == b':' || c == b'@'
}

/// Whether an entity's system identifier is taken: a URI reference as RFC 3986 reads one, with no
/// fragment.
fn system_id(s: &str) -> bool {
    let b = s.as_bytes();
    let encoded = |k: usize| {
        b.get(k + 1..k + 3)
            .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
    };
    if b.iter().enumerate().any(|(k, &c)| c == b'%' && !encoded(k)) {
        return false;
    }
    let (reference, query) = s.split_once('?').unwrap_or((s, ""));
    if !query.bytes().all(|c| pchar(c) || c == b'/' || c == b'?') {
        return false;
    }
    let first = &reference[..reference.find('/').unwrap_or(reference.len())];
    let rest = match first.split_once(':') {
        Some((scheme, _)) => {
            let mut c = scheme.bytes();
            if !c.next().is_some_and(|c| c.is_ascii_alphabetic())
                || !c.all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
            {
                return false;
            }
            &reference[scheme.len() + 1..]
        }
        None => reference,
    };
    let path = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find('/').unwrap_or(after.len());
            if !authority(&after[..end]) {
                return false;
            }
            &after[end..]
        }
        None => rest,
    };
    path.bytes().all(|c| pchar(c) || c == b'/')
}

/// Whether a URI's authority is one RFC 3986 reads, as libxml reads it: a user, a host, and a
/// port of one digit or more after a colon.
fn authority(a: &str) -> bool {
    let (user, host) = a.rsplit_once('@').unwrap_or(("", a));
    if !user.bytes().all(|c| uri_char(c) || c == b':') {
        return false;
    }
    let port = match host.strip_prefix('[') {
        Some(literal) => match literal.split_once(']') {
            Some((ip, port)) if ip.bytes().all(|c| uri_char(c) || c == b':') => {
                match port.strip_prefix(':') {
                    Some(p) => Some(p),
                    None if port.is_empty() => None,
                    None => return false,
                }
            }
            _ => return false,
        },
        None => {
            let (name, port) = host
                .rsplit_once(':')
                .map_or((host, None), |(n, p)| (n, Some(p)));
            if !name.bytes().all(uri_char) {
                return false;
            }
            port
        }
    };
    port.is_none_or(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}

fn pubid_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b" \r\n-'()+,./:=?;!*#@$_%".contains(&b)
}

fn bad() -> Error {
    Error::new(INVALID)
}

fn odd_name() -> Error {
    Error::new(format!(
        "xml with a name past {NAME_BYTES} bytes whose part after its first colon is not a plain local part is not read"
    ))
}

/// Text added to an attribute's value as it is normalized: its white space each a space.
fn push_normalized(value: Option<&mut String>, text: &str) {
    if let Some(v) = value {
        v.extend(text.chars().map(|c| {
            if matches!(c, '\t' | '\n' | '\r') {
                ' '
            } else {
                c
            }
        }));
    }
}

/// A text being read, and the place reached in it.
struct Scan<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn at(&self, lit: &str) -> bool {
        self.s.as_bytes()[self.i..].starts_with(lit.as_bytes())
    }

    fn eat(&mut self, lit: &str) -> bool {
        let at = self.at(lit);
        if at {
            self.i += lit.len();
        }
        at
    }

    fn blanks(&mut self) -> bool {
        let from = self.i;
        while blank(self.s.as_bytes().get(self.i)) {
            self.i += 1;
        }
        self.i > from
    }

    fn ch(&self) -> Option<char> {
        match self.peek()? {
            b if b < 0x80 => Some(b as char),
            _ => self.s[self.i..].chars().next(),
        }
    }

    /// Passes over one character XML allows; refused at the end or at one it does not.
    fn char(&mut self) -> Result<()> {
        match self.ch() {
            Some(c) if is_char(c) => {
                self.i += c.len_utf8();
                Ok(())
            }
            _ => Err(bad()),
        }
    }

    /// The name starting here, past it; refused where none starts here or it is too long.
    fn name(&mut self) -> Result<&'a str> {
        let from = self.i;
        match self.ch() {
            Some(c) if name_start(c) => self.i += c.len_utf8(),
            _ => return Err(bad()),
        }
        self.name_rest(from)
    }

    /// The name token starting here, past it.
    fn nmtoken(&mut self) -> Result<&'a str> {
        let from = self.i;
        match self.ch() {
            Some(c) if name_char(c) => self.i += c.len_utf8(),
            _ => return Err(bad()),
        }
        self.name_rest(from)
    }

    fn name_rest(&mut self, from: usize) -> Result<&'a str> {
        while let Some(c) = self.ch().filter(|&c| name_char(c)) {
            self.i += c.len_utf8();
        }
        let name = &self.s[from..self.i];
        if name.len() > NAME_BYTES {
            return Err(bad());
        }
        Ok(name)
    }

    fn take(&mut self, ok: impl Fn(char) -> bool) {
        while let Some(c) = self.ch().filter(|&c| ok(c)) {
            self.i += c.len_utf8();
        }
    }

    /// An element's or an attribute's name, split as libxml splits a QName: a name opening with
    /// `:` is whole and has no prefix; else the part before a colon is its prefix where a part
    /// follows the colon, and a second colon and a name after it belong to the local part; where
    /// none follows, the name tokens after the colon are the name's, and it has no prefix. Its
    /// prefix, where it has one, and the name as written. Each part is held to [`NAME_BYTES`].
    fn qname(&mut self) -> Result<(Option<&'a str>, &'a str)> {
        let from = self.i;
        let part_start = |c: char| c != ':' && name_start(c);
        let part_char = |c: char| c != ':' && name_char(c);
        let long = |n: &str| n.len() > NAME_BYTES;
        match self.ch() {
            Some(':') => {
                self.i += 1;
                self.take(name_char);
                let name = &self.s[from..self.i];
                return if long(name) {
                    Err(bad())
                } else {
                    Ok((None, name))
                };
            }
            Some(c) if part_start(c) => self.take(part_char),
            _ => return Err(bad()),
        }
        let prefix = &self.s[from..self.i];
        if long(prefix) {
            return Err(bad());
        }
        if self.peek() != Some(b':') {
            return Ok((None, prefix));
        }
        self.i += 1;
        let local = self.i;
        let plain = match self.ch() {
            Some(c) if part_start(c) => {
                self.take(part_char);
                if long(&self.s[local..self.i]) {
                    return Err(bad());
                }
                if self.peek() == Some(b':') {
                    self.i += 1;
                    if self.ch().is_some_and(name_start) {
                        self.take(name_char);
                    }
                    false
                } else {
                    true
                }
            }
            _ => {
                self.take(name_char);
                let name = &self.s[from..self.i];
                if long(name) {
                    return Err(odd_name());
                }
                return Ok((None, name));
            }
        };
        let name = &self.s[from..self.i];
        if !plain && long(&self.s[local..self.i]) {
            return Err(odd_name());
        }
        Ok((Some(prefix), name))
    }

    /// A character reference at `&#`, past it: its character, refused where XML does not allow
    /// it.
    fn char_ref(&mut self) -> Result<char> {
        self.i += 2;
        let radix = if self.eat("x") { 16 } else { 10 };
        let (mut v, mut digits) = (0u32, 0);
        while let Some(d) = self.peek().and_then(|b| (b as char).to_digit(radix)) {
            v = (v * radix + d).min(0x11_0000);
            digits += 1;
            self.i += 1;
        }
        if digits == 0 || !self.eat(";") {
            return Err(bad());
        }
        char::from_u32(v).filter(|&c| is_char(c)).ok_or_else(bad)
    }

    /// An entity reference's name at `&`, past its `;`.
    fn entity_ref(&mut self) -> Result<&'a str> {
        self.i += 1;
        let name = self.name()?;
        if !self.eat(";") {
            return Err(bad());
        }
        Ok(name)
    }

    /// Passes over a run of character data, up to the next `<` or `&`.
    fn char_data(&mut self) -> Result<()> {
        while let Some(b) = self.peek() {
            match b {
                b'<' | b'&' => break,
                b']' if self.at("]]>") => return Err(bad()),
                _ => self.char()?,
            }
        }
        Ok(())
    }

    /// Passes over a comment at `<!--`.
    fn comment(&mut self) -> Result<()> {
        self.i += 4;
        let from = self.i;
        while !self.at("--") {
            self.char()?;
        }
        if !self.at("-->") || self.i - from > SECTION_BYTES {
            return Err(bad());
        }
        self.i += 3;
        Ok(())
    }

    /// Passes over a processing instruction at `<?`: its target any name but `xml`, any case.
    fn pi(&mut self) -> Result<()> {
        self.i += 2;
        if self.name()?.eq_ignore_ascii_case("xml") {
            return Err(bad());
        }
        if self.eat("?>") {
            return Ok(());
        }
        if !self.blanks() {
            return Err(bad());
        }
        let from = self.i;
        while !self.at("?>") {
            self.char()?;
        }
        if self.i - from > SECTION_BYTES {
            return Err(bad());
        }
        self.i += 2;
        Ok(())
    }

    /// Passes over a CDATA section at `<![CDATA[`.
    fn cdata(&mut self) -> Result<()> {
        self.i += 9;
        let from = self.i;
        while !self.at("]]>") {
            self.char()?;
        }
        if self.i - from > SECTION_BYTES {
            return Err(bad());
        }
        self.i += 3;
        Ok(())
    }

    fn quote(&mut self) -> Result<u8> {
        match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                self.i += 1;
                Ok(q)
            }
            _ => Err(bad()),
        }
    }

    /// A system literal, past it: its text.
    fn system_literal(&mut self) -> Result<&'a str> {
        let q = self.quote()?;
        let from = self.i;
        while self.peek() != Some(q) {
            self.char()?;
        }
        self.i += 1;
        Ok(&self.s[from..self.i - 1])
    }

    fn pubid_literal(&mut self) -> Result<()> {
        let q = self.quote()?;
        loop {
            match self.peek() {
                Some(b) if b == q => break,
                Some(b) if pubid_char(b) => self.i += 1,
                _ => return Err(bad()),
            }
        }
        self.i += 1;
        Ok(())
    }

    /// An external identifier's `SYSTEM` or `PUBLIC` literals: whether it has a public one, and
    /// its system literal; none where neither starts here.
    fn external_id(&mut self) -> Result<Option<(bool, &'a str)>> {
        let public = if self.eat("SYSTEM") {
            false
        } else if self.eat("PUBLIC") {
            self.space()?;
            self.pubid_literal()?;
            true
        } else {
            return Ok(None);
        };
        self.space()?;
        Ok(Some((public, self.system_literal()?)))
    }

    /// White space XML requires here.
    fn space(&mut self) -> Result<()> {
        if self.blanks() {
            Ok(())
        } else {
            Err(bad())
        }
    }

    fn expect(&mut self, lit: &str) -> Result<()> {
        if self.eat(lit) {
            Ok(())
        } else {
            Err(bad())
        }
    }

    /// Passes over white space, comments and processing instructions.
    fn misc(&mut self) -> Result<()> {
        loop {
            self.blanks();
            if self.at("<!--") {
                self.comment()?;
            } else if self.at("<?") {
                self.pi()?;
            } else {
                return Ok(());
            }
        }
    }
}

/// What an entity declares: its replacement text; or an external parsed entity, and whether a
/// reference to it in content loads (it names a public identifier, or a system one); or an
/// unparsed one.
enum Entity {
    Text(String),
    External(bool),
    Unparsed,
}

/// The internal subset as far as it is read.
#[derive(Default)]
struct Dtd {
    general: HashMap<String, Entity>,
    params: HashMap<String, Entity>,
    external: bool,
    references: bool,
    standalone: bool,
    /// Whether a namespace declaration's attribute, by element, is declared CDATA: its first
    /// declaration's type.
    cdata: HashMap<(String, String), bool>,
}

impl Dtd {
    /// Whether an entity nobody declared is let through: where the DOCTYPE names an external
    /// subset, or a parameter entity was referred to, and the declaration does not say standalone.
    fn lenient(&self) -> bool {
        !self.standalone && (self.external || self.references)
    }
}

/// The reading of one value: its DOCTYPE, where it has one, and the entities being expanded.
#[derive(Default)]
struct Reader {
    dtd: Option<Dtd>,
    open: Vec<String>,
    depth: usize,
    in_entity: usize,
    bytes: usize,
    references: usize,
}

impl Reader {
    /// Counts one entity reference and its text against what one value may expand.
    fn count(&mut self, len: usize) -> Result<()> {
        self.references += 1;
        self.bytes += len;
        if self.references > EXPANDED_REFERENCES || self.bytes > EXPANDED_BYTES {
            return Err(Error::new(format!(
                "xml whose entities expand past {EXPANDED_BYTES} bytes or {EXPANDED_REFERENCES} references is not read"
            )));
        }
        Ok(())
    }

    /// Reads content to the end of `sc`, every element closed in it; or, as `root`, the element
    /// starting at `sc` and no further. `at` is where `sc` starts in the value, for the main text.
    fn content<'a>(&mut self, sc: &mut Scan<'a>, root: bool, at: Option<usize>) -> Result<()> {
        let limit = ELEMENT_DEPTH + usize::from(root);
        let mut open: Vec<&'a str> = Vec::new();
        loop {
            let Some(b) = sc.peek() else {
                return if open.is_empty() && !root {
                    Ok(())
                } else {
                    Err(bad())
                };
            };
            match b {
                b'<' if sc.at("</") => {
                    sc.i += 2;
                    let (_, name) = sc.qname()?;
                    if open.pop() != Some(name) {
                        return Err(bad());
                    }
                    sc.blanks();
                    sc.expect(">")?;
                    if root && open.is_empty() {
                        return Ok(());
                    }
                }
                b'<' if sc.at("<?") => sc.pi()?,
                b'<' if sc.at("<!--") => sc.comment()?,
                b'<' if sc.at("<![CDATA[") => sc.cdata()?,
                b'<' => {
                    if open.len() >= limit {
                        return Err(bad());
                    }
                    let (name, empty) = self.start_tag(sc, at)?;
                    if !empty {
                        open.push(name);
                    } else if root && open.is_empty() {
                        return Ok(());
                    }
                }
                b'&' if sc.at("&#") => {
                    sc.char_ref()?;
                }
                b'&' => {
                    let name = sc.entity_ref()?;
                    self.content_entity(name)?;
                }
                _ => sc.char_data()?,
            }
        }
    }

    /// A start tag at `<`, past it: its name, and whether it closes itself. Its attributes are
    /// each named once, but for the namespace declarations libxml passes over: one binding a
    /// prefix to nothing, one binding `xml` or `xmlns`, one binding the namespace of either. A
    /// declaration's value is read normalized, and normalized again where the DOCTYPE declares
    /// its attribute other than CDATA.
    fn start_tag<'a>(&mut self, sc: &mut Scan<'a>, at: Option<usize>) -> Result<(&'a str, bool)> {
        sc.i += 1;
        let (_, name) = sc.qname()?;
        let mut names = HashSet::new();
        let mut any = false;
        let empty = loop {
            let spaced = sc.blanks();
            if sc.eat("/>") {
                break true;
            }
            if sc.eat(">") {
                break false;
            }
            if !spaced {
                return Err(bad());
            }
            let (prefix, attribute) = sc.qname()?;
            sc.blanks();
            sc.expect("=")?;
            sc.blanks();
            any = true;
            let declares = match prefix {
                Some("xmlns") => Some(&attribute[6..]),
                None if attribute == "xmlns" => Some(""),
                _ => None,
            };
            let Some(bound) = declares else {
                self.att_value(sc, None)?;
                if !names.insert(attribute) {
                    return Err(bad());
                }
                continue;
            };
            let mut value = String::new();
            self.att_value(sc, Some(&mut value))?;
            let cdata = self.dtd.as_ref().is_none_or(|d| {
                d.cdata
                    .get(&(name.to_string(), attribute.to_string()))
                    .is_none_or(|&c| c)
            });
            let value = if cdata {
                value
            } else {
                value
                    .split(' ')
                    .filter(|w| !w.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let passed = value == XML_NAMESPACE
                || value == XMLNS_NAMESPACE
                || (!bound.is_empty() && (value.is_empty() || bound == "xml" || bound == "xmlns"));
            if !passed && !names.insert(attribute) {
                return Err(bad());
            }
        };
        if any && at.is_some_and(|at| at + sc.i > LOOKUP_BYTES) {
            return Err(Error::new(format!(
                "xml holding a start tag with attributes that ends past byte {LOOKUP_BYTES} is not read"
            )));
        }
        Ok((name, empty))
    }

    /// An attribute value at its opening quote, past its closing one; into `value`, where given,
    /// its text as an attribute's is normalized: white space a space, references replaced.
    fn att_value(&mut self, sc: &mut Scan, mut value: Option<&mut String>) -> Result<()> {
        let q = sc.quote()?;
        loop {
            match sc.peek() {
                Some(b) if b == q => break,
                Some(b'<') | None => return Err(bad()),
                Some(b'&') if sc.at("&#") => {
                    let c = sc.char_ref()?;
                    if let Some(v) = value.as_deref_mut() {
                        v.push(c);
                    }
                }
                Some(b'&') => {
                    let name = sc.entity_ref()?;
                    self.att_entity(name, value.as_deref_mut())?;
                }
                Some(_) => {
                    let from = sc.i;
                    sc.char()?;
                    push_normalized(value.as_deref_mut(), &sc.s[from..sc.i]);
                }
            }
        }
        sc.i += 1;
        Ok(())
    }

    /// The entity of `name` that a reference in content reads.
    fn content_entity(&mut self, name: &str) -> Result<()> {
        if PREDEFINED.contains(&name) {
            return Ok(());
        }
        let text = match self
            .dtd
            .as_ref()
            .map(|d| (d.general.get(name), d.lenient()))
        {
            Some((Some(Entity::Text(t)), _)) => t.clone(),
            Some((Some(Entity::External(true)), _)) => return self.count(0),
            Some((None, true)) if self.in_entity == 0 => return Ok(()),
            _ => return Err(bad()),
        };
        if self.open.iter().any(|n| n == name) || self.depth + 2 > ENTITY_DEPTH {
            return Err(bad());
        }
        self.count(text.len())?;
        self.open.push(name.to_string());
        self.depth += 2;
        self.in_entity += 1;
        let read = self.content(&mut Scan { s: &text, i: 0 }, false, None);
        self.in_entity -= 1;
        self.depth -= 2;
        self.open.pop();
        read
    }

    /// The entity of `name` that a reference in an attribute value reads: internal, its text
    /// holding no `<`; into `value`, where given, its text.
    fn att_entity(&mut self, name: &str, mut value: Option<&mut String>) -> Result<()> {
        if let Some(k) = PREDEFINED.iter().position(|&p| p == name) {
            if let Some(v) = value.as_deref_mut() {
                v.push(['<', '>', '&', '\'', '"'][k]);
            }
            return Ok(());
        }
        let text = match self
            .dtd
            .as_ref()
            .map(|d| (d.general.get(name), d.lenient()))
        {
            Some((Some(Entity::Text(t)), _)) => t.clone(),
            Some((None, true)) if self.in_entity == 0 => return Ok(()),
            _ => return Err(bad()),
        };
        if self.open.iter().any(|n| n == name) || self.depth + 1 > ENTITY_DEPTH {
            return Err(bad());
        }
        self.count(text.len())?;
        self.open.push(name.to_string());
        self.depth += 1;
        let mut sc = Scan { s: &text, i: 0 };
        let mut read = Ok(());
        while let Some(b) = sc.peek() {
            read = match b {
                b'<' => Err(bad()),
                b'&' if sc.at("&#") => sc.char_ref().map(|c| {
                    if let Some(v) = value.as_deref_mut() {
                        v.push(c);
                    }
                }),
                b'&' => sc
                    .entity_ref()
                    .and_then(|n| self.att_entity(n, value.as_deref_mut())),
                _ => {
                    let from = sc.i;
                    sc.char()
                        .map(|()| push_normalized(value.as_deref_mut(), &sc.s[from..sc.i]))
                }
            };
            if read.is_err() {
                break;
            }
        }
        self.depth -= 1;
        self.open.pop();
        read
    }

    /// The whole value read as a document, as `xmlCtxtReadDoc` reads it: the declaration's
    /// version 1.something and its encoding one the server knows; white space, comments and
    /// processing instructions; the DOCTYPE; the root element; then white space, comments and
    /// processing instructions only.
    fn document(&mut self, s: &str, decl: &Decl) -> Result<()> {
        if decl.len > 0 {
            let v = decl.version.unwrap_or("");
            if !(v.starts_with("1.") && v[2..].bytes().all(|b| b.is_ascii_digit())) {
                return Err(bad());
            }
            if let Some(e) = decl.encoding {
                let mut b = e.bytes();
                if !b.next().is_some_and(|b| b.is_ascii_alphabetic())
                    || !b.all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
                {
                    return Err(bad());
                }
                if !ENCODINGS.contains(&e.to_ascii_uppercase().as_str()) {
                    return Err(Error::new(format!(
                        "xml declaring the encoding \"{e}\" before a DOCTYPE is not read"
                    )));
                }
            }
        }
        self.dtd = Some(Dtd {
            standalone: decl.standalone == Some(true),
            ..Dtd::default()
        });
        let mut sc = Scan { s, i: decl.len };
        sc.misc()?;
        sc.expect("<!DOCTYPE")?;
        self.doctype(&mut sc)?;
        sc.misc()?;
        if sc.peek() != Some(b'<') || sc.at("<!") || sc.at("</") {
            return Err(bad());
        }
        self.content(&mut sc, true, Some(0))?;
        sc.misc()?;
        if sc.peek().is_some() {
            return Err(bad());
        }
        Ok(())
    }

    /// A DOCTYPE past `<!DOCTYPE`: its name, its external identifier, its internal subset; where
    /// it has none, a subset opening straight after its `>` is read as its own, as libxml reads
    /// it.
    fn doctype(&mut self, sc: &mut Scan) -> Result<()> {
        sc.blanks();
        sc.name()?;
        sc.blanks();
        if sc.external_id()?.is_some() {
            self.dtd_mut().external = true;
        }
        sc.blanks();
        let subset = sc.eat("[");
        if subset {
            self.internal_subset(sc)?;
            sc.blanks();
        }
        sc.expect(">")?;
        if !subset && sc.eat("[") {
            self.internal_subset(sc)?;
            sc.blanks();
            sc.expect(">")?;
        }
        if sc.i > LOOKUP_BYTES {
            return Err(Error::new(format!(
                "xml whose DOCTYPE ends past byte {LOOKUP_BYTES} is not read"
            )));
        }
        Ok(())
    }

    fn dtd_mut(&mut self) -> &mut Dtd {
        self.dtd.get_or_insert_with(Dtd::default)
    }

    /// The internal subset past `[`, past its `]`: declarations, comments, processing
    /// instructions and parameter entity references. A parameter entity whose declarations were
    /// the last read is not referred to again before another declaration.
    fn internal_subset(&mut self, sc: &mut Scan) -> Result<()> {
        let mut last: Option<String> = None;
        loop {
            sc.blanks();
            match sc.peek() {
                Some(b']') => {
                    sc.i += 1;
                    return Ok(());
                }
                Some(b'%') => {
                    let name = {
                        sc.i += 1;
                        let name = sc.name()?;
                        sc.expect(";")?;
                        name
                    };
                    self.parameter_entity(name, &mut last)?;
                }
                Some(b'<') => {
                    self.markup(sc)?;
                    last = None;
                }
                _ => return Err(bad()),
            }
        }
    }

    /// A parameter entity referred to between declarations: an internal one's text read as
    /// declarations.
    fn parameter_entity(&mut self, name: &str, last: &mut Option<String>) -> Result<()> {
        let dtd = self.dtd_mut();
        let lenient = dtd.lenient();
        dtd.references = true;
        let text = match dtd.params.get(name) {
            Some(Entity::Text(t)) => t.clone(),
            Some(_) => return self.count(0),
            None if lenient => return Ok(()),
            None => return Err(bad()),
        };
        if last.as_deref() == Some(name) {
            return Err(bad());
        }
        if text.contains('%') {
            return Err(Error::new(
                "xml whose parameter entity holds '%' is not read",
            ));
        }
        self.count(text.len())?;
        let mut sc = Scan { s: &text, i: 0 };
        loop {
            sc.blanks();
            match sc.peek() {
                None => return Ok(()),
                Some(b'<') => {
                    self.markup(&mut sc)?;
                    *last = Some(name.to_string());
                }
                _ => return Err(bad()),
            }
        }
    }

    /// One markup declaration, comment or processing instruction at `<`.
    fn markup(&mut self, sc: &mut Scan) -> Result<()> {
        if sc.at("<!--") {
            sc.comment()
        } else if sc.at("<?") {
            sc.pi()
        } else if sc.eat("<!ELEMENT") {
            element_decl(sc)
        } else if sc.eat("<!ATTLIST") {
            self.attlist_decl(sc)
        } else if sc.eat("<!ENTITY") {
            self.entity_decl(sc)
        } else if sc.eat("<!NOTATION") {
            notation_decl(sc)
        } else {
            Err(bad())
        }
    }

    /// An attribute-list declaration past `<!ATTLIST`: each default value read as an attribute
    /// value, with the entities declared so far.
    fn attlist_decl(&mut self, sc: &mut Scan) -> Result<()> {
        sc.space()?;
        let element = sc.name()?;
        loop {
            let spaced = sc.blanks();
            if sc.eat(">") {
                return Ok(());
            }
            if !spaced {
                return Err(bad());
            }
            let attribute = sc.name()?;
            sc.space()?;
            let cdata = att_type(sc)?;
            if attribute == "xmlns" || attribute.starts_with("xmlns:") {
                let key = (element.to_string(), attribute.to_string());
                self.dtd_mut().cdata.entry(key).or_insert(cdata);
            }
            sc.space()?;
            if !sc.eat("#REQUIRED") && !sc.eat("#IMPLIED") {
                if sc.eat("#FIXED") {
                    sc.space()?;
                }
                self.att_value(sc, None)?;
            }
        }
    }

    /// An entity declaration past `<!ENTITY`, kept where none of its name came before: its value
    /// with character references replaced, or its external identifier, its system identifier a
    /// URI; after `NDATA`, libxml lets the notation's name be missing.
    fn entity_decl(&mut self, sc: &mut Scan) -> Result<()> {
        sc.space()?;
        let parameter = sc.eat("%");
        if parameter {
            sc.space()?;
        }
        let name = sc.name()?;
        sc.space()?;
        let entity = if matches!(sc.peek(), Some(b'"' | b'\'')) {
            Entity::Text(entity_value(sc)?)
        } else if let Some((public, system)) = sc.external_id()? {
            if !system_id(system) {
                return Err(bad());
            }
            let spaced = sc.blanks();
            if !parameter && sc.eat("NDATA") {
                if !spaced {
                    return Err(bad());
                }
                sc.space()?;
                if sc.ch().is_some_and(name_start) {
                    sc.name()?;
                }
                Entity::Unparsed
            } else {
                Entity::External(public || !system.is_empty())
            }
        } else {
            return Err(bad());
        };
        sc.blanks();
        sc.expect(">")?;
        let dtd = self.dtd_mut();
        let map = if parameter {
            &mut dtd.params
        } else {
            &mut dtd.general
        };
        map.entry(name.to_string()).or_insert(entity);
        Ok(())
    }
}

/// An entity's value at its opening quote, past its closing one: its replacement text, its
/// character references replaced and its entity references kept; a parameter entity reference
/// refused.
fn entity_value(sc: &mut Scan) -> Result<String> {
    let q = sc.quote()?;
    let mut text = String::new();
    loop {
        match sc.peek() {
            Some(b) if b == q => break,
            Some(b'%') | None => return Err(bad()),
            Some(b'&') if sc.at("&#") => text.push(sc.char_ref()?),
            Some(b'&') => {
                let name = sc.entity_ref()?;
                text.push('&');
                text.push_str(name);
                text.push(';');
            }
            Some(_) => {
                let from = sc.i;
                sc.char()?;
                text.push_str(&sc.s[from..sc.i]);
            }
        }
    }
    sc.i += 1;
    Ok(text)
}

/// An element declaration past `<!ELEMENT`: `EMPTY`, `ANY`, mixed content or a content model.
fn element_decl(sc: &mut Scan) -> Result<()> {
    sc.space()?;
    sc.name()?;
    sc.space()?;
    if !sc.eat("EMPTY") && !sc.eat("ANY") {
        sc.expect("(")?;
        sc.blanks();
        if sc.eat("#PCDATA") {
            sc.blanks();
            if sc.eat(")") {
                sc.eat("*");
            } else {
                loop {
                    sc.blanks();
                    if !sc.eat("|") {
                        break;
                    }
                    sc.blanks();
                    sc.name()?;
                }
                sc.expect(")*")?;
            }
        } else {
            children(sc, 1)?;
        }
    }
    sc.blanks();
    sc.expect(">")
}

/// A content model's group past its `(`: particles joined all by `,` or all by `|`.
fn children(sc: &mut Scan, depth: usize) -> Result<()> {
    if depth > MODEL_DEPTH {
        return Err(bad());
    }
    sc.blanks();
    particle(sc, depth)?;
    let mut joint = None;
    loop {
        sc.blanks();
        match sc.peek() {
            Some(b')') => break,
            Some(j @ (b'|' | b',')) if joint.is_none_or(|k| k == j) => {
                joint = Some(j);
                sc.i += 1;
                sc.blanks();
                particle(sc, depth)?;
            }
            _ => return Err(bad()),
        }
    }
    sc.i += 1;
    if matches!(sc.peek(), Some(b'?' | b'*' | b'+')) {
        sc.i += 1;
    }
    Ok(())
}

fn particle(sc: &mut Scan, depth: usize) -> Result<()> {
    if sc.eat("(") {
        return children(sc, depth + 1);
    }
    sc.name()?;
    if matches!(sc.peek(), Some(b'?' | b'*' | b'+')) {
        sc.i += 1;
    }
    Ok(())
}

/// An attribute's type: a keyword, a list of notations or an enumeration of name tokens; whether
/// it is CDATA.
fn att_type(sc: &mut Scan) -> Result<bool> {
    for kw in [
        "CDATA", "IDREFS", "IDREF", "ID", "ENTITY", "ENTITIES", "NMTOKENS", "NMTOKEN",
    ] {
        if sc.eat(kw) {
            return Ok(kw == "CDATA");
        }
    }
    let notation = sc.eat("NOTATION");
    if notation {
        sc.space()?;
    }
    sc.expect("(")?;
    loop {
        sc.blanks();
        if notation {
            sc.name()?;
        } else {
            sc.nmtoken()?;
        }
        sc.blanks();
        if sc.eat(")") {
            return Ok(false);
        }
        sc.expect("|")?;
    }
}

/// A notation declaration past `<!NOTATION`: a system identifier, or a public one with or
/// without a system one.
fn notation_decl(sc: &mut Scan) -> Result<()> {
    sc.space()?;
    sc.name()?;
    sc.space()?;
    if sc.eat("SYSTEM") {
        sc.space()?;
        sc.system_literal()?;
    } else if sc.eat("PUBLIC") {
        sc.space()?;
        sc.pubid_literal()?;
        if sc.blanks() && matches!(sc.peek(), Some(b'"' | b'\'')) {
            sc.system_literal()?;
        }
    } else {
        return Err(bad());
    }
    sc.blanks();
    sc.expect(">")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values as PostgreSQL 18.6 built with libxml 2.9.14 answers them under `xmloption` content:
    /// what `'…'::xml` prints, or the first line of its refusal.
    const SERVER: &[(&str, std::result::Result<&str, &str>)] = &[
        ("<a/>", Ok("<a/>")),
        ("", Ok("")),
        ("<?xml version=\"1.0\" encoding=\"UTF-8\"?><a>b</a>", Ok("<a>b</a>")),
        ("<?xml version=\"1.1\"?><a/>", Ok("<?xml version=\"1.1\"?><a/>")),
        ("<?xml version=\"1.0\" standalone=\"yes\"?><a/>", Ok("<?xml version=\"1.0\" standalone=\"yes\"?><a/>")),
        ("<a>text</a><b/>", Ok("<a>text</a><b/>")),
        ("plain text", Ok("plain text")),
        ("<a x=\"1\" y='2'>&amp;&lt;</a>", Ok("<a x=\"1\" y='2'>&amp;&lt;</a>")),
        ("<é>ü</é>", Ok("<é>ü</é>")),
        (" ", Ok(" ")),
        ("<?xml version=\"1.0\"?>", Ok("")),
        ("<?xml version=\"1.0\"?>\n<a/>", Ok("<a/>")),
        ("<?xml version=\"1.0\"?>\n\n<a/>", Ok("\n<a/>")),
        ("<?xml version=\"1.0\"?>\r\n<a/>", Ok("\r\n<a/>")),
        ("<?xml version=\"1.1\"?>\n<a/>", Ok("<?xml version=\"1.1\"?>\n<a/>")),
        ("\n<a/>", Ok("<a/>")),
        ("\n\n<a/>", Ok("\n<a/>")),
        ("<?xml version='1.0' encoding='UTF-8' standalone='no'?>\n\n<a/>", Ok("<?xml version=\"1.0\" standalone=\"no\"?>\n\n<a/>")),
        ("<?xml version=\"foo\"?><a/>", Ok("<?xml version=\"foo\"?><a/>")),
        ("<?xml version=\"\"?><a/>", Ok("<?xml version=\"\"?><a/>")),
        ("<?xml version=\"2.0\"?>", Ok("<?xml version=\"2.0\"?>")),
        ("<?xml version=\"1.0\" encoding=\"LATIN1\"?><a/>", Ok("<a/>")),
        ("<?xml version=\"1.0\" encoding=\"nonsense!\"?><a/>", Ok("<a/>")),
        ("<?xml version=\"1.0\" standalone=\"no\"?><a/>", Ok("<?xml version=\"1.0\" standalone=\"no\"?><a/>")),
        ("<?xml version=\"1.0\" standalone='yes' ?><a/>", Ok("<?xml version=\"1.0\" standalone=\"yes\"?><a/>")),
        ("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><a/>", Ok("<?xml version=\"1.0\" standalone=\"yes\"?><a/>")),
        ("<?xml\tversion=\"1.0\"\rencoding=\"UTF-8\"\nstandalone=\"no\"?><a/>", Ok("<?xml version=\"1.0\" standalone=\"no\"?><a/>")),
        ("<?xml version = \"1.0\" ?><a/>", Ok("<a/>")),
        ("<?xml version=\"1.0\" encoding=\"\" standalone=\"no\"?><a/>", Ok("<?xml version=\"1.0\" standalone=\"no\"?><a/>")),
        ("<?xml version='1\"0'?><a/>", Ok("<?xml version=\"1\"0\"?><a/>")),
        ("<?xml?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml", Err("invalid XML content: invalid XML declaration")),
        ("<?xml ", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\"", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=1.0?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version='1.0\"?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\"encoding=\"UTF-8\"?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" standalone=\"maybe\"?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" standalone=\"YES\"?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" standalone=yes?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" standalone=\"yes\" encoding=\"UTF-8\"?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" encoding=UTF-8?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" ?  >", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"1.0\" foo=\"bar\"?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml version=\"é\"?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml encoding=\"UTF-8\"?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml\u{b} version=\"1.0\"?><a/>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml×?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xml÷?>", Err("invalid XML content: invalid XML declaration")),
        ("<?xmlé?>", Ok("<?xmlé?>")),
        ("<?xml·?>", Ok("<?xml·?>")),
        ("<?xml\u{3000}?>", Err("invalid XML content")),
        ("<?xml😀?>", Ok("<?xml😀?>")),
        ("<?xml-stylesheet href=\"a\"?><a/>", Ok("<?xml-stylesheet href=\"a\"?><a/>")),
        ("<?XML version=\"1.0\"?><a/>", Err("invalid XML content")),
        (" <?xml version=\"1.0\"?><a/>", Err("invalid XML content")),
        ("<?xml version=\"1.0\"?><?xml version=\"1.0\"?>", Err("invalid XML content")),
        ("\u{feff}<a/>", Ok("\u{feff}<a/>")),
        ("\u{feff}<?xml version=\"1.0\"?><a/>", Err("invalid XML content")),
        ("a<b", Err("invalid XML content")),
        ("a>b", Ok("a>b")),
        ("a&b", Err("invalid XML content")),
        ("a&amp", Err("invalid XML content")),
        ("&lt;&gt;&quot;&apos;&amp;", Ok("&lt;&gt;&quot;&apos;&amp;")),
        ("&foo;", Err("invalid XML content")),
        ("&#65;&#x41;&#9;&#10;&#13;&#x1F600;&#xFFFD;&#x10FFFF;&#00000000000065;", Ok("&#65;&#x41;&#9;&#10;&#13;&#x1F600;&#xFFFD;&#x10FFFF;&#00000000000065;")),
        ("&#X41;", Err("invalid XML content")),
        ("&#0;", Err("invalid XML content")),
        ("&#1;", Err("invalid XML content")),
        ("&#xD800;", Err("invalid XML content")),
        ("&#xFFFE;", Err("invalid XML content")),
        ("&#x110000;", Err("invalid XML content")),
        ("&#;", Err("invalid XML content")),
        ("&#x;", Err("invalid XML content")),
        ("&#65", Err("invalid XML content")),
        ("a]]>b", Err("invalid XML content")),
        ("a]]b", Ok("a]]b")),
        ("\u{1}", Err("invalid XML content")),
        ("a\u{b}b", Err("invalid XML content")),
        ("\u{7f}\u{80}\u{9f}\u{85}\u{2028}\u{fdd0}\u{1fffe}", Ok("\u{7f}\u{80}\u{9f}\u{85}\u{2028}\u{fdd0}\u{1fffe}")),
        ("\u{fffe}", Err("invalid XML content")),
        ("\u{ffff}", Err("invalid XML content")),
        ("<a>", Err("invalid XML content")),
        ("</a>", Err("invalid XML content")),
        ("<a></a>", Ok("<a></a>")),
        ("<a></b>", Err("invalid XML content")),
        ("<a><b></a></b>", Err("invalid XML content")),
        ("<a >x</a >", Ok("<a >x</a >")),
        ("<a></a\t\r\n >", Ok("<a></a\t\r\n >")),
        ("< a/>", Err("invalid XML content")),
        ("<a/ >", Err("invalid XML content")),
        ("<a></ a>", Err("invalid XML content")),
        ("<a x=1/>", Err("invalid XML content")),
        ("<a x/>", Err("invalid XML content")),
        ("<a x=\"1\" x=\"2\"/>", Err("invalid XML content")),
        ("<a x=\"1\"y=\"2\"/>", Err("invalid XML content")),
        ("<a x = \"1\" />", Ok("<a x = \"1\" />")),
        ("<a\tx=\"1\"\ny='2'\r/>", Ok("<a\tx=\"1\"\ny='2'\r/>")),
        ("<a x=\"<\"/>", Err("invalid XML content")),
        ("<a x=\">\"/>", Ok("<a x=\">\"/>")),
        ("<a x=\"&\"/>", Err("invalid XML content")),
        ("<a x=\"&foo;\"/>", Err("invalid XML content")),
        ("<a x=\"&#60;\"/>", Ok("<a x=\"&#60;\"/>")),
        ("<a x=\"&#0;\"/>", Err("invalid XML content")),
        ("<a x=\"\u{1}\"/>", Err("invalid XML content")),
        ("<a x=\"\u{ffff}\"/>", Err("invalid XML content")),
        ("<a x=\"'\"/>", Ok("<a x=\"'\"/>")),
        ("<a x='\"'/>", Ok("<a x='\"'/>")),
        ("<!---->", Ok("<!---->")),
        ("<!-- - -->", Ok("<!-- - -->")),
        ("<!-- a -- b -->", Err("invalid XML content")),
        ("<!-- a --->", Err("invalid XML content")),
        ("<!--->", Err("invalid XML content")),
        ("<!-->", Err("invalid XML content")),
        ("<!-- \u{1} -->", Err("invalid XML content")),
        ("<!-- a", Err("invalid XML content")),
        ("<![CDATA[<&>]]>", Ok("<![CDATA[<&>]]>")),
        ("<![CDATA[]]>", Ok("<![CDATA[]]>")),
        ("<![CDATA[x]]]]>", Ok("<![CDATA[x]]]]>")),
        ("<![CDATA[\u{1}]]>", Err("invalid XML content")),
        ("<![CDATA[x]>", Err("invalid XML content")),
        ("<![cdata[x]]>", Err("invalid XML content")),
        ("<?a?>", Ok("<?a?>")),
        ("<?a b??>", Ok("<?a b??>")),
        ("<?a\tb?>", Ok("<?a\tb?>")),
        ("<?a/b?>", Err("invalid XML content")),
        ("<?a?b?>", Err("invalid XML content")),
        ("<?a:b?>", Ok("<?a:b?>")),
        ("<??>", Err("invalid XML content")),
        ("<? a?>", Err("invalid XML content")),
        ("<?1a?>", Err("invalid XML content")),
        ("<?a b", Err("invalid XML content")),
        ("<?a \u{1}?>", Err("invalid XML content")),
        ("<?xml-foo?>", Ok("<?xml-foo?>")),
        ("<?Xml?>", Err("invalid XML content")),
        ("<😀/>", Ok("<😀/>")),
        ("<Ⅰ/>", Ok("<Ⅰ/>")),
        ("<·/>", Err("invalid XML content")),
        ("<a·/>", Ok("<a·/>")),
        ("<\u{300}/>", Err("invalid XML content")),
        ("<;/>", Err("invalid XML content")),
        ("<Ϳ/>", Ok("<Ϳ/>")),
        ("<←/>", Err("invalid XML content")),
        ("<\u{3000}/>", Err("invalid XML content")),
        ("<\u{fdef}/>", Err("invalid XML content")),
        ("<a‿/>", Ok("<a‿/>")),
        ("<a⁁/>", Err("invalid XML content")),
        ("<٠/>", Ok("<٠/>")),
        ("<:/>", Ok("<:/>")),
        ("<a:/>", Ok("<a:/>")),
        ("<:a/>", Ok("<:a/>")),
        ("<a::b/>", Ok("<a::b/>")),
        ("<a:b:c/>", Ok("<a:b:c/>")),
        ("<a:b:c:d/>", Ok("<a:b:c:d/>")),
        ("<a:1></a:1>", Ok("<a:1></a:1>")),
        ("<a:b:-/>", Err("invalid XML content")),
        ("<a:a:1/>", Err("invalid XML content")),
        ("<x a:b:-=\"v\"/>", Err("invalid XML content")),
        ("<p:a></p:a>", Ok("<p:a></p:a>")),
        ("<p:a></a>", Err("invalid XML content")),
        ("<a></p:a>", Err("invalid XML content")),
        ("<a:bc></a:b>", Err("invalid XML content")),
        ("<a:b:></a:b:>", Ok("<a:b:></a:b:>")),
        ("<nosuchprefix:tag/>", Ok("<nosuchprefix:tag/>")),
        ("<invalidns xmlns='&lt;'/>", Ok("<invalidns xmlns='&lt;'/>")),
        ("<a xmlns=\"u\" xmlns=\"u\"/>", Err("invalid XML content")),
        ("<a xmlns=\"\" xmlns=\"u\"/>", Err("invalid XML content")),
        ("<a xmlns:p=\"u\" xmlns:p=\"v\"/>", Err("invalid XML content")),
        ("<a xmlns:p=\"\" xmlns:p=\"u\"/>", Ok("<a xmlns:p=\"\" xmlns:p=\"u\"/>")),
        ("<a xmlns:p=\"u\" xmlns:p=\"\"/>", Ok("<a xmlns:p=\"u\" xmlns:p=\"\"/>")),
        ("<a xmlns:p=\" \" xmlns:p=\"u\"/>", Err("invalid XML content")),
        ("<a xmlns:p=\"&#xA;\" xmlns:p=\"\"/>", Ok("<a xmlns:p=\"&#xA;\" xmlns:p=\"\"/>")),
        ("<a xmlns:xml=\"x\" xmlns:xml=\"y\"/>", Ok("<a xmlns:xml=\"x\" xmlns:xml=\"y\"/>")),
        ("<a xmlns:xmlns=\"x\" xmlns:xmlns=\"y\"/>", Ok("<a xmlns:xmlns=\"x\" xmlns:xmlns=\"y\"/>")),
        ("<a xmlns:p=\"http://www.w3.org/XML/1998/namespace\" xmlns:p=\"u\"/>", Ok("<a xmlns:p=\"http://www.w3.org/XML/1998/namespace\" xmlns:p=\"u\"/>")),
        ("<a xmlns:p=\"&#104;ttp://www.w3.org/XML/1998/namespace\" xmlns:p=\"u\"/>", Ok("<a xmlns:p=\"&#104;ttp://www.w3.org/XML/1998/namespace\" xmlns:p=\"u\"/>")),
        ("<a xmlns=\"http://www.w3.org/2000/xmlns/\" xmlns=\"u\"/>", Ok("<a xmlns=\"http://www.w3.org/2000/xmlns/\" xmlns=\"u\"/>")),
        ("<a xmlns:p:q=\"\" xmlns:p:q=\"u\"/>", Ok("<a xmlns:p:q=\"\" xmlns:p:q=\"u\"/>")),
        ("<a xmlns:p:q=\"u\" xmlns:p:q=\"u\"/>", Err("invalid XML content")),
        ("<a xmlns::p=\"\" xmlns::p=\"u\"/>", Err("invalid XML content")),
        ("<a xmlns:1=\"\" xmlns:1=\"u\"/>", Err("invalid XML content")),
        ("<a xmlns:p=\"u\" xmlns:q=\"u\" p:x=\"1\" q:x=\"2\"/>", Ok("<a xmlns:p=\"u\" xmlns:q=\"u\" p:x=\"1\" q:x=\"2\"/>")),
        ("<a xmlns:p=\"\" p:x=\"1\" p:x=\"2\"/>", Err("invalid XML content")),
        ("<a xml:space=\"foo\" xml:id=\"1 2\"/>", Ok("<a xml:space=\"foo\" xml:id=\"1 2\"/>")),
        ("<!DOCTYPE", Err("invalid XML content")),
        ("<a><!DOCTYPE a></a>", Err("invalid XML content")),
        ("<a/><!DOCTYPE a>", Err("invalid XML content")),
        ("<!doctype a><a/>", Err("invalid XML content")),
        ("<!ELEMENT a ANY>", Err("invalid XML content")),
        ("<!-- c --><!DOCTYPE a><a/>", Ok("<!-- c --><!DOCTYPE a><a/>")),
        ("<?p?><!DOCTYPE a><a/>", Ok("<?p?><!DOCTYPE a><a/>")),
        (" <!DOCTYPE a><a/>", Ok(" <!DOCTYPE a><a/>")),
        ("<!DOCTYPE a><a/>", Ok("<!DOCTYPE a><a/>")),
        ("<!DOCTYPE a>", Err("invalid XML content")),
        ("<!DOCTYPE a><a/><b/>", Err("invalid XML content")),
        ("<!DOCTYPE a><a/>text", Err("invalid XML content")),
        ("<!DOCTYPE a>text<a/>", Err("invalid XML content")),
        ("<!DOCTYPE a><a/> \n", Ok("<!DOCTYPE a><a/> \n")),
        ("<!DOCTYPE a><a/><!--c--><?p?>", Ok("<!DOCTYPE a><a/><!--c--><?p?>")),
        ("<!DOCTYPE a><a/><?xml version=\"1.0\"?>", Err("invalid XML content")),
        ("<!DOCTYPE a><a/><![CDATA[x]]>", Err("invalid XML content")),
        ("<!DOCTYPE a><a/>&amp;", Err("invalid XML content")),
        ("<!DOCTYPE a><b/>", Ok("<!DOCTYPE a><b/>")),
        ("<!DOCTYPE><a/>", Err("invalid XML content")),
        ("<!DOCTYPEa><a/>", Ok("<!DOCTYPEa><a/>")),
        ("<!DOCTYPE 1a><a/>", Err("invalid XML content")),
        ("<!DOCTYPE :a><a/>", Ok("<!DOCTYPE :a><a/>")),
        ("<!DOCTYPE a\n[\n]\n><a/>", Ok("<!DOCTYPE a\n[\n]\n><a/>")),
        ("<!DOCTYPE a SYSTEM \"x.dtd\"><a/>", Ok("<!DOCTYPE a SYSTEM \"x.dtd\"><a/>")),
        ("<!DOCTYPE a PUBLIC \"-//x//y\" \"x.dtd\"><a/>", Ok("<!DOCTYPE a PUBLIC \"-//x//y\" \"x.dtd\"><a/>")),
        ("<!DOCTYPE a PUBLIC \"-//x//y\"><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM \"x\"y><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM 'x'[<!ELEMENT a ANY>]><a/>", Ok("<!DOCTYPE a SYSTEM 'x'[<!ELEMENT a ANY>]><a/>")),
        ("<!DOCTYPE a PUBLIC \"{\" \"u\"><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a PUBLIC \"-'()+,./:=?;!*#@$_%\" \"u\"><a/>", Ok("<!DOCTYPE a PUBLIC \"-'()+,./:=?;!*#@$_%\" \"u\"><a/>")),
        ("<!DOCTYPE a SYSTEM \">\"><a/>", Ok("<!DOCTYPE a SYSTEM \">\"><a/>")),
        ("<!DOCTYPE a []x><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a>[]><a/>", Ok("<!DOCTYPE a>[]><a/>")),
        ("<!DOCTYPE a> []><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [ ]>[]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a>[<!ENTITY e \"x\">]><a>&e;</a>", Ok("<!DOCTYPE a>[<!ENTITY e \"x\">]><a>&e;</a>")),
        ("<!DOCTYPE a><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("\u{feff}<!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"1.0\"?>  <!--c-->\n<?p x?><!DOCTYPE a><a/>", Ok("  <!--c-->\n<?p x?><!DOCTYPE a><a/>")),
        ("<?xml version=\"1.1\"?><!DOCTYPE a><a/>", Ok("<?xml version=\"1.1\"?><!DOCTYPE a><a/>")),
        ("<?xml version=\"1.\"?><!DOCTYPE a><a/>", Ok("<?xml version=\"1.\"?><!DOCTYPE a><a/>")),
        ("<?xml version=\"1.12345\"?><!DOCTYPE a><a/>", Ok("<?xml version=\"1.12345\"?><!DOCTYPE a><a/>")),
        ("<?xml version=\"2.0\"?><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"10.0\"?><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"foo\"?><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"1\"?><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"1.0\" encoding=\"LATIN1\"?><!DOCTYPE a><a>é</a>", Ok("<!DOCTYPE a><a>é</a>")),
        ("<?xml version=\"1.0\" encoding=\"utf-16le\"?><!DOCTYPE a><a/>", Ok("<!DOCTYPE a><a/>")),
        ("<?xml version=\"1.0\" encoding=\"\"?><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"1.0\" encoding=\"1abc\"?><!DOCTYPE a><a/>", Err("invalid XML content")),
        ("<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE a><a/>", Ok("<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE a><a/>")),
        ("<!DOCTYPE a [<!ELEMENT a EMPTY>]><a>x</a>", Ok("<!DOCTYPE a [<!ELEMENT a EMPTY>]><a>x</a>")),
        ("<!DOCTYPE a [<!ELEMENT a (#PCDATA|b)*>]><a/>", Ok("<!DOCTYPE a [<!ELEMENT a (#PCDATA|b)*>]><a/>")),
        ("<!DOCTYPE a [<!ELEMENT a (#PCDATA|b)>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a (#PCDATA)*>]><a/>", Ok("<!DOCTYPE a [<!ELEMENT a (#PCDATA)*>]><a/>")),
        ("<!DOCTYPE a [<!ELEMENT a (#PCDATA)+>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a ((b,c)?,(d|e)*)>]><a/>", Ok("<!DOCTYPE a [<!ELEMENT a ((b,c)?,(d|e)*)>]><a/>")),
        ("<!DOCTYPE a [<!ELEMENT a (b,c|d)>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a (b ?)>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a ()>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a empty>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a ANYx>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENTa ANY>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ELEMENT a (b|#PCDATA)*>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a>]><a/>", Ok("<!DOCTYPE a [<!ATTLIST a>]><a/>")),
        ("<!DOCTYPE a [<!ATTLIST a x CDATA #FIXED \"d\">]><a x=\"e\"/>", Ok("<!DOCTYPE a [<!ATTLIST a x CDATA #FIXED \"d\">]><a x=\"e\"/>")),
        ("<!DOCTYPE a [<!ATTLIST a x ID #IMPLIED y IDREFS \"q\" z NOTATION (p|q) #REQUIRED w (1|-x) \"1\">]><a/>", Ok("<!DOCTYPE a [<!ATTLIST a x ID #IMPLIED y IDREFS \"q\" z NOTATION (p|q) #REQUIRED w (1|-x) \"1\">]><a/>")),
        ("<!DOCTYPE a [<!ATTLIST a x CDATA \"<\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a x CDATA \"&foo;\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a x CDATA \"&e;\"><!ENTITY e \"y\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"y\"><!ATTLIST a x CDATA \"&e;\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e \"y\"><!ATTLIST a x CDATA \"&e;\">]><a/>")),
        ("<!DOCTYPE a [<!ATTLIST a x cdata #IMPLIED>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a x CDATA#IMPLIED>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a x (p q) \"p\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a x CDATA #FIXED\"d\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ATTLIST a xmlns:p NMTOKEN #IMPLIED>]><a xmlns:p=\" \" xmlns:p=\"u\"/>", Ok("<!DOCTYPE a [<!ATTLIST a xmlns:p NMTOKEN #IMPLIED>]><a xmlns:p=\" \" xmlns:p=\"u\"/>")),
        ("<!DOCTYPE a [<!ATTLIST a xmlns:p CDATA #IMPLIED>]><a xmlns:p=\" \" xmlns:p=\"u\"/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"<b/>\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e \"<b/>\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e \"<b>\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"<b>\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e \"<b>\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e \"<b>\">]><a x=\"&e;\"/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"b\">]><a x=\"&e;\"/>", Ok("<!DOCTYPE a [<!ENTITY e \"b\">]><a x=\"&e;\"/>")),
        ("<!DOCTYPE a [<!ENTITY e \"&f;\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e \"&f;\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e \"&f;\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"&f;\"><!ENTITY f \"y\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e \"&f;\"><!ENTITY f \"y\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e \"&e;\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"&\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"&#38;\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"&#38;#38;\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e \"&#38;#38;\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e \"&#60;\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"&#60;\">]><a x=\"&e;\"/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"&#38;#60;\">]><a x=\"&e;\"/>", Ok("<!DOCTYPE a [<!ENTITY e \"&#38;#60;\">]><a x=\"&e;\"/>")),
        ("<!DOCTYPE a [<!ENTITY e \"&#38;#0;\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e \"&#38;#0;\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e \"&#38;#0;\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"%p;\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"%\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"x&#1;\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"x\"><!ENTITY e \"<\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e \"x\"><!ENTITY e \"<\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e \"<\"><!ENTITY e \"x\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY lt \"<\">]><a>&lt;</a>", Ok("<!DOCTYPE a [<!ENTITY lt \"<\">]><a>&lt;</a>")),
        ("<!DOCTYPE a [<!ENTITY e \"]]>\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"]]\">]><a>&e;></a>", Ok("<!DOCTYPE a [<!ENTITY e \"]]\">]><a>&e;></a>")),
        ("<!DOCTYPE a [<!ENTITY e \"<?xml version='1.0'?>\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"<![CDATA[x]]>\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e \"<![CDATA[x]]>\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e \"x\">]>&e;<a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"x\">]><a/>&e;", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"<b>\"><!ENTITY f \"</b>\">]><a>&e;&f;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e x>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e\"x\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e \"x\" junk>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY a:b \"x\">]><a>&a:b;</a>", Ok("<!DOCTYPE a [<!ENTITY a:b \"x\">]><a>&a:b;</a>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"x\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"x\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"x\">]><a x=\"&e;\"/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e PUBLIC \"p\" \"\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e PUBLIC \"p\" \"\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e PUBLIC \"p\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"u\" NDATA n>]><a/>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"u\" NDATA n>]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"u\" NDATA n>]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"u\"NDATA n>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % e SYSTEM \"u\" NDATA n>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"a b\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"a#b\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"%41\">]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"%41\">]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"%zz\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"1:a\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"a+.-:b\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"a+.-:b\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"//u@h:80/p?q\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"//u@h:80/p?q\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"//h:/p\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"//[::1]/a\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"//[::1]/a\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"//[]/a\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY e SYSTEM \"//[]/a\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY e SYSTEM \"é\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!NOTATION n SYSTEM \">\">]><a/>", Ok("<!DOCTYPE a [<!NOTATION n SYSTEM \">\">]><a/>")),
        ("<!DOCTYPE a [<!NOTATION n PUBLIC \"p\">]><a/>", Ok("<!DOCTYPE a [<!NOTATION n PUBLIC \"p\">]><a/>")),
        ("<!DOCTYPE a [<!NOTATION n>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!-- c -- d -->]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [x]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<![INCLUDE[]]>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<?xml version=\"1.0\"?>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM \"u\"><a x=\"&f;\">&f;</a>", Ok("<!DOCTYPE a SYSTEM \"u\"><a x=\"&f;\">&f;</a>")),
        ("<!DOCTYPE a SYSTEM \"u\" [<!ENTITY e \"&f;\">]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM \"u\" [<!ENTITY e \"&f;\">]><a x=\"&e;\"/>", Ok("<!DOCTYPE a SYSTEM \"u\" [<!ENTITY e \"&f;\">]><a x=\"&e;\"/>")),
        ("<!DOCTYPE a SYSTEM \"u\" [<!ENTITY e \"<b x='&f;'/>\">]><a>&e;</a>", Err("invalid XML content")),
        ("<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE a SYSTEM \"x\"><a>&f;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM \"u\"><a xmlns:p=\"&f;\" xmlns:p=\"u\"/>", Ok("<!DOCTYPE a SYSTEM \"u\"><a xmlns:p=\"&f;\" xmlns:p=\"u\"/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"x\">]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p \"x\">]><a/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"> %p;]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"> %p;]><a/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ENTITY f 'x'>\"> %p;]><a>&f;</a>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!ENTITY f 'x'>\"> %p;]><a>&f;</a>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ENTITY e '&#38;'>\"> %p;]><a>&e;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ENTITY e '&#38;#38;#60;'>\"> %p;]><a>&e;</a>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!ENTITY e '&#38;#38;#60;'>\"> %p;]><a>&e;</a>")),
        ("<!DOCTYPE a [<!ENTITY % p \"x\"> %p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a\"> %p; ANY>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [%p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a SYSTEM \"u\" [%p;]><a/>", Ok("<!DOCTYPE a SYSTEM \"u\" [%p;]><a/>")),
        ("<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE a SYSTEM \"u\" [%p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p SYSTEM \"u\"> %p; %q;]><a>&f;</a>", Ok("<!DOCTYPE a [<!ENTITY % p SYSTEM \"u\"> %p; %q;]><a>&f;</a>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"> %p; %q;]><a>&f;</a>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"> %p; %q;]><a>&f;</a>")),
        ("<!DOCTYPE a [<!ENTITY % p \"\"> %p; %p;]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p \"\"> %p; %p;]><a/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"> %p; %p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\">%p;%p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"> %p; <!--x--> %p;]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"> %p; <!--x--> %p;]><a/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"><!ENTITY % q \"<!--q-->\"> %p; %q; %p;]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"><!ENTITY % q \"<!--q-->\"> %p; %q; %p;]><a/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!--c-->\"><!ENTITY % q \" \"> %p; %q; %p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"> %p;<!ENTITY % p \"x\"> %p;]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"> %p;<!ENTITY % p \"x\"> %p;]><a/>")),
        ("<!DOCTYPE a [<!ENTITY % p \"x\"><!ENTITY % p \"<!--c-->\"> %p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"> %p ;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"> % p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY %p \"x\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p \"<!ELEMENT a ANY>\"><!ENTITY % q \"%p;\">]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % q \"a\"><!ELEMENT %q; ANY>]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p SYSTEM \">\"> %p;]><a/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY % p SYSTEM \"\"> %p;]><a/>", Ok("<!DOCTYPE a [<!ENTITY % p SYSTEM \"\"> %p;]><a/>")),
        ("<!DOCTYPE a [<!ENTITY f SYSTEM \"u\" NDATA >]><a/>", Ok("<!DOCTYPE a [<!ENTITY f SYSTEM \"u\" NDATA >]><a/>")),
        ("<!DOCTYPE a [<!ENTITY f SYSTEM \"u\" NDATA >]><a>&f;</a>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY f SYSTEM \"u\" NDATA >]><a x=\"&f;\"/>", Err("invalid XML content")),
        ("<!DOCTYPE a [<!ENTITY f SYSTEM \"u\" NDATA>]><a/>", Err("invalid XML content")),
    ];

    fn answer(s: &str) -> std::result::Result<String, String> {
        match xml_in(s) {
            Ok(kept) => {
                assert_eq!(kept, s);
                Ok(xml_out(s))
            }
            Err(e) => Err(e.0),
        }
    }

    fn taken(s: &str) -> bool {
        xml_in(s).is_ok()
    }

    fn refused_by_name(s: &str) -> bool {
        xml_in(s).is_err_and(|e| e.0.starts_with("xml ") && e.0.ends_with(" is not read"))
    }

    fn nest(open: &str, close: &str, n: usize) -> String {
        format!("{}{}", open.repeat(n), close.repeat(n))
    }

    #[test]
    fn values_are_taken_refused_and_written_as_the_server_does() {
        for (s, want) in SERVER {
            let want = want.map(str::to_string).map_err(str::to_string);
            assert_eq!(answer(s), want, "{s:?}");
        }
    }

    #[test]
    fn the_type_tables_values_are_kept_and_written_as_the_server_writes_them() {
        for (s, out) in [
            ("<a/>", "<a/>"),
            ("", ""),
            (
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><a>b</a>",
                "<a>b</a>",
            ),
            ("<?xml version=\"1.1\"?><a/>", "<?xml version=\"1.1\"?><a/>"),
            (
                "<?xml version=\"1.0\" standalone=\"yes\"?><a/>",
                "<?xml version=\"1.0\" standalone=\"yes\"?><a/>",
            ),
            ("<a>text</a><b/>", "<a>text</a><b/>"),
            ("plain text", "plain text"),
            (
                "<a x=\"1\" y='2'>&amp;&lt;</a>",
                "<a x=\"1\" y='2'>&amp;&lt;</a>",
            ),
            ("<é>ü</é>", "<é>ü</é>"),
            ("<b>c</b>", "<b>c</b>"),
        ] {
            assert_eq!(answer(s), Ok(out.to_string()), "{s:?}");
        }
    }

    #[test]
    fn elements_nest_256_deep_in_content_and_257_in_a_document() {
        assert!(taken(&nest("<a>", "</a>", 256)));
        assert!(!taken(&nest("<a>", "</a>", 257)));
        assert!(taken(&format!(
            "{}<b/>{}",
            "<a>".repeat(255),
            "</a>".repeat(255)
        )));
        assert!(!taken(&format!(
            "{}<b/>{}",
            "<a>".repeat(256),
            "</a>".repeat(256)
        )));
        assert!(taken(&format!("<!DOCTYPE a>{}", nest("<a>", "</a>", 257))));
        assert!(!taken(&format!("<!DOCTYPE a>{}", nest("<a>", "</a>", 258))));
        let entity = |n| {
            format!(
                "<!DOCTYPE a [<!ENTITY e \"{}\">]><a>&e;</a>",
                nest("<a>", "</a>", n)
            )
        };
        assert!(taken(&entity(256)));
        assert!(!taken(&entity(257)));
        let model = |n| {
            format!(
                "<!DOCTYPE a [<!ELEMENT a {}>]><a/>",
                nest("(", ")", n).replacen(")", "b)", 1)
            )
        };
        assert!(taken(&model(128)));
        assert!(!taken(&model(129)));
    }

    /// A DOCTYPE declaring `e0` as `x` and each `e{i}` as one reference to `e{i-1}`, `entities`
    /// in all, read as content where `content`, else in an attribute.
    fn chain(entities: usize, content: bool) -> String {
        let mut s = String::from("<!DOCTYPE a [<!ENTITY e0 \"x\">");
        for i in 1..entities {
            s.push_str(&format!("<!ENTITY e{i} \"&e{};\">", i - 1));
        }
        let last = entities - 1;
        if content {
            s.push_str(&format!("]><a>&e{last};</a>"));
        } else {
            s.push_str(&format!("]><a x=\"&e{last};\"/>"));
        }
        s
    }

    #[test]
    fn entities_nest_20_deep_in_content_and_40_in_an_attribute() {
        assert!(taken(&chain(20, true)));
        assert!(!taken(&chain(21, true)));
        assert!(taken(&chain(40, false)));
        assert!(!taken(&chain(41, false)));
        let mixed = |attribute: usize, content: usize| {
            let mut s = String::from("<!DOCTYPE a [<!ENTITY f0 \"1\">");
            for i in 1..attribute {
                s.push_str(&format!("<!ENTITY f{i} \"&f{};\">", i - 1));
            }
            s.push_str(&format!("<!ENTITY g0 \"<b x='&f{};'/>\">", attribute - 1));
            for i in 1..content {
                s.push_str(&format!("<!ENTITY g{i} \"&g{};\">", i - 1));
            }
            s.push_str(&format!("]><a>&g{};</a>", content - 1));
            s
        };
        assert!(taken(&mixed(38, 1)));
        assert!(!taken(&mixed(39, 1)));
        assert!(taken(&mixed(26, 7)));
        assert!(!taken(&mixed(27, 7)));
        assert!(taken(&mixed(2, 19)));
        assert!(!taken(&mixed(1, 20)));
    }

    #[test]
    fn names_hold_50000_bytes_each_part() {
        let a = |n| "a".repeat(n);
        assert!(taken(&format!("<{}/>", a(50_000))));
        assert!(!taken(&format!("<{}/>", a(50_001))));
        assert!(taken(&format!("<{}/>", "é".repeat(25_000))));
        assert!(!taken(&format!("<{}/>", "é".repeat(25_001))));
        assert!(taken(&format!("<p:{}/>", a(50_000))));
        assert!(!taken(&format!("<p:{}/>", a(50_001))));
        assert!(taken(&format!("<{}:{}/>", a(30_000), a(30_000))));
        assert!(!taken(&format!("<:{}/>", a(50_000))));
        assert!(!taken(&format!("<a x=\"1\" {}=\"1\"/>", a(50_001))));
        assert!(!taken(&format!("<?{}?>", a(50_001))));
        assert!(!taken(&format!(
            "<!DOCTYPE a [<!ENTITY {} \"x\">]><a/>",
            a(50_001)
        )));
    }

    #[test]
    fn comments_instructions_and_cdata_hold_10000000_bytes() {
        let x = |n| "x".repeat(n);
        for (open, close) in [("<!--", "-->"), ("<?p ", "?>"), ("<![CDATA[", "]]>")] {
            assert!(
                taken(&format!("{open}{}{close}", x(SECTION_BYTES))),
                "{open}"
            );
            assert!(
                !taken(&format!("{open}{}{close}", x(SECTION_BYTES + 1))),
                "{open}"
            );
        }
        assert!(taken(&format!("<?p   {}?>", x(SECTION_BYTES - 1))));
        assert!(taken(&format!("<!--{}-->", "é".repeat(SECTION_BYTES / 2))));
        assert!(!taken(&format!(
            "<!--{}-->",
            "é".repeat(SECTION_BYTES / 2 + 1)
        )));
    }

    #[test]
    fn a_start_tag_with_attributes_ends_by_byte_10000000() {
        let tag = |n| format!("<a x=\"{}\"/>", "x".repeat(n));
        assert!(taken(&tag(LOOKUP_BYTES - 9)));
        assert!(!taken(&tag(LOOKUP_BYTES - 8)));
        assert!(taken(&format!(
            "<a x=\"1\">{}</a>",
            "x".repeat(LOOKUP_BYTES)
        )));
    }

    #[test]
    fn what_the_server_takes_and_this_does_not_is_refused_by_name() {
        let x = |n| "x".repeat(n);
        assert!(refused_by_name(&format!("{}<a x=\"1\"/>", x(LOOKUP_BYTES))));
        assert!(refused_by_name(&format!(
            "<!DOCTYPE a [<!--{}--><!ELEMENT a ANY>]><a/>",
            x(LOOKUP_BYTES - 10)
        )));
        assert!(refused_by_name(
            "<?xml version=\"1.0\" encoding=\"ISO646-US\"?><!DOCTYPE a><a/>"
        ));
        assert!(refused_by_name(
            "<!DOCTYPE a [<!ENTITY % p \"&#37;q;\"><!ENTITY % q \"<!ELEMENT a ANY>\"> %p;]><a/>"
        ));
        assert!(refused_by_name(&format!("<{}:/>", "a".repeat(50_000))));
        let tenfold = |levels: usize, leaf: &str| {
            let mut s = format!("<!DOCTYPE a [<!ENTITY e0 \"{leaf}\">");
            for i in 1..=levels {
                s.push_str(&format!(
                    "<!ENTITY e{i} \"{}\">",
                    format!("&e{};", i - 1).repeat(10)
                ));
            }
            s.push_str(&format!("]><a>&e{levels};</a>"));
            s
        };
        assert!(taken(&tenfold(4, "xxxxxxxxxx")));
        assert!(refused_by_name(&tenfold(5, "xxxxxxxxxx")));
        let flat = |n: usize| format!("<!DOCTYPE a [<!ENTITY e \"x\">]><a>{}</a>", "&e;".repeat(n));
        assert!(taken(&flat(EXPANDED_REFERENCES)));
        assert!(refused_by_name(&flat(EXPANDED_REFERENCES + 1)));
    }
}
