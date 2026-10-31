//! PostgreSQL 19's own Unicode categories and properties (`categories.txt`, written by inner-warren's
//! `categories.nu` from its source), and the character classes its builtin provider reads from
//! them, as `unicode_category.c` gives them: under pg_c_utf8 the POSIX forms of digit, alnum and
//! punct, under pg_unicode_fast Unicode's.

use std::sync::OnceLock;

// The General Categories, as `unicode_category.h` numbers them.
const DECIMAL_NUMBER: u8 = 9;
const SPACE_SEPARATOR: u8 = 12;
const CONTROL: u8 = 15;
const SURROGATE: u8 = 18;
const DASH_PUNCTUATION: u8 = 19;
const OPEN_PUNCTUATION: u8 = 20;
const CLOSE_PUNCTUATION: u8 = 21;
const CONNECTOR_PUNCTUATION: u8 = 22;
const OTHER_PUNCTUATION: u8 = 23;
const MATH_SYMBOL: u8 = 24;
const CURRENCY_SYMBOL: u8 = 25;
const MODIFIER_SYMBOL: u8 = 26;
const OTHER_SYMBOL: u8 = 27;
const INITIAL_PUNCTUATION: u8 = 28;
const FINAL_PUNCTUATION: u8 = 29;
const UNASSIGNED: u8 = 0;

// ASCII's properties, as the header's bits.
const PROP_ALPHABETIC: u8 = 1 << 0;
const PROP_LOWERCASE: u8 = 1 << 1;
const PROP_UPPERCASE: u8 = 1 << 2;
const PROP_WHITE_SPACE: u8 = 1 << 5;

const TAB: u32 = 0x09;

struct Tables {
    categories: Vec<(u32, u32, u8)>,
    ascii: [(u8, u8); 128],
    alphabetic: Vec<(u32, u32)>,
    lowercase: Vec<(u32, u32)>,
    uppercase: Vec<(u32, u32)>,
    white_space: Vec<(u32, u32)>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let hex = |s: &str| u32::from_str_radix(s, 16).expect("a code point of categories.txt");
        let mut t = Tables {
            categories: Vec::new(),
            ascii: [(0, 0); 128],
            alphabetic: Vec::new(),
            lowercase: Vec::new(),
            uppercase: Vec::new(),
            white_space: Vec::new(),
        };
        let mut section = "";
        for line in include_str!("categories.txt").lines() {
            if let Some(name) = line.strip_prefix("= ") {
                section = name;
                continue;
            }
            let f: Vec<&str> = line.split(' ').collect();
            let number = |s: &str| s.parse::<u8>().expect("a number of categories.txt");
            match section {
                "categories" => t.categories.push((hex(f[0]), hex(f[1]), number(f[2]))),
                "ascii" => t.ascii[hex(f[0]) as usize] = (number(f[1]), number(f[2])),
                name => {
                    let range = (hex(f[0]), hex(f[1]));
                    match name {
                        "alphabetic" => t.alphabetic.push(range),
                        "lowercase" => t.lowercase.push(range),
                        "uppercase" => t.uppercase.push(range),
                        _ => t.white_space.push(range),
                    }
                }
            }
        }
        t
    })
}

/// Whether a code point lies in one of the ranges (`range_search`).
fn in_ranges(ranges: &[(u32, u32)], code: u32) -> bool {
    ranges
        .binary_search_by(|&(first, last)| {
            if code < first {
                std::cmp::Ordering::Greater
            } else if code > last {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// A code point's General Category (`unicode_category`).
fn category(code: u32) -> u8 {
    let t = tables();
    if code < 0x80 {
        return t.ascii[code as usize].0;
    }
    match t.categories.binary_search_by(|&(first, last, _)| {
        if code < first {
            std::cmp::Ordering::Greater
        } else if code > last {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => t.categories[i].2,
        Err(_) => UNASSIGNED,
    }
}

fn property(code: u32, bit: u8, ranges: &[(u32, u32)]) -> bool {
    if code < 0x80 {
        return tables().ascii[code as usize].1 & bit != 0;
    }
    in_ranges(ranges, code)
}

fn alphabetic(code: u32) -> bool {
    property(code, PROP_ALPHABETIC, &tables().alphabetic)
}

fn white_space(code: u32) -> bool {
    property(code, PROP_WHITE_SPACE, &tables().white_space)
}

fn punctuation(cat: u8) -> bool {
    matches!(
        cat,
        CONNECTOR_PUNCTUATION
            | DASH_PUNCTUATION
            | OPEN_PUNCTUATION
            | CLOSE_PUNCTUATION
            | INITIAL_PUNCTUATION
            | FINAL_PUNCTUATION
            | OTHER_PUNCTUATION
    )
}

fn symbol(cat: u8) -> bool {
    matches!(
        cat,
        MATH_SYMBOL | CURRENCY_SYMBOL | MODIFIER_SYMBOL | OTHER_SYMBOL
    )
}

/// `pg_u_isdigit`: under `posix` ASCII's digits alone.
pub fn isdigit(code: u32, posix: bool) -> bool {
    if posix {
        (0x30..=0x39).contains(&code)
    } else {
        category(code) == DECIMAL_NUMBER
    }
}

/// `pg_u_isalpha`.
pub fn isalpha(code: u32) -> bool {
    alphabetic(code)
}

/// `pg_u_isalnum`.
pub fn isalnum(code: u32, posix: bool) -> bool {
    isalpha(code) || isdigit(code, posix)
}

/// `pg_u_isupper`.
pub fn isupper(code: u32) -> bool {
    property(code, PROP_UPPERCASE, &tables().uppercase)
}

/// `pg_u_islower`.
pub fn islower(code: u32) -> bool {
    property(code, PROP_LOWERCASE, &tables().lowercase)
}

/// `pg_u_isblank`.
pub fn isblank(code: u32) -> bool {
    code == TAB || category(code) == SPACE_SEPARATOR
}

/// `pg_u_isgraph`.
pub fn isgraph(code: u32) -> bool {
    let cat = category(code);
    !(matches!(cat, CONTROL | SURROGATE | UNASSIGNED) || isspace(code))
}

/// `pg_u_isprint`.
pub fn isprint(code: u32) -> bool {
    if category(code) == CONTROL {
        return false;
    }
    isgraph(code) || isblank(code)
}

/// `pg_u_ispunct`: under `posix` punctuation and symbols that are not letters.
pub fn ispunct(code: u32, posix: bool) -> bool {
    if posix {
        if isalpha(code) {
            return false;
        }
        let cat = category(code);
        punctuation(cat) || symbol(cat)
    } else {
        punctuation(category(code))
    }
}

/// `pg_u_isspace`.
pub fn isspace(code: u32) -> bool {
    white_space(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_classify_as_postgresql_s_builtin_provider() {
        assert!(isalpha('a' as u32) && isalpha(0x1500) && !isalpha('1' as u32));
        assert!(isdigit(0x0663, false) && !isdigit(0x0663, true));
        assert!(isupper(0xc9) && islower(0xe9) && !isupper(0x1500));
        assert!(isspace(0x2003) && isblank(0x2003) && !isblank('\n' as u32));
        assert!(ispunct('$' as u32, true) && !ispunct('$' as u32, false));
        assert!(
            !isgraph(0x0378) && isgraph(0x1500) && isprint(' ' as u32) && !isprint('\n' as u32)
        );
    }
}
