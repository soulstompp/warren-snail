//! The OIDs of a file's own objects. PostgreSQL hands a database's objects OIDs from 16384 on, as
//! it makes each; a snail does the same, as its DDL makes each object, in the order of its
//! statements, so that a reg* value of one names it (`'t'::regclass`). The DDL is kept in the file,
//! and read again statement by statement as the file opens, so each object comes back to the OID it
//! was given. An OID is never handed out again: an object dropped takes its number with it.

use std::collections::{BTreeMap, HashMap};

/// The first OID PostgreSQL hands an object a database makes (`FirstNormalObjectId`).
pub const FIRST: u32 = 16384;

/// The columns every table of PostgreSQL's has beside its own, which none of its own may be named.
pub const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid"];

/// An object of the file that PostgreSQL gives an OID, by what names it. A relation, a type, a
/// collation and a function are named schema-qualified, `public` where the DDL names no schema.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Object {
    /// A schema the DDL made, or one a file's kept statement names that no statement made;
    /// `public` only where the DDL made it again after dropping it.
    Namespace(String),
    /// A table, an index, a sequence, a view, or the relation behind a composite type.
    Class(String),
    /// A table's or a view's row type, an enum, a composite, a range, a multirange or a domain.
    Type(String),
    /// The array of a type, by the type's name.
    Array(String),
    /// A table's or a domain's constraint: the table or domain, and its name.
    Constraint(String, String),
    /// A column's DEFAULT: the table, and the column.
    Default(String, String),
    Collation(String),
    /// A function: its name, and its arguments' types as PostgreSQL names them, by commas.
    Proc(String, String),
    /// An enum's label: the enum, and the label.
    Label(String, String),
    /// A view's rule, `_RETURN`, by the view's name.
    Rule(String),
    /// A range's cast to its multirange, by the range's name.
    Cast(String),
    /// A trigger a constraint makes, by the constraint's table, its name and the trigger's place
    /// among its own (`trigger`'s): a foreign key's four, a deferrable key's or EXCLUDE's one.
    Trigger(String, String, u8),
}

/// The triggers a constraint makes, as PostgreSQL makes them: a foreign key that is enforced its
/// action on DELETE and on UPDATE on the table it references, then its check on INSERT and on
/// UPDATE on its own; a deferrable key or EXCLUDE its recheck on INSERT or UPDATE. The places are
/// `Trigger`'s third part.
pub mod trigger {
    pub const ACTION_DELETE: u8 = 0;
    pub const ACTION_UPDATE: u8 = 1;
    pub const CHECK_INSERT: u8 = 2;
    pub const CHECK_UPDATE: u8 = 3;
    /// A deferrable key's or EXCLUDE's recheck.
    pub const RECHECK: u8 = 0;
}

impl Object {
    /// The schema-qualified name it is known by: a constraint's, a DEFAULT's, a label's and a
    /// trigger's, its table's, domain's, enum's or constraint's table's.
    pub fn name(&self) -> &str {
        match self {
            Object::Namespace(n)
            | Object::Class(n)
            | Object::Type(n)
            | Object::Array(n)
            | Object::Collation(n)
            | Object::Proc(n, _)
            | Object::Constraint(n, _)
            | Object::Default(n, _)
            | Object::Label(n, _)
            | Object::Rule(n)
            | Object::Cast(n)
            | Object::Trigger(n, _, _) => n,
        }
    }

    /// The reg* kind whose values name it, where one does.
    pub fn reg_kind(&self) -> Option<crate::regs::RegKind> {
        use crate::regs::RegKind;
        Some(match self {
            Object::Namespace(_) => RegKind::Namespace,
            Object::Class(_) => RegKind::Class,
            Object::Type(_) | Object::Array(_) => RegKind::Type,
            Object::Collation(_) => RegKind::Collation,
            Object::Proc(..) => RegKind::Proc,
            _ => return None,
        })
    }
}

/// A name schema-qualified: `public` where it names no schema.
pub fn full(name: &str) -> String {
    if name.contains('.') {
        name.to_string()
    } else {
        format!("public.{name}")
    }
}

/// A qualified name's schema and its own name.
pub fn parts(name: &str) -> (&str, &str) {
    name.rsplit_once('.').unwrap_or(("public", name))
}

/// The OIDs a file's DDL has handed out, and the next it hands out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Oids {
    by_object: HashMap<Object, u32>,
    by_oid: BTreeMap<u32, Object>,
    next: u32,
    /// Whether the DDL dropped the release's schema `public`, whose OID is 2200: a `public` made
    /// again is the file's own, with an OID the DDL hands it.
    public_dropped: bool,
}

/// The OID of the schema `public` as initdb makes it.
pub const PUBLIC: u32 = 2200;

impl Default for Oids {
    fn default() -> Self {
        Oids {
            by_object: HashMap::new(),
            by_oid: BTreeMap::new(),
            next: FIRST,
            public_dropped: false,
        }
    }
}

impl Oids {
    /// The objects a statement leaves standing, in the order it makes them: each one new gets the
    /// next OID; one renamed keeps its own; one no longer standing gives its OID up for good.
    pub(crate) fn settle(&mut self, standing: &[Object], renamed: &[(Object, Object)]) {
        for (from, to) in renamed {
            if let Some(oid) = self.by_object.remove(from) {
                self.by_object.insert(to.clone(), oid);
                self.by_oid.insert(oid, to.clone());
            }
        }
        let now: std::collections::HashSet<&Object> = standing.iter().collect();
        let gone: Vec<Object> = self
            .by_object
            .keys()
            .filter(|o| !now.contains(o))
            .cloned()
            .collect();
        for o in gone {
            if let Some(oid) = self.by_object.remove(&o) {
                self.by_oid.remove(&oid);
            }
        }
        for o in standing {
            if !self.by_object.contains_key(o) {
                let oid = self.next;
                self.next += 1;
                self.by_object.insert(o.clone(), oid);
                self.by_oid.insert(oid, o.clone());
            }
        }
    }

    /// The objects a statement made or touched, in the order it makes them: each one new gets the
    /// next OID, and every other object keeps its own.
    pub(crate) fn give(&mut self, made: &[Object]) {
        for o in made {
            if !self.by_object.contains_key(o) {
                let oid = self.next;
                self.next += 1;
                self.by_object.insert(o.clone(), oid);
                self.by_oid.insert(oid, o.clone());
            }
        }
    }

    /// The OID an object was given.
    pub fn of(&self, o: &Object) -> Option<u32> {
        self.by_object.get(o).copied()
    }

    /// The object an OID names, where the file gave it.
    pub fn object(&self, oid: u32) -> Option<&Object> {
        self.by_oid.get(&oid)
    }

    /// Every object with its OID, by OID.
    pub fn each(&self) -> impl Iterator<Item = (u32, &Object)> {
        self.by_oid.iter().map(|(o, x)| (*o, x))
    }

    /// The next OID it would hand out.
    pub fn next(&self) -> u32 {
        self.next
    }

    /// The release's `public` given up: the DDL dropped it.
    pub(crate) fn drop_public(&mut self) {
        self.public_dropped = true;
    }

    /// Whether the DDL dropped the release's `public`, OID 2200.
    pub fn public_dropped(&self) -> bool {
        self.public_dropped
    }

    /// The OID of the schema `public` as the file has it: the release's until the DDL drops it,
    /// then the one CREATE SCHEMA public handed out, none while it stands nowhere.
    pub fn public(&self) -> Option<u32> {
        match self.public_dropped {
            false => Some(PUBLIC),
            true => self.of(&Object::Namespace("public".into())),
        }
    }
}

thread_local! {
    /// The objects of the file a round reads, whose OIDs its reg* values name; none where no round
    /// is under way, or where the round reads several snails, whose OIDs each count from 16384.
    static NAMING: std::cell::RefCell<Naming> = const { std::cell::RefCell::new(Naming::None) };
}

#[derive(Clone)]
enum Naming {
    None,
    One(std::sync::Arc<Oids>),
    Several,
}

/// The file's objects named while it is held: what a round installs, and what a caller holds while
/// it writes an answer's reg* values as text.
pub struct Named(Naming);

/// Holds a file's objects for its reg* values to name, until what it gives back is dropped. Inside
/// a round across several snails, one snail's round names none of them still.
pub fn named(oids: std::sync::Arc<Oids>) -> Named {
    Named(NAMING.with(|n| {
        let mut held = n.borrow_mut();
        match &*held {
            Naming::Several => Naming::Several,
            _ => std::mem::replace(&mut *held, Naming::One(oids)),
        }
    }))
}

/// Holds that the round reads several snails: an OID of their objects names none, and a name of
/// one is refused, since each file counts its OIDs from 16384.
pub fn named_several() -> Named {
    Named(NAMING.with(|n| n.replace(Naming::Several)))
}

impl Drop for Named {
    fn drop(&mut self) {
        let before = std::mem::replace(&mut self.0, Naming::None);
        NAMING.with(|n| *n.borrow_mut() = before);
    }
}

/// What the held file says of an OID: its object, none where it gave no such OID; refused where
/// the round reads several snails.
pub(crate) fn held_object(oid: u32) -> crate::Result<Option<Object>> {
    NAMING.with(|n| match &*n.borrow() {
        Naming::None => Ok(None),
        Naming::One(o) => Ok(o.object(oid).cloned()),
        Naming::Several if oid >= FIRST => Err(several()),
        Naming::Several => Ok(None),
    })
}

/// The OID the held file gave an object; refused where the round reads several snails.
pub(crate) fn held_oid(o: &Object) -> crate::Result<Option<u32>> {
    NAMING.with(|n| match &*n.borrow() {
        Naming::None => Ok(None),
        Naming::One(x) => Ok(x.of(o)),
        Naming::Several => Err(several()),
    })
}

/// The objects the held file gave OIDs, each with its OID; none where no file is held.
pub(crate) fn held_each<T>(f: impl FnOnce(&Oids) -> T) -> Option<T> {
    NAMING.with(|n| match &*n.borrow() {
        Naming::One(o) => Some(f(o)),
        _ => None,
    })
}

fn several() -> crate::Error {
    crate::Error::new(
        "an OID of a file's own object names nothing in a round across several snails: each file \
         hands out its OIDs from 16384",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_object_keeps_its_oid_and_a_dropped_ones_is_never_handed_out_again() {
        let mut o = Oids::default();
        let t = Object::Class("public.t".into());
        let u = Object::Class("public.u".into());
        o.settle(std::slice::from_ref(&t), &[]);
        o.settle(&[t.clone(), u.clone()], &[]);
        assert_eq!((o.of(&t), o.of(&u)), (Some(16384), Some(16385)));
        o.settle(std::slice::from_ref(&u), &[]);
        assert_eq!((o.of(&t), o.object(16384)), (None, None));
        o.settle(&[u.clone(), t.clone()], &[]);
        assert_eq!((o.of(&u), o.of(&t)), (Some(16385), Some(16386)));
        let v = Object::Class("public.v".into());
        o.settle(&[v.clone(), t.clone()], &[(u.clone(), v.clone())]);
        assert_eq!((o.of(&v), o.of(&u), o.next()), (Some(16385), None, 16387));
    }

    #[test]
    fn several_snails_name_no_object_of_theirs_by_oid() {
        let mut o = Oids::default();
        o.settle(&[Object::Class("public.t".into())], &[]);
        {
            let _n = named(std::sync::Arc::new(o));
            assert!(held_object(16384).unwrap().is_some());
            {
                let _s = named_several();
                assert!(held_object(16384).is_err());
                assert!(held_object(1259).unwrap().is_none());
            }
            assert!(held_object(16384).unwrap().is_some());
        }
        assert!(held_object(16384).unwrap().is_none());
    }
}
