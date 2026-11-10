//! A built-in view's carried text, PostgreSQL's pretty text at wrap column 0, written at another
//! wrap column as pg_get_viewdef_worker writes it: each target list and FROM list rejoined, an item
//! kept on its list's line where it fits within the column, a target list's item after one of
//! several lines on a line of its own, an item that begins its own line (a CASE) left there; with
//! no wrap column, every item on its list's line.

/// The words that begin a clause on a line of its own, which end the list before them.
const CLAUSES: [&str; 13] = [
    "FROM", "WHERE", "GROUP", "HAVING", "WINDOW", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR",
    "UNION", "INTERSECT", "EXCEPT",
];

/// The text written at `wrap`, as PostgreSQL writes it at that column.
pub(crate) fn rewrapped(text: &str, wrap: i32) -> String {
    let mut out = String::with_capacity(text.len());
    emit(text, 0, text.len(), wrap, &mut out);
    out
}

/// Writes `text[from..to]` into `out`: each parenthesized part in turn, each target list after a
/// SELECT and each FROM list rejoined.
fn emit(text: &str, from: usize, to: usize, wrap: i32, out: &mut String) {
    let b = text.as_bytes();
    let mut i = from;
    while i < to {
        match b[i] {
            b'\'' | b'"' => {
                let j = quoted_end(b, i).min(to);
                out.push_str(&text[i..j]);
                i = j;
            }
            b'(' | b'[' => {
                let j = closing(b, i).min(to);
                out.push(b[i] as char);
                emit(text, i + 1, j, wrap, out);
                if j < to {
                    out.push(b[j] as char);
                }
                i = j + 1;
            }
            _ if word_at(b, i, "SELECT") => {
                out.push_str("SELECT");
                i = distinct(text, i + "SELECT".len(), out);
                i = list(text, i, to, wrap, out, true);
            }
            _ if word_at(b, i, "FROM") && begins_line(b, i) => {
                out.push_str("FROM");
                i = list(text, i + "FROM".len(), to, wrap, out, false);
            }
            _ => {
                let n = text[i..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&text[i..i + n]);
                i += n;
            }
        }
    }
}

/// A SELECT's DISTINCT, or DISTINCT ON and its expressions, written as it stands: where the list
/// begins.
fn distinct(text: &str, at: usize, out: &mut String) -> usize {
    let b = text.as_bytes();
    if !word_at(b, at + 1, "DISTINCT") {
        return at;
    }
    let mut i = at + 1 + "DISTINCT".len();
    if word_at(b, i + 1, "ON") {
        i += 1 + "ON".len();
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        if b.get(i) == Some(&b'(') {
            i = closing(b, i) + 1;
        }
    }
    out.push_str(&text[at..i]);
    i
}

/// A target list (`target`) or a FROM list from `at`, just past its keyword, to its end: the line
/// that begins its query's next clause, or the end of the part it lies in. Each item after the
/// first written on the list's line where it fits, else on a line of its own as the item stood
/// at wrap 0.
fn list(text: &str, at: usize, to: usize, wrap: i32, out: &mut String, target: bool) -> usize {
    let b = text.as_bytes();
    let end = list_end(b, at, to, target);
    // The commas that end a line at the list's own depth: its items' bounds.
    let mut commas = Vec::new();
    let mut i = at;
    while i < end {
        match b[i] {
            b'\'' | b'"' => i = quoted_end(b, i),
            b'(' | b'[' => i = closing(b, i) + 1,
            b',' if b.get(i + 1) == Some(&b'\n') => {
                commas.push(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    let mut starts = vec![at];
    starts.extend(commas.iter().map(|c| c + 1));
    let mut ends: Vec<usize> = commas.clone();
    ends.push(end);
    let mut last_multiline = false;
    for (k, (&s, &e)) in starts.iter().zip(&ends).enumerate() {
        // An item begins its own line where its first word does, a CASE; any other begins past
        // the line its list breaks onto, whose indentation is kept for a break.
        let (indent, from) = match b.get(s) {
            Some(b'\n') => {
                let content = s + 1 + b[s + 1..e].iter().take_while(|c| **c == b' ').count();
                match word_at(b, content, "CASE") {
                    true => (None, s),
                    false => (Some(&text[s..content]), content),
                }
            }
            Some(b' ') => (None, s + 1),
            _ => (None, s),
        };
        out.push_str(if k == 0 { " " } else { ", " });
        // The first FROM item is written where it stands; every other item on a line of its own
        // first, to be measured.
        if !target && k == 0 {
            if b.get(from) == Some(&b'\n') {
                trim_spaces(out);
            }
            emit(text, from, e, wrap, out);
            continue;
        }
        let mut item = String::new();
        emit(text, from, e, wrap, &mut item);
        if wrap >= 0 {
            let leading = item.starts_with('\n');
            if leading {
                trim_spaces(out);
            } else if k > 0 {
                let line = out.len() - out.rfind('\n').map_or(0, |n| n + 1);
                if line + item.len() > wrap as usize || (target && last_multiline) {
                    trim_spaces(out);
                    out.push_str(indent.unwrap_or("\n"));
                }
            }
            last_multiline = item[usize::from(leading)..].contains('\n');
        }
        out.push_str(&item);
    }
    end
}

/// Where a list ends: before the newline whose line begins its query's next clause, or before a
/// semicolon, or at the end of the part it lies in.
fn list_end(b: &[u8], at: usize, to: usize, target: bool) -> usize {
    let mut i = at;
    while i < to {
        match b[i] {
            b'\'' | b'"' => i = quoted_end(b, i),
            b'(' | b'[' => i = closing(b, i) + 1,
            b';' => return i,
            b'\n' => {
                let word = i + 1 + b[i + 1..to].iter().take_while(|c| **c == b' ').count();
                let ends = CLAUSES
                    .iter()
                    .any(|c| (target || *c != "FROM") && word_at(b, word, c));
                if ends {
                    return i;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    to.min(i)
}

/// Whether `word` stands at `i`, whole.
fn word_at(b: &[u8], i: usize, word: &str) -> bool {
    let w = word.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    b.len() >= i + w.len()
        && &b[i..i + w.len()] == w
        && (i == 0 || !ident(b[i - 1]))
        && b.get(i + w.len()).is_none_or(|c| !ident(*c))
}

/// Whether only spaces stand between `i` and the line's start.
fn begins_line(b: &[u8], i: usize) -> bool {
    b[..i].iter().rev().take_while(|c| **c != b'\n').all(|c| *c == b' ')
}

/// Past a quoted string or name that begins at `i`, its doubled quotes inside it.
fn quoted_end(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// The parenthesis or bracket that closes the one at `i`.
fn closing(b: &[u8], i: usize) -> usize {
    let mut depth = 0;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'\'' | b'"' => {
                j = quoted_end(b, j);
                continue;
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j += 1;
    }
    b.len()
}

/// The spaces a line ends in taken off, as removeStringInfoSpaces takes them.
fn trim_spaces(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::rewrapped;

    /// 18.6's text of `CREATE VIEW v AS SELECT a.x, b.y FROM a, b` at wrap 0, 12 and 11: a list's
    /// last item measured without the semicolon that ends the statement.
    #[test]
    fn a_list_s_last_item_is_measured_without_the_semicolon() {
        let zero = " SELECT a.x,\n    b.y\n   FROM a,\n    b;";
        assert_eq!(rewrapped(zero, 12), " SELECT a.x,\n    b.y\n   FROM a, b;");
        assert_eq!(rewrapped(zero, 11), zero);
    }
}
