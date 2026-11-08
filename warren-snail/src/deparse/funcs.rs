//! The pg_get_function_* functions over a snail's catalog: a function's arguments, result and
//! definition as PostgreSQL 18's ruleutils.c prints them from its pg_proc row, the file's own or
//! a built-in's. A built-in's argument defaults and SQL body are parse trees the snail does not
//! carry; for the functions that have them, the text PostgreSQL printed is carried instead.

use super::cat::{self, Cat};
use super::print::{format_type_be, quote, Printer};
use crate::{Error, Result};
use std::collections::HashMap;

type Raw = HashMap<&'static str, Option<String>>;

/// A function's pg_proc row, what the printers read of it.
struct Proc {
    oid: u32,
    name: String,
    namespace: u32,
    lang: u32,
    kind: char,
    strict: bool,
    secdef: bool,
    leakproof: bool,
    retset: bool,
    volatile: char,
    parallel: char,
    cost: f64,
    rows: f64,
    support: u32,
    nargdefaults: usize,
    rettype: u32,
    argtypes: Vec<u32>,
    allargtypes: Option<Vec<u32>>,
    argmodes: Option<Vec<char>>,
    argnames: Option<Vec<String>>,
    src: String,
    bin: Option<String>,
    config: bool,
    built_in: bool,
}

/// The elements of an array as COPY's text writes it, `{a,"b c"}`.
fn elements(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('{').trim_end_matches('}');
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut escaped) = (false, false);
    for c in inner.chars() {
        match c {
            _ if escaped => {
                cur.push(c);
                escaped = false;
            }
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !inner.is_empty() {
        out.push(cur);
    }
    out
}

fn proc_of(cat: &Cat, oid: u32) -> Option<Proc> {
    let own = crate::pgcatalog::file_rows(cat.catalog, "pg_proc");
    let carried = crate::pgcatalog::built_in("pg_proc", cat.release);
    let is = |r: &&Raw| r.get("oid").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(oid);
    let (r, built_in) = match own.iter().find(is) {
        Some(r) => (r.clone(), false),
        None => (carried.iter().find(is)?.clone(), true),
    };
    let f = |k: &str| r.get(k).cloned().flatten();
    let num = |k: &str| f(k).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let truth = |k: &str| f(k).as_deref() == Some("t");
    let letter = |k: &str| f(k).and_then(|s| s.chars().next()).unwrap_or(' ');
    let oids = |s: String| -> Vec<u32> { s.split([' ', ',', '{', '}']).filter_map(|x| x.parse().ok()).collect() };
    Some(Proc {
        oid,
        name: f("proname").unwrap_or_default(),
        namespace: num("pronamespace"),
        lang: num("prolang"),
        kind: letter("prokind"),
        strict: truth("proisstrict"),
        secdef: truth("prosecdef"),
        leakproof: truth("proleakproof"),
        retset: truth("proretset"),
        volatile: letter("provolatile"),
        parallel: letter("proparallel"),
        cost: f("procost").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        rows: f("prorows").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        support: num("prosupport"),
        nargdefaults: num("pronargdefaults") as usize,
        rettype: num("prorettype"),
        argtypes: f("proargtypes").map(oids).unwrap_or_default(),
        allargtypes: f("proallargtypes").map(oids),
        argmodes: f("proargmodes").map(|s| elements(&s).iter().filter_map(|m| m.chars().next()).collect()),
        argnames: f("proargnames").map(|s| elements(&s)),
        src: f("prosrc").unwrap_or_default(),
        bin: f("probin"),
        config: f("proconfig").is_some(),
        built_in,
    })
}

/// PostgreSQL's text carried for a built-in, under a key `pgcatalog.nu` drew it by.
fn carried(cat: &Cat, key: &str) -> Option<String> {
    crate::read::sysviews::carried_definition(key, cat.release)
}

/// An ordered-set aggregate's direct arguments, which print before `ORDER BY`.
fn direct_arguments(cat: &Cat, p: &Proc) -> Option<usize> {
    if p.kind != 'a' {
        return None;
    }
    let rows = crate::pgcatalog::built_in("pg_aggregate", cat.release);
    let r = rows
        .iter()
        .find(|r| r.get("aggfnoid").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(p.oid))?;
    let kind = r.get("aggkind").cloned().flatten()?;
    if kind != "o" && kind != "h" {
        return None;
    }
    r.get("aggnumdirectargs").cloned().flatten()?.parse().ok()
}

/// print_function_arguments: the arguments, those of `TABLE` alone where `table`, each default
/// where `defaults`; and how many were printed.
fn arguments(cat: &Cat, p: &Proc, table: bool, defaults: bool) -> Result<(String, usize)> {
    if defaults && p.nargdefaults > 0 {
        let text = carried(cat, &format!("fnargs {}", p.oid)).ok_or_else(|| {
            Error::new(format!(
                "{}'s argument defaults are a parse tree the snail does not carry",
                p.name
            ))
        })?;
        let n = text.split(", ").filter(|a| !a.is_empty()).count();
        return Ok((text, n));
    }
    let types = p.allargtypes.clone().unwrap_or_else(|| p.argtypes.clone());
    let order_by_at = direct_arguments(cat, p);
    let mut out = String::new();
    let mut printed = 0;
    let mut i = 0;
    while i < types.len() {
        let mode = p.argmodes.as_ref().and_then(|m| m.get(i).copied()).unwrap_or('i');
        let word = match mode {
            'i' if p.kind == 'p' => "IN ",
            'i' | 't' => "",
            'b' => "INOUT ",
            'o' => "OUT ",
            'v' => "VARIADIC ",
            other => return Err(Error::new(format!("invalid parameter mode '{other}'"))),
        };
        if table != (mode == 't') {
            i += 1;
            continue;
        }
        if Some(printed) == order_by_at {
            if printed > 0 {
                out.push(' ');
            }
            out.push_str("ORDER BY ");
        } else if printed > 0 {
            out.push_str(", ");
        }
        out.push_str(word);
        if let Some(n) = p.argnames.as_ref().and_then(|n| n.get(i)).filter(|n| !n.is_empty()) {
            out.push_str(&format!("{} ", quote(n)));
        }
        out.push_str(&format_type_be(cat, types[i]));
        printed += 1;
        // A hypothetical-set aggregate's last argument prints again after ORDER BY.
        if Some(printed) == order_by_at && i == types.len() - 1 {
            continue;
        }
        i += 1;
    }
    Ok((out, printed))
}

/// print_function_rettype.
fn result_type(cat: &Cat, p: &Proc) -> Result<String> {
    if p.retset {
        let (table, n) = arguments(cat, p, true, false)?;
        if n > 0 {
            return Ok(format!("TABLE({table})"));
        }
        return Ok(format!("SETOF {}", format_type_be(cat, p.rettype)));
    }
    Ok(format_type_be(cat, p.rettype))
}

/// A float4 as `%g` prints it.
fn g(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e6 {
        format!("{}", x as i64)
    } else {
        let s = format!("{x:e}");
        match s.split_once('e') {
            Some((m, e)) => {
                let e: i32 = e.parse().unwrap_or(0);
                if (-4..6).contains(&e) {
                    format!("{x}")
                } else {
                    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
                }
            }
            None => s,
        }
    }
}

fn language(cat: &Cat, oid: u32) -> String {
    crate::pgcatalog::built_in("pg_language", cat.release)
        .iter()
        .find(|r| r.get("oid").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(oid))
        .and_then(|r| r.get("lanname").cloned().flatten())
        .unwrap_or_else(|| oid.to_string())
}

/// pg_get_functiondef.
fn functiondef(cat: &Cat, p: &Proc) -> Result<String> {
    if p.kind == 'a' {
        return Err(Error::new(format!("\"{}\" is an aggregate function", p.name)));
    }
    if p.built_in {
        if let Some(t) = carried(cat, &format!("fndef {}", p.oid)) {
            return Ok(t);
        }
    }
    if p.config {
        return Err(Error::new(format!("{}'s SET clauses are not printed as PostgreSQL prints them", p.name)));
    }
    let function = p.kind != 'p';
    let mut buf = format!(
        "CREATE OR REPLACE {} {}.{}(",
        if function { "FUNCTION" } else { "PROCEDURE" },
        quote(&cat.namespace(p.namespace)),
        quote(&p.name)
    );
    buf.push_str(&arguments(cat, p, false, true)?.0);
    buf.push_str(")\n");
    if function {
        buf.push_str(&format!(" RETURNS {}\n", result_type(cat, p)?));
    }
    buf.push_str(&format!(" LANGUAGE {}\n", quote(&language(cat, p.lang))));
    let before = buf.len();
    if p.kind == 'w' {
        buf.push_str(" WINDOW");
    }
    match p.volatile {
        'i' => buf.push_str(" IMMUTABLE"),
        's' => buf.push_str(" STABLE"),
        _ => {}
    }
    match p.parallel {
        's' => buf.push_str(" PARALLEL SAFE"),
        'r' => buf.push_str(" PARALLEL RESTRICTED"),
        _ => {}
    }
    if p.strict {
        buf.push_str(" STRICT");
    }
    if p.secdef {
        buf.push_str(" SECURITY DEFINER");
    }
    if p.leakproof {
        buf.push_str(" LEAKPROOF");
    }
    let cost = if p.lang == 12 || p.lang == 13 { 1.0 } else { 100.0 };
    if p.cost != cost {
        buf.push_str(&format!(" COST {}", g(p.cost)));
    }
    if p.rows > 0.0 && p.rows != 1000.0 {
        buf.push_str(&format!(" ROWS {}", g(p.rows)));
    }
    if p.support != 0 {
        let printer = Printer {
            cat,
            paren: false,
            indent: false,
            level: 0,
            buf: String::new(),
            relations: Vec::new(),
            q: Default::default(),
        };
        buf.push_str(&format!(" SUPPORT {}", printer.function_name(p.support, &[cat::INTERNAL])));
    }
    if buf.len() != before {
        buf.push('\n');
    }
    if p.lang == 14 && p.src.is_empty() {
        return Err(Error::new(format!("{}'s SQL body is a parse tree the snail does not carry", p.name)));
    }
    buf.push_str("AS ");
    if let Some(bin) = &p.bin {
        buf.push_str(&format!("{}, ", super::print::literal(bin)));
    }
    let mut quote = format!("${}", if function { "function" } else { "procedure" });
    while p.src.contains(&quote) {
        quote.push('x');
    }
    quote.push('$');
    buf.push_str(&format!("{quote}{}{quote}\n", p.src));
    Ok(buf)
}

/// The functions this module answers, by name.
pub(crate) fn is_function_printer(name: &str) -> bool {
    matches!(
        name,
        "pg_get_function_arguments"
            | "pg_get_function_identity_arguments"
            | "pg_get_function_result"
            | "pg_get_function_arg_default"
            | "pg_get_functiondef"
            | "pg_get_function_sqlbody"
    )
}

/// One of the pg_get_function_* functions of a function's OID: its text, None where PostgreSQL's
/// is NULL.
pub(crate) fn printed(cat: &Cat, name: &str, oid: u32, nth: Option<i64>) -> Result<Option<String>> {
    let Some(p) = proc_of(cat, oid) else {
        return Ok(None);
    };
    Ok(match name {
        "pg_get_function_arguments" => Some(arguments(cat, &p, false, true)?.0),
        "pg_get_function_identity_arguments" => Some(arguments(cat, &p, false, false)?.0),
        "pg_get_function_result" => (p.kind != 'p').then(|| result_type(cat, &p)).transpose()?,
        "pg_get_functiondef" => Some(functiondef(cat, &p)?),
        "pg_get_function_sqlbody" => {
            if p.built_in {
                carried(cat, &format!("fnbody {}", p.oid))
            } else {
                None
            }
        }
        "pg_get_function_arg_default" => {
            let n = nth.unwrap_or(0);
            let types = p.allargtypes.as_ref().unwrap_or(&p.argtypes);
            let input = |i: usize| matches!(p.argmodes.as_ref().and_then(|m| m.get(i)), None | Some('i' | 'b' | 'v'));
            if n < 1 || n as usize > types.len() || !input(n as usize - 1) || p.nargdefaults == 0 {
                None
            } else if p.built_in {
                carried(cat, &format!("fndefault {} {n}", p.oid))
            } else {
                None
            }
        }
        _ => None,
    })
}
