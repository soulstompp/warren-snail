//! What a DROP takes with it, as PostgreSQL's `dependency.c` finds it over pg_depend's rows: each
//! object that depends on one dropped, visited newest first and put before it, each owned object
//! dropped by its owner, and the words PostgreSQL reports them in, a RESTRICT refused where any is
//! reached by a normal dependency.

use std::collections::HashMap;

use crate::catalog::Catalog;
use crate::deparse::depend::{self as d, Addr};
use crate::error::{Error, Result};
use crate::regs::RegKind;

const ORIGINAL: u32 = 0x0001;
const NORMAL: u32 = 0x0002;
const AUTO: u32 = 0x0004;
const INTERNAL: u32 = 0x0008;
const PARTITION: u32 = 0x0010;
const EXTENSION: u32 = 0x0020;
const REVERSE: u32 = 0x0040;
const SUBOBJECT: u32 = 0x0100;

/// The most objects a report names, the rest counted.
const REPORTED: usize = 100;

/// One row of pg_depend: an object, an object it depends on, and how.
#[derive(Clone, Copy, Debug)]
struct Dep {
    obj: Addr,
    on: Addr,
    kind: char,
}

/// An object a DROP takes: how it was reached, and the object it was reached from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Taken {
    pub(crate) addr: Addr,
    flags: u32,
    dependee: Option<Addr>,
}

impl Taken {
    /// A part of an object the DROP takes whole, which goes with it.
    pub(crate) fn part_of_whole(&self) -> bool {
        self.flags & SUBOBJECT != 0
    }
}

/// What a DROP takes, in the order it drops them, and what PostgreSQL says of it: a NOTICE for a
/// CASCADE that takes more than it names.
pub(crate) struct Dropped {
    pub(crate) taken: Vec<Taken>,
    pub(crate) notice: Option<String>,
}

fn same(a: Addr, b: Addr) -> bool {
    a.class == b.class && a.oid == b.oid
}

/// pg_depend's rows of the file, each read back from its text.
fn deps(catalog: &Catalog) -> Result<Vec<Dep>> {
    let rows = crate::pgcatalog::file_rows_read(catalog, "pg_depend")?;
    let num = |r: &HashMap<&'static str, Option<String>>, k: &str| -> i64 {
        r.get(k)
            .cloned()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Ok(rows
        .iter()
        .map(|r| Dep {
            obj: Addr {
                class: num(r, "classid") as u32,
                oid: num(r, "objid") as u32,
                sub: num(r, "objsubid") as i32,
            },
            on: Addr {
                class: num(r, "refclassid") as u32,
                oid: num(r, "refobjid") as u32,
                sub: num(r, "refobjsubid") as i32,
            },
            kind: r
                .get("deptype")
                .cloned()
                .flatten()
                .and_then(|v| v.chars().next())
                .unwrap_or('n'),
        })
        .collect())
}

/// The types the rows of pg_depend `after` holds and `before` does not depend on, each OID once:
/// those a dependency a declaration records names.
pub(crate) fn types_newly_depended_on(before: &Catalog, after: &Catalog) -> Result<Vec<u32>> {
    let rows = |c: &Catalog| -> Result<Vec<Dep>> {
        let _named = crate::oids::named(c.oids.clone());
        deps(c)
    };
    let key = |x: &Dep| {
        let (o, n) = (x.obj, x.on);
        (o.class, o.oid, o.sub, n.class, n.oid, n.sub, x.kind)
    };
    let was: std::collections::HashSet<_> = rows(before)?.iter().map(key).collect();
    let mut out: Vec<u32> = Vec::new();
    for x in rows(after)? {
        if x.on.class == d::TYPE && !was.contains(&key(&x)) && !out.contains(&x.on.oid) {
            out.push(x.on.oid);
        }
    }
    Ok(out)
}

/// As `object_address_comparator`: OID falling, then the catalog, then the column.
fn visiting_order(a: &Addr, b: &Addr) -> std::cmp::Ordering {
    b.oid
        .cmp(&a.oid)
        .then(a.class.cmp(&b.class))
        .then((a.sub as u32).cmp(&(b.sub as u32)))
}

struct Walk<'a> {
    /// What each object depends on, and what depends on each, by its catalog and OID.
    on: HashMap<(u32, u32), Vec<Dep>>,
    of: HashMap<(u32, u32), Vec<Dep>>,
    targets: Vec<Taken>,
    /// Every object the DROP names.
    named: &'a [Addr],
    words: &'a Words<'a>,
}

impl Walk<'_> {
    /// As `object_address_present_add_flags`: whether the object, or the whole it is part of, is
    /// taken already, its flags added where it is; a whole taken after its part marks the part.
    fn taken_add(&mut self, object: Addr, flags: u32) -> bool {
        let mut found = false;
        for t in self.targets.iter_mut().rev() {
            if !same(t.addr, object) {
                continue;
            }
            if t.addr.sub == object.sub {
                t.flags |= flags;
                found = true;
            } else if t.addr.sub == 0 {
                found = true;
            } else if object.sub == 0 && flags != 0 {
                t.flags |= flags | SUBOBJECT;
            }
        }
        found
    }

    /// As `findDependentObjects`.
    fn find(&mut self, object: Addr, flags: u32, stack: &mut Vec<(Addr, u32)>) -> Result<()> {
        if stack_add(object, flags, stack) || self.taken_add(object, flags) {
            return Ok(());
        }
        if object.pinned() {
            return Err(Error::new(format!(
                "cannot drop {} because it is required by the database system",
                self.words.of(object)
            )));
        }
        // An object owned by another, reached other than from its owner, is dropped by its owner.
        let mut owner: Option<Addr> = None;
        let own: Vec<Dep> = self
            .on
            .get(&(object.class, object.oid))
            .into_iter()
            .flatten()
            .filter(|x| object.sub == 0 || x.obj.sub == object.sub)
            .copied()
            .collect();
        for x in own {
            let other = x.on;
            if same(other, object) && object.sub == 0 {
                continue;
            }
            if !matches!(x.kind, 'i' | 'e') {
                continue;
            }
            if stack.is_empty() {
                let named = self
                    .named
                    .iter()
                    .any(|n| same(*n, other) && (n.sub == other.sub || n.sub == 0));
                if named {
                    return Ok(());
                }
                if owner.is_none() || x.kind == 'e' {
                    owner = Some(other);
                }
                continue;
            }
            if stack_add(other, 0, stack) {
                continue;
            }
            self.find(other, REVERSE, stack)?;
            if !self.taken_add(object, flags) {
                return Err(Error::new(format!(
                    "deletion of owning object {} failed to delete {}",
                    self.words.of(other),
                    self.words.of(object)
                )));
            }
            return Ok(());
        }
        if let Some(o) = owner {
            let (it, by) = (self.words.of(object), self.words.of(o));
            return Err(Error::new(format!(
                "cannot drop {it} because {by} requires it\nHINT:  You can drop {by} instead."
            )));
        }
        let mut dependents: Vec<(Addr, u32)> = self
            .of
            .get(&(object.class, object.oid))
            .into_iter()
            .flatten()
            .filter(|x| object.sub == 0 || x.on.sub == object.sub)
            .filter(|x| !(same(x.obj, object) && object.sub == 0))
            .map(|x| {
                let how = match x.kind {
                    'a' | 'x' => AUTO,
                    'i' => INTERNAL,
                    'P' | 'S' => PARTITION,
                    'e' => EXTENSION,
                    _ => NORMAL,
                };
                (x.obj, how)
            })
            .collect();
        dependents.sort_by(|a, b| visiting_order(&a.0, &b.0));
        stack.push((object, flags));
        for (other, how) in dependents {
            self.find(other, how, stack)?;
        }
        let (_, flags) = stack.pop().expect("the object's own level");
        let dependee = stack.last().map(|(a, _)| *a);
        self.targets.push(Taken {
            addr: object,
            flags,
            dependee,
        });
        Ok(())
    }
}

/// As `stack_address_present_add_flags`: whether the object, or the whole it is part of, is being
/// visited further out, its flags added where it is.
fn stack_add(object: Addr, flags: u32, stack: &mut [(Addr, u32)]) -> bool {
    let mut found = false;
    for (a, f) in stack.iter_mut() {
        if !same(*a, object) {
            continue;
        }
        if a.sub == object.sub {
            *f |= flags;
            found = true;
        } else if a.sub == 0 {
            found = true;
        } else if object.sub == 0 && flags != 0 {
            *f |= flags | SUBOBJECT;
        }
    }
    found
}

/// What a DROP of `named` takes, as `performMultipleDeletions` finds it, and what it says: refused
/// under RESTRICT where any object is reached by a normal dependency, in PostgreSQL's words with
/// its DETAIL after a colon and its HINT.
pub(crate) fn dropped(catalog: &Catalog, named: &[Addr], cascade: bool) -> Result<Dropped> {
    let _named = crate::oids::named(catalog.oids.clone());
    let words = Words::of_file(catalog);
    let mut walk = Walk {
        on: HashMap::new(),
        of: HashMap::new(),
        targets: Vec::new(),
        named,
        words: &words,
    };
    for x in deps(catalog)? {
        walk.on.entry((x.obj.class, x.obj.oid)).or_default().push(x);
        walk.of.entry((x.on.class, x.on.oid)).or_default().push(x);
    }
    for &n in named {
        walk.find(n, ORIGINAL, &mut Vec::new())?;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut unreported = 0;
    let mut refused = false;
    for t in walk.targets.iter().rev() {
        if t.flags & (ORIGINAL | SUBOBJECT) != 0 {
            continue;
        }
        if t.flags & (AUTO | INTERNAL | PARTITION | EXTENSION) != 0 {
            continue;
        }
        let it = words.of(t.addr);
        let line = match cascade {
            false => {
                refused = true;
                format!("{it} depends on {}", t.dependee.map_or_else(String::new, |o| words.of(o)))
            }
            true => format!("drop cascades to {it}"),
        };
        if lines.len() < REPORTED {
            lines.push(line);
        } else {
            unreported += 1;
        }
    }
    let mut detail = lines.join("\n");
    match unreported {
        0 => {}
        1 => detail.push_str("\nand 1 other object (see server log for list)"),
        n => detail.push_str(&format!("\nand {n} other objects (see server log for list)")),
    }
    if refused {
        let what = match named {
            [one] => format!(
                "cannot drop {} because other objects depend on it",
                words.of(*one)
            ),
            _ => "cannot drop desired object(s) because other objects depend on them".to_string(),
        };
        return Err(Error::new(format!(
            "{what}: {detail}\nHINT:  Use DROP ... CASCADE to drop the dependent objects too."
        )));
    }
    let notice = match lines.len() {
        0 => None,
        1 if unreported == 0 => Some(detail),
        n => Some(format!("drop cascades to {} other objects", n + unreported)),
    };
    Ok(Dropped {
        taken: walk.targets,
        notice,
    })
}

/// The words PostgreSQL's getObjectDescription names an object by, read from the file's own rows.
struct Words<'a> {
    release: u8,
    catalog: &'a Catalog,
    kinds: HashMap<u32, char>,
    columns: HashMap<(u32, i32), String>,
    constraints: HashMap<u32, (String, u32)>,
    defaults: HashMap<u32, (u32, i32)>,
    rules: HashMap<u32, (String, u32)>,
    casts: HashMap<u32, (u32, u32)>,
}

impl<'a> Words<'a> {
    fn of_file(catalog: &'a Catalog) -> Words<'a> {
        let rows = |t: &str| crate::pgcatalog::file_rows(catalog, t);
        let text = |r: &HashMap<&'static str, Option<String>>, k: &str| -> String {
            r.get(k).cloned().flatten().unwrap_or_default()
        };
        let num = |r: &HashMap<&'static str, Option<String>>, k: &str| -> i64 {
            text(r, k).parse().unwrap_or(0)
        };
        let mut w = Words {
            release: catalog.release,
            catalog,
            kinds: HashMap::new(),
            columns: HashMap::new(),
            constraints: HashMap::new(),
            defaults: HashMap::new(),
            rules: HashMap::new(),
            casts: HashMap::new(),
        };
        for r in rows("pg_class") {
            let kind = text(&r, "relkind").chars().next().unwrap_or('r');
            w.kinds.insert(num(&r, "oid") as u32, kind);
        }
        for r in rows("pg_attribute") {
            let at = (num(&r, "attrelid") as u32, num(&r, "attnum") as i32);
            w.columns.insert(at, text(&r, "attname"));
        }
        for r in rows("pg_constraint") {
            let c = (text(&r, "conname"), num(&r, "conrelid") as u32);
            w.constraints.insert(num(&r, "oid") as u32, c);
        }
        for r in rows("pg_attrdef") {
            let at = (num(&r, "adrelid") as u32, num(&r, "adnum") as i32);
            w.defaults.insert(num(&r, "oid") as u32, at);
        }
        for r in rows("pg_rewrite") {
            let rule = (text(&r, "rulename"), num(&r, "ev_class") as u32);
            w.rules.insert(num(&r, "oid") as u32, rule);
        }
        for r in rows("pg_cast") {
            let cast = (num(&r, "castsource") as u32, num(&r, "casttarget") as u32);
            w.casts.insert(num(&r, "oid") as u32, cast);
        }
        w
    }

    fn reg(&self, kind: RegKind, oid: u32) -> String {
        crate::regs::reg_text(kind, oid, self.release)
    }

    /// A relation, as getRelationDescription names it: its kind and its name, bare where the search
    /// path finds it.
    fn relation(&self, oid: u32) -> String {
        let kind = match self.kinds.get(&oid) {
            Some('r' | 'p') => "table",
            Some('i' | 'I') => "index",
            Some('S') => "sequence",
            Some('t') => "toast table",
            Some('v') => "view",
            Some('m') => "materialized view",
            Some('c') => "composite type",
            Some('f') => "foreign table",
            _ => "relation",
        };
        format!("{kind} {}", self.reg(RegKind::Class, oid))
    }

    fn column(&self, rel: u32, attno: i32) -> String {
        let name = self.columns.get(&(rel, attno)).cloned().unwrap_or_default();
        format!("column {name} of {}", self.relation(rel))
    }

    fn of(&self, a: Addr) -> String {
        match a.class {
            d::CLASS if a.sub != 0 => self.column(a.oid, a.sub),
            d::CLASS => self.relation(a.oid),
            d::TYPE => format!("type {}", self.reg(RegKind::Type, a.oid)),
            d::PROC => format!("function {}", self.reg(RegKind::Procedure, a.oid)),
            d::COLLATION => format!("collation {}", self.reg(RegKind::Collation, a.oid)),
            d::NAMESPACE => format!("schema {}", self.schema(a.oid)),
            d::CONSTRAINT => match self.constraints.get(&a.oid) {
                Some((n, rel)) if *rel != 0 => format!("constraint {n} on {}", self.relation(*rel)),
                Some((n, _)) => format!("constraint {n}"),
                None => format!("constraint {}", a.oid),
            },
            d::ATTRDEF => match self.defaults.get(&a.oid) {
                Some(&(rel, attno)) => format!("default value for {}", self.column(rel, attno)),
                None => format!("default value {}", a.oid),
            },
            d::REWRITE => match self.rules.get(&a.oid) {
                Some((n, rel)) => format!("rule {n} on {}", self.relation(*rel)),
                None => format!("rule {}", a.oid),
            },
            d::CAST => match self.casts.get(&a.oid) {
                Some(&(from, to)) => format!(
                    "cast from {} to {}",
                    self.reg(RegKind::Type, from),
                    self.reg(RegKind::Type, to)
                ),
                None => format!("cast {}", a.oid),
            },
            _ => format!("object {} of catalog {}", a.oid, a.class),
        }
    }

    /// A schema's name as get_namespace_name gives it, unquoted.
    fn schema(&self, oid: u32) -> String {
        match self.catalog.oids.object(oid) {
            Some(crate::oids::Object::Namespace(n)) => n.clone(),
            _ => crate::pgcatalog::built_in("pg_namespace", self.release)
                .iter()
                .find(|r| r.get("oid").cloned().flatten() == Some(oid.to_string()))
                .and_then(|r| r.get("nspname").cloned().flatten())
                .unwrap_or_else(|| oid.to_string()),
        }
    }
}
