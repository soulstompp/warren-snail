//! A statement as PostgreSQL's grammar binds its operators, where PostgreSQL's dialect in sqlparser
//! binds them otherwise: written again with its parentheses, so that all that reads it after reads
//! one tree.

use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator};
use sqlparser::dialect::{Dialect, PostgreSqlDialect, Precedence};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

/// PostgreSQL's dialect, but for what PostgreSQL's grammar binds otherwise: a prefix operator but
/// `+` and `-` over all that binds tighter than an operator of its own, `+` and `-` among it; `+`
/// and `-` before `^`; the operators of `~` beside any other, `~~` and its kin as the LIKE and ILIKE
/// PostgreSQL's grammar writes them as. `parenthesized` marks each by parentheses, so that its
/// printing reads the same under PostgreSQL's dialect.
#[derive(Debug, Default)]
struct Grammar {
    parenthesized: bool,
    /// Whether a prefix operator's operand is a prefix operator's, which printed beside it reads as
    /// another operator.
    side_by_side: std::cell::Cell<bool>,
}

type Parsed = std::result::Result<Expr, ParserError>;

/// The methods of PostgreSQL's dialect that take nothing, as it answers them.
macro_rules! as_postgresql {
    ($($name:ident),* $(,)?) => {
        $(fn $name(&self) -> bool { PostgreSqlDialect {}.$name() })*
    };
}

/// Every method PostgreSQL's dialect answers of its own but its precedences and its prefix and infix
/// readings, as it answers them.
macro_rules! postgresql_otherwise {
    () => {
        fn dialect(&self) -> std::any::TypeId {
            std::any::TypeId::of::<PostgreSqlDialect>()
        }

        fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
            PostgreSqlDialect {}.identifier_quote_style(identifier)
        }

        fn is_delimited_identifier_start(&self, ch: char) -> bool {
            PostgreSqlDialect {}.is_delimited_identifier_start(ch)
        }

        fn is_identifier_start(&self, ch: char) -> bool {
            PostgreSqlDialect {}.is_identifier_start(ch)
        }

        fn is_identifier_part(&self, ch: char) -> bool {
            PostgreSqlDialect {}.is_identifier_part(ch)
        }

        fn is_reserved_for_identifier(&self, kw: Keyword) -> bool {
            PostgreSqlDialect {}.is_reserved_for_identifier(kw)
        }

        fn is_table_alias(&self, kw: &Keyword, parser: &mut Parser) -> bool {
            PostgreSqlDialect {}.is_table_alias(kw, parser)
        }

        fn is_custom_operator_part(&self, ch: char) -> bool {
            PostgreSqlDialect {}.is_custom_operator_part(ch)
        }

        fn prec_value(&self, prec: Precedence) -> u8 {
            PostgreSqlDialect {}.prec_value(prec)
        }

        as_postgresql!(
            supports_unicode_string_literal,
            supports_filter_during_aggregation,
            supports_group_by_expr,
            supports_alter_user_as_alter_role,
            allow_extract_custom,
            allow_extract_single_quotes,
            supports_create_index_with_clause,
            supports_explain_with_utility_options,
            supports_listen_notify,
            supports_exclude_constraint,
            supports_factorial_operator,
            supports_bitwise_shift_operators,
            supports_comment_on,
            supports_load_extension,
            supports_named_fn_args_with_colon_operator,
            supports_named_fn_args_with_expr_name,
            supports_empty_projections,
            supports_nested_comments,
            supports_string_escape_constant,
            supports_numeric_literal_underscores,
            supports_array_typedef_with_brackets,
            supports_geometric_types,
            supports_order_by_using_operator,
            supports_set_names,
            supports_alter_column_type_using,
            supports_left_associative_joins_without_parens,
            supports_notnull_operator,
            supports_interval_options,
            supports_insert_table_alias,
            supports_create_table_like_parenthesized,
            supports_select_wildcard_with_alias,
            supports_comma_separated_trim,
            supports_xml_expressions,
            supports_aliased_function_args,
            supports_comment_optimizer_hint,
        );
    };
}

/// PostgreSQL's dialect, with the one reading it lacks: a COLLATE after an operator that binds
/// tighter (`x::text COLLATE "C"`, `a[1] COLLATE "C"`), which PostgreSQL's grammar reads and its
/// dialect in sqlparser refuses. Every statement PostgreSQL's dialect reads it reads the same.
#[derive(Debug)]
pub(crate) struct Postgres;

/// The statements and expressions written by a person are read in it.
pub(crate) const POSTGRES: Postgres = Postgres;

impl Dialect for Postgres {
    postgresql_otherwise!();

    fn get_next_precedence(&self, parser: &Parser) -> Option<std::result::Result<u8, ParserError>> {
        PostgreSqlDialect {}.get_next_precedence(parser)
    }

    fn parse_infix(&self, parser: &mut Parser, expr: &Expr, _precedence: u8) -> Option<Parsed> {
        collated(parser, expr)
    }
}

/// A COLLATE met where an operand is read on: its operand in parentheses, so that its printing reads
/// as the same tree under PostgreSQL's dialect, which takes a COLLATE only after a whole operand.
/// PostgreSQL's dialect gives COLLATE no precedence in a column's definition, so it is never met
/// here there.
fn collated(parser: &mut Parser, expr: &Expr) -> Option<Parsed> {
    match &parser.peek_token_ref().token {
        Token::Word(w) if w.keyword == Keyword::COLLATE => {}
        _ => return None,
    }
    parser.next_token();
    Some(parser.parse_object_name(false).map(|collation| Expr::Collate {
        expr: Box::new(Expr::Nested(Box::new(expr.clone()))),
        collation,
    }))
}

/// Whether an expression's printing begins with a prefix operator's.
fn begins_with_prefix(e: &Expr) -> bool {
    match e {
        Expr::UnaryOp { .. } => true,
        Expr::BinaryOp { left: x, .. }
        | Expr::Collate { expr: x, .. }
        | Expr::AtTimeZone { timestamp: x, .. } => begins_with_prefix(x),
        _ => false,
    }
}

impl Grammar {
    fn marked(&self, e: Expr) -> Expr {
        match self.parenthesized {
            true => Expr::Nested(Box::new(e)),
            false => e,
        }
    }

    /// A prefix operator's operand, in parentheses where it would read otherwise without them: a sum,
    /// and what begins with a prefix operator of its own.
    fn operand(&self, operand: Expr) -> Expr {
        if begins_with_prefix(&operand) {
            self.side_by_side.set(true);
            return self.marked(operand);
        }
        match operand {
            Expr::BinaryOp {
                op: BinaryOperator::Plus | BinaryOperator::Minus,
                ..
            } => self.marked(operand),
            other => other,
        }
    }

    fn prefix(&self, parser: &mut Parser, op: UnaryOperator) -> Parsed {
        parser.next_token();
        let operand = parser.parse_subexpr(self.prec_value(Precedence::PgOther))?;
        let operand = self.operand(operand);
        Ok(Expr::UnaryOp {
            op,
            expr: Box::new(operand),
        })
    }

    fn sign(&self, parser: &mut Parser, op: UnaryOperator) -> Parsed {
        parser.next_token();
        let operand = parser.parse_subexpr(self.prec_value(Precedence::Caret))?;
        let operand = match begins_with_prefix(&operand) {
            true => self.operand(operand),
            false => operand,
        };
        let signed = Expr::UnaryOp {
            op,
            expr: Box::new(operand),
        };
        Ok(match parser.peek_token_ref().token {
            Token::Caret => self.marked(signed),
            _ => signed,
        })
    }
}

impl Dialect for Grammar {
    postgresql_otherwise!();

    fn get_next_precedence(&self, parser: &Parser) -> Option<std::result::Result<u8, ParserError>> {
        match parser.peek_token_ref().token {
            Token::Tilde
            | Token::TildeAsterisk
            | Token::ExclamationMarkTilde
            | Token::ExclamationMarkTildeAsterisk
            | Token::DoubleTilde
            | Token::DoubleTildeAsterisk
            | Token::ExclamationMarkDoubleTilde
            | Token::ExclamationMarkDoubleTildeAsterisk => {
                Some(Ok(self.prec_value(Precedence::PgOther)))
            }
            _ => PostgreSqlDialect {}.get_next_precedence(parser),
        }
    }

    fn parse_prefix(&self, parser: &mut Parser) -> Option<Parsed> {
        let op = match parser.peek_token_ref().token {
            Token::Minus => return Some(self.sign(parser, UnaryOperator::Minus)),
            Token::Plus => return Some(self.sign(parser, UnaryOperator::Plus)),
            Token::AtSign => UnaryOperator::PGAbs,
            Token::PGSquareRoot => UnaryOperator::PGSquareRoot,
            Token::PGCubeRoot => UnaryOperator::PGCubeRoot,
            Token::Tilde => UnaryOperator::BitwiseNot,
            Token::DoubleExclamationMark => UnaryOperator::PGPrefixFactorial,
            Token::Sharp => UnaryOperator::Hash,
            Token::AtDashAt => UnaryOperator::AtDashAt,
            Token::AtAt => UnaryOperator::DoubleAt,
            Token::QuestionMarkDash => UnaryOperator::QuestionDash,
            Token::QuestionPipe => UnaryOperator::QuestionPipe,
            _ => return None,
        };
        Some(self.prefix(parser, op))
    }

    fn parse_infix(&self, parser: &mut Parser, expr: &Expr, _precedence: u8) -> Option<Parsed> {
        let token = parser.peek_token_ref().token.clone();
        let (binary, like) = match token {
            Token::Tilde => (Some(BinaryOperator::PGRegexMatch), None),
            Token::TildeAsterisk => (Some(BinaryOperator::PGRegexIMatch), None),
            Token::ExclamationMarkTilde => (Some(BinaryOperator::PGRegexNotMatch), None),
            Token::ExclamationMarkTildeAsterisk => (Some(BinaryOperator::PGRegexNotIMatch), None),
            Token::DoubleTilde => (None, Some((false, false))),
            Token::ExclamationMarkDoubleTilde => (None, Some((true, false))),
            Token::DoubleTildeAsterisk => (None, Some((false, true))),
            Token::ExclamationMarkDoubleTildeAsterisk => (None, Some((true, true))),
            _ => return collated(parser, expr),
        };
        // Of ANY, ALL and SOME, as PostgreSQL's dialect reads them.
        if let Token::Word(w) = &parser.peek_nth_token_ref(1).token {
            if matches!(w.keyword, Keyword::ANY | Keyword::ALL | Keyword::SOME) {
                return None;
            }
        }
        parser.next_token();
        let right = match parser.parse_subexpr(self.prec_value(Precedence::PgOther)) {
            Ok(right) => Box::new(right),
            Err(e) => return Some(Err(e)),
        };
        let left = Box::new(expr.clone());
        let read = match (binary, like) {
            (Some(op), _) => Expr::BinaryOp { left, op, right },
            (_, Some((negated, false))) => Expr::Like {
                negated,
                any: false,
                expr: left,
                pattern: right,
                escape_char: None,
            },
            (_, Some((negated, true))) => Expr::ILike {
                negated,
                any: false,
                expr: left,
                pattern: right,
                escape_char: None,
            },
            _ => unreachable!("an operator of `~` is one of these"),
        };
        Some(Ok(self.marked(read)))
    }
}

/// Whether text may hold an operator the two dialects bind otherwise.
fn binds_otherwise(sql: &str) -> bool {
    sql.contains(['~', '@', '^', '#', '!', '?']) || sql.contains("|/")
}

/// The statement as PostgreSQL's grammar binds it, written with its parentheses, where PostgreSQL's
/// dialect in sqlparser binds it otherwise; none where it binds it the same, or reads no statement.
pub(crate) fn bound(sql: &str) -> Option<String> {
    if !binds_otherwise(sql) {
        return None;
    }
    let plain = crate::read::image::parse_sql(sql).ok()?;
    let grammar = Grammar::default();
    let read = parsed(&grammar, sql).ok()?;
    if read == plain && !grammar.side_by_side.get() {
        return None;
    }
    let grammar = Grammar {
        parenthesized: true,
        ..Grammar::default()
    };
    let marked = parsed(&grammar, sql).ok()?;
    Some(
        marked
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// An expression PostgreSQL's dialect in sqlparser read, as PostgreSQL's grammar binds it.
pub(crate) fn expression(e: Expr) -> Expr {
    let text = e.to_string();
    if !binds_otherwise(&text) {
        return e;
    }
    let grammar = Grammar {
        parenthesized: true,
        ..Grammar::default()
    };
    crate::read::image::tokens(&text)
        .map_err(ParserError::from)
        .and_then(|t| {
            Parser::new(&grammar)
                .with_tokens_with_locations(t)
                .parse_expr()
        })
        .unwrap_or(e)
}

/// An expression's tokens as PostgreSQL's grammar binds them; none where they read no expression.
pub(crate) fn expression_of(tokens: &[Token]) -> Option<Expr> {
    let grammar = Grammar {
        parenthesized: true,
        ..Grammar::default()
    };
    let mut p = Parser::new(&grammar).with_tokens(tokens.to_vec());
    let e = p.parse_expr().ok()?;
    (p.index() >= tokens.len()).then_some(e)
}

/// A question PostgreSQL's dialect in sqlparser read, as PostgreSQL's grammar binds it.
pub(crate) fn query(q: &sqlparser::ast::Query) -> sqlparser::ast::Query {
    let read = bound(&q.to_string()).and_then(|b| crate::read::image::parse_sql(&b).ok());
    match read.as_deref() {
        Some([sqlparser::ast::Statement::Query(b)]) => (**b).clone(),
        _ => q.clone(),
    }
}

/// The statements of `sql` as `grammar` reads them, its tokens the record image operators' reading.
fn parsed(
    grammar: &Grammar,
    sql: &str,
) -> std::result::Result<Vec<sqlparser::ast::Statement>, ParserError> {
    let tokens = crate::read::image::tokens(sql)?;
    Parser::new(grammar)
        .with_tokens_with_locations(tokens)
        .parse_statements()
}
