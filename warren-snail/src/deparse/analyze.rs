//! An expression read as PostgreSQL's parser reads it (parse_expr.c, parse_oper.c, parse_func.c,
//! parse_coerce.c): each literal given its type, each operator and function resolved over the
//! catalog of the file's release, and every coercion the parser makes put in its place, so that
//! the tree printed is the one PostgreSQL would print.

use super::cat::{self, Cat};
use crate::{Error, Result};
use std::borrow::Cow;
use std::cell::RefCell;
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    UnaryOperator, Value as SqlValue,
};

/// How a coercion or a call is written: a call, a cast written out, a cast the parser made, or
/// one of SQL's own spellings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Call,
    Explicit,
    Implicit,
    Sql,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoolOp {
    And,
    Or,
    Not,
}

/// How strongly a coercion may convert: implicitly, in an assignment, or written out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Context {
    Implicit,
    Assignment,
    Explicit,
}

/// An expression as PostgreSQL's parser leaves it.
#[derive(Clone, Debug)]
pub(crate) enum Node {
    /// A column: its relation in the range table (from 1) and its query level (0 its own), and
    /// the relation and column it was named through, where that differs (a named join).
    Var {
        attno: i16,
        ty: u32,
        typmod: i32,
        rel: u32,
        up: u32,
        syn: (u32, i16),
    },
    /// A domain CHECK's VALUE.
    DomainValue {
        ty: u32,
        typmod: i32,
    },
    /// A constant: its type, typmod and its type's text output; None for NULL.
    Const {
        ty: u32,
        typmod: i32,
        text: Option<String>,
    },
    Op {
        oid: u32,
        args: Vec<Node>,
        ty: u32,
    },
    Func {
        oid: u32,
        args: Vec<Node>,
        ty: u32,
        form: Form,
        variadic: bool,
    },
    Relabel {
        arg: Box<Node>,
        ty: u32,
        typmod: i32,
        form: Form,
    },
    Io {
        arg: Box<Node>,
        ty: u32,
        form: Form,
    },
    ArrayCoerce {
        arg: Box<Node>,
        ty: u32,
        typmod: i32,
        form: Form,
    },
    Domain {
        arg: Box<Node>,
        ty: u32,
        form: Form,
    },
    Bool {
        op: BoolOp,
        args: Vec<Node>,
    },
    NullTest {
        arg: Box<Node>,
        not: bool,
    },
    BoolTest {
        arg: Box<Node>,
        test: &'static str,
    },
    Distinct {
        args: Vec<Node>,
    },
    NullIf {
        args: Vec<Node>,
        ty: u32,
    },
    AnyAll {
        oid: u32,
        any: bool,
        args: Vec<Node>,
    },
    Case {
        arg: Option<Box<Node>>,
        whens: Vec<(Node, Node)>,
        default: Box<Node>,
        ty: u32,
    },
    CaseTest {
        ty: u32,
        typmod: i32,
    },
    Array {
        elements: Vec<Node>,
        ty: u32,
        elem: u32,
        multidims: bool,
    },
    Row {
        args: Vec<Node>,
        ty: u32,
        explicit: bool,
    },
    Coalesce {
        args: Vec<Node>,
        ty: u32,
    },
    MinMax {
        greatest: bool,
        args: Vec<Node>,
        ty: u32,
    },
    SqlValue {
        name: &'static str,
        typmod: Option<i32>,
        ty: u32,
    },
    Collate {
        arg: Box<Node>,
        coll: u32,
    },
    Subscript {
        container: Box<Node>,
        subs: Vec<(Option<Node>, Option<Node>, bool)>,
        ty: u32,
        typmod: i32,
    },
    Field {
        arg: Box<Node>,
        name: String,
        ty: u32,
        typmod: i32,
    },
    /// An aggregate call: its arguments, its ORDER BY, DISTINCT, `*`, FILTER.
    Agg {
        oid: u32,
        args: Vec<Node>,
        order: Vec<super::query::Sort>,
        distinct: bool,
        star: bool,
        filter: Option<Box<Node>>,
        variadic: bool,
        ty: u32,
    },
    /// GROUPING(…): its arguments as written.
    Grouping {
        args: Vec<Node>,
    },
    /// A window function call: its window by its place in the query's windows.
    Window {
        oid: u32,
        args: Vec<Node>,
        star: bool,
        filter: Option<Box<Node>>,
        win: usize,
        ty: u32,
    },
    /// A subquery as an expression: its kind, the left side, operator and right type it is
    /// compared by (ANY, ALL), and the query.
    SubLink {
        kind: super::query::SubKind,
        test: Option<(Box<Node>, u32, u32)>,
        query: Box<super::query::Query>,
        ty: u32,
        typmod: i32,
    },
    List(Vec<Node>),
    /// A statement's parameter, `$n`: its type as the statement's reading has settled it where it
    /// stands, unknown until a coercion settles it.
    Param {
        n: usize,
        ty: u32,
    },
    /// An argument a call gives by name, `name => arg`.
    Named {
        name: String,
        arg: Box<Node>,
    },
}

impl Node {
    /// Its type, as PostgreSQL's exprType gives it.
    pub(crate) fn ty(&self) -> u32 {
        use Node as N;
        match self {
            N::Named { arg, .. } => arg.ty(),
            N::Var { ty, .. }
            | N::DomainValue { ty, .. }
            | N::Const { ty, .. }
            | N::Op { ty, .. }
            | N::Func { ty, .. }
            | N::Relabel { ty, .. }
            | N::Io { ty, .. }
            | N::ArrayCoerce { ty, .. }
            | N::Domain { ty, .. }
            | N::NullIf { ty, .. }
            | N::Case { ty, .. }
            | N::CaseTest { ty, .. }
            | N::Array { ty, .. }
            | N::Row { ty, .. }
            | N::Coalesce { ty, .. }
            | N::MinMax { ty, .. }
            | N::SqlValue { ty, .. }
            | N::Subscript { ty, .. }
            | N::Field { ty, .. }
            | N::Agg { ty, .. }
            | N::Window { ty, .. }
            | N::SubLink { ty, .. }
            | N::Param { ty, .. } => *ty,
            N::Bool { .. }
            | N::NullTest { .. }
            | N::BoolTest { .. }
            | N::Distinct { .. }
            | N::AnyAll { .. } => cat::BOOL,
            N::Grouping { .. } => cat::INT4,
            N::Collate { arg, .. } => arg.ty(),
            N::List(_) => 0,
        }
    }

    /// Its typmod, as PostgreSQL's exprTypmod gives it.
    pub(crate) fn typmod(&self) -> i32 {
        use Node as N;
        match self {
            N::Var { typmod, .. }
            | N::DomainValue { typmod, .. }
            | N::Const { typmod, .. }
            | N::Relabel { typmod, .. }
            | N::ArrayCoerce { typmod, .. }
            | N::CaseTest { typmod, .. }
            | N::Subscript { typmod, .. }
            | N::Field { typmod, .. }
            | N::SubLink { typmod, .. } => *typmod,
            N::Func { .. } => self.length_coercion().unwrap_or(-1),
            N::SqlValue { typmod, .. } => typmod.unwrap_or(-1),
            N::Collate { arg, .. } => arg.typmod(),
            N::Case { whens, default, ty, .. } => {
                let m = default.typmod();
                if default.ty() == *ty
                    && m >= 0
                    && whens.iter().all(|(_, r)| r.ty() == *ty && r.typmod() == m)
                {
                    m
                } else {
                    -1
                }
            }
            N::Coalesce { args, ty } | N::MinMax { args, ty, .. } => common_typmod(args, *ty),
            N::NullIf { args, .. } => args.first().map_or(-1, Node::typmod),
            N::Array { elements, elem, multidims, .. } if !*multidims => {
                common_typmod(elements, *elem)
            }
            _ => -1,
        }
    }

    /// The typmod a length coercion applies, where it is one: a cast function's second argument.
    pub(crate) fn length_coercion(&self) -> Option<i32> {
        match self {
            Node::Func {
                args,
                form: Form::Explicit | Form::Implicit,
                ..
            } if args.len() >= 2 => match &args[1] {
                Node::Const {
                    ty: cat::INT4,
                    text: Some(t),
                    ..
                } => t.parse().ok(),
                _ => None,
            },
            _ => None,
        }
    }

    fn hide(&mut self) {
        match self {
            Node::Func { form, .. }
            | Node::Relabel { form, .. }
            | Node::Io { form, .. }
            | Node::ArrayCoerce { form, .. }
            | Node::Domain { form, .. } => *form = Form::Implicit,
            Node::Row { explicit, .. } => *explicit = false,
            _ => {}
        }
    }

    /// Whether it reads a column.
    pub(crate) fn has_vars(&self) -> bool {
        let mut found = false;
        self.walk(&mut |n| found |= matches!(n, Node::Var { .. }));
        found
    }

    pub(crate) fn walk(&self, f: &mut dyn FnMut(&Node)) {
        use Node as N;
        f(self);
        match self {
            N::Op { args, .. }
            | N::Func { args, .. }
            | N::Bool { args, .. }
            | N::Distinct { args, .. }
            | N::NullIf { args, .. }
            | N::AnyAll { args, .. }
            | N::Row { args, .. }
            | N::Coalesce { args, .. }
            | N::MinMax { args, .. }
            | N::Grouping { args }
            | N::List(args)
            | N::Array { elements: args, .. } => args.iter().for_each(|a| a.walk(f)),
            N::Agg { args, filter, order, .. } => {
                args.iter().for_each(|a| a.walk(f));
                order.iter().for_each(|s| s.expr.walk(f));
                if let Some(x) = filter {
                    x.walk(f);
                }
            }
            N::Window { args, filter, .. } => {
                args.iter().for_each(|a| a.walk(f));
                if let Some(x) = filter {
                    x.walk(f);
                }
            }
            N::SubLink { test: Some((l, _, _)), .. } => l.walk(f),
            N::Relabel { arg, .. }
            | N::Io { arg, .. }
            | N::ArrayCoerce { arg, .. }
            | N::Domain { arg, .. }
            | N::NullTest { arg, .. }
            | N::BoolTest { arg, .. }
            | N::Collate { arg, .. }
            | N::Field { arg, .. }
            | N::Named { arg, .. } => arg.walk(f),
            N::Case { arg, whens, default, .. } => {
                if let Some(a) = arg {
                    a.walk(f);
                }
                for (w, r) in whens {
                    w.walk(f);
                    r.walk(f);
                }
                default.walk(f);
            }
            N::Subscript { container, subs, .. } => {
                container.walk(f);
                for (a, b, _) in subs {
                    a.iter().chain(b.iter()).for_each(|x| x.walk(f));
                }
            }
            _ => {}
        }
    }
}

fn common_typmod(args: &[Node], ty: u32) -> i32 {
    let Some(first) = args.first() else {
        return -1;
    };
    let m = first.typmod();
    if m >= 0 && args.iter().all(|a| a.ty() == ty && a.typmod() == m) {
        m
    } else {
        -1
    }
}

/// A column an expression may name: its name, number, type and typmod.
#[derive(Clone, Debug)]
pub(crate) struct Column {
    pub name: String,
    pub attno: i16,
    pub ty: u32,
    pub typmod: i32,
}

/// What an expression is read against: the columns of its relation, and a domain's VALUE.
pub(crate) struct Analyzer<'c, 'a> {
    pub cat: &'c Cat<'a>,
    pub columns: Vec<Column>,
    pub value: Option<(u32, i32)>,
    /// The query level an expression of a query is read at; none for one over a single relation.
    pub scope: Option<&'c super::query::Scope<'c>>,
}

pub(crate) fn not_read(e: &impl std::fmt::Display) -> Error {
    Error::new(format!("{e} is not printed as PostgreSQL prints it"))
}

/// What brackets a literal's number where a fold writes it as a mark (`fold`).
const MARK: char = '\u{F8FF}';

thread_local! {
    /// The literals a fold is typing, by the number each mark carries: its text, and each type and
    /// text a coercion of it gave.
    static MARKED: RefCell<Option<Vec<Marked>>> = const { RefCell::new(None) };
}

/// A literal a fold is typing: its text, and each type and text a coercion of it gave.
#[derive(Default)]
pub(crate) struct Marked {
    pub text: String,
    pub typed: Vec<(u32, String)>,
}

/// The mark a fold writes in place of its `k`th literal.
pub(crate) fn mark(k: usize) -> String {
    format!("{MARK}{k}{MARK}")
}

/// The literal a fold's mark stands for, where a fold is typing it.
fn original(text: &str) -> Option<String> {
    let k: usize = text.strip_prefix(MARK)?.strip_suffix(MARK)?.parse().ok()?;
    MARKED.with(|m| m.borrow().as_ref().and_then(|m| m.get(k)).map(|x| x.text.clone()))
}

/// A text with each quoted mark of a fold in it written as the literal it stands for, quoted: what
/// the DDL's reader reads a statement's copy by, wherever it reads a literal's value.
pub(crate) fn unmarked(text: &str) -> Cow<'_, str> {
    if !text.contains(MARK) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&format!("'{MARK}")) {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let end = after[MARK.len_utf8()..].find(MARK).map(|e| e + 2 * MARK.len_utf8());
        match end.and_then(|e| original(&after[..e]).map(|o| (e, o))) {
            Some((e, o)) if after[e..].starts_with('\'') => {
                out.push_str(&super::print::literal(&o));
                rest = &after[e + 1..];
            }
            _ => {
                out.push('\'');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// An expression with each mark of a fold in it read as the literal it stands for; none where it
/// holds none.
pub(crate) fn unmarked_expr(e: &Expr) -> Option<Expr> {
    if !e.to_string().contains(MARK) {
        return None;
    }
    let mut e = e.clone();
    let _ = sqlparser::ast::visit_expressions_mut(&mut e, |x| {
        if let Expr::Value(v) = x {
            if let SqlValue::SingleQuotedString(s) = &mut v.value {
                if let Some(o) = original(s) {
                    *s = o;
                }
            }
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    Some(e)
}

/// The literals a fold types, held until `typed`.
pub(crate) struct Marking(());

impl Marking {
    pub(crate) fn of(texts: Vec<String>) -> Marking {
        let marked = texts
            .into_iter()
            .map(|text| Marked {
                text,
                typed: Vec::new(),
            })
            .collect();
        MARKED.with(|m| *m.borrow_mut() = Some(marked));
        Marking(())
    }

    /// The number of literals marked.
    pub(crate) fn len(&self) -> usize {
        MARKED.with(|m| m.borrow().as_ref().map_or(0, Vec::len))
    }

    /// Each literal and what its coercions gave it.
    pub(crate) fn typed(self) -> Vec<Marked> {
        MARKED.with(|m| m.borrow_mut().take()).unwrap_or_default()
    }
}

impl Drop for Marking {
    fn drop(&mut self) {
        MARKED.with(|m| *m.borrow_mut() = None);
    }
}

/// An unknown literal's text read as its type's input reads it, its output's text, and where it is
/// a fold's mark, the literal it stands for typed so.
fn typed_text(cat: &Cat, text: &str, ty: u32, typmod: i32) -> Result<String> {
    let k = text
        .strip_prefix(MARK)
        .and_then(|t| t.strip_suffix(MARK))
        .and_then(|n| n.parse::<usize>().ok());
    let (Some(k), Some(original)) = (k, original(text)) else {
        return super::constant_text(cat, text, ty, typmod);
    };
    let out = super::constant_text(cat, &original, ty, typmod)?;
    MARKED.with(|m| {
        if let Some(x) = m.borrow_mut().as_mut().and_then(|m| m.get_mut(k)) {
            x.typed.push((ty, out.clone()));
        }
    });
    Ok(out)
}

/// An identifier as PostgreSQL's scanner folds it.
pub(crate) fn folded(i: &sqlparser::ast::Ident) -> String {
    match i.quote_style {
        Some(_) => i.value.clone(),
        None => i.value.to_lowercase(),
    }
}

impl Analyzer<'_, '_> {
    /// An expression as the parser leaves it.
    pub(crate) fn expr(&self, e: &Expr) -> Result<Node> {
        use Expr as E;
        Ok(match e {
            E::Nested(x) => self.expr(x)?,
            E::Identifier(i) => self.column(&[folded(i)], e)?,
            E::CompoundIdentifier(parts) => {
                let names: Vec<String> = parts.iter().map(folded).collect();
                self.column(&names, e)?
            }
            E::Value(v) => self.value(&v.value, e)?,
            E::TypedString(t) => {
                let lit = match &t.value.value {
                    SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => s.clone(),
                    other => return Err(not_read(other)),
                };
                let (ty, typmod) = self.data_type(&t.data_type)?;
                let node = unknown(Some(lit));
                self.coerce_to_target(node, ty, typmod, Context::Explicit, Form::Explicit)?
            }
            E::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                expr,
                data_type,
                ..
            } => {
                let node = self.expr(expr)?;
                let (ty, typmod) = self.data_type(data_type)?;
                self.coerce_to_target(node, ty, typmod, Context::Explicit, Form::Explicit)?
            }
            E::UnaryOp { op, expr } => self.unary(*op, expr, e)?,
            E::BinaryOp { left, op, right } => self.binary(left, op, right, e)?,
            E::IsNull(x) | E::IsNotNull(x) => Node::NullTest {
                arg: Box::new(self.expr(x)?),
                not: matches!(e, E::IsNotNull(_)),
            },
            E::IsTrue(x)
            | E::IsNotTrue(x)
            | E::IsFalse(x)
            | E::IsNotFalse(x)
            | E::IsUnknown(x)
            | E::IsNotUnknown(x) => {
                let test = match e {
                    E::IsTrue(_) => " IS TRUE",
                    E::IsNotTrue(_) => " IS NOT TRUE",
                    E::IsFalse(_) => " IS FALSE",
                    E::IsNotFalse(_) => " IS NOT FALSE",
                    E::IsUnknown(_) => " IS UNKNOWN",
                    _ => " IS NOT UNKNOWN",
                };
                let arg = self.boolean(self.expr(x)?)?;
                Node::BoolTest {
                    arg: Box::new(arg),
                    test,
                }
            }
            E::IsDistinctFrom(a, b) | E::IsNotDistinctFrom(a, b) => {
                let d = Node::Distinct {
                    args: self.distinct(a, b)?,
                };
                match e {
                    E::IsDistinctFrom(..) => d,
                    _ => Node::Bool {
                        op: BoolOp::Not,
                        args: vec![d],
                    },
                }
            }
            E::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let (o1, o2, op) = match negated {
                    false => (">=", "<=", BoolOp::And),
                    true => ("<", ">", BoolOp::Or),
                };
                let a = self.op(o1, None, Some(self.expr(expr)?), self.expr(low)?)?;
                let b = self.op(o2, None, Some(self.expr(expr)?), self.expr(high)?)?;
                Node::Bool {
                    op,
                    args: vec![self.boolean(a)?, self.boolean(b)?],
                }
            }
            E::InList {
                expr,
                list,
                negated,
            } => self.in_list(expr, list, *negated)?,
            E::Like {
                negated,
                expr,
                pattern,
                escape_char,
                any: false,
            }
            | E::ILike {
                negated,
                expr,
                pattern,
                escape_char,
                any: false,
            } => {
                let op = match (matches!(e, E::ILike { .. }), negated) {
                    (false, false) => "~~",
                    (false, true) => "!~~",
                    (true, false) => "~~*",
                    (true, true) => "!~~*",
                };
                let mut pat = self.expr(pattern)?;
                if let Some(esc) = escape_char {
                    let esc = self.expr(esc)?;
                    pat = self.func_call("like_escape", None, vec![pat, esc], e)?;
                }
                self.op(op, None, Some(self.expr(expr)?), pat)?
            }
            E::SimilarTo {
                negated,
                expr,
                pattern,
                escape_char,
            } => {
                let mut args = vec![self.expr(pattern)?];
                if let Some(esc) = escape_char {
                    args.push(self.expr(esc)?);
                }
                let pat = self.func_call("similar_to_escape", None, args, e)?;
                let op = if *negated { "!~" } else { "~" };
                self.op(op, None, Some(self.expr(expr)?), pat)?
            }
            E::AnyOp { right, .. } | E::AllOp { right, .. } if matches!(right.as_ref(), E::Subquery(_)) => self.sublink(e)?,
            E::AnyOp {
                left,
                compare_op,
                right,
                ..
            }
            | E::AllOp {
                left,
                compare_op,
                right,
            } => {
                let name = binary_name(compare_op).ok_or_else(|| not_read(e))?;
                self.any_all(&name, matches!(e, E::AnyOp { .. }), self.expr(left)?, self.expr(right)?)?
            }
            E::Case {
                operand,
                conditions,
                else_result,
                ..
            } => self.case(operand.as_deref(), conditions, else_result.as_deref(), e)?,
            E::Array(a) => self.array(&a.elem, e)?,
            E::Tuple(items) => {
                let args = items.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
                Node::Row {
                    args,
                    ty: cat::RECORD,
                    explicit: false,
                }
            }
            E::Collate { expr, collation } => {
                let arg = self.expr(expr)?;
                let coll = self.collation_named(collation)?;
                Node::Collate {
                    arg: Box::new(arg),
                    coll,
                }
            }
            E::Position { expr, r#in } => {
                let args = vec![self.expr(r#in)?, self.expr(expr)?];
                self.sql_call("position", args, e)?
            }
            E::Substring {
                expr,
                substring_from,
                substring_for,
                special,
                ..
            } => {
                let mut args = vec![self.expr(expr)?];
                if let Some(f) = substring_from {
                    args.push(self.expr(f)?);
                } else {
                    args.push(self.value(&SqlValue::Number("1".into(), false), e)?);
                }
                if let Some(f) = substring_for {
                    args.push(self.expr(f)?);
                }
                if *special {
                    self.func_call("substring", None, args, e)?
                } else {
                    self.sql_call("substring", args, e)?
                }
            }
            E::Trim {
                expr,
                trim_where,
                trim_what,
                trim_characters,
            } => {
                use sqlparser::ast::TrimWhereField as W;
                let name = match trim_where {
                    None | Some(W::Both) => "btrim",
                    Some(W::Leading) => "ltrim",
                    Some(W::Trailing) => "rtrim",
                };
                let mut args = vec![self.expr(expr)?];
                if let Some(w) = trim_what {
                    args.push(self.expr(w)?);
                }
                if let Some(cs) = trim_characters {
                    for c in cs {
                        args.push(self.expr(c)?);
                    }
                }
                self.sql_call(name, args, e)?
            }
            E::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                let mut args = vec![
                    self.expr(expr)?,
                    self.expr(overlay_what)?,
                    self.expr(overlay_from)?,
                ];
                if let Some(f) = overlay_for {
                    args.push(self.expr(f)?);
                }
                self.sql_call("overlay", args, e)?
            }
            E::Extract { field, expr, .. } => {
                let f = field.to_string().to_lowercase();
                let f = f.trim_matches('"').to_string();
                let text = Node::Const {
                    ty: cat::TEXT,
                    typmod: -1,
                    text: Some(f),
                };
                self.sql_call("extract", vec![text, self.expr(expr)?], e)?
            }
            E::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                let args = vec![self.expr(time_zone)?, self.expr(timestamp)?];
                self.sql_call("timezone", args, e)?
            }
            E::Interval(iv)
                if iv.leading_field.is_none()
                    && iv.last_field.is_none()
                    && iv.leading_precision.is_none()
                    && iv.fractional_seconds_precision.is_none() =>
            {
                let lit = match iv.value.as_ref() {
                    E::Value(v) => match &v.value {
                        SqlValue::SingleQuotedString(s) => s.clone(),
                        _ => return Err(not_read(e)),
                    },
                    _ => return Err(not_read(e)),
                };
                self.coerce_to_target(unknown(Some(lit)), cat::INTERVAL, -1, Context::Explicit, Form::Explicit)?
            }
            E::Ceil { expr, .. } | E::Floor { expr, .. } => {
                let name = if matches!(e, E::Ceil { .. }) { "ceil" } else { "floor" };
                self.func_call(name, None, vec![self.expr(expr)?], e)?
            }
            E::Function(f) => match self.query_function(f, e)? {
                Some(n) => n,
                None => self.function(f, e)?,
            },
            E::Subquery(_) | E::Exists { .. } | E::InSubquery { .. } => self.sublink(e)?,
            E::CompoundFieldAccess { root, access_chain } => {
                let mut node = self.expr(root)?;
                let mut subs: Vec<(Option<Node>, Option<Node>, bool)> = Vec::new();
                for a in access_chain {
                    match a {
                        sqlparser::ast::AccessExpr::Subscript(s) => {
                            use sqlparser::ast::Subscript as S;
                            match s {
                                S::Index { index } => subs.push((None, Some(self.int(index)?), false)),
                                S::Slice {
                                    lower_bound,
                                    upper_bound,
                                    stride: None,
                                } => {
                                    let lo = lower_bound.as_ref().map(|x| self.int(x)).transpose()?;
                                    let hi = upper_bound.as_ref().map(|x| self.int(x)).transpose()?;
                                    subs.push((lo, hi, true));
                                }
                                _ => return Err(not_read(e)),
                            }
                        }
                        sqlparser::ast::AccessExpr::Dot(Expr::Identifier(i)) => {
                            if !subs.is_empty() {
                                node = self.subscripted(node, std::mem::take(&mut subs), e)?;
                            }
                            node = self.field(node, &folded(i), e)?;
                        }
                        _ => return Err(not_read(e)),
                    }
                }
                if !subs.is_empty() {
                    node = self.subscripted(node, subs, e)?;
                }
                node
            }
            other => return Err(not_read(other)),
        })
    }

    fn int(&self, e: &Expr) -> Result<Node> {
        let n = self.expr(e)?;
        self.coerce_to_target(n, cat::INT4, -1, Context::Assignment, Form::Implicit)
            .map_err(|_| Error::new("array subscript must have type integer"))
    }

    fn subscripted(&self, container: Node, subs: Vec<(Option<Node>, Option<Node>, bool)>, e: &Expr) -> Result<Node> {
        let t = container.ty();
        let base = self.cat.base(t);
        let elem = self.cat.element(base);
        if elem == 0 {
            return Err(not_read(e));
        }
        let slice = subs.iter().any(|s| s.2);
        let container = if base != t {
            Node::Relabel {
                arg: Box::new(container),
                ty: base,
                typmod: -1,
                form: Form::Implicit,
            }
        } else {
            container
        };
        let typmod = container.typmod();
        Ok(Node::Subscript {
            container: Box::new(container),
            subs,
            ty: if slice { base } else { elem },
            typmod,
        })
    }

    fn field(&self, node: Node, name: &str, e: &Expr) -> Result<Node> {
        let t = self.cat.base(node.ty());
        let relid = self.cat.ty(t).map_or(0, |x| x.relid);
        let fields = super::row_fields(self.cat, relid);
        let f = fields
            .into_iter()
            .find(|f| f.0 == name)
            .ok_or_else(|| Error::new(format!("column \"{name}\" not found in data type")))?;
        let _ = e;
        Ok(Node::Field {
            arg: Box::new(node),
            name: name.to_string(),
            ty: f.1,
            typmod: f.2,
        })
    }

    fn column(&self, names: &[String], e: &Expr) -> Result<Node> {
        if let Some(s) = self.scope {
            return s
                .column(names)
                .ok_or_else(|| Error::new(format!("column \"{}\" does not exist: {e}", names.join("."))));
        }
        let name = names.last().map(String::as_str).unwrap_or("");
        if names.len() == 1 && name == "value" {
            if let Some((ty, typmod)) = self.value {
                return Ok(Node::DomainValue { ty, typmod });
            }
        }
        match self.columns.iter().find(|c| c.name == name) {
            Some(c) => Ok(Node::Var {
                attno: c.attno,
                ty: c.ty,
                typmod: c.typmod,
                rel: 1,
                up: 0,
                syn: (1, c.attno),
            }),
            None => Err(Error::new(format!("column \"{name}\" does not exist: {e}"))),
        }
    }

    fn value(&self, v: &SqlValue, e: &impl std::fmt::Display) -> Result<Node> {
        Ok(match v {
            SqlValue::Number(s, _) => number(s, false),
            SqlValue::SingleQuotedString(s)
            | SqlValue::EscapedStringLiteral(s)
            | SqlValue::NationalStringLiteral(s) => unknown(Some(s.clone())),
            SqlValue::DollarQuotedString(d) => unknown(Some(d.value.clone())),
            SqlValue::Boolean(b) => Node::Const {
                ty: cat::BOOL,
                typmod: -1,
                text: Some(if *b { "t" } else { "f" }.into()),
            },
            SqlValue::Null => unknown(None),
            SqlValue::Placeholder(p) => super::params::reference(p).ok_or_else(|| not_read(e))??,
            SqlValue::HexStringLiteral(_) | SqlValue::SingleQuotedByteStringLiteral(_) => {
                return Err(not_read(e))
            }
            _ => return Err(not_read(e)),
        })
    }

    pub(crate) fn data_type(&self, dt: &DataType) -> Result<(u32, i32)> {
        let text = dt.to_string();
        let oid = crate::regs::reg_parse(crate::regs::RegKind::Type, &text, self.cat.release)?;
        let typmod = match crate::catalog::held_type(dt) {
            Ok(t) if self.cat.ty(oid).is_some_and(|x| x.typtype != 'd') => {
                crate::pgcatalog::typmod_of(t)
            }
            _ => -1,
        };
        Ok((oid, typmod))
    }

    fn collation_named(&self, name: &sqlparser::ast::ObjectName) -> Result<u32> {
        let n = crate::catalog::object_name(name);
        crate::regs::reg_parse(crate::regs::RegKind::Collation, &n, self.cat.release)
    }

    fn unary(&self, op: UnaryOperator, x: &Expr, e: &Expr) -> Result<Node> {
        let mut inner = x;
        while let Expr::Nested(i) = inner {
            inner = i;
        }
        match op {
            UnaryOperator::Not => {
                let arg = self.boolean(self.expr(x)?)?;
                Ok(Node::Bool {
                    op: BoolOp::Not,
                    args: vec![arg],
                })
            }
            UnaryOperator::Minus => {
                if let Expr::Value(v) = inner {
                    if let SqlValue::Number(s, _) = &v.value {
                        return Ok(number(s, true));
                    }
                }
                self.op("-", None, None, self.expr(x)?)
            }
            UnaryOperator::Plus => self.op("+", None, None, self.expr(x)?),
            UnaryOperator::BitwiseNot => self.op("~", None, None, self.expr(x)?),
            UnaryOperator::PGSquareRoot => self.op("|/", None, None, self.expr(x)?),
            UnaryOperator::PGCubeRoot => self.op("||/", None, None, self.expr(x)?),
            UnaryOperator::PGAbs => self.op("@", None, None, self.expr(x)?),
            _ => Err(not_read(e)),
        }
    }

    fn binary(&self, left: &Expr, op: &BinaryOperator, right: &Expr, e: &Expr) -> Result<Node> {
        match op {
            BinaryOperator::And | BinaryOperator::Or => {
                let which = if matches!(op, BinaryOperator::And) {
                    BoolOp::And
                } else {
                    BoolOp::Or
                };
                // The grammar folds a chain written on the left into one: (a AND b) AND c.
                let mut l = self.expr(left)?;
                let mut raw = left;
                while let Expr::Nested(i) = raw {
                    raw = i;
                }
                let folds = matches!(raw, Expr::BinaryOp { op: o, .. } if o == op);
                let r = self.boolean(self.expr(right)?)?;
                match (&mut l, folds) {
                    (Node::Bool { op: o, args }, true) if *o == which => {
                        args.push(r);
                        Ok(l)
                    }
                    _ => Ok(Node::Bool {
                        op: which,
                        args: vec![self.boolean(l)?, r],
                    }),
                }
            }
            _ => {
                let (name, schema) = match op {
                    BinaryOperator::PGCustomBinaryOperator(parts) => match parts.as_slice() {
                        [n] => (n.clone(), None),
                        [s, n] => (n.clone(), Some(self.schema_oid(s)?)),
                        _ => return Err(not_read(e)),
                    },
                    other => (binary_name(other).ok_or_else(|| not_read(e))?, None),
                };
                if let (Some(l), Some(r)) = (row_written(left), row_written(right)) {
                    return self.row_comparison(&name, schema, l, r);
                }
                self.op(&name, schema, Some(self.expr(left)?), self.expr(right)?)
            }
        }
    }

    /// make_row_comparison_op: a row compared with a row column by column, each pair by the
    /// operator its types find; `=` all of them, `<>` any, and an ordering the rows over the
    /// columns so coerced.
    fn row_comparison(&self, name: &str, schema: Option<u32>, l: &[Expr], r: &[Expr]) -> Result<Node> {
        let largs = l.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
        let rargs = r.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
        self.rows_compared(name, schema, largs, rargs)
    }

    /// The same, over the rows' fields as they are analysed.
    fn rows_compared(
        &self,
        name: &str,
        schema: Option<u32>,
        largs: Vec<Node>,
        rargs: Vec<Node>,
    ) -> Result<Node> {
        if largs.len() != rargs.len() {
            return Err(Error::new("unequal number of entries in row expressions"));
        }
        let mut ops = Vec::with_capacity(largs.len());
        for (a, b) in largs.into_iter().zip(rargs) {
            let o = self.op(name, schema, Some(a), b)?;
            if o.ty() != cat::BOOL {
                return Err(Error::new(format!(
                    "row comparison operator must yield type boolean, not type {}",
                    self.type_name(o.ty())
                )));
            }
            ops.push(o);
        }
        if ops.len() == 1 {
            return Ok(ops.remove(0));
        }
        match name {
            "=" => Ok(Node::Bool { op: BoolOp::And, args: ops }),
            "<>" => Ok(Node::Bool { op: BoolOp::Or, args: ops }),
            _ => {
                let (mut ls, mut rs) = (Vec::new(), Vec::new());
                for o in ops {
                    if let Node::Op { mut args, .. } = o {
                        rs.extend(args.pop());
                        ls.extend(args.pop());
                    }
                }
                let row = |args| Node::Row { args, ty: cat::RECORD, explicit: false };
                self.op(name, schema, Some(row(ls)), row(rs))
            }
        }
    }

    fn schema_oid(&self, s: &str) -> Result<u32> {
        crate::regs::reg_parse(crate::regs::RegKind::Namespace, s, self.cat.release)
    }

    /// An expression coerced to boolean, as a condition is.
    pub(crate) fn boolean(&self, n: Node) -> Result<Node> {
        let t = n.ty();
        if t == cat::BOOL {
            return Ok(n);
        }
        self.coerce_to_target(n, cat::BOOL, -1, Context::Assignment, Form::Implicit)
            .map_err(|_| Error::new("argument must be type boolean"))
    }

    /// An operator applied, as make_op applies it.
    pub(crate) fn op(&self, name: &str, schema: Option<u32>, left: Option<Node>, right: Node) -> Result<Node> {
        let rt = right.ty();
        let o = match &left {
            Some(l) => self.oper(name, schema, l.ty(), rt)?,
            None => self.left_oper(name, schema, rt)?,
        };
        let (mut args, actual, mut declared) = match left {
            Some(l) => {
                let lt = l.ty();
                (vec![l, right], vec![lt, rt], vec![o.left, o.right])
            }
            None => (vec![right], vec![rt], vec![o.right]),
        };
        let ty = self.generic(&actual, &mut declared, o.result)?;
        self.fn_arguments(&mut args, &actual, &declared)?;
        Ok(Node::Op { oid: o.oid, args, ty })
    }

    fn oper(&self, name: &str, schema: Option<u32>, l: u32, r: u32) -> Result<super::cat::Oper> {
        let candidates = self.cat.opers_named(name, 'b', schema);
        // The exact match, an unknown side read as the other's type, then as its base type.
        let (mut a, mut b, mut was_unknown) = (l, r, false);
        if a == cat::UNKNOWN && b != 0 {
            a = b;
            was_unknown = true;
        } else if b == cat::UNKNOWN && a != 0 {
            b = a;
            was_unknown = true;
        }
        if let Some(o) = candidates.iter().find(|o| o.left == a && o.right == b) {
            return Ok((*o).clone());
        }
        if was_unknown {
            let base = self.cat.base(a);
            if base != a {
                if let Some(o) = candidates.iter().find(|o| o.left == base && o.right == base) {
                    return Ok((*o).clone());
                }
            }
        }
        let list: Vec<Vec<u32>> = candidates.iter().map(|o| vec![o.left, o.right]).collect();
        let found = |not: &str| format!("operator {not}: {} {name} {}", self.type_name(l), self.type_name(r));
        match self.choose(&[l, r], &list) {
            Chosen::One(i) => Ok(candidates[i].clone()),
            Chosen::NotFound => Err(Error::new(found("does not exist"))),
            Chosen::NotUnique => Err(Error::new(found("is not unique"))),
        }
    }

    fn left_oper(&self, name: &str, schema: Option<u32>, r: u32) -> Result<super::cat::Oper> {
        let candidates = self.cat.opers_named(name, 'l', schema);
        if let Some(o) = candidates.iter().find(|o| o.right == r) {
            return Ok((*o).clone());
        }
        let list: Vec<Vec<u32>> = candidates.iter().map(|o| vec![o.right]).collect();
        let found = |not: &str| format!("operator {not}: {name} {}", self.type_name(r));
        match self.choose(&[r], &list) {
            Chosen::One(i) => Ok(candidates[i].clone()),
            Chosen::NotFound => Err(Error::new(found("does not exist"))),
            Chosen::NotUnique => Err(Error::new(found("is not unique"))),
        }
    }

    /// The operator a name and its arguments' types find, where one is found.
    pub(crate) fn op_lookup(&self, name: &str, kind: char, l: u32, r: u32) -> Option<u32> {
        match kind {
            'l' => self.left_oper(name, None, r).ok().map(|o| o.oid),
            _ => self.oper(name, None, l, r).ok().map(|o| o.oid),
        }
    }

    pub(crate) fn type_name(&self, t: u32) -> String {
        super::print::format_type(self.cat, t, -1)
    }

    /// What oper_select_candidate and func_select_candidate come to: the candidate chosen, none
    /// that takes the arguments, or several and none of them best.
    fn choose(&self, input: &[u32], candidates: &[Vec<u32>]) -> Chosen {
        let matching: Vec<usize> = (0..candidates.len())
            .filter(|&i| self.can_coerce_all(input, &candidates[i], Context::Implicit))
            .collect();
        match matching.len() {
            0 => Chosen::NotFound,
            1 => Chosen::One(matching[0]),
            _ => self
                .select_candidate(input, candidates, matching)
                .map_or(Chosen::NotUnique, Chosen::One),
        }
    }

    fn select_candidate(&self, input: &[u32], all: &[Vec<u32>], mut cands: Vec<usize>) -> Option<usize> {
        let n = input.len();
        let base: Vec<u32> = input
            .iter()
            .map(|&t| if t == cat::UNKNOWN { t } else { self.cat.base(t) })
            .collect();
        let unknowns = base.iter().filter(|&&t| t == cat::UNKNOWN).count();
        let keep_best = |cands: &[usize], score: &dyn Fn(&[u32]) -> usize| -> Vec<usize> {
            let best = cands.iter().map(|&c| score(&all[c])).max().unwrap_or(0);
            cands.iter().copied().filter(|&c| score(&all[c]) == best).collect()
        };
        cands = keep_best(&cands, &|args: &[u32]| {
            (0..n).filter(|&i| base[i] != cat::UNKNOWN && args[i] == base[i]).count()
        });
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        let slot: Vec<char> = base.iter().map(|&t| self.cat.category(t)).collect();
        cands = keep_best(&cands, &|args: &[u32]| {
            (0..n)
                .filter(|&i| {
                    base[i] != cat::UNKNOWN
                        && (args[i] == base[i]
                            || (self.cat.category(args[i]) == slot[i] && self.cat.preferred(args[i])))
                })
                .count()
        });
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        if unknowns == 0 {
            return None;
        }
        let mut slot_cat = vec![' '; n];
        let mut slot_pref = vec![false; n];
        let mut resolved = true;
        for i in 0..n {
            if base[i] != cat::UNKNOWN {
                continue;
            }
            let mut c0 = None;
            let mut conflict = false;
            for &c in &cands {
                let t = all[c][i];
                let (cc, pp) = (self.cat.category(t), self.cat.preferred(t));
                match c0 {
                    None => {
                        c0 = Some(cc);
                        slot_pref[i] = pp;
                    }
                    Some(x) if x == cc => slot_pref[i] |= pp,
                    Some(_) if cc == 'S' => {
                        c0 = Some(cc);
                        slot_pref[i] = pp;
                    }
                    Some(_) => conflict = true,
                }
            }
            slot_cat[i] = c0.unwrap_or(' ');
            if conflict && slot_cat[i] != 'S' {
                resolved = false;
                break;
            }
        }
        if resolved {
            let kept: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|&c| {
                    (0..n).all(|i| {
                        if base[i] != cat::UNKNOWN {
                            return true;
                        }
                        let t = all[c][i];
                        self.cat.category(t) == slot_cat[i]
                            && !(slot_pref[i] && !self.cat.preferred(t))
                    })
                })
                .collect();
            if !kept.is_empty() {
                cands = kept;
            }
            if cands.len() == 1 {
                return Some(cands[0]);
            }
        }
        if unknowns < n {
            let mut known = None;
            for &t in &base {
                if t == cat::UNKNOWN {
                    continue;
                }
                match known {
                    None => known = Some(t),
                    Some(k) if k != t => {
                        known = None;
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(k) = known {
                let all_k = vec![k; n];
                let ok: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&c| self.can_coerce_all(&all_k, &all[c], Context::Implicit))
                    .collect();
                if ok.len() == 1 {
                    return Some(ok[0]);
                }
            }
        }
        None
    }

    /// can_coerce_type over several arguments.
    pub(crate) fn can_coerce_all(&self, input: &[u32], target: &[u32], ctx: Context) -> bool {
        if input.len() != target.len() {
            return false;
        }
        let mut generics = false;
        for (&i, &t) in input.iter().zip(target) {
            if i == t {
                continue;
            }
            if i == cat::INTERNAL || t == cat::INTERNAL {
                return false;
            }
            if t == cat::ANY {
                continue;
            }
            if cat::polymorphic(t) {
                generics = true;
                continue;
            }
            if i == cat::UNKNOWN {
                continue;
            }
            if self.pathway(t, i, ctx).is_some() {
                continue;
            }
            if i == cat::RECORD && self.cat.complex(t) {
                continue;
            }
            if t == cat::RECORD && self.cat.complex(i) {
                continue;
            }
            if t == cat::RECORD_ARRAY && self.cat.complex(self.cat.element(i)) {
                continue;
            }
            return false;
        }
        if generics {
            let mut declared = target.to_vec();
            return self.generic(input, &mut declared, 0).is_ok();
        }
        true
    }

    /// find_coercion_pathway: how a value of `source` becomes one of `target` in a context.
    pub(crate) fn pathway(&self, target: u32, source: u32, ctx: Context) -> Option<Path> {
        let s = self.cat.base(source);
        let t = self.cat.base(target);
        if s == t {
            return Some(Path::Relabel);
        }
        if s == cat::INTERNAL || t == cat::INTERNAL {
            return None;
        }
        if let Some(c) = self.cat.cast(s, t) {
            let needs = match c.context {
                'i' => Context::Implicit,
                'a' => Context::Assignment,
                _ => Context::Explicit,
            };
            if ctx < needs {
                return None;
            }
            return match c.method {
                'f' => Some(Path::Func(c.func)),
                'i' => Some(Path::Io),
                _ => Some(Path::Relabel),
            };
        }
        if t != cat::OIDVECTOR && t != cat::INT2VECTOR {
            let (te, se) = (self.cat.element(t), self.cat.element(s));
            if te != 0 && se != 0 && self.pathway(te, se, ctx).is_some() {
                return Some(Path::Array);
            }
        }
        if ctx >= Context::Assignment && self.cat.category(t) == 'S' {
            return Some(Path::Io);
        }
        if ctx >= Context::Explicit && self.cat.category(s) == 'S' {
            return Some(Path::Io);
        }
        None
    }

    /// coerce_to_target_type: the coercion, then the typmod's.
    pub(crate) fn coerce_to_target(&self, node: Node, target: u32, typmod: i32, ctx: Context, form: Form) -> Result<Node> {
        let from = node.ty();
        if !self.can_coerce_all(&[from], &[target], ctx) {
            return Err(Error::new(format!(
                "cannot cast type {} to {}",
                self.type_name(from),
                self.type_name(target)
            )));
        }
        let (inner, collate) = match node {
            Node::Collate { arg, coll } => (*arg, Some(coll)),
            n => (n, None),
        };
        // The typmod's coercion hides the type's where the type's made a node of its own.
        let before = format!("{inner:?}");
        let coerced = self.coerce(inner, from, target, typmod, ctx, form)?;
        let hide = format!("{coerced:?}") != before && !matches!(coerced, Node::Const { .. });
        let out = self.coerce_typmod(coerced, target, typmod, ctx, form, hide)?;
        Ok(match collate {
            Some(coll) if self.cat.collatable(target) => Node::Collate {
                arg: Box::new(out),
                coll,
            },
            _ => out,
        })
    }

    /// coerce_type.
    pub(crate) fn coerce(&self, node: Node, from: u32, target: u32, typmod: i32, ctx: Context, form: Form) -> Result<Node> {
        if target == from {
            return Ok(node);
        }
        if matches!(
            target,
            cat::ANY | cat::ANYELEMENT | cat::ANYNONARRAY | cat::ANYCOMPATIBLE | cat::ANYCOMPATIBLENONARRAY
        ) {
            return Ok(node);
        }
        if matches!(
            target,
            cat::ANYARRAY
                | cat::ANYENUM
                | cat::ANYRANGE
                | cat::ANYMULTIRANGE
                | cat::ANYCOMPATIBLEARRAY
                | cat::ANYCOMPATIBLERANGE
                | cat::ANYCOMPATIBLEMULTIRANGE
        ) && from != cat::UNKNOWN
        {
            let base = self.cat.base(from);
            if base != from {
                return Ok(Node::Relabel {
                    arg: Box::new(node),
                    ty: base,
                    typmod: -1,
                    form,
                });
            }
            return Ok(node);
        }
        if from == cat::UNKNOWN {
            if let Node::Const { text, .. } = &node {
                let (base, base_typmod) = self.cat.base_and_typmod(target, typmod);
                let input_typmod = if base == cat::INTERVAL { base_typmod } else { -1 };
                let text = match text {
                    Some(t) => Some(typed_text(self.cat, t, base, input_typmod)?),
                    None => None,
                };
                let c = Node::Const {
                    ty: base,
                    typmod: input_typmod,
                    text,
                };
                return self.to_domain(c, base, base_typmod, target, ctx, form, false);
            }
            // A parameter not yet settled takes the type it is coerced to, as PostgreSQL's
            // variable_coerce_param_hook gives it.
            if let Node::Param { n, .. } = node {
                return super::params::settled(self.cat, n, target);
            }
        }
        if let Node::Collate { arg, coll } = node {
            let inner = self.coerce(*arg, from, target, typmod, ctx, form)?;
            return Ok(if self.cat.collatable(target) {
                Node::Collate {
                    arg: Box::new(inner),
                    coll,
                }
            } else {
                inner
            });
        }
        if let Some(path) = self.pathway(target, from, ctx) {
            let (base, base_typmod) = self.cat.base_and_typmod(target, typmod);
            if path != Path::Relabel {
                let r = self.build_coercion(node, path, base, base_typmod, ctx, form)?;
                return self.to_domain(r, base, base_typmod, target, ctx, form, true);
            }
            let r = self.to_domain(node.clone(), base, base_typmod, target, ctx, form, false)?;
            if matches!(r, Node::Domain { .. }) {
                return Ok(r);
            }
            return Ok(Node::Relabel {
                arg: Box::new(node),
                ty: target,
                typmod: -1,
                form,
            });
        }
        if (from == cat::RECORD && self.cat.complex(target)) || (target == cat::RECORD && self.cat.complex(from)) {
            return Ok(node);
        }
        Err(Error::new(format!(
            "cannot cast type {} to {}",
            self.type_name(from),
            self.type_name(target)
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn to_domain(&self, mut arg: Node, base: u32, base_typmod: i32, target: u32, ctx: Context, form: Form, hide: bool) -> Result<Node> {
        if base == target {
            return Ok(arg);
        }
        if hide {
            arg.hide();
        }
        let arg = self.coerce_typmod(arg, base, base_typmod, ctx, Form::Implicit, false)?;
        Ok(Node::Domain {
            arg: Box::new(arg),
            ty: target,
            form,
        })
    }

    /// coerce_type_typmod.
    fn coerce_typmod(&self, mut node: Node, target: u32, typmod: i32, ctx: Context, form: Form, hide: bool) -> Result<Node> {
        if typmod == node.typmod() {
            return Ok(node);
        }
        if hide {
            node.hide();
        }
        let func = if typmod < 0 {
            None
        } else {
            self.typmod_function(target)
        };
        match func {
            Some((f, array)) => {
                let path = if array { Path::Array } else { Path::Func(f) };
                self.build_coercion(node, path, target, typmod, ctx, form)
            }
            None => Ok(relabeled(node, target, typmod, form)),
        }
    }

    /// find_typmod_coercion_function: the length coercion of a type, of an array's element.
    fn typmod_function(&self, t: u32) -> Option<(u32, bool)> {
        let ty = self.cat.ty(t)?;
        let (t, array) = if self.cat.element(t) != 0 {
            (ty.elem, true)
        } else {
            (t, false)
        };
        let c = self.cat.cast(t, t)?;
        (c.func != 0).then_some((c.func, array))
    }

    fn build_coercion(&self, node: Node, path: Path, target: u32, typmod: i32, ctx: Context, form: Form) -> Result<Node> {
        Ok(match path {
            Path::Func(f) => {
                let nargs = self.cat.proc(f).map_or(1, |p| p.args.len());
                let mut args = vec![node];
                if nargs >= 2 {
                    args.push(Node::Const {
                        ty: cat::INT4,
                        typmod: -1,
                        text: Some(typmod.to_string()),
                    });
                }
                if nargs == 3 {
                    args.push(Node::Const {
                        ty: cat::BOOL,
                        typmod: -1,
                        text: Some(if ctx == Context::Explicit { "t" } else { "f" }.into()),
                    });
                }
                Node::Func {
                    oid: f,
                    args,
                    ty: target,
                    form,
                    variadic: false,
                }
            }
            Path::Array => {
                let from = node.ty();
                let (sb, sm) = self.cat.base_and_typmod(from, node.typmod());
                let test = Node::CaseTest {
                    ty: self.cat.element(sb),
                    typmod: sm,
                };
                let te = self.cat.element(target);
                let elem = self.coerce_to_target(test, te, typmod, ctx, form)?;
                Node::ArrayCoerce {
                    arg: Box::new(node),
                    ty: target,
                    typmod: elem.typmod(),
                    form,
                }
            }
            Path::Io => Node::Io {
                arg: Box::new(node),
                ty: target,
                form,
            },
            Path::Relabel => relabeled(node, target, typmod, form),
        })
    }

    /// enforce_generic_type_consistency: the polymorphic arguments' types settled, `declared`
    /// rewritten to them, and the result's type.
    pub(crate) fn generic(&self, actual: &[u32], declared: &mut [u32], result: u32) -> Result<u32> {
        if !declared.iter().any(|&d| cat::polymorphic(d)) && !cat::polymorphic(result) {
            return Ok(result);
        }
        let mut elem: Option<u32> = None;
        let mut range: Option<u32> = None;
        let mut multirange: Option<u32> = None;
        let mut array: Option<u32> = None;
        let agree = |slot: &mut Option<u32>, t: u32| -> Result<()> {
            match slot {
                None => {
                    *slot = Some(t);
                    Ok(())
                }
                Some(x) if *x == t => Ok(()),
                Some(_) => Err(Error::new("arguments declared polymorphic are not all alike")),
            }
        };
        let mut compat: Vec<u32> = Vec::new();
        // An argument declared a range, a multirange or an array is one, as
        // check_generic_type_consistency holds it.
        let kind = |t: u32| self.cat.ty(t).map_or(' ', |x| x.typtype);
        let of_kind = |b: u32, k: char, what: &str| -> Result<()> {
            match kind(b) == k {
                true => Ok(()),
                false => Err(Error::new(format!("argument declared {what} is not one"))),
            }
        };
        for (i, &d) in declared.iter().enumerate() {
            let a = actual[i];
            if a == cat::UNKNOWN {
                continue;
            }
            let b = self.cat.base(a);
            match d {
                cat::ANYELEMENT | cat::ANYNONARRAY | cat::ANYENUM => agree(&mut elem, a)?,
                cat::ANYARRAY => agree(&mut array, b)?,
                cat::ANYRANGE => {
                    of_kind(b, 'r', "anyrange")?;
                    agree(&mut range, b)?
                }
                cat::ANYMULTIRANGE => {
                    of_kind(b, 'm', "anymultirange")?;
                    agree(&mut multirange, b)?
                }
                cat::ANYCOMPATIBLE => compat.push(a),
                cat::ANYCOMPATIBLENONARRAY => {
                    if self.cat.element(b) != 0 {
                        return Err(Error::new("argument declared anycompatiblenonarray is an array"));
                    }
                    compat.push(a)
                }
                cat::ANYCOMPATIBLEARRAY => match self.cat.element(b) {
                    0 => return Err(Error::new("argument declared anycompatiblearray is not an array")),
                    e => compat.push(e),
                },
                cat::ANYCOMPATIBLERANGE | cat::ANYCOMPATIBLEMULTIRANGE => {
                    let (k, what) = match d {
                        cat::ANYCOMPATIBLERANGE => ('r', "anycompatiblerange"),
                        _ => ('m', "anycompatiblemultirange"),
                    };
                    of_kind(b, k, what)?;
                    if let Some(s) = super::range_subtype(self.cat, b) {
                        compat.push(s);
                    }
                }
                _ => {}
            }
        }
        if let Some(a) = array {
            let e = self.cat.element(a);
            if e == 0 {
                return Err(Error::new("argument declared anyarray is not an array"));
            }
            agree(&mut elem, e)?;
        }
        if let Some(r) = range {
            if let Some(s) = super::range_subtype(self.cat, r) {
                agree(&mut elem, s)?;
            }
        }
        // The element anynonarray and anyenum stand for is no array, and an enum.
        if let Some(e) = elem {
            let e = self.cat.base(e);
            if declared.contains(&cat::ANYNONARRAY) && self.cat.element(e) != 0 {
                return Err(Error::new("argument declared anynonarray is an array"));
            }
            if declared.contains(&cat::ANYENUM) && kind(e) != 'e' {
                return Err(Error::new("argument declared anyenum is not an enum"));
            }
        }
        let elem_t = match elem {
            Some(e) => Some(e),
            None if declared.iter().any(|&d| matches!(d, cat::ANYELEMENT | cat::ANYNONARRAY | cat::ANYARRAY | cat::ANYENUM)) => {
                // Every such argument unknown: read as text.
                Some(cat::TEXT)
            }
            None => None,
        };
        let compat_t = if compat.is_empty() {
            None
        } else {
            Some(self.common_type_of(&compat)?)
        };
        let map = |d: u32| -> u32 {
            match d {
                cat::ANYELEMENT | cat::ANYNONARRAY | cat::ANYENUM => elem_t.unwrap_or(d),
                cat::ANYARRAY => array.or_else(|| elem_t.map(|e| self.cat.array_of(e))).unwrap_or(d),
                cat::ANYRANGE => range.unwrap_or(d),
                cat::ANYMULTIRANGE => multirange.unwrap_or(d),
                cat::ANYCOMPATIBLE | cat::ANYCOMPATIBLENONARRAY => compat_t.unwrap_or(d),
                cat::ANYCOMPATIBLEARRAY => compat_t.map(|c| self.cat.array_of(c)).unwrap_or(d),
                _ => d,
            }
        };
        for d in declared.iter_mut() {
            *d = map(*d);
        }
        Ok(map(result))
    }

    /// make_fn_arguments: each argument coerced to its declared type.
    pub(crate) fn fn_arguments(&self, args: &mut [Node], actual: &[u32], declared: &[u32]) -> Result<()> {
        for (i, a) in args.iter_mut().enumerate() {
            if actual[i] != declared[i] {
                let n = std::mem::replace(a, Node::List(Vec::new()));
                *a = self.coerce(n, actual[i], declared[i], -1, Context::Implicit, Form::Implicit)?;
            }
        }
        Ok(())
    }

    /// select_common_type over types.
    pub(crate) fn common_type_of(&self, types: &[u32]) -> Result<u32> {
        let Some(&first) = types.first() else {
            return Ok(cat::TEXT);
        };
        if first != cat::UNKNOWN && types.iter().all(|&t| t == first) {
            return Ok(first);
        }
        let mut p = self.cat.base(first);
        let (mut pc, mut pp) = (self.cat.category(p), self.cat.preferred(p));
        for &t in &types[1..] {
            let n = self.cat.base(t);
            if n == cat::UNKNOWN || n == p {
                continue;
            }
            let (nc, np) = (self.cat.category(n), self.cat.preferred(n));
            if p == cat::UNKNOWN {
                (p, pc, pp) = (n, nc, np);
            } else if nc != pc {
                return Err(Error::new(format!(
                    "types {} and {} cannot be matched",
                    self.type_name(p),
                    self.type_name(n)
                )));
            } else if !pp
                && self.can_coerce_all(&[p], &[n], Context::Implicit)
                && !self.can_coerce_all(&[n], &[p], Context::Implicit)
            {
                (p, pc, pp) = (n, nc, np);
            }
        }
        Ok(if p == cat::UNKNOWN { cat::TEXT } else { p })
    }

    pub(crate) fn common_type(&self, nodes: &[Node]) -> Result<u32> {
        self.common_type_of(&nodes.iter().map(Node::ty).collect::<Vec<_>>())
    }

    /// coerce_to_common_type.
    pub(crate) fn to_common(&self, n: Node, t: u32) -> Result<Node> {
        let from = n.ty();
        if from == t {
            return Ok(n);
        }
        self.coerce(n, from, t, -1, Context::Implicit, Form::Implicit)
    }

    fn distinct(&self, a: &Expr, b: &Expr) -> Result<Vec<Node>> {
        match self.op("=", None, Some(self.expr(a)?), self.expr(b)?)? {
            Node::Op { args, .. } => Ok(args),
            _ => unreachable!(),
        }
    }

    fn any_all(&self, name: &str, any: bool, left: Node, right: Node) -> Result<Node> {
        let lt = left.ty();
        let at = right.ty();
        let rt = if at == cat::UNKNOWN {
            cat::UNKNOWN
        } else {
            let e = self.cat.element(self.cat.base(at));
            if e == 0 {
                return Err(Error::new("op ANY/ALL (array) requires array on right side"));
            }
            e
        };
        let o = self.oper(name, None, lt, rt)?;
        let actual = [lt, rt];
        let mut declared = [o.left, o.right];
        self.generic(&actual, &mut declared, o.result)?;
        let right_decl = if cat::polymorphic(o.right) {
            at
        } else {
            self.cat.array_of(declared[1])
        };
        let mut args = vec![left, right];
        self.fn_arguments(&mut args, &[lt, at], &[declared[0], right_decl])?;
        Ok(Node::AnyAll { oid: o.oid, any, args })
    }

    fn in_list(&self, expr: &Expr, list: &[Expr], negated: bool) -> Result<Node> {
        let name = if negated { "<>" } else { "=" };
        let any = !negated;
        let lexpr = self.expr(expr)?;
        let rexprs = list.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
        let (rvars, rnonvars): (Vec<Node>, Vec<Node>) = rexprs.iter().cloned().partition(Node::has_vars);
        let mut result: Option<Node> = None;
        let mut rest = rexprs;
        if rnonvars.len() > 1 {
            let mut all = vec![lexpr.clone()];
            all.extend(rnonvars.iter().cloned());
            if let Ok(scalar) = self.common_type(&all) {
                let ok = all.iter().all(|n| self.can_coerce_all(&[n.ty()], &[scalar], Context::Implicit));
                let array = if ok && scalar != cat::RECORD {
                    self.cat.array_of(scalar)
                } else {
                    0
                };
                if array != 0 {
                    let elements = rnonvars
                        .into_iter()
                        .map(|n| self.to_common(n, scalar))
                        .collect::<Result<Vec<_>>>()?;
                    let arr = Node::Array {
                        elements,
                        ty: array,
                        elem: scalar,
                        multidims: false,
                    };
                    result = Some(self.any_all(name, any, lexpr.clone(), arr)?);
                    rest = rvars;
                }
            }
        }
        for r in rest {
            // A row against a row, field by field (`row_comparison`).
            let cmp = match (&lexpr, r) {
                (Node::Row { args: l, .. }, Node::Row { args: r, .. }) => {
                    self.rows_compared(name, None, l.clone(), r)?
                }
                (_, r) => self.op(name, None, Some(lexpr.clone()), r)?,
            };
            let cmp = self.boolean(cmp)?;
            result = Some(match result {
                None => cmp,
                Some(prev) => Node::Bool {
                    op: if any { BoolOp::Or } else { BoolOp::And },
                    args: vec![prev, cmp],
                },
            });
        }
        result.ok_or_else(|| Error::new("IN with no list"))
    }

    fn case(
        &self,
        operand: Option<&Expr>,
        conditions: &[sqlparser::ast::CaseWhen],
        else_result: Option<&Expr>,
        e: &Expr,
    ) -> Result<Node> {
        let arg = match operand {
            Some(o) => {
                let a = self.expr(o)?;
                // An argument of unknown type is read as text.
                let a = if a.ty() == cat::UNKNOWN {
                    self.coerce(a, cat::UNKNOWN, cat::TEXT, -1, Context::Implicit, Form::Implicit)?
                } else {
                    a
                };
                Some(a)
            }
            None => None,
        };
        let mut whens = Vec::new();
        let mut results = Vec::new();
        for c in conditions {
            let cond = match &arg {
                Some(a) => {
                    let test = Node::CaseTest {
                        ty: a.ty(),
                        typmod: a.typmod(),
                    };
                    self.op("=", None, Some(test), self.expr(&c.condition)?)?
                }
                None => self.expr(&c.condition)?,
            };
            whens.push(self.boolean(cond)?);
            results.push(self.expr(&c.result)?);
        }
        let default = match else_result {
            Some(x) => self.expr(x)?,
            None => unknown(None),
        };
        let mut all = results.clone();
        all.push(default.clone());
        let ty = self.common_type(&all)?;
        let results = results
            .into_iter()
            .map(|r| self.to_common(r, ty))
            .collect::<Result<Vec<_>>>()?;
        let default = self.to_common(default, ty)?;
        let _ = e;
        Ok(Node::Case {
            arg: arg.map(Box::new),
            whens: whens.into_iter().zip(results).collect(),
            default: Box::new(default),
            ty,
        })
    }

    fn array(&self, elems: &[Expr], e: &Expr) -> Result<Node> {
        let nodes = elems.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
        let multidims = elems
            .iter()
            .any(|x| matches!(x, Expr::Array(_)));
        if nodes.is_empty() {
            return Err(Error::new(format!("cannot determine type of empty array: {e}")));
        }
        if multidims {
            let ty = self.common_type(&nodes)?;
            let elements = nodes
                .into_iter()
                .map(|n| self.to_common(n, ty))
                .collect::<Result<Vec<_>>>()?;
            return Ok(Node::Array {
                elements,
                ty,
                elem: self.cat.element(ty),
                multidims: true,
            });
        }
        let elem = self.common_type(&nodes)?;
        let elements = nodes
            .into_iter()
            .map(|n| self.to_common(n, elem))
            .collect::<Result<Vec<_>>>()?;
        Ok(Node::Array {
            elements,
            ty: self.cat.array_of(elem),
            elem,
            multidims: false,
        })
    }

    fn function(&self, f: &sqlparser::ast::Function, e: &Expr) -> Result<Node> {
        let parts: Vec<String> = f
            .name
            .0
            .iter()
            .filter_map(|p| p.as_ident().map(folded))
            .collect();
        let (schema, name) = match parts.as_slice() {
            [n] => (None, n.clone()),
            [s, n] => (Some(s.clone()), n.clone()),
            _ => return Err(not_read(e)),
        };
        // Each argument, and its name where it is given by one.
        let mut names: Vec<Option<String>> = Vec::new();
        let args: Vec<&Expr> = match &f.args {
            FunctionArguments::None => {
                if schema.is_none() {
                    if let Some(n) = self.sql_value(&name, None) {
                        return Ok(n);
                    }
                }
                Vec::new()
            }
            FunctionArguments::List(l) => {
                if l.duplicate_treatment.is_some() || !l.clauses.is_empty() {
                    return Err(not_read(e));
                }
                let mut out = Vec::with_capacity(l.args.len());
                for a in &l.args {
                    let (named, x) = match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => (None, x),
                        FunctionArg::ExprNamed {
                            name: Expr::Identifier(i),
                            arg: FunctionArgExpr::Expr(x),
                            operator: sqlparser::ast::FunctionArgOperator::RightArrow,
                        } => (Some(folded(i)), x),
                        _ => return Err(not_read(e)),
                    };
                    match &named {
                        Some(n) if names.iter().flatten().any(|m| m == n) => {
                            return Err(Error::new(format!("argument name \"{n}\" used more than once")))
                        }
                        None if names.iter().any(Option::is_some) => {
                            return Err(Error::new("positional argument cannot follow named argument"))
                        }
                        _ => {}
                    }
                    names.push(named);
                    out.push(x);
                }
                out
            }
            FunctionArguments::Subquery(_) => return Err(not_read(e)),
        };
        if names.iter().any(Option::is_some) {
            if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() {
                return Err(not_read(e));
            }
            let nodes = args.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
            let schema = schema.map(|s| self.schema_oid(&s)).transpose()?;
            return self.func_call_named(&name, schema, nodes, names);
        }
        if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() {
            return Err(not_read(e));
        }
        if schema.is_none() {
            match name.as_str() {
                "current_time" | "current_timestamp" | "localtime" | "localtimestamp" if args.len() == 1 => {
                    let p = match args[0] {
                        Expr::Value(v) => match &v.value {
                            SqlValue::Number(s, _) => s.parse::<i32>().ok(),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(n) = self.sql_value(&name, p) {
                        return Ok(n);
                    }
                }
                "coalesce" | "greatest" | "least" | "nullif" => {
                    let nodes = args.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
                    return self.keyword_call(&name, nodes);
                }
                _ => {}
            }
        }
        let nodes = args.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
        let schema = schema.map(|s| self.schema_oid(&s)).transpose()?;
        self.func_call(&name, schema, nodes, e)
    }

    fn keyword_call(&self, name: &str, nodes: Vec<Node>) -> Result<Node> {
        match name {
            "nullif" => {
                let [a, b]: [Node; 2] = nodes
                    .try_into()
                    .map_err(|_| Error::new("NULLIF takes two arguments"))?;
                let ty = a.ty();
                match self.op("=", None, Some(a), b)? {
                    Node::Op { args, .. } => Ok(Node::NullIf { args, ty }),
                    _ => unreachable!(),
                }
            }
            _ => {
                let ty = self.common_type(&nodes)?;
                let args = nodes
                    .into_iter()
                    .map(|n| self.to_common(n, ty))
                    .collect::<Result<Vec<_>>>()?;
                Ok(match name {
                    "coalesce" => Node::Coalesce { args, ty },
                    g => Node::MinMax {
                        greatest: g == "greatest",
                        args,
                        ty,
                    },
                })
            }
        }
    }

    fn sql_value(&self, name: &str, typmod: Option<i32>) -> Option<Node> {
        let (n, ty): (&'static str, u32) = match name {
            "current_date" => ("CURRENT_DATE", 1082),
            "current_time" => ("CURRENT_TIME", 1266),
            "current_timestamp" => ("CURRENT_TIMESTAMP", 1184),
            "localtime" => ("LOCALTIME", 1083),
            "localtimestamp" => ("LOCALTIMESTAMP", 1114),
            "current_role" => ("CURRENT_ROLE", 19),
            "current_user" => ("CURRENT_USER", 19),
            "user" => ("USER", 19),
            "session_user" => ("SESSION_USER", 19),
            "current_catalog" => ("CURRENT_CATALOG", 19),
            "current_schema" => ("CURRENT_SCHEMA", 19),
            _ => return None,
        };
        if typmod.is_some() && ty == 19 {
            return None;
        }
        Some(Node::SqlValue { name: n, typmod, ty })
    }

    /// A call written in SQL's own spelling: its function resolved as a call is.
    fn sql_call(&self, name: &str, args: Vec<Node>, e: &Expr) -> Result<Node> {
        let mut n = self.func_call(name, Some(11), args, e)?;
        if let Node::Func { form, .. } = &mut n {
            *form = Form::Sql;
        }
        Ok(n)
    }

    /// A function called with arguments given by name, as ParseFuncOrColumn resolves it: each
    /// candidate whose parameters take the arguments by place, then by name at a place none took,
    /// every parameter left one with a DEFAULT; the arguments coerced in the call's order, each
    /// given by name kept with its name.
    fn func_call_named(&self, name: &str, schema: Option<u32>, args: Vec<Node>, names: Vec<Option<String>>) -> Result<Node> {
        let actual: Vec<u32> = args.iter().map(Node::ty).collect();
        let candidates = self.cat.procs_named(name, schema);
        // Each candidate that matches: its declared types in the call's order, and the parameter
        // each argument takes.
        let mut forms: Vec<(usize, Vec<u32>, Vec<usize>)> = Vec::new();
        for (i, p) in candidates.iter().enumerate() {
            if p.argnames.is_empty() || actual.len() > p.args.len() {
                continue;
            }
            let mut taken = vec![false; p.args.len()];
            let mut at = Vec::with_capacity(names.len());
            for (k, n) in names.iter().enumerate() {
                let j = match n {
                    None => Some(k),
                    Some(n) => p.argnames.iter().position(|x| x == n),
                };
                match j {
                    Some(j) if j < p.args.len() && !taken[j] => {
                        taken[j] = true;
                        at.push(j);
                    }
                    _ => break,
                }
            }
            let required = p.args.len() - p.nargdefaults.min(p.args.len());
            if at.len() != names.len() || (0..required).any(|j| !taken[j]) {
                continue;
            }
            forms.push((i, at.iter().map(|&j| p.args[j]).collect(), at));
        }
        let shown = || {
            names
                .iter()
                .zip(&actual)
                .map(|(n, &t)| match n {
                    Some(n) => format!("{n} => {}", self.type_name(t)),
                    None => self.type_name(t),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let chosen = match forms.iter().position(|f| f.1 == actual) {
            Some(c) => Some(c),
            None => {
                let lists: Vec<Vec<u32>> = forms.iter().map(|f| f.1.clone()).collect();
                match self.choose(&actual, &lists) {
                    Chosen::One(c) => Some(c),
                    Chosen::NotFound => None,
                    Chosen::NotUnique => {
                        return Err(Error::new(format!("function {name}({}) is not unique", shown())))
                    }
                }
            }
        };
        let Some(c) = chosen else {
            return Err(Error::new(format!("function {name}({}) does not exist", shown())));
        };
        let (pi, mut declared, _) = forms[c].clone();
        let p = candidates[pi];
        let ty = self.generic(&actual, &mut declared, p.result)?;
        let mut args = args;
        self.fn_arguments(&mut args, &actual, &declared)?;
        let args = args
            .into_iter()
            .zip(names)
            .map(|(a, n)| match n {
                Some(name) => Node::Named {
                    name,
                    arg: Box::new(a),
                },
                None => a,
            })
            .collect();
        Ok(Node::Func {
            oid: p.oid,
            args,
            ty,
            form: Form::Call,
            variadic: false,
        })
    }

    /// A function called, as ParseFuncOrColumn resolves and coerces it.
    pub(crate) fn func_call(&self, name: &str, schema: Option<u32>, args: Vec<Node>, e: &Expr) -> Result<Node> {
        let actual: Vec<u32> = args.iter().map(Node::ty).collect();
        let candidates = self.cat.procs_named(name, schema);
        // Each candidate as its arguments are matched: variadic ones spread, defaults dropped.
        let mut forms: Vec<(usize, Vec<u32>, bool)> = Vec::new();
        for (i, p) in candidates.iter().enumerate() {
            let n = p.args.len();
            if actual.len() == n {
                forms.push((i, p.args.clone(), false));
            }
            if p.variadic != 0 && actual.len() >= n && n > 0 {
                let mut a = p.args[..n - 1].to_vec();
                let elem = if p.variadic == cat::ANY { cat::ANY } else { p.variadic };
                while a.len() < actual.len() {
                    a.push(elem);
                }
                if a.len() == actual.len() && !(actual.len() == n && forms.iter().any(|f| f.0 == i && f.1 == a)) {
                    forms.push((i, a, true));
                }
            }
            if p.nargdefaults > 0 && actual.len() < n && actual.len() >= n - p.nargdefaults {
                forms.push((i, p.args[..actual.len()].to_vec(), false));
            }
        }
        // The exact match first.
        let chosen = forms.iter().position(|f| f.1 == actual);
        let chosen = match chosen {
            Some(c) => Some(c),
            None => {
                if args.len() == 1 && schema.is_none() {
                    if let Some(t) = self.cat.type_named(None, name) {
                        let coercion = match &args[0] {
                            Node::Const { ty: cat::UNKNOWN, .. } => true,
                            _ => matches!(
                                self.pathway(t, actual[0], Context::Explicit),
                                Some(Path::Relabel | Path::Io)
                            ),
                        };
                        if coercion {
                            let a = args.into_iter().next().unwrap();
                            return self.coerce_to_target(a, t, -1, Context::Explicit, Form::Call);
                        }
                    }
                }
                let lists: Vec<Vec<u32>> = forms.iter().map(|f| f.1.clone()).collect();
                match self.choose(&actual, &lists) {
                    Chosen::One(c) => Some(c),
                    Chosen::NotFound => None,
                    Chosen::NotUnique => {
                        return Err(Error::new(format!(
                            "function {name}({}) is not unique",
                            actual.iter().map(|&t| self.type_name(t)).collect::<Vec<_>>().join(", ")
                        )))
                    }
                }
            }
        };
        let Some(c) = chosen else {
            return Err(Error::new(format!(
                "function {name}({}) does not exist",
                actual.iter().map(|&t| self.type_name(t)).collect::<Vec<_>>().join(", ")
            )));
        };
        let (pi, mut declared, spread) = forms[c].clone();
        let p = candidates[pi];
        let ty = self.generic(&actual, &mut declared, p.result)?;
        let mut args = args;
        self.fn_arguments(&mut args, &actual, &declared)?;
        // The spread arguments gathered into the array the function takes: a VARIADIC call.
        let variadic = spread && p.variadic != cat::ANY;
        if variadic {
            let fixed = p.args.len() - 1;
            let tail: Vec<Node> = args.split_off(fixed);
            let elem = declared.get(fixed).copied().unwrap_or(p.variadic);
            args.push(Node::Array {
                elements: tail,
                ty: self.cat.array_of(elem),
                elem,
                multidims: false,
            });
        }
        let _ = e;
        Ok(Node::Func {
            oid: p.oid,
            args,
            ty,
            form: Form::Call,
            variadic,
        })
    }
}

/// The values of a row written out, `(a, b)`, with its parentheses taken off.
fn row_written(e: &Expr) -> Option<&[Expr]> {
    let mut x = e;
    while let Expr::Nested(i) = x {
        x = i;
    }
    match x {
        Expr::Tuple(v) => Some(v),
        _ => None,
    }
}

/// What a candidate's selection comes to.
enum Chosen {
    One(usize),
    NotFound,
    NotUnique,
}

/// How a coercion converts: by its type alone, a function, element by element, or through text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Path {
    Relabel,
    Func(u32),
    Array,
    Io,
}

/// applyRelabelType: a Const relabeled in place, a relabel over a relabel collapsed.
fn relabeled(node: Node, ty: u32, typmod: i32, form: Form) -> Node {
    let mut node = node;
    while let Node::Relabel { arg, .. } = node {
        node = *arg;
    }
    match node {
        Node::Const { text, .. } => Node::Const { ty, typmod, text },
        n if n.ty() == ty && n.typmod() == typmod => n,
        n => Node::Relabel {
            arg: Box::new(n),
            ty,
            typmod,
            form,
        },
    }
}

fn unknown(text: Option<String>) -> Node {
    Node::Const {
        ty: cat::UNKNOWN,
        typmod: -1,
        text,
    }
}

/// A number written out, as make_const types it: an integer of 32 bits, then of 64, else a numeric.
fn number(s: &str, negative: bool) -> Node {
    let clean: String = s.chars().filter(|&c| c != '_').collect();
    let radix = |p: &str, r: u32| -> Option<i128> {
        let lower = clean.to_lowercase();
        lower.strip_prefix(p).and_then(|d| i128::from_str_radix(d, r).ok())
    };
    let int = radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
        .or_else(|| {
            clean
                .bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| clean.parse::<i128>().ok())
                .flatten()
        });
    if let Some(v) = int {
        let v = if negative { -v } else { v };
        if let Ok(x) = i32::try_from(v) {
            return Node::Const {
                ty: cat::INT4,
                typmod: -1,
                text: Some(x.to_string()),
            };
        }
        if let Ok(x) = i64::try_from(v) {
            return Node::Const {
                ty: cat::INT8,
                typmod: -1,
                text: Some(x.to_string()),
            };
        }
    }
    Node::Const {
        ty: cat::NUMERIC,
        typmod: -1,
        text: Some(numeric_text(&clean, negative)),
    }
}

/// A decimal literal as numeric_in reads it and numeric_out writes it: its digits after the point
/// those written, less the exponent, never fewer than none; no sign on zero.
fn numeric_text(lit: &str, negative: bool) -> String {
    let (mantissa, exp) = match lit.find(['e', 'E']) {
        Some(i) => (&lit[..i], lit[i + 1..].parse::<i64>().unwrap_or(0)),
        None => (lit, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: String = int.chars().chain(frac.chars()).collect();
    let scale = (frac.len() as i64 - exp).max(0);
    let point = int.len() as i64 + exp;
    let mut whole = String::new();
    let mut part = String::new();
    for (i, c) in digits.chars().enumerate() {
        if (i as i64) < point {
            whole.push(c);
        } else {
            part.push(c);
        }
    }
    for _ in digits.len() as i64..point {
        whole.push('0');
    }
    if point < 0 {
        part = "0".repeat((-point) as usize) + &part;
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let mut part: String = part.chars().take(scale as usize).collect();
    while (part.len() as i64) < scale {
        part.push('0');
    }
    let zero = whole == "0" && part.chars().all(|c| c == '0');
    let sign = if negative && !zero { "-" } else { "" };
    if part.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{part}")
    }
}

/// The operator's name a binary operator of sqlparser's is written with.
pub(crate) fn binary_name(op: &BinaryOperator) -> Option<String> {
    use BinaryOperator as B;
    Some(
        match op {
            B::Plus => "+",
            B::Minus => "-",
            B::Multiply => "*",
            B::Divide => "/",
            B::Modulo => "%",
            B::StringConcat => "||",
            B::Gt => ">",
            B::Lt => "<",
            B::GtEq => ">=",
            B::LtEq => "<=",
            B::Eq => "=",
            B::NotEq => "<>",
            B::BitwiseOr => "|",
            B::BitwiseAnd => "&",
            B::PGBitwiseXor => "#",
            B::PGBitwiseShiftLeft => "<<",
            B::PGBitwiseShiftRight => ">>",
            B::PGExp => "^",
            B::PGRegexMatch => "~",
            B::PGRegexIMatch => "~*",
            B::PGRegexNotMatch => "!~",
            B::PGRegexNotIMatch => "!~*",
            B::PGLikeMatch => "~~",
            B::PGILikeMatch => "~~*",
            B::PGNotLikeMatch => "!~~",
            B::PGNotILikeMatch => "!~~*",
            B::PGStartsWith => "^@",
            B::Arrow => "->",
            B::LongArrow => "->>",
            B::HashArrow => "#>",
            B::HashLongArrow => "#>>",
            B::AtAt => "@@",
            B::AtArrow => "@>",
            B::ArrowAt => "<@",
            B::HashMinus => "#-",
            B::AtQuestion => "@?",
            B::Question => "?",
            B::QuestionAnd => "?&",
            B::QuestionPipe => "?|",
            B::PGOverlap => "&&",
            B::PGCustomBinaryOperator(p) if p.len() == 1 => return Some(p[0].clone()),
            _ => return None,
        }
        .to_string(),
    )
}
