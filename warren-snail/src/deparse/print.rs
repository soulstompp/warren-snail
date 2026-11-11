//! An expression printed as PostgreSQL 18's ruleutils.c prints it: get_rule_expr and the
//! functions beside it, under the flags that choose parentheses and indentation.

use super::analyze::{BoolOp, Form, Node};
use super::cat::{self, Cat};
use crate::regs::RegKind;
use crate::Result;

/// The indentation ruleutils.c steps by.
pub(crate) const INDENT_STD: i32 = 8;
pub(crate) const INDENT_VAR: i32 = 4;
const INDENT_LIMIT: i32 = 40;

/// What a print is under: parentheses only where needed, indentation, and the names of the
/// relations its columns are read from.
pub(crate) struct Printer<'c, 'a> {
    pub cat: &'c Cat<'a>,
    pub paren: bool,
    pub indent: bool,
    pub level: i32,
    pub buf: String,
    /// Each relation's columns by number, and the name a column is qualified with where several
    /// relations are read (None where it is not qualified).
    pub relations: Vec<(Vec<String>, Option<String>)>,
    /// The query levels around an expression of a query; empty for one over a single relation.
    pub q: super::viewdef::Ctx,
}

/// A name as quote_identifier writes it.
pub(crate) fn quote(s: &str) -> String {
    crate::read::calls::quote_ident(s)
}

/// A literal as simple_quote_literal writes it under standard_conforming_strings.
pub(crate) fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn char_typmod(t: i32) -> String {
    if t > 4 {
        format!("({})", t - 4)
    } else {
        String::new()
    }
}

fn interval_typmod(t: i32) -> String {
    const MONTH: i32 = 1 << 1;
    const YEAR: i32 = 1 << 2;
    const DAY: i32 = 1 << 3;
    const HOUR: i32 = 1 << 10;
    const MINUTE: i32 = 1 << 11;
    const SECOND: i32 = 1 << 12;
    let (fields, precision) = ((t >> 16) & 0x7fff, t & 0xffff);
    let f = match fields {
        YEAR => " year",
        MONTH => " month",
        DAY => " day",
        HOUR => " hour",
        MINUTE => " minute",
        SECOND => " second",
        _ if fields == YEAR | MONTH => " year to month",
        _ if fields == DAY | HOUR => " day to hour",
        _ if fields == DAY | HOUR | MINUTE => " day to minute",
        _ if fields == DAY | HOUR | MINUTE | SECOND => " day to second",
        _ if fields == HOUR | MINUTE => " hour to minute",
        _ if fields == HOUR | MINUTE | SECOND => " hour to second",
        _ if fields == MINUTE | SECOND => " minute to second",
        _ => "",
    };
    if precision != 0xffff {
        format!("{f}({precision})")
    } else {
        f.into()
    }
}

/// A type's name as format_type_be writes it: as format_type_with_typemod with no typmod, but `bit`
/// and `character` bare where that form quotes them or writes bpchar.
pub(crate) fn format_type_be(cat: &Cat, oid: u32) -> String {
    match oid {
        1560 => "bit".into(),
        1042 => "character".into(),
        _ => format_type(cat, oid, -1),
    }
}

/// A type's name as format_type_with_typemod writes it: the SQL standard's names, a modifier as
/// the type writes it, an array's element's name with `[]`, a name the search path does not find
/// by itself qualified.
pub(crate) fn format_type(cat: &Cat, oid: u32, typmod: i32) -> String {
    if oid == 0 {
        return "-".into();
    }
    let Some(ty) = cat.ty(oid) else {
        return "???".into();
    };
    // A true array not of plain storage: oidvector and int2vector keep their own names.
    if cat.element(oid) != 0 && ty.typtype != 'd' && ty.storage != 'p' {
        return format!("{}[]", format_type(cat, ty.elem, typmod));
    }
    let with = (typmod >= 0).then_some(typmod);
    let timed = |base: &str, tz: &str| match with {
        Some(t) => format!("{base}({t}) {tz}"),
        None => format!("{base} {tz}"),
    };
    match oid {
        1560 => match with {
            Some(t) => format!("bit({t})"),
            None => "\"bit\"".into(),
        },
        1042 => match with {
            Some(t) => format!("character{}", char_typmod(t)),
            None => "bpchar".into(),
        },
        1043 => format!("character varying{}", with.map(char_typmod).unwrap_or_default()),
        1562 => match with {
            Some(t) => format!("bit varying({t})"),
            None => "bit varying".into(),
        },
        1700 => match with {
            Some(t) => format!(
                "numeric({},{})",
                ((t - 4) >> 16) & 0xffff,
                (((t - 4) & 0x7ff) ^ 1024) - 1024
            ),
            None => "numeric".into(),
        },
        1186 => match with {
            Some(t) => format!("interval{}", interval_typmod(t)),
            None => "interval".into(),
        },
        1083 => timed("time", "without time zone"),
        1266 => timed("time", "with time zone"),
        1114 => timed("timestamp", "without time zone"),
        1184 => timed("timestamp", "with time zone"),
        16 => "boolean".into(),
        700 => "real".into(),
        701 => "double precision".into(),
        21 => "smallint".into(),
        23 => "integer".into(),
        20 => "bigint".into(),
        18 => "\"char\"".into(),
        _ => {
            let name = crate::regs::reg_text(RegKind::Type, oid, cat.release);
            match with {
                Some(t) => format!("{name}({t})"),
                None => name,
            }
        }
    }
}

impl Printer<'_, '_> {
    pub(crate) fn push(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    /// appendContextKeyword.
    pub(crate) fn keyword(&mut self, s: &str, before: i32, after: i32, plus: i32) {
        if self.indent {
            self.level += before;
            while self.buf.ends_with(' ') {
                self.buf.pop();
            }
            self.buf.push('\n');
            let amount = if self.level < INDENT_LIMIT {
                self.level.max(0) + plus
            } else {
                let a = INDENT_LIMIT + (self.level - INDENT_LIMIT) / (INDENT_STD / 2);
                a % INDENT_LIMIT + plus
            };
            for _ in 0..amount {
                self.buf.push(' ');
            }
            self.buf.push_str(s);
            self.level += after;
            if self.level < 0 {
                self.level = 0;
            }
        } else {
            self.buf.push_str(s);
        }
    }

    fn open(&mut self) {
        if !self.paren {
            self.push("(");
        }
    }

    fn close(&mut self) {
        if !self.paren {
            self.push(")");
        }
    }

    /// get_rule_expr_paren.
    pub(crate) fn paren_expr(&mut self, n: &Node, implicit: bool, parent: &Node) -> Result<()> {
        let need = self.paren && !self.simple(n, parent);
        if need {
            self.push("(");
        }
        self.expr(n, implicit)?;
        if need {
            self.push(")");
        }
        Ok(())
    }

    fn binary_op_name(&self, n: &Node) -> Option<String> {
        match n {
            Node::Op { oid, args, .. } if args.len() == 2 => {
                let op = self.operator_name(*oid, args[0].ty(), args[1].ty());
                (op.chars().count() == 1).then_some(op)
            }
            _ => None,
        }
    }

    /// isSimpleNode.
    fn simple(&self, n: &Node, parent: &Node) -> bool {
        use Node as N;
        let cast_parent = |p: &Node| matches!(p, N::Func { form: Form::Explicit | Form::Implicit | Form::Sql, .. });
        let own_parens = |p: &Node| {
            matches!(
                p,
                N::Subscript { .. }
                    | N::Array { .. }
                    | N::Row { .. }
                    | N::Coalesce { .. }
                    | N::MinMax { .. }
                    | N::NullIf { .. }
                    | N::Case { .. }
                    | N::Agg { .. }
                    | N::Grouping { .. }
                    | N::Window { .. }
            )
        };
        match n {
            N::Var { .. } | N::Const { .. } | N::DomainValue { .. } => true,
            N::Subscript { .. }
            | N::Array { .. }
            | N::Row { .. }
            | N::Coalesce { .. }
            | N::MinMax { .. }
            | N::SqlValue { .. }
            | N::NullIf { .. }
            | N::Func { .. }
            | N::Case { .. }
            | N::Agg { .. }
            | N::Grouping { .. }
            | N::Window { .. } => true,
            N::Field { .. } => !matches!(parent, N::Field { .. }),
            N::Domain { arg, .. } | N::Relabel { arg, .. } | N::Io { arg, .. } | N::ArrayCoerce { arg, .. } => {
                self.simple(arg, n)
            }
            N::Op { .. } | N::NullTest { .. } | N::BoolTest { .. } | N::Distinct { .. } | N::SubLink { .. } => {
                if let (N::Op { .. }, true, N::Op { args: pargs, .. }) = (n, self.paren, parent) {
                    let Some(op) = self.binary_op_name(n) else {
                        return false;
                    };
                    let lo = op == "+" || op == "-";
                    let hi = op == "*" || op == "/" || op == "%";
                    if !(lo || hi) {
                        return false;
                    }
                    let Some(pop) = self.binary_op_name(parent) else {
                        return false;
                    };
                    let plo = pop == "+" || pop == "-";
                    let phi = pop == "*" || pop == "/" || pop == "%";
                    if !(plo || phi) {
                        return false;
                    }
                    if hi && plo {
                        return true;
                    }
                    if lo && phi {
                        return false;
                    }
                    return std::ptr::eq(n, &pargs[0]);
                }
                if let N::Func { .. } = parent {
                    return !cast_parent(parent);
                }
                matches!(parent, N::Bool { .. }) || own_parens(parent)
            }
            N::Bool { op, .. } => match parent {
                N::Bool { op: pop, .. } => {
                    if self.paren {
                        match op {
                            BoolOp::Not | BoolOp::And => {
                                matches!(pop, BoolOp::And | BoolOp::Or)
                            }
                            BoolOp::Or => *pop == BoolOp::Or,
                        }
                    } else {
                        false
                    }
                }
                N::Func { .. } => !cast_parent(parent),
                p => own_parens(p),
            },
            _ => false,
        }
    }

    /// generate_operator_name: bare where the operator's name and its arguments' types find it.
    pub(crate) fn operator_name(&self, oid: u32, l: u32, r: u32) -> String {
        let Some(o) = self.cat.oper(oid) else {
            return oid.to_string();
        };
        let a = super::analyze::Analyzer {
            cat: self.cat,
            columns: Vec::new(),
            value: None,
            scope: None,
        };
        let found = match o.kind {
            'l' => a.op_lookup(&o.name, 'l', 0, r),
            _ => a.op_lookup(&o.name, 'b', l, r),
        };
        if found == Some(oid) {
            o.name.clone()
        } else {
            format!("OPERATOR({}.{})", quote(&self.cat.namespace(o.namespace)), o.name)
        }
    }

    /// generate_function_name: bare where the search path finds it by its name and arguments.
    pub(crate) fn function_name(&self, oid: u32, args: &[u32]) -> String {
        let Some(p) = self.cat.proc(oid) else {
            return oid.to_string();
        };
        let found = self
            .cat
            .procs_named(&p.name, None)
            .into_iter()
            .find(|c| c.args == args)
            .map(|c| c.oid);
        let shown = found == Some(oid) || (found.is_none() && self.cat.path().contains(&p.namespace));
        if shown {
            quote(&p.name)
        } else {
            format!("{}.{}", quote(&self.cat.namespace(p.namespace)), quote(&p.name))
        }
    }

    fn collation_name(&self, oid: u32) -> String {
        crate::regs::reg_text(RegKind::Collation, oid, self.cat.release)
    }

    /// get_rule_expr.
    pub(crate) fn expr(&mut self, n: &Node, implicit: bool) -> Result<()> {
        use Node as N;
        match n {
            N::Var { attno, .. } => {
                if self.q.ns.is_empty() {
                    self.var(*attno, 0)
                } else {
                    self.variable(n, 0, false);
                }
            }
            N::Agg { .. } => self.agg(n)?,
            N::Grouping { args } => {
                self.push("GROUPING(");
                self.list(args, true)?;
                self.push(")");
            }
            N::Window { .. } => self.window_call(n)?,
            N::SubLink { .. } => self.sublink_expr(n)?,
            N::DomainValue { .. } => self.push("VALUE"),
            N::Const { .. } => self.constant(n, 0),
            N::Subscript { container, subs, .. } => {
                let parens = !matches!(container.as_ref(), N::Var { .. } | N::Field { .. });
                if parens {
                    self.push("(");
                }
                self.expr(container, implicit)?;
                if parens {
                    self.push(")");
                }
                for (lo, hi, slice) in subs {
                    self.push("[");
                    if *slice {
                        if let Some(l) = lo {
                            self.expr(l, false)?;
                        }
                        self.push(":");
                    }
                    if let Some(h) = hi {
                        self.expr(h, false)?;
                    }
                    self.push("]");
                }
            }
            N::Func { .. } => self.func(n, implicit)?,
            N::Op { oid, args, .. } => {
                self.open();
                if args.len() == 2 {
                    self.paren_expr(&args[0], true, n)?;
                    let name = self.operator_name(*oid, args[0].ty(), args[1].ty());
                    self.push(&format!(" {name} "));
                    self.paren_expr(&args[1], true, n)?;
                } else {
                    let name = self.operator_name(*oid, 0, args[0].ty());
                    self.push(&format!("{name} "));
                    self.paren_expr(&args[0], true, n)?;
                }
                self.close();
            }
            N::Distinct { args, .. } => {
                self.open();
                self.paren_expr(&args[0], true, n)?;
                self.push(" IS DISTINCT FROM ");
                self.paren_expr(&args[1], true, n)?;
                self.close();
            }
            N::NullIf { args, .. } => {
                self.push("NULLIF(");
                self.list(args, true)?;
                self.push(")");
            }
            N::AnyAll { oid, any, args } => {
                self.open();
                self.paren_expr(&args[0], true, n)?;
                let elem = self.cat.element(self.cat.base(args[1].ty()));
                let name = self.operator_name(*oid, args[0].ty(), elem);
                self.push(&format!(" {name} {} (", if *any { "ANY" } else { "ALL" }));
                self.paren_expr(&args[1], true, n)?;
                self.push(")");
                self.close();
            }
            N::Bool { op, args } => match op {
                BoolOp::And | BoolOp::Or => {
                    let word = if *op == BoolOp::And { " AND " } else { " OR " };
                    self.open();
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            self.push(word);
                        }
                        self.paren_expr(a, false, n)?;
                    }
                    self.close();
                }
                BoolOp::Not => {
                    self.open();
                    self.push("NOT ");
                    self.paren_expr(&args[0], false, n)?;
                    self.close();
                }
            },
            N::Field { arg, name, .. } => {
                let parens = !matches!(arg.as_ref(), N::Subscript { .. } | N::Field { .. });
                if parens {
                    self.push("(");
                }
                self.expr(arg, true)?;
                if parens {
                    self.push(")");
                }
                self.push(&format!(".{}", quote(name)));
            }
            N::Relabel { arg, ty, typmod, form } => {
                if *form == Form::Implicit && !implicit {
                    self.paren_expr(arg, false, n)?;
                } else {
                    self.coercion(arg, *ty, *typmod, n)?;
                }
            }
            N::Io { arg, ty, form } => {
                if *form == Form::Implicit && !implicit {
                    self.paren_expr(arg, false, n)?;
                } else {
                    self.coercion(arg, *ty, -1, n)?;
                }
            }
            N::ArrayCoerce { arg, ty, typmod, form } => {
                if *form == Form::Implicit && !implicit {
                    self.paren_expr(arg, false, n)?;
                } else {
                    self.coercion(arg, *ty, *typmod, n)?;
                }
            }
            N::Collate { arg, coll } => {
                self.open();
                self.paren_expr(arg, implicit, n)?;
                let c = self.collation_name(*coll);
                self.push(&format!(" COLLATE {c}"));
                self.close();
            }
            N::Case { arg, whens, default, .. } => {
                self.keyword("CASE", 0, INDENT_VAR, 0);
                if let Some(a) = arg {
                    self.push(" ");
                    self.expr(a, true)?;
                }
                for (w, r) in whens {
                    let mut shown = w;
                    if arg.is_some() {
                        if let N::Op { args, .. } = w {
                            if args.len() == 2 && matches!(strip_implicit(&args[0]), N::CaseTest { .. }) {
                                shown = &args[1];
                            }
                        }
                    }
                    if !self.indent {
                        self.push(" ");
                    }
                    self.keyword("WHEN ", 0, 0, 0);
                    self.expr(shown, false)?;
                    self.push(" THEN ");
                    self.expr(r, true)?;
                }
                if !self.indent {
                    self.push(" ");
                }
                self.keyword("ELSE ", 0, 0, 0);
                self.expr(default, true)?;
                if !self.indent {
                    self.push(" ");
                }
                self.keyword("END", -INDENT_VAR, 0, 0);
            }
            N::CaseTest { .. } => self.push("CASE_TEST_EXPR"),
            N::Array { elements, ty, .. } => {
                self.push("ARRAY[");
                self.list(elements, true)?;
                self.push("]");
                if elements.is_empty() {
                    let t = format_type(self.cat, *ty, -1);
                    self.push(&format!("::{t}"));
                }
            }
            N::Row { args, ty, explicit } => {
                self.push("ROW(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.toplevel(a, true)?;
                }
                if *ty != cat::RECORD {
                    let fields = super::row_fields(self.cat, self.cat.ty(*ty).map_or(0, |t| t.relid));
                    for _ in args.len()..fields.len() {
                        self.push(", NULL");
                    }
                }
                self.push(")");
                if *explicit {
                    let t = format_type(self.cat, *ty, -1);
                    self.push(&format!("::{t}"));
                }
            }
            N::Coalesce { args, .. } => {
                self.push("COALESCE(");
                self.list(args, true)?;
                self.push(")");
            }
            N::MinMax { greatest, args, .. } => {
                self.push(if *greatest { "GREATEST(" } else { "LEAST(" });
                self.list(args, true)?;
                self.push(")");
            }
            N::SqlValue { name, typmod, .. } => match typmod {
                Some(p) => self.push(&format!("{name}({p})")),
                None => self.push(name),
            },
            N::NullTest { arg, not } => {
                self.open();
                self.paren_expr(arg, true, n)?;
                self.push(if *not { " IS NOT NULL" } else { " IS NULL" });
                self.close();
            }
            N::BoolTest { arg, test } => {
                self.open();
                self.paren_expr(arg, false, n)?;
                self.push(test);
                self.close();
            }
            N::Domain { arg, ty, form } => {
                if *form == Form::Implicit && !implicit {
                    self.expr(arg, false)?;
                } else {
                    self.coercion(arg, *ty, -1, n)?;
                }
            }
            N::List(items) => self.list(items, implicit)?,
            N::Param { n, .. } => self.parameter(*n),
            N::Named { name, arg, .. } => {
                self.push(&format!("{} => ", quote(name)));
                self.expr(arg, implicit)?;
            }
        }
        Ok(())
    }

    /// get_parameter: `$n`, or in a SQL function's body the parameter's name where proargnames
    /// gives one, qualified by the function's name where a query around it reads a relation.
    fn parameter(&mut self, n: usize) {
        let named = self.q.function.as_ref().and_then(|f| {
            let name = f.1.as_ref()?.get(n.checked_sub(1)?)?;
            Some((f.0.clone(), name.clone()))
        });
        match named {
            Some((function, name)) => {
                if self.q.ns.iter().any(|d| !d.names.is_empty()) {
                    self.push(&format!("{}.", quote(&function)));
                }
                self.push(&quote(&name));
            }
            None => self.push(&format!("${n}")),
        }
    }

    fn list(&mut self, items: &[Node], implicit: bool) -> Result<()> {
        for (i, a) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.expr(a, implicit)?;
        }
        Ok(())
    }

    /// get_rule_expr_toplevel: a column alone as itself, whole-row included.
    pub(crate) fn toplevel(&mut self, n: &Node, implicit: bool) -> Result<()> {
        if matches!(n, Node::Var { .. }) && !self.q.ns.is_empty() {
            self.variable(n, 0, true);
            return Ok(());
        }
        self.expr(n, implicit)
    }

    /// get_variable: a column by its name, qualified where several relations are read.
    pub(crate) fn var(&mut self, attno: i16, rel: usize) {
        let Some((names, prefix)) = self.relations.get(rel) else {
            self.push(&format!("${attno}"));
            return;
        };
        let name = names
            .get((attno - 1).max(0) as usize)
            .cloned()
            .unwrap_or_else(|| format!("?column{attno}?"));
        let shown = match prefix {
            Some(p) => format!("{}.{}", quote(p), quote(&name)),
            None => quote(&name),
        };
        self.push(&shown);
    }

    /// get_coercion_expr.
    fn coercion(&mut self, arg: &Node, ty: u32, typmod: i32, parent: &Node) -> Result<()> {
        match arg {
            Node::Const { ty: t, typmod: -1, .. } if *t == ty => self.constant(arg, -1),
            _ => {
                self.open();
                self.paren_expr(arg, false, parent)?;
                self.close();
            }
        }
        let t = format_type(self.cat, ty, typmod);
        self.push(&format!("::{t}"));
        Ok(())
    }

    /// get_const_expr: `showtype` -1 bare, 0 labeled where the reading needs it.
    pub(crate) fn constant(&mut self, n: &Node, showtype: i32) {
        let Node::Const { ty, typmod, text } = n else {
            return;
        };
        let Some(text) = text else {
            self.push("NULL");
            if showtype >= 0 {
                let t = format_type(self.cat, *ty, *typmod);
                self.push(&format!("::{t}"));
            }
            return;
        };
        let mut label = false;
        match *ty {
            cat::INT4 => {
                if text.starts_with('-') {
                    self.push(&format!("'{text}'"));
                    label = true;
                } else {
                    self.push(text);
                }
            }
            cat::NUMERIC => {
                let digit = text.as_bytes().first().is_some_and(u8::is_ascii_digit);
                if digit && text.contains(['e', 'E', '.']) {
                    self.push(text);
                } else {
                    self.push(&format!("'{text}'"));
                    label = true;
                }
            }
            cat::BOOL => self.push(if text == "t" { "true" } else { "false" }),
            _ => self.push(&literal(text)),
        }
        if showtype < 0 {
            return;
        }
        let label = match *ty {
            cat::BOOL | cat::UNKNOWN => false,
            cat::INT4 => label,
            cat::NUMERIC => label || *typmod >= 0,
            _ => true,
        };
        if label || showtype > 0 {
            let t = format_type(self.cat, *ty, *typmod);
            self.push(&format!("::{t}"));
        }
    }

    /// get_func_expr.
    fn func(&mut self, n: &Node, implicit: bool) -> Result<()> {
        let Node::Func { oid, args, ty, form, variadic } = n else {
            return Ok(());
        };
        if *form == Form::Implicit && !implicit {
            return self.paren_expr(&args[0], false, n);
        }
        if matches!(form, Form::Explicit | Form::Implicit) {
            let typmod = n.length_coercion().unwrap_or(-1);
            return self.coercion(&args[0], *ty, typmod, n);
        }
        if *form == Form::Sql && self.sql_syntax(n)? {
            return Ok(());
        }
        let types: Vec<u32> = args.iter().map(Node::ty).collect();
        let name = self.function_name(*oid, &types);
        self.push(&format!("{name}("));
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            if *variadic && i + 1 == args.len() {
                self.push("VARIADIC ");
            }
            self.expr(a, true)?;
        }
        self.push(")");
        Ok(())
    }

    /// get_func_sql_syntax.
    fn sql_syntax(&mut self, n: &Node) -> Result<bool> {
        let Node::Func { oid, args, .. } = n else {
            return Ok(false);
        };
        let Some(p) = self.cat.proc(*oid) else {
            return Ok(false);
        };
        let name = p.name.clone();
        match (name.as_str(), args.len()) {
            ("timezone", 2) => {
                self.push("(");
                self.paren_expr(&args[1], false, n)?;
                self.push(" AT TIME ZONE ");
                self.paren_expr(&args[0], false, n)?;
                self.push(")");
            }
            ("timezone", 1) => {
                self.push("(");
                self.paren_expr(&args[0], false, n)?;
                self.push(" AT LOCAL)");
            }
            ("overlaps", 4) => {
                self.push("((");
                self.expr(&args[0], false)?;
                self.push(", ");
                self.expr(&args[1], false)?;
                self.push(") OVERLAPS (");
                self.expr(&args[2], false)?;
                self.push(", ");
                self.expr(&args[3], false)?;
                self.push("))");
            }
            ("extract", 2) => {
                let Node::Const { text: Some(f), .. } = &args[0] else {
                    return Ok(false);
                };
                self.push(&format!("EXTRACT({} FROM ", quote(f)));
                self.expr(&args[1], false)?;
                self.push(")");
            }
            ("is_normalized", _) => {
                self.push("(");
                self.paren_expr(&args[0], false, n)?;
                self.push(" IS");
                if let Some(Node::Const { text: Some(f), .. }) = args.get(1) {
                    self.push(&format!(" {f}"));
                }
                self.push(" NORMALIZED)");
            }
            ("pg_collation_for", 1) => {
                self.push("COLLATION FOR (");
                self.expr(&args[0], false)?;
                self.push(")");
            }
            ("normalize", _) => {
                self.push("NORMALIZE(");
                self.expr(&args[0], false)?;
                if let Some(Node::Const { text: Some(f), .. }) = args.get(1) {
                    self.push(&format!(", {f}"));
                }
                self.push(")");
            }
            ("overlay", 3 | 4) => {
                self.push("OVERLAY(");
                self.expr(&args[0], false)?;
                self.push(" PLACING ");
                self.expr(&args[1], false)?;
                self.push(" FROM ");
                self.expr(&args[2], false)?;
                if let Some(f) = args.get(3) {
                    self.push(" FOR ");
                    self.expr(f, false)?;
                }
                self.push(")");
            }
            ("position", 2) => {
                self.push("POSITION((");
                self.expr(&args[1], false)?;
                self.push(") IN (");
                self.expr(&args[0], false)?;
                self.push("))");
            }
            ("substring", 2 | 3) if p.args.get(1) == Some(&cat::INT4) => {
                self.push("SUBSTRING(");
                self.expr(&args[0], false)?;
                self.push(" FROM ");
                self.expr(&args[1], false)?;
                if let Some(f) = args.get(2) {
                    self.push(" FOR ");
                    self.expr(f, false)?;
                }
                self.push(")");
            }
            ("substring", 3) => {
                self.push("SUBSTRING(");
                self.expr(&args[0], false)?;
                self.push(" SIMILAR ");
                self.expr(&args[1], false)?;
                self.push(" ESCAPE ");
                self.expr(&args[2], false)?;
                self.push(")");
            }
            ("btrim" | "ltrim" | "rtrim", 1 | 2) => {
                let w = match name.as_str() {
                    "btrim" => "BOTH",
                    "ltrim" => "LEADING",
                    _ => "TRAILING",
                };
                self.push(&format!("TRIM({w}"));
                if let Some(c) = args.get(1) {
                    self.push(" ");
                    self.expr(c, false)?;
                }
                self.push(" FROM ");
                self.expr(&args[0], false)?;
                self.push(")");
            }
            ("system_user", 0) => self.push("SYSTEM_USER"),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// strip_implicit_coercions.
fn strip_implicit(n: &Node) -> &Node {
    match n {
        Node::Func { form: Form::Implicit, args, .. } => strip_implicit(&args[0]),
        Node::Relabel { form: Form::Implicit, arg, .. }
        | Node::Io { form: Form::Implicit, arg, .. }
        | Node::ArrayCoerce { form: Form::Implicit, arg, .. }
        | Node::Domain { form: Form::Implicit, arg, .. } => strip_implicit(arg),
        n => n,
    }
}
