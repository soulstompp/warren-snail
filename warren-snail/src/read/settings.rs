//! PostgreSQL's settings as a writer's session takes them: those the snail answers under, fixed;
//! those that change no answer, taken at any value PostgreSQL takes; those the session reads where
//! they act, taken so too; those it answers under one value of, taken at that value alone; the
//! transaction's modes; and the server's own, refused by name.
//! Each value is read and shown as PostgreSQL's `guc.c` reads and shows it.

use crate::{Error, Result};

/// One of PostgreSQL's settings, as its release's `pg_settings` lists it.
#[derive(Debug)]
pub(crate) struct Guc {
    pub(crate) name: &'static str,
    vartype: &'static str,
    unit: &'static str,
    pub(crate) boot: &'static str,
    enumvals: Vec<&'static str>,
    min: &'static str,
    max: &'static str,
}

fn read_table(text: &'static str) -> Vec<Guc> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            Guc {
                name: f[0],
                vartype: f[1],
                unit: f[2],
                boot: f[3],
                enumvals: f[4].split(',').filter(|s| !s.is_empty()).collect(),
                min: f[5],
                max: f[6],
            }
        })
        .collect()
}

/// The settings of the file's release: 19's for 19, 18.6's for the releases before it.
fn table(release: u8) -> &'static [Guc] {
    static T18: std::sync::OnceLock<Vec<Guc>> = std::sync::OnceLock::new();
    static T19: std::sync::OnceLock<Vec<Guc>> = std::sync::OnceLock::new();
    if release >= 19 {
        T19.get_or_init(|| read_table(include_str!("../settings19.txt")))
    } else {
        T18.get_or_init(|| read_table(include_str!("../settings18.txt")))
    }
}

/// A setting by its name, in any case, as the file's release has it.
pub(crate) fn find(release: u8, name: &str) -> Option<&'static Guc> {
    table(release)
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case(name))
}

/// How the snail takes a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// One the snail answers under, at its one value.
    Fixed,
    /// One that changes no answer: taken at any value PostgreSQL takes.
    Taken,
    /// One the session reads where it acts, as PostgreSQL's session does: taken at any value
    /// PostgreSQL takes.
    Read,
    /// One the snail answers under one value of: taken at it, refused at any other for the reason.
    Own(&'static str, &'static str),
    /// The transaction's modes, and the session's for the transactions after.
    Mode,
    /// The schemas a bare name means: refused, a snail naming each table by its schema.
    Path,
    /// A fact of the snail PostgreSQL shows: its value.
    Preset(&'static str),
    /// A name with a dot, the session's own variable: taken at any value.
    Custom,
    /// The server's own.
    Server,
}

/// The settings that change no answer: the planner's, the memory and workers a statement may use,
/// the messages sent and logged, and VACUUM's, which a snail never runs.
const TAKEN: [&str; 16] = [
    "statement_timeout",
    "lock_timeout",
    "application_name",
    "client_min_messages",
    "check_function_bodies",
    "trace_notify",
    "trace_sort",
    "synchronize_seqscans",
    "escape_string_warning",
    "default_toast_compression",
    "icu_validation_level",
    "row_security",
    "stats_fetch_consistency",
    "commit_siblings",
    "synchronous_commit",
    "log_parameter_max_length_on_error",
];

/// The settings the snail answers under one value of, and why.
const OWN: [(&str, &str); 17] = [
    (
        "idle_in_transaction_session_timeout",
        "the snail ends no session",
    ),
    ("idle_session_timeout", "the snail ends no session"),
    (
        "transaction_timeout",
        "the snail never cancels a transaction",
    ),
    (
        "client_connection_check_interval",
        "a snail has no connection",
    ),
    ("gin_fuzzy_search_limit", "a snail answers every row"),
    ("xmloption", "it changes how xml is read"),
    ("xmlbinary", "it changes how xml writes bytes"),
    ("array_nulls", "it changes how an array's NULL is read"),
    ("transform_null_equals", "it changes what = NULL means"),
    (
        "quote_all_identifiers",
        "it changes the names the snail writes",
    ),
    ("backslash_quote", "it changes how a literal is read"),
    ("exit_on_error", "the snail ends no session"),
    ("default_tablespace", "a snail has no tablespace"),
    ("temp_tablespaces", "a snail has no tablespace"),
    (
        "default_table_access_method",
        "a snail's tables are its own",
    ),
    (
        "restrict_nonsystem_relation_kind",
        "it changes which relations a statement reads",
    ),
    (
        "timezone_abbreviations",
        "the snail reads PostgreSQL's Default set alone",
    ),
];

/// How the snail takes the setting of that name, lower case.
pub(crate) fn kind(release: u8, name: &str) -> Option<Kind> {
    const FIXED: [&str; 10] = [
        "timezone",
        "datestyle",
        "intervalstyle",
        "extra_float_digits",
        "bytea_output",
        "standard_conforming_strings",
        "lc_monetary",
        "lc_numeric",
        "lc_time",
        "default_text_search_config",
    ];
    if FIXED.contains(&name) {
        return Some(Kind::Fixed);
    }
    if name.contains('.') {
        return Some(Kind::Custom);
    }
    let g = find(release, name)?;
    Some(match name {
        "transaction_isolation"
        | "transaction_read_only"
        | "transaction_deferrable"
        | "default_transaction_isolation"
        | "default_transaction_read_only"
        | "default_transaction_deferrable" => Kind::Mode,
        "search_path" => Kind::Path,
        "session_replication_role" => Kind::Read,
        "client_encoding" => Kind::Own("UTF8", "the snail answers in UTF8"),
        "server_encoding" => Kind::Preset("UTF8"),
        "integer_datetimes" => Kind::Preset("on"),
        "max_identifier_length" => Kind::Preset("63"),
        _ => match OWN.iter().find(|(n, _)| *n == name) {
            Some((_, why)) => Kind::Own(g.boot, why),
            None if g.boot.is_empty() && name == "application_name" => Kind::Taken,
            None if TAKEN.contains(&name)
                || name.starts_with("enable_")
                || name.starts_with("jit")
                || name.starts_with("geqo")
                || name.starts_with("vacuum_")
                || name.starts_with("debug_print_")
                || name == "debug_pretty_print"
                || name == "debug_parallel_query"
                || name.ends_with("_cost")
                || name.ends_with("_collapse_limit")
                || matches!(
                    name,
                    "work_mem"
                        | "maintenance_work_mem"
                        | "hash_mem_multiplier"
                        | "temp_buffers"
                        | "logical_decoding_work_mem"
                        | "vacuum_buffer_usage_limit"
                        | "effective_cache_size"
                        | "effective_io_concurrency"
                        | "maintenance_io_concurrency"
                        | "io_combine_limit"
                        | "max_parallel_workers"
                        | "max_parallel_workers_per_gather"
                        | "max_parallel_maintenance_workers"
                        | "parallel_leader_participation"
                        | "min_parallel_index_scan_size"
                        | "min_parallel_table_scan_size"
                        | "default_statistics_target"
                        | "cursor_tuple_fraction"
                        | "plan_cache_mode"
                        | "constraint_exclusion"
                        | "recursive_worktable_factor"
                        | "gin_pending_list_limit"
                        | "wal_skip_threshold"
                        | "file_copy_method"
                        | "backend_flush_after"
                        | "min_eager_agg_group_size"
                ) =>
            {
                Kind::Taken
            }
            None => Kind::Server,
        },
    })
}

/// What a SET of a setting comes to.
#[derive(Debug)]
pub(crate) enum Taken {
    /// Nothing: a fixed setting set to the value the snail answers under.
    Nothing,
    /// The value SHOW then gives, or the default where none.
    Stored(Option<String>),
    /// The transaction's or the session's mode, as the statement that sets it.
    Mode(String),
}

/// A SET of `name` to `value`, or to its default where none, as its kind takes it: what it comes
/// to, or its refusal in PostgreSQL's words or the snail's.
pub(crate) fn taken(
    release: u8,
    monetary: crate::monetary::Monetary,
    text_search: crate::tsearch::TextSearch,
    name: &str,
    value: Option<&str>,
) -> Result<Taken> {
    let k = kind(release, name)
        .ok_or_else(|| Error::new(format!("unrecognized configuration parameter \"{name}\"")))?;
    let g = find(release, name);
    Ok(match k {
        Kind::Fixed => {
            let words = vec![value.unwrap_or("default").to_string()];
            super::statement::fixed_setting(name, &words, monetary, text_search)?;
            Taken::Nothing
        }
        Kind::Taken | Kind::Read => Taken::Stored(match value {
            Some(v) => Some(normalized(g.expect("a setting of the release"), v)?),
            None => None,
        }),
        Kind::Custom => Taken::Stored(value.map(str::to_string)),
        Kind::Own(own, why) => {
            let shown = match value {
                None => None,
                Some(v) if name == "client_encoding" => {
                    let clean: String = v
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect::<String>()
                        .to_ascii_lowercase();
                    Some(match clean.as_str() {
                        "utf8" | "unicode" => "UTF8".to_string(),
                        _ => v.to_string(),
                    })
                }
                Some(v) => Some(normalized(g.expect("a setting of the release"), v)?),
            };
            if let Some(s) = shown.as_deref().filter(|s| *s != own) {
                return Err(Error::new(match why.is_empty() {
                    true => format!("SET {name} to {s} is refused: the snail reads no {name}"),
                    false => format!(
                        "SET {name} to {s} is refused: the snail answers under {name} {}, as {why}",
                        if own.is_empty() { "''" } else { own }
                    ),
                }));
            }
            Taken::Stored(shown)
        }
        Kind::Mode => {
            let g = g.expect("a setting of the release");
            let value = match value {
                Some(v) => normalized(g, v)?,
                None => g.boot.to_string(),
            };
            let words = match (name, value.as_str()) {
                (n, v) if n.ends_with("isolation") => format!("ISOLATION LEVEL {v}"),
                (n, "on") if n.ends_with("read_only") => "READ ONLY".into(),
                (n, _) if n.ends_with("read_only") => "READ WRITE".into(),
                (_, "on") => "DEFERRABLE".into(),
                _ => "NOT DEFERRABLE".into(),
            };
            Taken::Mode(match name.starts_with("default_") {
                true => format!("SET SESSION CHARACTERISTICS AS TRANSACTION {words}"),
                false => format!("SET TRANSACTION {words}"),
            })
        }
        Kind::Path => return Err(no_path(&format!("SET {name}"))),
        Kind::Preset(_) => {
            return Err(Error::new(format!(
                "parameter \"{name}\" cannot be changed"
            )))
        }
        Kind::Server => return Err(server_own(&format!("SET {name}"), name)),
    })
}

fn no_path(what: &str) -> Error {
    Error::new(format!(
        "{what} is refused: a snail names each table by its schema, and has no search path"
    ))
}

fn server_own(what: &str, name: &str) -> Error {
    Error::new(format!(
        "{what} is refused: {name} is the server's own setting, and a snail has no server"
    ))
}

/// The session's settings as a statement reads them: those it set, and its modes as SHOW gives
/// them; and what set_config set since the statement began, each name, value and whether LOCAL.
pub(crate) struct View {
    pub(crate) release: u8,
    pub(crate) monetary: String,
    pub(crate) lc_monetary: crate::monetary::Monetary,
    pub(crate) text_search: crate::tsearch::TextSearch,
    pub(crate) values: std::collections::BTreeMap<String, String>,
    pub(crate) modes: std::collections::BTreeMap<&'static str, &'static str>,
    pub(crate) changed: Vec<(String, Taken, bool)>,
}

/// SHOW of a setting: its name as PostgreSQL's SHOW names its column, and its value.
pub(crate) fn shown(view: &View, name: &str) -> Result<(String, String)> {
    let release = view.release;
    let unknown = || Error::new(format!("unrecognized configuration parameter \"{name}\""));
    let k = kind(release, name).ok_or_else(unknown)?;
    let column = find(release, name).map_or(name.to_string(), |g| g.name.to_string());
    let value = match k {
        Kind::Fixed => match name {
            "lc_monetary" => view.monetary.clone(),
            "default_text_search_config" => view.text_search.name().to_string(),
            _ => super::statement::SETTINGS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
                .expect("a fixed setting"),
        },
        Kind::Custom => view.values.get(name).cloned().ok_or_else(unknown)?,
        Kind::Taken | Kind::Read | Kind::Own(..) => match view.values.get(name) {
            Some(v) => v.clone(),
            None => match k {
                Kind::Own(own, why) if !why.is_empty() => own.to_string(),
                Kind::Own(..) => {
                    return Err(Error::new(format!(
                        "SHOW {name} is refused: the snail reads no {name}"
                    )))
                }
                _ => default_shown(find(release, name).expect("a setting of the release")),
            },
        },
        Kind::Mode => view.modes.get(name).expect("a mode's setting").to_string(),
        Kind::Preset(v) => v.to_string(),
        Kind::Path => return Err(no_path(&format!("SHOW {name}"))),
        Kind::Server => return Err(server_own(&format!("SHOW {name}"), name)),
    };
    Ok((column, value))
}

thread_local! {
    static VIEW: std::cell::RefCell<Option<View>> = const { std::cell::RefCell::new(None) };
}

/// The session's settings handed to the statement's set_config and current_setting while it runs;
/// what set_config set comes back as it ends.
pub(crate) struct Viewing;

pub(crate) fn viewing(view: View) -> Viewing {
    VIEW.with(|v| *v.borrow_mut() = Some(view));
    Viewing
}

impl Viewing {
    /// What set_config set while the statement ran.
    pub(crate) fn changed(self) -> Vec<(String, Taken, bool)> {
        VIEW.with(|v| v.borrow_mut().take())
            .map(|v| v.changed)
            .unwrap_or_default()
    }
}

impl Drop for Viewing {
    fn drop(&mut self) {
        VIEW.with(|v| v.borrow_mut().take());
    }
}

/// current_setting(name, missing_ok), where a session's settings are handed to the statement: its
/// value, or none where missing_ok and no such setting is; where none are handed, `None`.
pub(crate) fn current_setting(name: &str, missing_ok: bool) -> Option<Result<Option<String>>> {
    VIEW.with(|v| {
        let v = v.borrow();
        let view = v.as_ref()?;
        let lower = name.to_lowercase();
        Some(match shown(view, &lower) {
            Ok((_, value)) => Ok(Some(value)),
            Err(_)
                if missing_ok && kind(view.release, &lower).is_none_or(|k| k == Kind::Custom) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        })
    })
}

/// set_config(name, value, is_local), where a session's settings are handed to the statement: the
/// value SHOW then gives; where none are handed, `None`.
pub(crate) fn set_config(name: &str, value: Option<&str>, local: bool) -> Option<Result<String>> {
    VIEW.with(|v| {
        let mut v = v.borrow_mut();
        let view = v.as_mut()?;
        let lower = name.to_lowercase();
        Some((|| {
            let t = taken(view.release, view.lc_monetary, view.text_search, &lower, value)?;
            match &t {
                Taken::Stored(Some(s)) => {
                    view.values.insert(lower.clone(), s.clone());
                }
                Taken::Stored(None) => {
                    view.values.remove(&lower);
                }
                _ => {}
            }
            let mode = matches!(t, Taken::Mode(_));
            view.changed.push((lower.clone(), t, local));
            match (mode, value, find(view.release, &lower)) {
                (true, Some(v), Some(g)) => normalized(g, v),
                (true, None, Some(g)) => Ok(g.boot.to_string()),
                _ => Ok(shown(view, &lower)?.1),
            }
        })())
    })
}

/// A setting's value as PostgreSQL's SET reads it and SHOW writes it back, or PostgreSQL's words
/// where it refuses it.
pub(crate) fn normalized(g: &Guc, value: &str) -> Result<String> {
    let invalid = || {
        Error::new(format!(
            "invalid value for parameter \"{}\": \"{value}\"",
            g.name
        ))
    };
    Ok(match g.vartype {
        "bool" => match crate::read::truth(value) {
            Some(true) => "on".into(),
            Some(false) => "off".into(),
            None => {
                return Err(Error::new(format!(
                    "parameter \"{}\" requires a Boolean value",
                    g.name
                )))
            }
        },
        "integer" => {
            let v = parse_number(value, g.unit, true).ok_or_else(invalid)?;
            let v = v.round();
            let (min, max) = (
                g.min.parse::<f64>().unwrap_or(f64::MIN),
                g.max.parse::<f64>().unwrap_or(f64::MAX),
            );
            if !(i32::MIN as f64..=i32::MAX as f64).contains(&v) {
                return Err(invalid());
            }
            if v < min || v > max {
                let (lo, hi) = (min as i64, max as i64);
                return Err(Error::new(format!(
                    "{}{} is outside the valid range for parameter \"{}\" ({}{} .. {}{})",
                    v as i64,
                    unit_suffix(g.unit),
                    g.name,
                    lo,
                    unit_suffix(g.unit),
                    hi,
                    unit_suffix(g.unit)
                )));
            }
            show_int(v as i64, g.unit)
        }
        "real" => {
            let v = parse_number(value, g.unit, false).ok_or_else(invalid)?;
            let (min, max) = (
                g.min.parse::<f64>().unwrap_or(f64::MIN),
                g.max.parse::<f64>().unwrap_or(f64::MAX),
            );
            if v < min || v > max {
                return Err(Error::new(format!(
                    "{}{} is outside the valid range for parameter \"{}\" ({}{} .. {}{})",
                    g_format(v),
                    unit_suffix(g.unit),
                    g.name,
                    g_format(min),
                    unit_suffix(g.unit),
                    g_format(max),
                    unit_suffix(g.unit)
                )));
            }
            show_real(v, g.unit)
        }
        "enum" => {
            let lower = value.to_ascii_lowercase();
            match g.enumvals.iter().find(|e| e.eq_ignore_ascii_case(&lower)) {
                Some(e) => e.to_string(),
                // Hidden spellings of on and off, which PostgreSQL's enums of them take.
                None if g.enumvals.contains(&"on") && g.enumvals.contains(&"off") => {
                    match crate::read::truth(value) {
                        Some(true) => "on".into(),
                        Some(false) => "off".into(),
                        None => return Err(invalid()),
                    }
                }
                None => return Err(invalid()),
            }
        }
        _ => value.to_string(),
    })
}

/// A setting's default, as SHOW writes it.
pub(crate) fn default_shown(g: &Guc) -> String {
    match g.vartype {
        "integer" => g
            .boot
            .parse()
            .map_or(g.boot.to_string(), |v| show_int(v, g.unit)),
        "real" => g
            .boot
            .parse()
            .map_or(g.boot.to_string(), |v| show_real(v, g.unit)),
        _ => g.boot.to_string(),
    }
}

/// The units a value may name, by its setting's base unit: each name and how many base units it is.
fn units(base: &str) -> &'static [(&'static str, f64)] {
    const KB: f64 = 1024.0;
    match base {
        "B" => &[
            ("TB", KB * KB * KB * KB),
            ("GB", KB * KB * KB),
            ("MB", KB * KB),
            ("kB", KB),
            ("B", 1.0),
        ],
        "kB" => &[
            ("TB", KB * KB * KB),
            ("GB", KB * KB),
            ("MB", KB),
            ("kB", 1.0),
            ("B", 1.0 / KB),
        ],
        "MB" => &[
            ("TB", KB * KB),
            ("GB", KB),
            ("MB", 1.0),
            ("kB", 1.0 / KB),
            ("B", 1.0 / (KB * KB)),
        ],
        "8kB" => &[
            ("TB", KB * KB * KB / 8.0),
            ("GB", KB * KB / 8.0),
            ("MB", KB / 8.0),
            ("kB", 1.0 / 8.0),
            ("B", 1.0 / 8192.0),
        ],
        "ms" => &[
            ("d", 86_400_000.0),
            ("h", 3_600_000.0),
            ("min", 60_000.0),
            ("s", 1000.0),
            ("ms", 1.0),
            ("us", 1.0 / 1000.0),
        ],
        "s" => &[
            ("d", 86_400.0),
            ("h", 3600.0),
            ("min", 60.0),
            ("s", 1.0),
            ("ms", 1.0 / 1000.0),
            ("us", 1.0 / 1_000_000.0),
        ],
        "min" => &[
            ("d", 1440.0),
            ("h", 60.0),
            ("min", 1.0),
            ("s", 1.0 / 60.0),
            ("ms", 1.0 / 60_000.0),
            ("us", 1.0 / 60_000_000.0),
        ],
        _ => &[],
    }
}

/// A value's suffix in a refusal of its range: its base unit, where the setting has one.
fn unit_suffix(base: &str) -> String {
    if base.is_empty() {
        String::new()
    } else {
        format!(" {base}")
    }
}

/// A number as `parse_int` (`integer`) or `parse_real` reads it: C's strtol of any base or strtod,
/// then a unit of its setting's, converted to the base unit and rounded to the unit after it.
fn parse_number(value: &str, base: &str, integer: bool) -> Option<f64> {
    let s = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (mut v, rest) = c_number(s, integer)?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let unit = rest.trim_end_matches(|c: char| c.is_ascii_whitespace());
    if !unit.is_empty() {
        let table = units(base);
        let i = table.iter().position(|(n, _)| *n == unit)?;
        v *= table[i].1;
        if let Some(next) = table.get(i + 1) {
            v = (v / next.1).round_ties_even() * next.1;
        }
    }
    v.is_finite().then_some(v)
}

/// The number C's strtol (base 0) or, past it, strtod reads at the start of `s`, and what is left.
fn c_number(s: &str, integer: bool) -> Option<(f64, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let neg = s.starts_with('-');
    if integer && b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        let start = i + 2;
        let end = start
            + b[start..]
                .iter()
                .take_while(|c| c.is_ascii_hexdigit())
                .count();
        if end > start {
            let v = i64::from_str_radix(&s[start..end], 16).ok()? as f64;
            return Some((if neg { -v } else { v }, &s[end..]));
        }
    }
    let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let int_end = i + digits;
    let real = matches!(b.get(int_end), Some(b'.' | b'e' | b'E'));
    if integer && !real {
        if digits == 0 {
            return None;
        }
        let text = &s[i..int_end];
        let v = if text.len() > 1 && text.starts_with('0') {
            i64::from_str_radix(text, 8).ok()? as f64
        } else {
            text.parse::<f64>().ok()?
        };
        return Some((if neg { -v } else { v }, &s[int_end..]));
    }
    // strtod: digits, a point, digits, an exponent.
    let mut j = int_end;
    let mut any = digits > 0;
    if b.get(j) == Some(&b'.') {
        j += 1;
        let f = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        any |= f > 0;
        j += f;
    }
    if !any {
        let lower = s[i..].to_ascii_lowercase();
        for word in ["infinity", "inf", "nan"] {
            if lower.starts_with(word) {
                let v = if word == "nan" {
                    f64::NAN
                } else {
                    f64::INFINITY
                };
                return (!v.is_nan()).then_some((if neg { -v } else { v }, &s[i + word.len()..]));
            }
        }
        return None;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        let e = b[k..].iter().take_while(|c| c.is_ascii_digit()).count();
        if e > 0 {
            j = k + e;
        }
    }
    Some((s[..j].parse::<f64>().ok()?, &s[j..]))
}

/// An integer setting as SHOW writes it: past 0, in the greatest unit that divides it.
fn show_int(v: i64, base: &str) -> String {
    if v <= 0 || base.is_empty() {
        return v.to_string();
    }
    for &(name, m) in units(base) {
        if m <= 1.0 || v % (m as i64) == 0 {
            return format!("{}{name}", (v as f64 / m).round_ties_even() as i64);
        }
    }
    v.to_string()
}

/// A real setting as SHOW writes it: past 0, in the greatest unit that gives a whole number within
/// 1e-8, or the smallest.
fn show_real(v: f64, base: &str) -> String {
    if v <= 0.0 || base.is_empty() {
        return g_format(v);
    }
    let mut shown = (v, "");
    for &(name, m) in units(base) {
        let x = v / m;
        shown = (x, name);
        if x > 0.0 && ((x.round_ties_even() / x) - 1.0).abs() <= 1e-8 {
            break;
        }
    }
    format!("{}{}", g_format(shown.0), shown.1)
}

/// A double as C's `%g` writes it: six significant digits, its trailing zeros dropped, an exponent
/// where it is under 1e-4 or 1e6 and over.
pub(crate) fn g_format(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    if !v.is_finite() {
        return if v.is_nan() {
            "nan".into()
        } else if v > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let e = format!("{v:.5e}");
    let (mantissa, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let trim = |s: String| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if !(-4..6).contains(&exp) {
        let m = trim(mantissa.to_string());
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (5 - exp).max(0) as usize;
        trim(format!("{v:.decimals$}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_read_and_shown_as_postgresqls_guc_reads_and_shows_them() {
        let g = |n: &str| find(18, n).unwrap();
        for (name, given, shown) in [
            ("work_mem", "65536", "64MB"),
            ("work_mem", "64MB", "64MB"),
            ("work_mem", "1.5MB", "1536kB"),
            ("work_mem", "0x1000", "4MB"),
            ("work_mem", "4097", "4097kB"),
            ("effective_cache_size", "1GB", "1GB"),
            ("effective_cache_size", "100", "800kB"),
            ("statement_timeout", "5000", "5s"),
            ("statement_timeout", "90s", "90s"),
            ("statement_timeout", "2min", "2min"),
            ("statement_timeout", "0", "0"),
            ("random_page_cost", "1.1", "1.1"),
            ("random_page_cost", "4", "4"),
            ("cpu_tuple_cost", "0.00001", "1e-05"),
            ("jit_above_cost", "1000000", "1e+06"),
            ("vacuum_cost_delay", "2", "2ms"),
            ("vacuum_cost_delay", "0.5", "500us"),
            ("enable_seqscan", "of", "off"),
            ("jit", "1", "on"),
            ("client_min_messages", "WARNING", "warning"),
            ("synchronous_commit", "true", "on"),
            ("application_name", "My App", "My App"),
        ] {
            assert_eq!(
                normalized(g(name), given).unwrap(),
                shown,
                "{name} = {given}"
            );
        }
        for (name, given, said) in [
            ("work_mem", "lots", "invalid value for parameter \"work_mem\": \"lots\""),
            (
                "work_mem",
                "10",
                "10 kB is outside the valid range for parameter \"work_mem\" (64 kB .. 2147483647 kB)",
            ),
            ("enable_seqscan", "maybe", "parameter \"enable_seqscan\" requires a Boolean value"),
            ("work_mem", "64 MiB", "invalid value for parameter \"work_mem\": \"64 MiB\""),
        ] {
            assert_eq!(normalized(g(name), given).unwrap_err().to_string(), said);
        }
        assert_eq!(default_shown(g("work_mem")), "4MB");
        assert_eq!(default_shown(g("effective_cache_size")), "4GB");
        assert_eq!(default_shown(find(19, "jit").unwrap()), "off");
    }

    #[test]
    fn each_setting_is_taken_or_refused_by_its_kind() {
        assert_eq!(kind(18, "timezone"), Some(Kind::Fixed));
        assert_eq!(kind(18, "work_mem"), Some(Kind::Taken));
        assert_eq!(kind(18, "enable_hashjoin"), Some(Kind::Taken));
        assert!(matches!(
            kind(18, "idle_session_timeout"),
            Some(Kind::Own("0", _))
        ));
        assert_eq!(kind(18, "statement_timeout"), Some(Kind::Taken));
        assert_eq!(kind(18, "session_replication_role"), Some(Kind::Read));
        assert_eq!(kind(18, "search_path"), Some(Kind::Path));
        assert_eq!(kind(18, "shared_buffers"), Some(Kind::Server));
        assert_eq!(kind(18, "myapp.user"), Some(Kind::Custom));
        assert_eq!(kind(18, "no_such_setting"), None);
        assert_eq!(kind(18, "enable_eager_aggregate"), None);
        assert_eq!(kind(19, "enable_eager_aggregate"), Some(Kind::Taken));
    }
}

thread_local! {
    static TIMEOUTS: std::cell::Cell<(Option<std::time::Instant>, Option<std::time::Duration>)> =
        const { std::cell::Cell::new((None, None)) };
}

/// A statement's statement_timeout and lock_timeout while it runs, as PostgreSQL's cancel it.
pub(crate) struct Timing((Option<std::time::Instant>, Option<std::time::Duration>));

/// The timeouts a statement runs under, each in milliseconds, none at 0, from now.
pub(crate) fn timing(statement_ms: u64, lock_ms: u64) -> Timing {
    let now = std::time::Instant::now();
    let given = (
        (statement_ms > 0).then(|| now + std::time::Duration::from_millis(statement_ms)),
        (lock_ms > 0).then(|| std::time::Duration::from_millis(lock_ms)),
    );
    Timing(TIMEOUTS.with(|t| t.replace(given)))
}

impl Drop for Timing {
    fn drop(&mut self) {
        TIMEOUTS.with(|t| t.set(self.0));
    }
}

/// A setting of milliseconds, as the session stands it, 0 where it is not set.
pub(crate) fn millis(values: &std::collections::BTreeMap<String, String>, name: &str) -> u64 {
    values
        .get(name)
        .and_then(|v| parse_number(v, "ms", true))
        .map_or(0, |v| v.max(0.0) as u64)
}

fn statement_timeout() -> Error {
    Error::new("canceling statement due to statement timeout")
}

/// Refused where the statement has run past its statement_timeout.
pub(crate) fn in_time() -> Result<()> {
    match TIMEOUTS.with(|t| t.get()).0 {
        Some(at) if std::time::Instant::now() >= at => Err(statement_timeout()),
        _ => Ok(()),
    }
}

/// A lock taken as `take` takes it, waiting where its argument is true: at once where no timeout
/// stands, else asked again until it is taken or a timeout passes, refused in PostgreSQL's words.
pub(crate) fn wait_for_lock(mut take: impl FnMut(bool) -> Result<bool>) -> Result<bool> {
    let (deadline, lock) = TIMEOUTS.with(|t| t.get());
    if deadline.is_none() && lock.is_none() {
        return take(true);
    }
    let began = std::time::Instant::now();
    loop {
        if take(false)? {
            return Ok(true);
        }
        let now = std::time::Instant::now();
        if lock.is_some_and(|l| now >= began + l) {
            return Err(Error::new("canceling statement due to lock timeout"));
        }
        if deadline.is_some_and(|d| now >= d) {
            return Err(statement_timeout());
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
