//! What a jsonpath asks of an element strand of a jsonb, as `extract_jsp_query` (jsonb_gin.c) reads it
//! for `@?` and `@@` under `jsonb_ops` and `jsonb_path_ops`: what must hold of a jsonb the path passes,
//! nothing where it finds nothing to ask.

use super::{read, Chain, Item, Op};
use crate::jsonb::{gin_key, gin_path_element, gin_scalar, GinScalar};
use crate::Result;

/// What must hold of a jsonb: it holds an element, or all or any of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asks {
    At(Vec<u8>),
    All(Vec<Asks>),
    Any(Vec<Asks>),
}

/// One accessor on the way down.
#[derive(Clone)]
enum Step {
    Key(String),
    Deep,
    Members,
    Elements,
}

/// How a path is read: `lax` as it is written, `path_ops` as `jsonb_path_ops` holds it.
#[derive(Clone, Copy)]
struct Reading {
    lax: bool,
    path_ops: bool,
}

/// What a jsonpath asks under `@?` where `exists`, otherwise under `@@`; none where it asks nothing,
/// which reads every row.
pub fn gin_asks(held: &str, exists: bool, path_ops: bool) -> Result<Option<Asks>> {
    let (lax, chain) = read(held)?;
    let r = Reading { lax, path_ops };
    Ok(match exists {
        true => r.path_expr(&[], &chain, None),
        false => r.bool_expr(&[], &chain, false),
    })
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The scalar a comparison's side is: one item and nothing applied to it.
fn scalar_of(chain: &Chain) -> Option<GinScalar<'_>> {
    match chain.as_slice() {
        [Item::Null] => Some(GinScalar::Null),
        [Item::Bool(b)] => Some(GinScalar::Bool(*b)),
        [Item::Num(n)] => Some(GinScalar::Num(n)),
        [Item::Str(s)] => Some(GinScalar::Str(std::str::from_utf8(s).ok()?)),
        _ => None,
    }
}

impl Reading {
    /// An accessor taken onto the way down: false where the operator class cannot follow it.
    fn add(&self, path: &mut Vec<Step>, item: &Item) -> bool {
        match (item, self.path_ops) {
            (Item::Root, _) => path.clear(),
            (Item::Key(k), _) => path.push(Step::Key(text(k))),
            (Item::AnyArray | Item::Index(_), _) => path.push(Step::Elements),
            (Item::Any(..), false) => path.push(Step::Deep),
            (Item::AnyKey, false) => path.push(Step::Members),
            _ => return false,
        }
        true
    }

    /// What `path == scalar` adds to the nodes a path's filters gave; an existence adds nothing.
    fn extract_nodes(
        &self,
        path: &[Step],
        scalar: Option<&GinScalar>,
        mut nodes: Vec<Asks>,
    ) -> Vec<Asks> {
        let Some(scalar) = scalar else {
            return nodes;
        };
        if self.path_ops {
            let keys: Vec<&str> = path
                .iter()
                .filter_map(|s| match s {
                    Step::Key(k) => Some(k.as_str()),
                    _ => None,
                })
                .collect();
            nodes.push(Asks::At(gin_path_element(&keys, scalar)));
            return nodes;
        }
        for s in path.iter().rev() {
            if let Step::Key(k) = s {
                nodes.push(Asks::At(gin_key(k)));
            }
        }
        let node = match scalar {
            GinScalar::Str(_) => match (self.lax, path.last()) {
                (true, _) | (false, Some(Step::Deep)) => Asks::Any(vec![
                    Asks::At(gin_scalar(scalar, true)),
                    Asks::At(gin_scalar(scalar, false)),
                ]),
                (false, None | Some(Step::Elements)) => Asks::At(gin_scalar(scalar, true)),
                _ => Asks::At(gin_scalar(scalar, false)),
            },
            _ => Asks::At(gin_scalar(scalar, false)),
        };
        nodes.push(node);
        nodes
    }

    /// What `EXISTS(chain)` asks, or `chain == scalar`, the path so far given.
    fn path_expr(&self, path: &[Step], chain: &Chain, scalar: Option<&GinScalar>) -> Option<Asks> {
        let mut path = path.to_vec();
        let mut nodes = Vec::new();
        let mut followed = true;
        for item in chain {
            match item {
                Item::Current => {}
                Item::Filter(arg) => nodes.extend(self.bool_expr(&path, arg, false)),
                item if self.add(&mut path, item) => {}
                _ => {
                    followed = false;
                    break;
                }
            }
        }
        if followed {
            nodes = self.extract_nodes(&path, scalar, nodes);
        }
        match nodes.len() {
            0 => None,
            1 => nodes.pop(),
            _ => Some(Asks::All(nodes)),
        }
    }

    /// What a predicate asks, or under `not` its negation: a predicate something is applied to asks
    /// nothing.
    fn bool_expr(&self, path: &[Step], chain: &Chain, not: bool) -> Option<Asks> {
        let [item] = chain.as_slice() else {
            return None;
        };
        match item {
            Item::Binary(op @ (Op::And | Op::Or), l, r) => {
                let (a, b) = (self.bool_expr(path, l, not), self.bool_expr(path, r, not));
                let all = (*op == Op::And) != not;
                match (a, b) {
                    (Some(a), Some(b)) => Some(match all {
                        true => Asks::All(vec![a, b]),
                        false => Asks::Any(vec![a, b]),
                    }),
                    (a, b) if all => a.or(b),
                    _ => None,
                }
            }
            Item::Not(a) => self.bool_expr(path, a, !not),
            Item::Exists(a) if !not => self.path_expr(path, a, None),
            Item::Binary(Op::Equal, l, r) if !not => {
                let (scalar, chain) = match (scalar_of(l), scalar_of(r)) {
                    (Some(s), _) => (s, r),
                    (None, Some(s)) => (s, l),
                    (None, None) => return None,
                };
                self.path_expr(path, chain, Some(&scalar))
            }
            _ => None,
        }
    }
}
