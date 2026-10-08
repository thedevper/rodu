//! JQL-lite: `assignee = me() AND status IN ("Todo", "In Progress") ORDER BY priority`.
//! The web filter bar, the CLI and the MCP search tool all parse into this one AST; stores compile
//! it to their own query language.

use crate::error::{Result, RoduError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Like,
    NotLike,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Ne => "!=",
            CompareOp::Like => "~",
            CompareOp::NotLike => "!~",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "=" => CompareOp::Eq,
            "!=" => CompareOp::Ne,
            "~" => CompareOp::Like,
            "!~" => CompareOp::NotLike,
            "<" => CompareOp::Lt,
            "<=" => CompareOp::Le,
            ">" => CompareOp::Gt,
            ">=" => CompareOp::Ge,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    String(String),
    Number(f64),
    /// Offset from now in milliseconds, e.g. -7d; `raw` is the text as written.
    Duration {
        ms: i64,
        raw: String,
    },
    Func(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    pub kind: ValueKind,
    /// Character offset in the query, for error carets.
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare { field: String, op: CompareOp, value: Value, pos: usize },
    In { field: String, negated: bool, values: Vec<Value>, pos: usize },
    Empty { field: String, negated: bool, pos: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub field: String,
    pub descending: bool,
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    pub filter: Option<Expr>,
    pub order_by: Vec<OrderBy>,
}

pub const MAX_QUERY_LENGTH: usize = 2000;

/// An error that points at a position in the query.
pub fn query_error(message: &str, pos: usize, source: &str, hint: Option<&str>) -> RoduError {
    let caret = format!("{source}\n{}^", " ".repeat(pos));
    let hint = match hint {
        Some(h) => format!("{h}\n{caret}"),
        None => caret,
    };
    RoduError::invalid(format!("Query error at {pos}: {message}")).with_hint(hint)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String, usize),
    Str(String, usize),
    Op(String, usize),
    Punct(char, usize),
    Eof(usize),
}

impl Token {
    fn pos(&self) -> usize {
        match self {
            Token::Word(_, p)
            | Token::Str(_, p)
            | Token::Op(_, p)
            | Token::Punct(_, p)
            | Token::Eof(p) => *p,
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

fn tokenize(source: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '"' || ch == '\'' {
            let start = i;
            i += 1;
            let mut text = String::new();
            while i < chars.len() && chars[i] != ch {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                text.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                return Err(query_error("unterminated string", start, source, None));
            }
            i += 1;
            tokens.push(Token::Str(text, start));
        } else if matches!(ch, '(' | ')' | ',') {
            tokens.push(Token::Punct(ch, i));
            i += 1;
        } else if "=!~<>".contains(ch) {
            let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let op =
                if ["!=", "!~", "<=", ">="].contains(&two.as_str()) { two } else { ch.to_string() };
            if op == "!" {
                return Err(query_error("expected \"!=\" or \"!~\"", i, source, None));
            }
            let width = op.chars().count();
            tokens.push(Token::Op(op, i));
            i += width;
        } else if is_word(ch) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect(), start));
        } else {
            return Err(query_error(&format!("unexpected character \"{ch}\""), i, source, None));
        }
    }
    tokens.push(Token::Eof(chars.len()));
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    index: usize,
    source: &'a str,
}

fn duration(text: &str) -> Option<i64> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'-' => (-1, &text[1..]),
        b'+' => (1, &text[1..]),
        _ => (1, text),
    };
    let unit = rest.chars().last()?.to_ascii_lowercase();
    let digits = &rest[..rest.len() - 1];
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let unit_ms: i64 = match unit {
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 604_800_000,
        _ => return None,
    };
    Some(sign * digits.parse::<i64>().ok()? * unit_ms)
}

fn is_number(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

impl<'a> Parser<'a> {
    fn parse(mut self) -> Result<Query> {
        let mut filter = None;
        if !self.at_keyword("ORDER") && !matches!(self.peek(), Token::Eof(_)) {
            filter = Some(self.parse_or()?);
        }
        let mut order_by = Vec::new();
        if self.accept_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let (field, pos) = self.expect_word("a field to order by")?;
                let descending = if self.accept_keyword("DESC") {
                    true
                } else {
                    self.accept_keyword("ASC");
                    false
                };
                order_by.push(OrderBy { field: field.to_lowercase(), descending, pos });
                if !self.accept_punct(',') {
                    break;
                }
            }
        }
        if !matches!(self.peek(), Token::Eof(_)) {
            return Err(self.error("expected AND, OR, ORDER BY or end of query"));
        }
        Ok(Query { filter, order_by })
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.accept_keyword("OR") {
            left = Expr::Or(Box::new(left), Box::new(self.parse_and()?));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        while self.accept_keyword("AND") {
            left = Expr::And(Box::new(left), Box::new(self.parse_unary()?));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.accept_keyword("NOT") {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.accept_punct('(') {
            let expr = self.parse_or()?;
            self.expect_punct(')')?;
            return Ok(expr);
        }
        self.parse_condition()
    }

    fn parse_condition(&mut self) -> Result<Expr> {
        let (field, pos) = self.expect_word("a field name")?;
        let field = field.to_lowercase();
        if self.accept_keyword("IS") {
            let negated = self.accept_keyword("NOT");
            self.expect_keyword("EMPTY")?;
            return Ok(Expr::Empty { field, negated, pos });
        }
        let negated = self.accept_keyword("NOT");
        if negated || self.at_keyword("IN") {
            self.expect_keyword("IN")?;
            self.expect_punct('(')?;
            let mut values = vec![self.parse_value()?];
            while self.accept_punct(',') {
                values.push(self.parse_value()?);
            }
            self.expect_punct(')')?;
            return Ok(Expr::In { field, negated, values, pos });
        }
        let op = match self.peek() {
            Token::Op(text, _) => CompareOp::parse(text),
            _ => None,
        };
        let Some(op) = op else {
            return Err(self.error("expected an operator (=, !=, ~, <, >, IN, IS)"));
        };
        self.index += 1;
        Ok(Expr::Compare { field, op, value: self.parse_value()?, pos })
    }

    fn parse_value(&mut self) -> Result<Value> {
        match self.peek().clone() {
            Token::Str(text, pos) => {
                self.index += 1;
                Ok(Value { kind: ValueKind::String(text), pos })
            }
            Token::Word(text, pos) => {
                self.index += 1;
                if self.accept_punct('(') {
                    self.expect_punct(')')?;
                    return Ok(Value { kind: ValueKind::Func(text.to_lowercase()), pos });
                }
                if let Some(ms) = duration(&text) {
                    return Ok(Value { kind: ValueKind::Duration { ms, raw: text }, pos });
                }
                if is_number(&text) {
                    let n = text.parse().unwrap_or(0.0);
                    return Ok(Value { kind: ValueKind::Number(n), pos });
                }
                Ok(Value { kind: ValueKind::String(text), pos })
            }
            _ => Err(self.error("expected a value")),
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Token::Word(text, _) if text.eq_ignore_ascii_case(keyword))
    }

    fn accept_keyword(&mut self, keyword: &str) -> bool {
        if !self.at_keyword(keyword) {
            return false;
        }
        self.index += 1;
        true
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if self.accept_keyword(keyword) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {keyword}")))
        }
    }

    fn accept_punct(&mut self, ch: char) -> bool {
        if !matches!(self.peek(), Token::Punct(c, _) if *c == ch) {
            return false;
        }
        self.index += 1;
        true
    }

    fn expect_punct(&mut self, ch: char) -> Result<()> {
        if self.accept_punct(ch) { Ok(()) } else { Err(self.error(&format!("expected \"{ch}\""))) }
    }

    fn expect_word(&mut self, what: &str) -> Result<(String, usize)> {
        match self.peek().clone() {
            Token::Word(text, pos) => {
                self.index += 1;
                Ok((text, pos))
            }
            _ => Err(self.error(&format!("expected {what}"))),
        }
    }

    fn error(&self, message: &str) -> RoduError {
        query_error(message, self.peek().pos(), self.source, None)
    }
}

/// Parses a JQL-lite query; an empty string matches everything.
pub fn parse_query(source: &str) -> Result<Query> {
    if source.chars().count() > MAX_QUERY_LENGTH {
        return Err(RoduError::limit(format!(
            "Query is longer than {MAX_QUERY_LENGTH} characters"
        )));
    }
    Parser { tokens: tokenize(source)?, index: 0, source }.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conditions_with_precedence_and_order() {
        let q = parse_query(r#"assignee = me() AND (status IN ("Todo", 'In Progress') OR NOT priority = high) ORDER BY priority DESC, key"#).unwrap();
        let Some(Expr::And(left, right)) = q.filter else { panic!("expected AND") };
        assert!(
            matches!(*left, Expr::Compare { ref field, op: CompareOp::Eq, value: Value { kind: ValueKind::Func(ref f), .. }, .. } if field == "assignee" && f == "me")
        );
        assert!(matches!(*right, Expr::Or(..)));
        assert_eq!(q.order_by.len(), 2);
        assert!(q.order_by[0].descending);
        assert_eq!(q.order_by[1].field, "key");
    }

    #[test]
    fn reads_durations_numbers_and_empty_checks() {
        let q = parse_query("updated > -7d AND estimate >= 3.5 AND due IS NOT EMPTY").unwrap();
        let text = format!("{q:?}");
        assert!(text.contains("Duration { ms: -604800000"));
        assert!(text.contains("Number(3.5)"));
        assert!(text.contains("Empty { field: \"due\", negated: true"));
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(parse_query("  ").unwrap(), Query::default());
        assert!(parse_query("ORDER BY rank").unwrap().filter.is_none());
    }

    #[test]
    fn points_at_the_error() {
        let err = parse_query("status = ").unwrap_err();
        assert_eq!(err.message, "Query error at 9: expected a value");
        assert!(err.hint.unwrap().ends_with("\n         ^"));
        assert!(parse_query("title = \"open").unwrap_err().message.contains("unterminated"));
        assert!(parse_query("a ! b").unwrap_err().message.contains("expected \"!=\""));
        assert!(parse_query("status = x y").unwrap_err().message.contains("expected AND, OR"));
    }

    #[test]
    fn refuses_very_long_queries() {
        let err = parse_query(&"a".repeat(MAX_QUERY_LENGTH + 1)).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Limit);
    }
}
