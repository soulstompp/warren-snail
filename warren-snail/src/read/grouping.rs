//! A GROUP BY's grouping sets, as PostgreSQL's parser reads them (`transformGroupClause`) and
//! expands them (`expand_grouping_sets`): each item's own sets, a plain expression one set of
//! itself, `ROLLUP (e1, …, en)` its n+1 prefixes, `()` last, `CUBE (…)` every subset of its elements,
//! GROUPING SETS those it lists, a ROLLUP or a CUBE among them expanded in place; then the sets of
//! the items multiplied, one of each item's, a member once in each. A parenthesized list at the top
//! is its expressions, and `()` there beside other items adds nothing. GROUP BY DISTINCT, written
//! as its mark before the first item (`spelled::SETS_DISTINCT`), drops a set met again once expanded.

use crate::{Error, Result};
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectNamePart};

/// The most elements a CUBE takes, and the most sets a GROUP BY makes, as PostgreSQL's parser
/// limits them.
const CUBE_MOST: usize = 12;
const SETS_MOST: f64 = 4096.0;

/// A GROUP BY read: each grouping expression once, in the order first met, and each set by its
/// members' places among them, ascending. `written` where it is written with sets (ROLLUP, CUBE,
/// GROUPING SETS or `()`); `too_many` where its sets pass PostgreSQL's most, refused after the
/// clause is read (`refused`).
pub(super) struct Sets<'q> {
    pub(super) exprs: Vec<&'q Expr>,
    pub(super) sets: Vec<Vec<usize>>,
    pub(super) written: bool,
    too_many: bool,
}

impl Sets<'_> {
    /// Whether it groups as a GROUP BY of plain expressions does: one set, every expression in it.
    pub(super) fn plain(&self) -> bool {
        self.sets.len() == 1
    }

    /// The places every set holds.
    pub(super) fn common(&self) -> Vec<usize> {
        (0..self.exprs.len())
            .filter(|i| self.sets.iter().all(|s| s.contains(i)))
            .collect()
    }

    /// Its sets refused where they pass the most PostgreSQL's parser makes.
    pub(super) fn refused(&self) -> Result<()> {
        match self.too_many {
            true => Err(Error::new("too many grouping sets present (maximum 4096)")),
            false => Ok(()),
        }
    }
}

/// Whether a GROUP BY item is its DISTINCT's mark.
pub(super) fn is_distinct_mark(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(i) if i.quote_style.is_some()
        && i.value.len() == 1
        && i.value.starts_with(super::spelled::SETS_DISTINCT))
}

/// Whether a GROUP BY's items are read as sets: a ROLLUP, a CUBE, GROUPING SETS, `()` or the
/// DISTINCT's mark among them.
pub(super) fn written(group_by: &[Expr]) -> bool {
    group_by.iter().any(|e| {
        matches!(e, Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_))
            || matches!(e, Expr::Tuple(xs) if xs.is_empty())
            || is_distinct_mark(e)
    })
}

/// Every expression a GROUP BY's items group by, as written, each where it stands: the members of
/// its sets, a parenthesized list's expressions.
pub(super) fn each_expr(group_by: &[Expr]) -> Vec<&Expr> {
    let mut out = Vec::new();
    for g in group_by {
        match g {
            Expr::Rollup(els) | Expr::Cube(els) | Expr::GroupingSets(els) => {
                for el in els {
                    for e in el {
                        match nested_set(e) {
                            Some((_, args)) => args.into_iter().for_each(|a| members(a, &mut out)),
                            None => members(e, &mut out),
                        }
                    }
                }
            }
            e if is_distinct_mark(e) => {}
            e => members(e, &mut out),
        }
    }
    out
}

/// A row's members, as PostgreSQL's parser reads a list in parentheses among grouping items: each
/// expression of it. Any other expression is itself.
fn members<'q>(e: &'q Expr, out: &mut Vec<&'q Expr>) {
    match e {
        Expr::Tuple(xs) => xs.iter().for_each(|x| members(x, out)),
        Expr::Nested(x) if matches!(x.as_ref(), Expr::Tuple(_)) => members(x, out),
        _ => out.push(e),
    }
}

/// A ROLLUP or a CUBE among GROUPING SETS' elements, which sqlparser reads as a call: whether a
/// CUBE, and its elements.
fn nested_set(e: &Expr) -> Option<(bool, Vec<&Expr>)> {
    let Expr::Function(f) = e else {
        return None;
    };
    let [ObjectNamePart::Identifier(i)] = f.name.0.as_slice() else {
        return None;
    };
    if i.quote_style.is_some() || f.over.is_some() || f.filter.is_some() {
        return None;
    }
    let cube = match i.value.to_ascii_lowercase().as_str() {
        "rollup" => false,
        "cube" => true,
        _ => return None,
    };
    let FunctionArguments::List(l) = &f.args else {
        return None;
    };
    if l.duplicate_treatment.is_some() || !l.clauses.is_empty() {
        return None;
    }
    let args = l
        .args
        .iter()
        .map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Some(x),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((cube, args))
}

/// A GROUP BY read and expanded: each expression read through `read` (an ordinal or an output name
/// the select list's item), placed once among those `same` finds alike.
pub(super) fn expand<'q>(
    group_by: &'q [Expr],
    read: &mut dyn FnMut(&'q Expr) -> Result<&'q Expr>,
    same: &dyn Fn(&Expr, &Expr) -> bool,
) -> Result<Sets<'q>> {
    let mut exprs: Vec<&'q Expr> = Vec::new();
    let mut place = |e: &'q Expr| -> Result<usize> {
        let e = read(e)?;
        Ok(match exprs.iter().position(|x| same(x, e)) {
            Some(at) => at,
            None => {
                exprs.push(e);
                exprs.len() - 1
            }
        })
    };
    // An element of a ROLLUP or a CUBE, or a set GROUPING SETS lists: its members' places.
    let mut element = |el: &[&'q Expr]| -> Result<Vec<usize>> {
        let mut m = Vec::new();
        for &e in el {
            members(e, &mut m);
        }
        m.into_iter().map(&mut place).collect()
    };
    let mut distinct = false;
    let mut written = false;
    let mut items: Vec<Vec<Vec<usize>>> = Vec::new();
    for (n, g) in group_by.iter().enumerate() {
        match g {
            e if n == 0 && is_distinct_mark(e) => distinct = true,
            Expr::Tuple(xs) if xs.is_empty() => written = true,
            Expr::Rollup(els) | Expr::Cube(els) => {
                written = true;
                let els = els
                    .iter()
                    .map(|el| element(&el.iter().collect::<Vec<_>>()))
                    .collect::<Result<Vec<_>>>()?;
                items.push(rolled(matches!(g, Expr::Cube(_)), els)?);
            }
            Expr::GroupingSets(els) => {
                written = true;
                let mut sets = Vec::new();
                for el in els {
                    match el.as_slice() {
                        [e] if nested_set(e).is_some() => {
                            let (cube, args) = nested_set(e).expect("just found");
                            let els = args
                                .into_iter()
                                .map(|a| element(&[a]))
                                .collect::<Result<Vec<_>>>()?;
                            sets.extend(rolled(cube, els)?);
                        }
                        el => sets.push(element(&el.iter().collect::<Vec<_>>())?),
                    }
                }
                items.push(sets);
            }
            e => {
                let mut m = Vec::new();
                members(e, &mut m);
                for x in m {
                    items.push(vec![element(&[x])?]);
                }
            }
        }
    }
    // The sets multiplied, counted as each item multiplies them, refused past the most.
    let mut count = 1.0f64;
    for i in &items {
        count *= i.len() as f64;
        if count > SETS_MOST {
            return Ok(Sets {
                exprs,
                sets: Vec::new(),
                written,
                too_many: true,
            });
        }
    }
    let mut sets: Vec<Vec<usize>> = vec![Vec::new()];
    for item in &items {
        let mut next = Vec::with_capacity(sets.len() * item.len());
        for s in &sets {
            for t in item {
                let mut u = s.clone();
                for &p in t {
                    if !u.contains(&p) {
                        u.push(p);
                    }
                }
                next.push(u);
            }
        }
        sets = next;
    }
    for s in &mut sets {
        s.sort_unstable();
        s.dedup();
    }
    if distinct {
        let mut kept: Vec<Vec<usize>> = Vec::new();
        for s in sets {
            if !kept.contains(&s) {
                kept.push(s);
            }
        }
        sets = kept;
    }
    Ok(Sets {
        exprs,
        sets,
        written: written || distinct,
        too_many: false,
    })
}

/// A ROLLUP's sets, its prefixes longest first and `()` last, or a CUBE's, every subset of its
/// elements as the bits of a count from none to all, the first element the lowest bit; a CUBE of
/// more elements than PostgreSQL's parser takes refused.
fn rolled(cube: bool, els: Vec<Vec<usize>>) -> Result<Vec<Vec<usize>>> {
    if cube {
        if els.len() > CUBE_MOST {
            return Err(Error::new("CUBE is limited to 12 elements"));
        }
        return Ok((0u32..1 << els.len())
            .map(|bits| {
                els.iter()
                    .enumerate()
                    .filter(|(j, _)| bits & (1 << j) != 0)
                    .flat_map(|(_, el)| el.iter().copied())
                    .collect()
            })
            .collect());
    }
    Ok((0..=els.len())
        .rev()
        .map(|n| els[..n].concat())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{GroupByExpr, SetExpr, Statement};

    fn group_by(sql: &str) -> Vec<Expr> {
        let parsed = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::PostgreSqlDialect {},
            &format!("SELECT 1 FROM t GROUP BY {sql}"),
        )
        .unwrap();
        let Statement::Query(q) = &parsed[0] else {
            panic!("a query")
        };
        let SetExpr::Select(s) = q.body.as_ref() else {
            panic!("a select")
        };
        match &s.group_by {
            GroupByExpr::Expressions(g, _) => g.clone(),
            _ => panic!("expressions"),
        }
    }

    /// The sets a GROUP BY expands to, each as its members written.
    fn sets_of(sql: &str) -> Vec<Vec<String>> {
        let g = group_by(sql);
        let s = expand(&g, &mut |e| Ok(e), &|a, b| a == b).unwrap();
        s.sets
            .iter()
            .map(|set| set.iter().map(|&i| s.exprs[i].to_string()).collect())
            .collect()
    }

    #[test]
    fn sqlparser_s_shapes_are_read() {
        let one = |sql: &str| group_by(sql).remove(0);
        let names = |xs: &[&str]| xs.iter().map(|x| Expr::Identifier((*x).into())).collect::<Vec<_>>();
        assert_eq!(one("ROLLUP (a, b)"), Expr::Rollup(vec![names(&["a"]), names(&["b"])]));
        assert_eq!(one("ROLLUP ((a, b))"), Expr::Rollup(vec![names(&["a", "b"])]));
        assert_eq!(one("CUBE ((a, b), c)"), Expr::Cube(vec![names(&["a", "b"]), names(&["c"])]));
        assert_eq!(
            one("GROUPING SETS ((a), b, (), (a, b))"),
            Expr::GroupingSets(vec![names(&["a"]), names(&["b"]), vec![], names(&["a", "b"])])
        );
        assert_eq!(one("()"), Expr::Tuple(vec![]));
        assert_eq!(one("(a, b)"), Expr::Tuple(names(&["a", "b"])));
        // A ROLLUP or a CUBE among GROUPING SETS' elements is read as a call.
        let Expr::GroupingSets(els) = one("GROUPING SETS (ROLLUP (a), CUBE ((a, b), c))") else {
            panic!("sets")
        };
        assert!(matches!(nested_set(&els[0][0]), Some((false, a)) if a.len() == 1));
        assert!(matches!(nested_set(&els[1][0]), Some((true, a)) if a.len() == 2));
    }

    #[test]
    fn a_rollup_is_its_prefixes() {
        assert_eq!(
            sets_of("ROLLUP (a, b, c)"),
            [vec!["a", "b", "c"], vec!["a", "b"], vec!["a"], vec![]]
        );
        assert_eq!(sets_of("ROLLUP ((a, b), c)"), [vec!["a", "b", "c"], vec!["a", "b"], vec![]]);
    }

    #[test]
    fn a_cube_is_its_subsets() {
        assert_eq!(
            sets_of("CUBE (a, b)"),
            [vec![], vec!["a"], vec!["b"], vec!["a", "b"]]
        );
        assert_eq!(sets_of("CUBE ((a, b))"), [vec![], vec!["a", "b"]]);
        let g = group_by("CUBE (a, b, c, d, e, f, g, h, i, j, k, l, m)");
        let e = expand(&g, &mut |e| Ok(e), &|a, b| a == b).err().unwrap();
        assert_eq!(e.to_string(), "CUBE is limited to 12 elements");
        // Twelve elements are taken; a pair counts once.
        let g = group_by("CUBE ((a, b), c, d, e, f, g, h, i, j, k, l, m)");
        assert_eq!(expand(&g, &mut |e| Ok(e), &|a, b| a == b).unwrap().sets.len(), 4096);
    }

    #[test]
    fn sets_multiply_their_members_once() {
        assert_eq!(
            sets_of("a, ROLLUP (b)"),
            [vec!["a", "b"], vec!["a"]]
        );
        assert_eq!(
            sets_of("ROLLUP (a), CUBE (b)"),
            [vec!["a"], vec!["a", "b"], vec![], vec!["b"]]
        );
        // Repeats are kept: two sets alike, and a member met twice in one set once.
        assert_eq!(sets_of("ROLLUP (a), a"), [vec!["a"], vec!["a"]]);
        assert_eq!(sets_of("GROUPING SETS ((a), (a))"), [vec!["a"], vec!["a"]]);
        // At the top, `()` beside other items adds nothing, and a list is its expressions.
        assert_eq!(sets_of("(), a"), [vec!["a"]]);
        assert_eq!(sets_of("(a, b)"), [vec!["a", "b"]]);
        assert_eq!(sets_of("()"), [Vec::<String>::new()]);
        assert_eq!(
            sets_of("GROUPING SETS ((a, b), ROLLUP (a), CUBE (b))"),
            [vec!["a", "b"], vec!["a"], vec![], vec![], vec!["b"]]
        );
        // Counted as the items multiply, before repeats drop.
        let g = group_by("CUBE (a, b, c, d, e, f, g), CUBE (h, i, j, k, l, m)");
        let s = expand(&g, &mut |e| Ok(e), &|a, b| a == b).unwrap();
        assert_eq!(
            s.refused().err().unwrap().to_string(),
            "too many grouping sets present (maximum 4096)"
        );
    }

    #[test]
    fn distinct_drops_repeated_sets() {
        let mark = format!("\"{}\", ", super::super::spelled::SETS_DISTINCT);
        assert_eq!(sets_of(&format!("{mark}ROLLUP (a), ROLLUP (a)")), [vec!["a"], vec![]]);
        assert_eq!(sets_of(&format!("{mark}ROLLUP (a, b), ROLLUP (b, a)")).len(), 4);
        assert_eq!(sets_of("ROLLUP (a), ROLLUP (a)").len(), 4);
    }
}
