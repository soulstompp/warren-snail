//! The array functions as `arrayfuncs.c`, `array_userfuncs.c` and `varlena.c` compute them: an
//! array's shape, finding, removing and replacing its elements, putting its items in order,
//! drawing them, an array as text and text as an array, and the arrays `array_agg` makes of
//! arrays; `parse_ident` as `misc.c` reads a name; an aclitem array as `acl.c` makes and unfolds
//! one, and an options array as `foreign.c` unfolds one.
//!
//! An array is an `Item::Array`, its elements in storage order. An item of an array is one value of
//! its first subscript: an element where it has one dimension, a sub-array of the rest otherwise.
//! Elements are equal and ordered by their keys (`composed::key`), as the element type's default
//! B-tree compares them. A NULL array handed to a function answers NULL.

use crate::catalog::ColumnType;
use crate::composed::{self, Composed, Item};
use crate::read::statement::Datum;
use crate::{Error, Result};
use std::cmp::Ordering;

/// The most dimensions an array has: MAXDIM.
const MAXDIM: usize = 6;
/// The most elements an array holds: MaxArraySize.
const MAX_ARRAY_SIZE: i64 = 134_217_727;
/// The most bytes PostgreSQL allocates at once: MaxAllocSize.
const MAX_ALLOC_SIZE: i64 = 0x3fff_ffff;

fn empty() -> Item {
    Item::Array {
        elements: Vec::new(),
        dims: Vec::new(),
        lower: Vec::new(),
    }
}

/// An array's parts: its elements, each dimension's length and lower bound. None for a NULL.
fn parts(a: &Item) -> Option<(&[Item], &[i32], &[i32])> {
    match a {
        Item::Array {
            elements,
            dims,
            lower,
        } => Some((elements, dims, lower)),
        _ => None,
    }
}

fn int4(v: i32) -> Item {
    Item::Scalar(Datum::Int(v.into()))
}

fn text(s: &str) -> Item {
    Item::Scalar(Datum::Text(s.to_string()))
}

/// An int4 element's value, none for a NULL.
fn int_of(e: &Item) -> Option<i32> {
    match e {
        Item::Scalar(Datum::Int(v)) => Some(*v as i32),
        _ => None,
    }
}

/// A one-dimensional array from 1 of the items given, none for no items.
fn one_dimensional(elements: Vec<Item>) -> Item {
    if elements.is_empty() {
        return empty();
    }
    Item::Array {
        dims: vec![elements.len() as i32],
        lower: vec![1],
        elements,
    }
}

fn array_size_exceeded() -> Error {
    Error::new(format!(
        "array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"
    ))
}

fn alloc_size_exceeded() -> Error {
    Error::new(format!(
        "array size exceeds the maximum allowed ({MAX_ALLOC_SIZE})"
    ))
}

/// The elements an array of these dimensions holds (`ArrayGetNItems`), refused where they pass
/// MaxArraySize or a dimension is negative.
fn n_items(dims: &[i32]) -> Result<i64> {
    let mut n: i64 = if dims.is_empty() { 0 } else { 1 };
    for &d in dims {
        if d < 0 {
            return Err(array_size_exceeded());
        }
        n *= i64::from(d);
        if n > MAX_ARRAY_SIZE {
            return Err(array_size_exceeded());
        }
    }
    Ok(n)
}

/// Refused where a dimension's last subscript passes int4 (`ArrayCheckBounds`).
fn check_bounds(dims: &[i32], lower: &[i32]) -> Result<()> {
    for (&d, &l) in dims.iter().zip(lower) {
        if d.checked_add(l).is_none() {
            return Err(Error::new(format!("array lower bound is too large: {l}")));
        }
    }
    Ok(())
}

/// Refused where the element type has no equality, as the type cache finds none.
fn equality(elem: ColumnType) -> Result<()> {
    if elem.grouped() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "could not identify an equality operator for type {}",
            elem.pg_name()
        )))
    }
}

/// Refused where the element type has no B-tree comparison.
fn comparison(elem: ColumnType) -> Result<()> {
    if elem.ordered() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "could not identify a comparison function for type {}",
            elem.pg_name()
        )))
    }
}

/// An element's key, none for a NULL.
fn key_of(elem: ColumnType, e: &Item) -> Option<Vec<u8>> {
    (!e.is_null()).then(|| composed::key(elem, e))
}

/// An element as its type's output function writes it.
fn element_text(elem: ColumnType, e: &Item) -> String {
    match e {
        Item::Scalar(d) => composed::scalar_out(elem, d),
        other => composed::text_out(elem, other),
    }
}

/// The number of dimensions (`array_ndims`); NULL for an array of none.
pub fn ndims(a: &Item) -> Option<i32> {
    let (_, dims, _) = parts(a)?;
    (!dims.is_empty()).then_some(dims.len() as i32)
}

/// Each dimension's bounds as `[lower:upper]` (`array_dims`); NULL for an array of none.
pub fn dims_text(a: &Item) -> Option<String> {
    let (_, dims, lower) = parts(a)?;
    if dims.is_empty() {
        return None;
    }
    Some(
        dims.iter()
            .zip(lower)
            .map(|(&d, &l)| format!("[{}:{}]", l, d.wrapping_add(l).wrapping_sub(1)))
            .collect(),
    )
}

/// The dimension `dim` counted from 1, none past the array's.
fn dimension(a: &Item, dim: i32) -> Option<(i32, i32)> {
    let (_, dims, lower) = parts(a)?;
    let i = usize::try_from(dim).ok()?.checked_sub(1)?;
    Some((*dims.get(i)?, lower[i]))
}

/// The length of dimension `dim` (`array_length`); NULL for a dimension the array has not.
pub fn length(a: &Item, dim: i32) -> Option<i32> {
    dimension(a, dim).map(|(d, _)| d)
}

/// The lower bound of dimension `dim` (`array_lower`).
pub fn lower(a: &Item, dim: i32) -> Option<i32> {
    dimension(a, dim).map(|(_, l)| l)
}

/// The upper bound of dimension `dim` (`array_upper`).
pub fn upper(a: &Item, dim: i32) -> Option<i32> {
    dimension(a, dim).map(|(d, l)| d.wrapping_add(l).wrapping_sub(1))
}

/// The number of elements in every dimension (`cardinality`); 0 for an array of none.
pub fn cardinality(a: &Item) -> Option<i32> {
    let (elements, _, _) = parts(a)?;
    Some(elements.len() as i32)
}

/// The bytes PostgreSQL's array takes for one element: its type's length, or its varlena's, and
/// the padding its alignment asks.
fn stored_bytes(elem: ColumnType, v: &Item) -> i64 {
    use crate::geo::GeoKind;
    let (len, align): (i64, i64) = match elem {
        ColumnType::Boolean | ColumnType::Char => (1, 1),
        ColumnType::Int2 => (2, 2),
        ColumnType::Tid => (6, 2),
        ColumnType::Int4
        | ColumnType::Date
        | ColumnType::Oid
        | ColumnType::Xid
        | ColumnType::Cid
        | ColumnType::Reg(..)
        | ColumnType::Float4 => (4, 4),
        ColumnType::Macaddr => (6, 4),
        ColumnType::Macaddr8 => (8, 4),
        ColumnType::Int8
        | ColumnType::Timestamptz
        | ColumnType::TimestamptzRounded(_)
        | ColumnType::Timestamp(_)
        | ColumnType::Time(_)
        | ColumnType::Money(_)
        | ColumnType::Xid8
        | ColumnType::PgLsn
        | ColumnType::Oid8
        | ColumnType::Float8 => (8, 8),
        ColumnType::Timetz(_) => (12, 8),
        ColumnType::Interval { .. } => (16, 8),
        ColumnType::Uuid => (16, 1),
        ColumnType::Name => (64, 1),
        ColumnType::Geometric(GeoKind::Point) => (16, 8),
        ColumnType::Geometric(GeoKind::Line | GeoKind::Circle) => (24, 8),
        ColumnType::Geometric(GeoKind::Lseg | GeoKind::Box) => (32, 8),
        _ => {
            let payload = match v {
                Item::Scalar(Datum::Text(s)) => s.len(),
                Item::Scalar(Datum::Bytes(b)) => b.len(),
                other => element_text(elem, other).len(),
            };
            (4 + payload as i64, 4)
        }
    };
    (len + align - 1) / align * align
}

/// An array of `dims`, each element `value`, its lower bounds `lower`'s or 1 (`array_fill`).
/// `lower` is none for the form without them; `Item::Null` for a NULL argument.
pub fn fill(elem: ColumnType, value: &Item, dims: &Item, lower: Option<&Item>) -> Result<Item> {
    let null_bounds = || Error::new("dimension array or low bound array cannot be null");
    let Some((dv, dd, _)) = parts(dims) else {
        return Err(null_bounds());
    };
    let lbs = match lower {
        Some(l) => Some(parts(l).ok_or_else(null_bounds)?),
        None => None,
    };
    let wrong = || Error::new("wrong number of array subscripts");
    let values = |v: &[Item]| -> Result<Vec<i32>> {
        v.iter()
            .map(int_of)
            .collect::<Option<Vec<i32>>>()
            .ok_or_else(|| Error::new("dimension values cannot be null"))
    };
    if dd.len() > 1 {
        return Err(wrong());
    }
    let dimv = values(dv)?;
    if dimv.len() > MAXDIM {
        return Err(Error::new(format!(
            "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
            dimv.len()
        )));
    }
    let lbsv = match lbs {
        Some((lv, ld, _)) => {
            if ld.len() > 1 {
                return Err(wrong());
            }
            let lbsv = values(lv)?;
            if lbsv.len() != dimv.len() {
                return Err(wrong());
            }
            lbsv
        }
        None => vec![1; dimv.len()],
    };
    let n = n_items(&dimv)?;
    check_bounds(&dimv, &lbsv)?;
    if n <= 0 {
        return Ok(empty());
    }
    if !value.is_null() && stored_bytes(elem, value) * n > MAX_ALLOC_SIZE {
        return Err(alloc_size_exceeded());
    }
    Ok(Item::Array {
        elements: vec![value.clone(); n as usize],
        dims: dimv,
        lower: lbsv,
    })
}

/// Where `x` is first found, from subscript `start` where it is given (`array_position`): an
/// element equal to it, a NULL where `x` is NULL; NULL where none is. `start` is none for the form
/// without it, `Some(None)` for a NULL. Refused for an array of more than one dimension.
pub fn position(
    elem: ColumnType,
    a: &Item,
    x: &Item,
    start: Option<Option<i32>>,
) -> Result<Option<i32>> {
    let Some((elements, dims, lower)) = parts(a) else {
        return Ok(None);
    };
    if dims.len() > 1 {
        return Err(searched_dimensions());
    }
    if dims.is_empty() {
        return Ok(None);
    }
    if x.is_null() && !elements.iter().any(Item::is_null) {
        return Ok(None);
    }
    let from = match start {
        Some(None) => return Err(Error::new("initial position must not be null")),
        Some(Some(s)) => i64::from(s),
        None => i64::from(lower[0]),
    };
    equality(elem)?;
    let wanted = key_of(elem, x);
    let mut at = i64::from(lower[0]) - 1;
    for e in elements {
        at += 1;
        if at < from {
            continue;
        }
        if key_of(elem, e) == wanted {
            return Ok(Some(at as i32));
        }
    }
    Ok(None)
}

fn searched_dimensions() -> Error {
    Error::new("searching for elements in multidimensional arrays is not supported")
}

/// Every subscript `x` is found at, as `position` finds it, an int4 array (`array_positions`);
/// empty where it is found nowhere.
pub fn positions(elem: ColumnType, a: &Item, x: &Item) -> Result<Item> {
    let Some((elements, dims, lower)) = parts(a) else {
        return Ok(Item::Null);
    };
    if dims.len() > 1 {
        return Err(searched_dimensions());
    }
    if dims.is_empty() || (x.is_null() && !elements.iter().any(Item::is_null)) {
        return Ok(empty());
    }
    equality(elem)?;
    let wanted = key_of(elem, x);
    let found = elements
        .iter()
        .enumerate()
        .filter(|(_, e)| key_of(elem, e) == wanted)
        .map(|(i, _)| int4((i64::from(lower[0]) + i as i64) as i32))
        .collect();
    Ok(one_dimensional(found))
}

/// Each element equal to `x`, or each NULL where `x` is NULL, taken out (`array_remove`); the array
/// unchanged where none is. Refused for an array of more than one dimension.
pub fn remove(elem: ColumnType, a: &Item, x: &Item) -> Result<Item> {
    replaced(elem, a, x, None)
}

/// Each element equal to `from`, or each NULL where `from` is NULL, made `to` (`array_replace`).
pub fn replace(elem: ColumnType, a: &Item, from: &Item, to: &Item) -> Result<Item> {
    replaced(elem, a, from, Some(to))
}

fn replaced(elem: ColumnType, a: &Item, x: &Item, to: Option<&Item>) -> Result<Item> {
    let Some((elements, dims, lower)) = parts(a) else {
        return Ok(Item::Null);
    };
    if elements.is_empty() {
        return Ok(a.clone());
    }
    if to.is_none() && dims.len() > 1 {
        return Err(Error::new(
            "removing elements from multidimensional arrays is not supported",
        ));
    }
    equality(elem)?;
    let wanted = key_of(elem, x);
    let mut out = Vec::with_capacity(elements.len());
    let mut changed = false;
    for e in elements {
        if key_of(elem, e) != wanted {
            out.push(e.clone());
            continue;
        }
        match to {
            None => changed = true,
            Some(t) => {
                changed |= !(e.is_null() && t.is_null());
                out.push(t.clone());
            }
        }
    }
    if !changed {
        return Ok(a.clone());
    }
    if out.is_empty() {
        return Ok(empty());
    }
    let mut dims = dims.to_vec();
    if to.is_none() {
        dims[0] = out.len() as i32;
    }
    Ok(Item::Array {
        elements: out,
        dims,
        lower: lower.to_vec(),
    })
}

/// The elements of each item of an array: `elements` cut into `dims[0]` runs.
fn items<'a>(elements: &'a [Item], dims: &[i32]) -> Vec<&'a [Item]> {
    let n = dims[0].max(1) as usize;
    let per = elements.len() / n;
    if per == 0 {
        return vec![&elements[..0]; dims[0] as usize];
    }
    elements.chunks(per).collect()
}

/// The array's items in reverse order, its shape kept (`array_reverse`).
pub fn reverse(a: &Item) -> Item {
    let Some((elements, dims, lower)) = parts(a) else {
        return Item::Null;
    };
    if dims.is_empty() || dims[0] < 2 {
        return a.clone();
    }
    Item::Array {
        elements: items(elements, dims)
            .into_iter()
            .rev()
            .flatten()
            .cloned()
            .collect(),
        dims: dims.to_vec(),
        lower: lower.to_vec(),
    }
}

/// Values put in order as PostgreSQL's quicksort puts them (`sort_template.h`), by `cmp`: so that
/// values `cmp` finds equal fall where PostgreSQL's sort leaves them.
fn pg_qsort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    let mut a = 0usize;
    let mut n = v.len();
    loop {
        if n < 7 {
            if n < 2 {
                return;
            }
            for pm in a + 1..a + n {
                let mut pl = pm;
                while pl > a && cmp(&v[pl - 1], &v[pl]) == Ordering::Greater {
                    v.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return;
        }
        if (a + 1..a + n).all(|pm| cmp(&v[pm - 1], &v[pm]) != Ordering::Greater) {
            return;
        }
        let mut pm = a + n / 2;
        if n > 7 {
            let mut pl = a;
            let mut pn = a + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(v, pl, pl + d, pl + 2 * d, cmp);
                pm = med3(v, pm - d, pm, pm + d, cmp);
                pn = med3(v, pn - 2 * d, pn - d, pn, cmp);
            }
            pm = med3(v, pl, pm, pn, cmp);
        }
        v.swap(a, pm);
        let (mut pa, mut pb) = (a + 1, a + 1);
        let (mut pc, mut pd) = (a + n - 1, a + n - 1);
        loop {
            while pb <= pc {
                let r = cmp(&v[pb], &v[a]);
                if r == Ordering::Greater {
                    break;
                }
                if r == Ordering::Equal {
                    v.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let r = cmp(&v[pc], &v[a]);
                if r == Ordering::Less {
                    break;
                }
                if r == Ordering::Equal {
                    v.swap(pc, pd);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            v.swap(pb, pc);
            pb += 1;
            pc -= 1;
        }
        let pn = a + n;
        let d1 = (pa - a).min(pb - pa);
        for i in 0..d1 {
            v.swap(a + i, pb - d1 + i);
        }
        let d1 = (pd - pc).min(pn - pd - 1);
        for i in 0..d1 {
            v.swap(pb + i, pn - d1 + i);
        }
        let d1 = pb - pa;
        let d2 = pd - pc;
        if d1 <= d2 {
            pg_qsort(&mut v[a..a + d1], cmp);
            a = pn - d2;
            n = d2;
        } else {
            pg_qsort(&mut v[pn - d2..pn], cmp);
            n = d1;
        }
    }
}

fn med3<T>(
    v: &[T],
    a: usize,
    b: usize,
    c: usize,
    cmp: &mut dyn FnMut(&T, &T) -> Ordering,
) -> usize {
    if cmp(&v[a], &v[b]) == Ordering::Less {
        if cmp(&v[b], &v[c]) == Ordering::Less {
            b
        } else if cmp(&v[a], &v[c]) == Ordering::Less {
            c
        } else {
            a
        }
    } else if cmp(&v[b], &v[c]) == Ordering::Greater {
        b
    } else if cmp(&v[a], &v[c]) == Ordering::Less {
        a
    } else {
        c
    }
}

/// The array's items in order, its shape kept (`array_sort`): its elements where it has one
/// dimension, each sub-array as arrays compare otherwise; descending where asked, the NULLs first
/// where asked. The one-argument form is ascending with the NULLs last, the two-argument form puts
/// the NULLs first where it is descending.
pub fn sort(elem: ColumnType, a: &Item, descending: bool, nulls_first: bool) -> Result<Item> {
    let Some((elements, dims, lower)) = parts(a) else {
        return Ok(Item::Null);
    };
    if dims.is_empty() || dims[0] < 2 {
        return Ok(a.clone());
    }
    comparison(elem)?;
    let runs = items(elements, dims);
    let sub_type = composed::type_of(Composed::Array {
        element: elem,
        domain: None,
    });
    let mut keyed: Vec<(Option<Vec<u8>>, &[Item])> = runs
        .into_iter()
        .map(|run| {
            let k = if dims.len() == 1 {
                key_of(elem, &run[0])
            } else {
                Some(composed::key(
                    sub_type,
                    &Item::Array {
                        elements: run.to_vec(),
                        dims: dims[1..].to_vec(),
                        lower: lower[1..].to_vec(),
                    },
                ))
            };
            (k, run)
        })
        .collect();
    pg_qsort(&mut keyed, &mut |x, y| match (&x.0, &y.0) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(p), Some(q)) => {
            if descending {
                q.cmp(p)
            } else {
                p.cmp(q)
            }
        }
    });
    Ok(Item::Array {
        elements: keyed
            .into_iter()
            .flat_map(|(_, run)| run)
            .cloned()
            .collect(),
        dims: dims.to_vec(),
        lower: lower.to_vec(),
    })
}

/// The array without its last `n` items, each lower bound 1 (`trim_array`). Refused where `n` is
/// negative or past the first dimension's length. The last subscript kept is an int4 as
/// `trim_array` computes it, wrapping below its least: so that trimming every item of an array
/// whose lower bound is the least int4 keeps them all, as PostgreSQL does.
pub fn trim(a: &Item, n: i32) -> Result<Item> {
    let Some((_, dims, lower)) = parts(a) else {
        return Ok(Item::Null);
    };
    let len = dims.first().copied().unwrap_or(0);
    if n < 0 || n > len {
        return Err(Error::new(format!(
            "number of elements to trim must be between 0 and {len}"
        )));
    }
    let at = match lower.first() {
        Some(&l) => vec![(
            None,
            Some(i64::from(
                l.wrapping_add(len).wrapping_sub(n).wrapping_sub(1),
            )),
        )],
        None => vec![(None, None)],
    };
    Ok(composed::slice(a, &at))
}

/// The first `n` items of the array after shuffling it as `array_shuffle_n` does: the item at
/// each place from the first swapped with one `draw` picks at or after it. Each dimension and lower
/// bound kept but the first's, `n` long and from 1 unless `keep_lower`.
fn shuffle_n(
    elements: &[Item],
    dims: &[i32],
    lower: &[i32],
    n: i32,
    keep_lower: bool,
    draw: &mut dyn FnMut(u64) -> u64,
) -> Item {
    if dims.is_empty() || dims[0] < 1 || n < 1 {
        return empty();
    }
    let mut runs = items(elements, dims);
    let count = runs.len() as u64;
    for i in 0..n as usize {
        let j = i + draw(count - i as u64) as usize;
        runs.swap(i, j);
    }
    let mut d = dims.to_vec();
    let mut l = lower.to_vec();
    d[0] = n;
    if !keep_lower {
        l[0] = 1;
    }
    Item::Array {
        elements: runs[..n as usize]
            .iter()
            .copied()
            .flatten()
            .cloned()
            .collect(),
        dims: d,
        lower: l,
    }
}

/// The array's items in an order `draw` picks, its shape kept (`array_shuffle`). `draw(k)` is a
/// uniform integer below `k`.
pub fn shuffle(a: &Item, draw: &mut dyn FnMut(u64) -> u64) -> Item {
    let Some((elements, dims, lower)) = parts(a) else {
        return Item::Null;
    };
    if dims.is_empty() || dims[0] < 2 {
        return a.clone();
    }
    shuffle_n(elements, dims, lower, dims[0], true, draw)
}

/// `n` of the array's items `draw` picks, the first dimension from 1 (`array_sample`). Refused
/// where `n` is negative or past the first dimension's length.
pub fn sample(a: &Item, n: i32, draw: &mut dyn FnMut(u64) -> u64) -> Result<Item> {
    let Some((elements, dims, lower)) = parts(a) else {
        return Ok(Item::Null);
    };
    let count = dims.first().copied().unwrap_or(0);
    if n < 0 || n > count {
        return Err(Error::new(format!(
            "sample size must be between 0 and {count}"
        )));
    }
    Ok(shuffle_n(elements, dims, lower, n, false, draw))
}

/// The elements as their type's output writes them, `delimiter` between two, a NULL written as
/// `null_string` or passed over where there is none (`array_to_string`).
pub fn to_string(
    elem: ColumnType,
    a: &Item,
    delimiter: &str,
    null_string: Option<&str>,
) -> Option<String> {
    let (elements, _, _) = parts(a)?;
    let mut out = String::new();
    let mut printed = false;
    for e in elements {
        let t = if e.is_null() {
            match null_string {
                Some(s) => s.to_string(),
                None => continue,
            }
        } else {
            element_text(elem, e)
        };
        if printed {
            out.push_str(delimiter);
        }
        out.push_str(&t);
        printed = true;
    }
    Some(out)
}

/// Text cut at each `delimiter`, left to right, as a text array (`string_to_array`): each
/// character its own element where the delimiter is NULL, the whole text one element where it is
/// empty, and no element for empty text; a piece equal to `null_string` a NULL.
pub fn string_to_array(s: &str, delimiter: Option<&str>, null_string: Option<&str>) -> Item {
    let piece = |p: &str| {
        if null_string == Some(p) {
            Item::Null
        } else {
            text(p)
        }
    };
    let pieces: Vec<Item> = match delimiter {
        _ if s.is_empty() => Vec::new(),
        Some("") => vec![piece(s)],
        Some(d) => s.split(d).map(piece).collect(),
        None => s
            .char_indices()
            .map(|(i, c)| piece(&s[i..i + c.len_utf8()]))
            .collect(),
    };
    one_dimensional(pieces)
}

/// The number of thresholds at or below `operand`, by a binary search of `thresholds`, which are
/// taken to be in order (`width_bucket`). Refused for thresholds of more than one dimension or
/// holding a NULL.
pub fn width_bucket(elem: ColumnType, operand: &Item, thresholds: &Item) -> Result<Option<i32>> {
    let Some((elements, dims, _)) = parts(thresholds) else {
        return Ok(None);
    };
    if operand.is_null() {
        return Ok(None);
    }
    if dims.len() > 1 {
        return Err(Error::new("thresholds must be one-dimensional array"));
    }
    if elements.iter().any(Item::is_null) {
        return Err(Error::new("thresholds array must not contain NULLs"));
    }
    if elem != ColumnType::Float8 {
        comparison(elem)?;
    }
    let k = composed::key(elem, operand);
    let (mut left, mut right) = (0usize, elements.len());
    while left < right {
        let mid = (left + right) / 2;
        if k < composed::key(elem, &elements[mid]) {
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    Ok(Some(left as i32))
}

/// The arrays `array_agg(anyarray)` gathers, a row at a time, stacked into one more dimension
/// (`accumArrayResultArr`, `makeArrayResultArr`): the new first from 1, the rest the first array's.
#[derive(Clone, Debug, Default)]
pub struct ArrayAgg {
    elements: Vec<Item>,
    dims: Vec<i32>,
    lower: Vec<i32>,
    rows: usize,
}

impl ArrayAgg {
    pub fn new() -> ArrayAgg {
        ArrayAgg::default()
    }

    /// One more row's array, `Item::Null` for a NULL. Refused for a NULL, an empty array, an array
    /// of the most dimensions, and an array whose dimensions or lower bounds are not the first's.
    pub fn push(&mut self, a: &Item) -> Result<()> {
        self.rows += 1;
        let Some((elements, dims, lower)) = parts(a) else {
            return Err(Error::new("cannot accumulate null arrays"));
        };
        if self.elements.len() as i64 + elements.len() as i64 > MAX_ARRAY_SIZE {
            return Err(Error::new(format!(
                "array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"
            )));
        }
        if self.dims.is_empty() {
            if dims.is_empty() {
                return Err(Error::new("cannot accumulate empty arrays"));
            }
            if dims.len() + 1 > MAXDIM {
                return Err(Error::new(format!(
                    "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
                    dims.len() + 1
                )));
            }
            self.dims = std::iter::once(0).chain(dims.iter().copied()).collect();
            self.lower = std::iter::once(1).chain(lower.iter().copied()).collect();
        } else if self.dims[1..] != *dims || self.lower[1..] != *lower {
            return Err(Error::new(
                "cannot accumulate arrays of different dimensionality",
            ));
        }
        self.elements.extend(elements.iter().cloned());
        self.dims[0] += 1;
        Ok(())
    }

    /// The arrays stacked; NULL where no row came.
    pub fn finish(&self) -> Result<Item> {
        if self.rows == 0 {
            return Ok(Item::Null);
        }
        n_items(&self.dims)?;
        check_bounds(&self.dims, &self.lower)?;
        Ok(Item::Array {
            elements: self.elements.clone(),
            dims: self.dims.clone(),
            lower: self.lower.clone(),
        })
    }
}

/// The subscripts of dimension `dim`, from its lower bound up, or down where `reverse`
/// (`generate_subscripts`); none for a dimension the array has not.
pub fn subscripts(a: &Item, dim: i32, reverse: bool) -> Vec<i32> {
    let Some((d, l)) = dimension(a, dim) else {
        return Vec::new();
    };
    let mut out: Vec<i32> = (0..d).map(|i| l + i).collect();
    if reverse {
        out.reverse();
    }
    out
}

/// The elements in storage order (`unnest`).
pub fn unnest(a: &Item) -> Vec<Item> {
    parts(a).map_or_else(Vec::new, |(e, _, _)| e.to_vec())
}

/// White space as PostgreSQL's scanner sees it.
fn scanner_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic() || c >= 0x80
}

fn ident_cont(c: u8) -> bool {
    c.is_ascii_digit() || c == b'$' || ident_start(c)
}

/// A qualified name's parts (`parse_ident`): each quoted, a doubled quote inside a quote, or of
/// letters, digits, `_` and `$` and lowered; parts parted by `.`, white space around each.
/// Refused where a part is missing or a quote unclosed or empty, and where text follows the last
/// part and `strict`.
pub fn parse_ident(s: &str, strict: bool) -> Result<Vec<String>> {
    let invalid = || Error::new(format!("string is not a valid identifier: \"{s}\""));
    let b = s.as_bytes();
    let at = |p: usize| b.get(p).copied().unwrap_or(0);
    let mut p = 0;
    while scanner_space(at(p)) {
        p += 1;
    }
    let mut out = Vec::new();
    loop {
        let mut missing = true;
        if at(p) == b'"' {
            let mut part = Vec::new();
            p += 1;
            loop {
                match b.get(p) {
                    None => return Err(invalid()),
                    Some(b'"') if at(p + 1) == b'"' => {
                        part.push(b'"');
                        p += 2;
                    }
                    Some(b'"') => {
                        p += 1;
                        break;
                    }
                    Some(&c) => {
                        part.push(c);
                        p += 1;
                    }
                }
            }
            if part.is_empty() {
                return Err(invalid());
            }
            out.push(String::from_utf8(part).map_err(|_| invalid())?);
            missing = false;
        } else if ident_start(at(p)) {
            let first = p;
            p += 1;
            while ident_cont(at(p)) {
                p += 1;
            }
            out.push(s[first..p].to_ascii_lowercase());
            missing = false;
        }
        if missing {
            return Err(invalid());
        }
        while scanner_space(at(p)) {
            p += 1;
        }
        match at(p) {
            b'.' => {
                p += 1;
                while scanner_space(at(p)) {
                    p += 1;
                }
            }
            0 => break,
            _ => {
                if strict {
                    return Err(invalid());
                }
                break;
            }
        }
    }
    Ok(out)
}

/// The privileges `acl.c` names, a bit each from the lowest: `convert_aclright_to_string`'s.
const ACL_RIGHT_NAMES: [&str; 15] = [
    "INSERT",
    "SELECT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "EXECUTE",
    "USAGE",
    "CREATE",
    "TEMPORARY",
    "CONNECT",
    "SET",
    "ALTER SYSTEM",
    "MAINTAIN",
];

/// The privileges an object of a kind has where its ACL is NULL (`acldefault`): PUBLIC's where it
/// has any, then its owner's, granted by its owner. `kind` is the `"char"` naming the kind: `c`
/// column, `r` table, `s` sequence, `d` database, `f` function, `l` language, `L` large object,
/// `n` schema, `p` parameter, `t` tablespace, `F` foreign-data wrapper, `S` foreign server, `T`
/// type. The array is one-dimensional even where it holds no item, as PostgreSQL makes it.
pub fn acldefault(kind: u8, owner: u32) -> Result<Item> {
    const INSERT: u64 = 1;
    const SELECT: u64 = 1 << 1;
    const UPDATE: u64 = 1 << 2;
    const DELETE: u64 = 1 << 3;
    const TRUNCATE: u64 = 1 << 4;
    const REFERENCES: u64 = 1 << 5;
    const TRIGGER: u64 = 1 << 6;
    const EXECUTE: u64 = 1 << 7;
    const USAGE: u64 = 1 << 8;
    const CREATE: u64 = 1 << 9;
    const CREATE_TEMP: u64 = 1 << 10;
    const CONNECT: u64 = 1 << 11;
    const SET: u64 = 1 << 12;
    const ALTER_SYSTEM: u64 = 1 << 13;
    const MAINTAIN: u64 = 1 << 14;
    let (world, own) = match kind {
        b'c' => (0, 0),
        b'r' => (
            0,
            INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER | MAINTAIN,
        ),
        b's' => (0, USAGE | SELECT | UPDATE),
        b'd' => (CREATE_TEMP | CONNECT, CREATE | CREATE_TEMP | CONNECT),
        b'f' => (EXECUTE, EXECUTE),
        b'l' => (USAGE, USAGE),
        b'L' => (0, SELECT | UPDATE),
        b'n' => (0, USAGE | CREATE),
        b'p' => (0, SET | ALTER_SYSTEM),
        b't' => (0, CREATE),
        b'F' | b'S' => (0, USAGE),
        b'T' => (USAGE, USAGE),
        other => {
            let shown: String = if other == 0 {
                String::new()
            } else {
                String::from_utf8_lossy(&[other]).into_owned()
            };
            return Err(Error::new(format!(
                "unrecognized object type abbreviation: {shown}"
            )));
        }
    };
    let mut elements = Vec::new();
    if world != 0 {
        elements.push(text(&crate::textual::aclitem_text(0, owner, world)));
    }
    if own != 0 {
        elements.push(text(&crate::textual::aclitem_text(owner, owner, own)));
    }
    Ok(Item::Array {
        dims: vec![elements.len() as i32],
        lower: vec![1],
        elements,
    })
}

/// An aclitem array as rows (`aclexplode`): for each item, each privilege it grants in the order of
/// their bits, as (grantor, grantee, privilege, whether granted with its option), PUBLIC's grantee
/// 0. Refused for an array not of one dimension or holding a NULL.
pub fn aclexplode(acl: &Item) -> Result<Vec<(u32, u32, &'static str, bool)>> {
    let Some((elements, dims, _)) = parts(acl) else {
        return Ok(Vec::new());
    };
    if dims.len() != 1 {
        return Err(Error::new("ACL arrays must be one-dimensional"));
    }
    let held: Vec<&str> = elements
        .iter()
        .map(|e| match e {
            Item::Scalar(Datum::Text(s)) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()
        .ok_or_else(|| Error::new("ACL arrays must not contain null values"))?;
    let mut rows = Vec::new();
    for s in held {
        let (grantee, grantor, held) = crate::textual::aclitem_parse(s)?;
        for (bit, name) in ACL_RIGHT_NAMES.iter().enumerate() {
            if held & (1 << bit) != 0 {
                rows.push((grantor, grantee, *name, held & (1 << (bit + 32)) != 0));
            }
        }
    }
    Ok(rows)
}

/// An options array as rows of (name, value) (`pg_options_to_table`): each element cut at its first
/// `=`, the value NULL where it has none. Refused where an element is NULL.
pub fn options_to_table(options: &Item) -> Result<Vec<(String, Option<String>)>> {
    let Some((elements, _, _)) = parts(options) else {
        return Ok(Vec::new());
    };
    elements
        .iter()
        .map(|e| match e {
            Item::Scalar(Datum::Text(s)) => Ok(match s.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (s.clone(), None),
            }),
            _ => Err(Error::new("null array element not allowed in this context")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(elem: ColumnType) -> ColumnType {
        composed::type_of(Composed::Array {
            element: elem,
            domain: None,
        })
    }

    fn arr(elem: ColumnType, s: &str) -> Item {
        composed::text_in(ty(elem), s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn out(elem: ColumnType, a: &Item) -> String {
        composed::text_out(ty(elem), a)
    }

    fn int(v: i64) -> Item {
        Item::Scalar(Datum::Int(v))
    }

    const I4: ColumnType = ColumnType::Int4;
    const TEXT: ColumnType = ColumnType::Text(None);

    // Each answer as PostgreSQL 19 RC1 gives it.

    #[test]
    fn the_shape_functions_read_each_dimension() {
        let a = arr(I4, "[2:3][-1:1]={{1,2,3},{4,5,6}}");
        assert_eq!(dims_text(&a).as_deref(), Some("[2:3][-1:1]"));
        assert_eq!(ndims(&a), Some(2));
        assert_eq!(length(&a, 2), Some(3));
        assert_eq!(lower(&a, 2), Some(-1));
        assert_eq!(upper(&a, 1), Some(3));
        assert_eq!(upper(&a, 3), None);
        assert_eq!(length(&a, 0), None);
        assert_eq!(cardinality(&a), Some(6));
        assert_eq!(ndims(&arr(I4, "{}")), None);
        assert_eq!(cardinality(&arr(I4, "{}")), Some(0));
        assert_eq!(subscripts(&a, 2, true), vec![1, 0, -1]);
    }

    #[test]
    fn array_fill_checks_its_bounds_before_it_fills() {
        let f = |dims: &str, lbs: Option<&str>| {
            let l = lbs.map(|l| arr(I4, l));
            fill(I4, &int(7), &arr(I4, dims), l.as_ref())
                .map(|a| out(I4, &a))
                .unwrap_or_else(|e| e.to_string())
        };
        assert_eq!(f("{2,2}", Some("{0,5}")), "[0:1][5:6]={{7,7},{7,7}}");
        assert_eq!(f("{0}", None), "{}");
        assert_eq!(
            f("{-1}", None),
            "array size exceeds the maximum allowed (134217727)"
        );
        assert_eq!(
            f("{1}", Some("{2147483647}")),
            "array lower bound is too large: 2147483647"
        );
        assert_eq!(f("{2}", Some("{1,1}")), "wrong number of array subscripts");
        assert_eq!(f("{NULL}", None), "dimension values cannot be null");
        assert_eq!(
            fill(I4, &int(7), &Item::Null, None)
                .unwrap_err()
                .to_string(),
            "dimension array or low bound array cannot be null"
        );
    }

    #[test]
    fn array_position_finds_null_and_starts_where_asked() {
        let a = arr(I4, "[0:4]={1,NULL,3,1,NULL}");
        assert_eq!(position(I4, &a, &int(1), None).unwrap(), Some(0));
        assert_eq!(position(I4, &a, &int(1), Some(Some(1))).unwrap(), Some(3));
        assert_eq!(position(I4, &a, &int(1), Some(Some(0))).unwrap(), Some(0));
        assert_eq!(position(I4, &a, &Item::Null, None).unwrap(), Some(1));
        assert_eq!(out(I4, &positions(I4, &a, &Item::Null).unwrap()), "{1,4}");
        assert_eq!(
            position(I4, &arr(I4, "{1,2}"), &Item::Null, Some(None)).unwrap(),
            None
        );
        assert_eq!(
            position(I4, &a, &Item::Null, Some(None))
                .unwrap_err()
                .to_string(),
            "initial position must not be null"
        );
        assert_eq!(
            position(I4, &arr(I4, "{{1}}"), &int(1), None)
                .unwrap_err()
                .to_string(),
            "searching for elements in multidimensional arrays is not supported"
        );
        let json = ColumnType::Textual(crate::textual::Textual::Json);
        assert_eq!(
            position(json, &arr(json, "{1}"), &Item::Null, None).unwrap(),
            None
        );
        assert_eq!(
            remove(json, &arr(json, "{1}"), &Item::Null)
                .unwrap_err()
                .to_string(),
            "could not identify an equality operator for type json"
        );
    }

    #[test]
    fn array_remove_and_replace_keep_the_shape_they_may() {
        let a = arr(I4, "[3:6]={1,2,1,NULL}");
        assert_eq!(out(I4, &remove(I4, &a, &int(1)).unwrap()), "[3:4]={2,NULL}");
        assert_eq!(
            out(I4, &remove(I4, &a, &int(9)).unwrap()),
            "[3:6]={1,2,1,NULL}"
        );
        assert_eq!(
            out(I4, &replace(I4, &a, &Item::Null, &int(0)).unwrap()),
            "[3:6]={1,2,1,0}"
        );
        assert_eq!(
            out(I4, &remove(I4, &arr(I4, "{1,1}"), &int(1)).unwrap()),
            "{}"
        );
        assert_eq!(
            remove(I4, &arr(I4, "{{1},{2}}"), &int(1))
                .unwrap_err()
                .to_string(),
            "removing elements from multidimensional arrays is not supported"
        );
    }

    #[test]
    fn array_sort_leaves_equal_numerics_where_postgresql_s_quicksort_does() {
        let n = ColumnType::NumericText(None);
        let s = |a: &str, d: bool, nf: bool| out(n, &sort(n, &arr(n, a), d, nf).unwrap());
        assert_eq!(
            s("{1.00,1.0,1,1.000,0.5,1.0}", false, false),
            "{0.5,1.00,1.0,1,1.000,1.0}"
        );
        assert_eq!(
            s("{3,1.0,1,1.00,2,1.000,1.0,0.5,1}", false, false),
            "{0.5,1.0,1,1.00,1,1.000,1.0,2,3}"
        );
        assert_eq!(s("[0:3]={2,NULL,1,3}", true, false), "[0:3]={3,2,1,NULL}");
        assert_eq!(s("{2,NULL,1,3}", true, true), "{NULL,3,2,1}");
        assert_eq!(
            out(
                I4,
                &sort(
                    I4,
                    &arr(I4, "[2:4][5:6]={{3,1},{1,2},{1,NULL}}"),
                    false,
                    false
                )
                .unwrap()
            ),
            "[2:4][5:6]={{1,2},{1,NULL},{3,1}}"
        );
    }

    #[test]
    fn reverse_trim_and_the_draws_move_whole_items() {
        let a = arr(I4, "[2:4][0:1]={{1,2},{3,4},{5,6}}");
        assert_eq!(out(I4, &reverse(&a)), "[2:4][0:1]={{5,6},{3,4},{1,2}}");
        assert_eq!(out(I4, &trim(&a, 1).unwrap()), "{{1,2},{3,4}}");
        let least = arr(I4, "[-2147483648:-2147483647]={1,2}");
        assert_eq!(out(I4, &trim(&least, 2).unwrap()), "{1,2}");
        assert_eq!(out(I4, &trim(&least, 1).unwrap()), "{1}");
        assert_eq!(
            trim(&a, 4).unwrap_err().to_string(),
            "number of elements to trim must be between 0 and 3"
        );
        // A draw of the last place each time.
        let mut last = |k: u64| k - 1;
        assert_eq!(
            out(I4, &shuffle(&a, &mut last)),
            "[2:4][0:1]={{5,6},{1,2},{3,4}}"
        );
        let mut first = |_: u64| 0;
        assert_eq!(
            out(I4, &sample(&a, 2, &mut first).unwrap()),
            "[1:2][0:1]={{1,2},{3,4}}"
        );
        assert_eq!(
            sample(&a, 4, &mut first).unwrap_err().to_string(),
            "sample size must be between 0 and 3"
        );
    }

    #[test]
    fn array_to_string_and_back() {
        let a = arr(TEXT, "{a,NULL,\"b,c\",\"\"}");
        assert_eq!(to_string(TEXT, &a, ",", None).as_deref(), Some("a,b,c,"));
        assert_eq!(
            to_string(TEXT, &a, "-", Some("*")).as_deref(),
            Some("a-*-b,c-")
        );
        let b = arr(ColumnType::Boolean, "{t,f}");
        assert_eq!(
            to_string(ColumnType::Boolean, &b, "", None).as_deref(),
            Some("tf")
        );
        let o = |s: &str, d: Option<&str>, n: Option<&str>| out(TEXT, &string_to_array(s, d, n));
        assert_eq!(o("a,,b,", Some(","), Some("")), "{a,NULL,b,NULL}");
        assert_eq!(o("aXXXb", Some("XX"), None), "{a,Xb}");
        assert_eq!(o("ab", None, Some("b")), "{a,NULL}");
        assert_eq!(o("", Some(","), None), "{}");
        assert_eq!(o("abc", Some(""), None), "{abc}");
    }

    #[test]
    fn width_bucket_counts_the_thresholds_at_or_below() {
        let t = arr(I4, "{1,3,3,5}");
        assert_eq!(width_bucket(I4, &int(3), &t).unwrap(), Some(3));
        assert_eq!(width_bucket(I4, &int(0), &t).unwrap(), Some(0));
        assert_eq!(width_bucket(I4, &int(9), &arr(I4, "{}")).unwrap(), Some(0));
        assert_eq!(
            width_bucket(I4, &int(1), &arr(I4, "{1,NULL}"))
                .unwrap_err()
                .to_string(),
            "thresholds array must not contain NULLs"
        );
    }

    #[test]
    fn array_agg_stacks_arrays_of_one_shape() {
        let mut g = ArrayAgg::new();
        assert_eq!(g.finish().unwrap(), Item::Null);
        g.push(&arr(I4, "[0:1]={1,2}")).unwrap();
        g.push(&arr(I4, "[0:1]={3,NULL}")).unwrap();
        assert_eq!(out(I4, &g.finish().unwrap()), "[1:2][0:1]={{1,2},{3,NULL}}");
        assert_eq!(
            g.push(&arr(I4, "{3,4}")).unwrap_err().to_string(),
            "cannot accumulate arrays of different dimensionality"
        );
        let mut e = ArrayAgg::new();
        assert_eq!(
            e.push(&arr(I4, "{}")).unwrap_err().to_string(),
            "cannot accumulate empty arrays"
        );
        assert_eq!(
            e.push(&Item::Null).unwrap_err().to_string(),
            "cannot accumulate null arrays"
        );
    }

    #[test]
    fn parse_ident_reads_quoted_and_lowered_parts() {
        let p = |s: &str, strict: bool| {
            parse_ident(s, strict)
                .map(|v| v.join("|"))
                .unwrap_or_else(|e| e.to_string())
        };
        assert_eq!(p(" Foo . \"Bar\"\"x\" ", true), "foo|Bar\"x");
        assert_eq!(p("a.b junk", false), "a|b");
        assert_eq!(
            p("a.b junk", true),
            "string is not a valid identifier: \"a.b junk\""
        );
        assert_eq!(p("a.", true), "string is not a valid identifier: \"a.\"");
        assert_eq!(
            p("\"\"", true),
            "string is not a valid identifier: \"\"\"\""
        );
        assert_eq!(p("ÉcolE$1", true), "École$1");
    }

    #[test]
    fn the_acl_functions_make_and_unfold_items() {
        let acl = ColumnType::Textual(crate::textual::Textual::Aclitem);
        assert_eq!(
            out(acl, &acldefault(b'd', 99999).unwrap()),
            "{=Tc/99999,99999=CTc/99999}"
        );
        let column = acldefault(b'c', 99999).unwrap();
        assert_eq!(dims_text(&column).as_deref(), Some("[1:0]"));
        assert_eq!(aclexplode(&column).unwrap(), vec![]);
        assert_eq!(
            aclexplode(&arr(acl, "{}")).unwrap_err().to_string(),
            "ACL arrays must be one-dimensional"
        );
        assert_eq!(
            aclexplode(&arr(acl, "{=r*w/77,NULL}"))
                .unwrap_err()
                .to_string(),
            "ACL arrays must not contain null values"
        );
        assert_eq!(
            aclexplode(&arr(acl, "{=r*w/77}")).unwrap(),
            vec![(77, 0, "SELECT", true), (77, 0, "UPDATE", false)]
        );
        assert_eq!(
            acldefault(b'x', 1).unwrap_err().to_string(),
            "unrecognized object type abbreviation: x"
        );
        assert_eq!(
            options_to_table(&arr(TEXT, "{a=1=2,b,c=}")).unwrap(),
            vec![
                ("a".into(), Some("1=2".into())),
                ("b".into(), None),
                ("c".into(), Some(String::new()))
            ]
        );
    }
}
