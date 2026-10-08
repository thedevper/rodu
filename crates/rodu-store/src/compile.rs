//! Compiles a JQL-lite AST to a parameterised SQLite WHERE / ORDER BY over `items i`.
//! Only whitelisted fields are accepted and every value is bound as a parameter.

use rodu_core::clock::iso;
use rodu_core::query::{CompareOp, Expr, OrderBy, Query, Value, ValueKind, query_error};
use rodu_core::{Category, ItemType, Priority, Result, RoduError};
use rusqlite::types::Value as SqlValue;
use time::{Duration, OffsetDateTime};

pub struct CompiledQuery {
    pub where_sql: String,
    pub params: Vec<SqlValue>,
    pub order_by: String,
}

pub struct CompileContext<'a> {
    /// Principal id for me(); None when nobody is signed in.
    pub me: Option<&'a str>,
    pub now: OffsetDateTime,
}

struct Fragment {
    sql: String,
    params: Vec<SqlValue>,
}

impl Fragment {
    fn new(sql: impl Into<String>, params: Vec<SqlValue>) -> Self {
        Self { sql: sql.into(), params }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Key,
    Text,
    Status,
    Enum(&'static [&'static str]),
    Number,
    Date,
    Day,
    Ref(RefKind),
    Fts,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Principal,
    Collection,
    Cycle,
    Item,
}

#[derive(Clone, Copy)]
struct FieldSpec {
    kind: Kind,
    column: &'static str,
}

const TYPES: &[&str] = &["epic", "story", "task", "bug", "subtask"];
const CATEGORIES: &[&str] = &["backlog", "active", "review", "done"];
const PRIORITIES: &[&str] = &["none", "urgent", "high", "normal", "low"];

const FIELDS: &[(&str, FieldSpec)] = &[
    ("key", FieldSpec { kind: Kind::Key, column: "i.key" }),
    ("id", FieldSpec { kind: Kind::Key, column: "i.id" }),
    ("title", FieldSpec { kind: Kind::Text, column: "i.title" }),
    ("body", FieldSpec { kind: Kind::Text, column: "i.body" }),
    ("text", FieldSpec { kind: Kind::Fts, column: "i.id" }),
    ("type", FieldSpec { kind: Kind::Enum(TYPES), column: "i.type" }),
    ("status", FieldSpec { kind: Kind::Status, column: "i.status" }),
    ("category", FieldSpec { kind: Kind::Enum(CATEGORIES), column: "i.category" }),
    ("priority", FieldSpec { kind: Kind::Enum(PRIORITIES), column: "i.priority" }),
    ("estimate", FieldSpec { kind: Kind::Number, column: "i.estimate" }),
    ("created", FieldSpec { kind: Kind::Date, column: "i.created_at" }),
    ("updated", FieldSpec { kind: Kind::Date, column: "i.updated_at" }),
    ("due", FieldSpec { kind: Kind::Day, column: "i.due_at" }),
    ("assignee", FieldSpec { kind: Kind::Ref(RefKind::Principal), column: "i.assignee_id" }),
    ("collection", FieldSpec { kind: Kind::Ref(RefKind::Collection), column: "i.collection_id" }),
    ("cycle", FieldSpec { kind: Kind::Ref(RefKind::Cycle), column: "i.cycle_id" }),
    ("parent", FieldSpec { kind: Kind::Ref(RefKind::Item), column: "i.parent_id" }),
];

const ALIASES: &[(&str, &str)] = &[
    ("status.category", "category"),
    ("sprint", "cycle"),
    ("project", "collection"),
    ("createdat", "created"),
    ("updatedat", "updated"),
    ("dueat", "due"),
];

const PRIORITY_ORDER: &str = "CASE i.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 WHEN 'low' THEN 3 ELSE 4 END";

const ORDER_FIELDS: &[(&str, &str)] = &[
    ("priority", PRIORITY_ORDER),
    ("created", "i.created_at"),
    ("updated", "i.updated_at"),
    ("due", "i.due_at"),
    ("estimate", "i.estimate"),
    ("key", "i.collection_id, i.number"),
    ("rank", "i.rank"),
    ("title", "i.title COLLATE NOCASE"),
    ("status", "i.status COLLATE NOCASE"),
];

const NULLABLE: &[&str] = &["i.assignee_id", "i.parent_id", "i.cycle_id", "i.estimate", "i.due_at"];

// The enum lists above must match the model; checked here so they cannot drift.
const _: () = {
    assert!(TYPES.len() == ItemType::ALL.len());
    assert!(CATEGORIES.len() == Category::ALL.len());
    assert!(PRIORITIES.len() == Priority::ALL.len());
};

fn canonical(name: &str) -> &str {
    ALIASES.iter().find(|(a, _)| *a == name).map_or(name, |(_, c)| c)
}

fn text(s: impl Into<String>) -> SqlValue {
    SqlValue::Text(s.into())
}

struct Compiler<'a> {
    ctx: &'a CompileContext<'a>,
    source: &'a str,
}

impl Compiler<'_> {
    fn error(&self, message: &str, pos: usize, hint: Option<&str>) -> RoduError {
        query_error(message, pos, self.source, hint)
    }

    fn expr(&self, e: &Expr) -> Result<Fragment> {
        match e {
            Expr::And(l, r) | Expr::Or(l, r) => {
                let joiner = if matches!(e, Expr::And(..)) { "AND" } else { "OR" };
                let left = self.expr(l)?;
                let right = self.expr(r)?;
                let mut params = left.params;
                params.extend(right.params);
                Ok(Fragment::new(format!("({} {joiner} {})", left.sql, right.sql), params))
            }
            Expr::Not(inner) => {
                let inner = self.expr(inner)?;
                Ok(Fragment::new(format!("(NOT {})", inner.sql), inner.params))
            }
            Expr::Empty { field, negated, pos } => {
                self.empty(self.field(field, *pos)?, *negated, *pos)
            }
            Expr::In { field, negated, values, pos } => {
                self.in_list(self.field(field, *pos)?, values, *negated)
            }
            Expr::Compare { field, op, value, pos } => {
                self.compare(self.field(field, *pos)?, field, *op, value, *pos)
            }
        }
    }

    fn order_by(&self, order: &[OrderBy]) -> Result<String> {
        let mut parts = Vec::new();
        for o in order {
            let name = canonical(&o.field);
            let Some((_, column)) = ORDER_FIELDS.iter().find(|(f, _)| *f == name) else {
                let names: Vec<&str> = ORDER_FIELDS.iter().map(|(f, _)| *f).collect();
                return Err(self.error(
                    &format!("cannot order by \"{}\"", o.field),
                    o.pos,
                    Some(&format!("Order by one of: {}", names.join(", "))),
                ));
            };
            let dir = if o.descending { "DESC" } else { "ASC" };
            let nulls = if name == "due" || name == "estimate" { " NULLS LAST" } else { "" };
            let cols: Vec<String> =
                column.split(", ").map(|c| format!("{c} {dir}{nulls}")).collect();
            parts.push(cols.join(", "));
        }
        parts.push("i.rank ASC".into());
        parts.push("i.id ASC".into());
        Ok(parts.join(", "))
    }

    fn field(&self, name: &str, pos: usize) -> Result<FieldSpec> {
        let name = canonical(name);
        FIELDS.iter().find(|(f, _)| *f == name).map(|(_, spec)| *spec).ok_or_else(|| {
            let names: Vec<&str> = FIELDS.iter().map(|(f, _)| *f).collect();
            self.error(
                &format!("unknown field \"{name}\""),
                pos,
                Some(&format!("Fields: {}", names.join(", "))),
            )
        })
    }

    fn compare(
        &self,
        spec: FieldSpec,
        name: &str,
        op: CompareOp,
        value: &Value,
        pos: usize,
    ) -> Result<Fragment> {
        if matches!(op, CompareOp::Like | CompareOp::NotLike) {
            if spec.kind == Kind::Fts {
                let matched = self.fts(&self.literal(value)?)?;
                return Ok(if op == CompareOp::Like {
                    matched
                } else {
                    Fragment::new(format!("(NOT {})", matched.sql), matched.params)
                });
            }
            if !matches!(spec.kind, Kind::Text | Kind::Key | Kind::Status) {
                return Err(self.error(
                    &format!("\"{name}\" does not support ~"),
                    pos,
                    Some("Use = or IN"),
                ));
            }
            let literal = self.literal(value)?;
            let mut escaped = String::with_capacity(literal.len());
            for c in literal.chars() {
                if matches!(c, '\\' | '%' | '_') {
                    escaped.push('\\');
                }
                escaped.push(c);
            }
            let not = if op == CompareOp::NotLike { "NOT " } else { "" };
            return Ok(Fragment::new(
                format!("{} {not}LIKE ? ESCAPE '\\'", spec.column),
                vec![text(format!("%{escaped}%"))],
            ));
        }
        if spec.kind == Kind::Fts {
            if op != CompareOp::Eq {
                return Err(self.error(
                    "\"text\" supports only = and ~",
                    pos,
                    Some("e.g. text ~ \"login\""),
                ));
            }
            return self.fts(&self.literal(value)?);
        }
        let ordering = matches!(spec.kind, Kind::Number | Kind::Date | Kind::Day);
        let allowed: &[CompareOp] = if ordering {
            &[
                CompareOp::Eq,
                CompareOp::Ne,
                CompareOp::Lt,
                CompareOp::Le,
                CompareOp::Gt,
                CompareOp::Ge,
            ]
        } else {
            &[CompareOp::Eq, CompareOp::Ne]
        };
        if !allowed.contains(&op) {
            let ops: Vec<&str> = allowed.iter().map(|o| o.as_str()).collect();
            return Err(self.error(
                &format!("\"{name}\" does not support {}", op.as_str()),
                pos,
                Some(&format!("Use {}", ops.join(", "))),
            ));
        }
        let rhs = self.value(spec, value)?;
        let column = if spec.kind == Kind::Status {
            format!("{} COLLATE NOCASE", spec.column)
        } else {
            spec.column.into()
        };
        // `!=` keeps rows where the field is empty, which is what people expect from a filter.
        let sql_op = if op == CompareOp::Ne { "IS NOT" } else { op.as_str() };
        Ok(Fragment::new(format!("{column} {sql_op} {}", rhs.sql), rhs.params))
    }

    fn in_list(&self, spec: FieldSpec, values: &[Value], negated: bool) -> Result<Fragment> {
        if spec.kind == Kind::Fts {
            return Err(self.error(
                "\"text\" does not support IN",
                values.first().map_or(0, |v| v.pos),
                None,
            ));
        }
        let mut list = Vec::new();
        let mut params = Vec::new();
        for v in values {
            let part = self.value(spec, v)?;
            list.push(part.sql);
            params.extend(part.params);
        }
        let list = list.join(", ");
        let column = if spec.kind == Kind::Status {
            format!("{} COLLATE NOCASE", spec.column)
        } else {
            spec.column.into()
        };
        let sql = if negated {
            format!("({} IS NULL OR {column} NOT IN ({list}))", spec.column)
        } else {
            format!("{column} IN ({list})")
        };
        Ok(Fragment::new(sql, params))
    }

    fn empty(&self, spec: FieldSpec, negated: bool, pos: usize) -> Result<Fragment> {
        if spec.kind == Kind::Text {
            let op = if negated { "!=" } else { "=" };
            return Ok(Fragment::new(format!("{} {op} ''", spec.column), vec![]));
        }
        if !NULLABLE.contains(&spec.column) {
            return Err(self.error(
                "this field is never empty",
                pos,
                Some("IS EMPTY works on assignee, parent, cycle, estimate, due, body"),
            ));
        }
        let not = if negated { "NOT " } else { "" };
        Ok(Fragment::new(format!("{} IS {not}NULL", spec.column), vec![]))
    }

    /// A value as a SQL expression (usually a single `?`) for the given field.
    fn value(&self, spec: FieldSpec, value: &Value) -> Result<Fragment> {
        match spec.kind {
            Kind::Key => {
                let literal = self.literal(value)?;
                let literal = if spec.column == "i.key" { literal.to_uppercase() } else { literal };
                Ok(Fragment::new("?", vec![text(literal)]))
            }
            Kind::Text | Kind::Status => Ok(Fragment::new("?", vec![text(self.literal(value)?)])),
            Kind::Enum(allowed) => {
                let literal = self.literal(value)?.to_lowercase();
                if !allowed.contains(&literal.as_str()) {
                    return Err(self.error(
                        &format!("\"{literal}\" is not a valid value"),
                        value.pos,
                        Some(&format!("Use one of: {}", allowed.join(", "))),
                    ));
                }
                Ok(Fragment::new("?", vec![text(literal)]))
            }
            Kind::Number => match value.kind {
                ValueKind::Number(n) => Ok(Fragment::new("?", vec![SqlValue::Real(n)])),
                _ => Err(self.error("expected a number", value.pos, None)),
            },
            Kind::Date | Kind::Day => {
                let stamp = self.timestamp(value)?;
                let stamp =
                    if spec.kind == Kind::Day { stamp.chars().take(10).collect() } else { stamp };
                Ok(Fragment::new("?", vec![text(stamp)]))
            }
            Kind::Ref(kind) => self.reference(kind, value),
            Kind::Fts => Err(self.error("\"text\" needs ~", value.pos, None)),
        }
    }

    fn reference(&self, kind: RefKind, value: &Value) -> Result<Fragment> {
        if let ValueKind::Func(name) = &value.kind {
            if name == "me" && kind == RefKind::Principal {
                let me = self.ctx.me.ok_or_else(|| {
                    self.error("me() needs a signed-in principal", value.pos, None)
                })?;
                return Ok(Fragment::new("?", vec![text(me)]));
            }
            if (name == "currentcycle" || name == "opencycle") && kind == RefKind::Cycle {
                return Ok(Fragment::new(
                    "(SELECT c.id FROM cycles c WHERE c.collection_id = i.collection_id AND c.state = 'active')",
                    vec![],
                ));
            }
            return Err(self.error(&format!("{name}() cannot be used here"), value.pos, None));
        }
        let literal = self.literal(value)?;
        Ok(match kind {
            RefKind::Principal => Fragment::new(
                "(SELECT p.id FROM principals p WHERE p.id = ? OR p.name = ? COLLATE NOCASE)",
                vec![text(literal.clone()), text(literal)],
            ),
            RefKind::Collection => Fragment::new(
                "(SELECT c.id FROM collections c WHERE c.id = ? OR c.key = ? COLLATE NOCASE)",
                vec![text(literal.clone()), text(literal)],
            ),
            RefKind::Cycle => Fragment::new(
                "(SELECT c.id FROM cycles c WHERE c.collection_id = i.collection_id AND (c.id = ? OR c.name = ? COLLATE NOCASE))",
                vec![text(literal.clone()), text(literal)],
            ),
            RefKind::Item => Fragment::new(
                "(SELECT p.id FROM items p WHERE p.id = ? OR p.key = ?)",
                vec![text(literal.clone()), text(literal.to_uppercase())],
            ),
        })
    }

    fn timestamp(&self, value: &Value) -> Result<String> {
        match &value.kind {
            ValueKind::Duration { ms, .. } => {
                // Huge offsets such as 999999w fall outside the calendar: refuse, never panic.
                let at =
                    self.ctx.now.checked_add(Duration::milliseconds(*ms)).ok_or_else(|| {
                        self.error(
                            "Relative date is out of range",
                            value.pos,
                            Some("Use a smaller offset, e.g. -30d"),
                        )
                    })?;
                Ok(iso(at))
            }
            ValueKind::Func(name) if name == "now" => Ok(iso(self.ctx.now)),
            ValueKind::Func(name) if name == "today" => {
                Ok(iso(self.ctx.now).chars().take(10).collect())
            }
            ValueKind::Func(name) => Err(self.error(
                &format!("{name}() is not a date"),
                value.pos,
                Some("Use now(), today(), -7d or 2026-01-31"),
            )),
            _ => {
                let literal = self.literal(value)?;
                if !is_date_literal(&literal) {
                    return Err(self.error(
                        &format!("\"{literal}\" is not a date"),
                        value.pos,
                        Some("Use 2026-01-31, -7d, now() or today()"),
                    ));
                }
                Ok(literal)
            }
        }
    }

    fn fts(&self, literal: &str) -> Result<Fragment> {
        let terms: Vec<&str> = literal.split_whitespace().collect();
        if terms.is_empty() {
            return Err(RoduError::invalid("Search text is empty"));
        }
        // Quote every term so FTS5 operators typed by a person are treated as plain words.
        let matched: Vec<String> =
            terms.iter().map(|t| format!("\"{}\"", t.replace('"', "\"\""))).collect();
        Ok(Fragment::new(
            "i.id IN (SELECT item_id FROM items_fts WHERE items_fts MATCH ?)",
            vec![text(matched.join(" "))],
        ))
    }

    fn literal(&self, value: &Value) -> Result<String> {
        match &value.kind {
            ValueKind::String(s) => Ok(s.clone()),
            ValueKind::Number(n) => Ok(format_number(*n)),
            ValueKind::Duration { raw, .. } => Ok(raw.clone()),
            ValueKind::Func(name) => {
                Err(self.error(&format!("{name}() cannot be used here"), value.pos, None))
            }
        }
    }
}

/// Numbers as JavaScript prints them: `3`, not `3.0`.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { n.to_string() }
}

/// `2026-01-31`, optionally followed by a time such as `T09:00:00Z`.
fn is_date_literal(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 10 {
        return false;
    }
    let date_ok = b[..10]
        .iter()
        .enumerate()
        .all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    if !date_ok {
        return false;
    }
    match &s[10..] {
        "" => true,
        rest => {
            let rest = rest.strip_prefix('T').unwrap_or("!");
            let rest = rest.strip_suffix('Z').unwrap_or(rest);
            !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit() || c == b':' || c == b'.')
        }
    }
}

/// Compiles a parsed query; `source` is only used to point at errors.
pub fn compile_query(
    query: &Query,
    ctx: &CompileContext<'_>,
    source: &str,
) -> Result<CompiledQuery> {
    let compiler = Compiler { ctx, source };
    let filter = match &query.filter {
        Some(e) => compiler.expr(e)?,
        None => Fragment::new("1 = 1", vec![]),
    };
    Ok(CompiledQuery {
        where_sql: filter.sql,
        params: filter.params,
        order_by: compiler.order_by(&query.order_by)?,
    })
}

/// Parses and compiles in one step.
pub fn to_sql(source: &str, ctx: &CompileContext<'_>) -> Result<CompiledQuery> {
    compile_query(&rodu_core::query::parse_query(source)?, ctx, source)
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn ctx() -> CompileContext<'static> {
        CompileContext { me: Some("user-1"), now: datetime!(2026-10-08 00:00 UTC) }
    }

    fn texts(params: &[SqlValue]) -> Vec<String> {
        params
            .iter()
            .map(|p| match p {
                SqlValue::Text(t) => t.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn compiles_an_empty_query_to_match_everything() {
        let sql = to_sql("", &ctx()).unwrap();
        assert_eq!(sql.where_sql, "1 = 1");
        assert!(sql.params.is_empty());
        assert_eq!(sql.order_by, "i.rank ASC, i.id ASC");
    }

    #[test]
    fn binds_every_value_as_a_parameter() {
        let sql = to_sql("title ~ \"x' OR 1=1 --\" AND status = 'In Progress'", &ctx()).unwrap();
        assert!(!sql.where_sql.contains("1=1"));
        assert_eq!(texts(&sql.params), ["%x' OR 1=1 --%", "In Progress"]);
    }

    #[test]
    fn resolves_me_and_relative_dates() {
        let sql = to_sql("assignee = me() AND updated >= -1d", &ctx()).unwrap();
        assert_eq!(texts(&sql.params), ["user-1", "2026-10-07T00:00:00.000Z"]);
    }

    #[test]
    fn treats_not_equal_as_including_empty_fields() {
        assert_eq!(to_sql("assignee != me()", &ctx()).unwrap().where_sql, "i.assignee_id IS NOT ?");
    }

    #[test]
    fn validates_enum_values_with_a_hint() {
        let err = to_sql("priority = critical", &ctx()).err().unwrap();
        assert!(err.hint.unwrap().contains("urgent"));
    }

    #[test]
    fn rejects_unknown_fields_and_misplaced_functions() {
        assert!(
            to_sql("password = x", &ctx())
                .err()
                .unwrap()
                .message
                .contains("unknown field \"password\"")
        );
        assert!(
            to_sql("title = me()", &ctx())
                .err()
                .unwrap()
                .message
                .contains("me() cannot be used here")
        );
        assert!(
            to_sql("title ; drop", &ctx()).err().unwrap().message.contains("unexpected character")
        );
    }

    #[test]
    fn quotes_full_text_terms_so_fts_operators_are_literal() {
        let sql = to_sql("text ~ \"login NEAR crash\"", &ctx()).unwrap();
        assert_eq!(texts(&sql.params), ["\"login\" \"NEAR\" \"crash\""]);
    }

    #[test]
    fn escapes_like_wildcards() {
        assert_eq!(texts(&to_sql("title ~ \"100%_\"", &ctx()).unwrap().params), ["%100\\%\\_%"]);
    }

    #[test]
    fn orders_by_priority_with_a_stable_tie_breaker() {
        let sql = to_sql("ORDER BY priority", &ctx()).unwrap();
        assert!(sql.order_by.starts_with("CASE i.priority "));
        assert!(sql.order_by.ends_with("END ASC, i.rank ASC, i.id ASC"));
        assert!(to_sql("ORDER BY body", &ctx()).err().unwrap().message.contains("cannot order by"));
    }

    #[test]
    fn requires_a_principal_for_me() {
        let anonymous = CompileContext { me: None, ..ctx() };
        assert!(to_sql("assignee = me()", &anonymous).err().unwrap().message.contains("signed-in"));
    }

    #[test]
    fn checks_dates() {
        assert!(to_sql("due < 2026-01-31", &ctx()).is_ok());
        assert!(to_sql("created > 2026-01-31T09:00:00Z", &ctx()).is_ok());
        assert!(to_sql("due < tomorrow", &ctx()).is_err());
        assert!(to_sql("due < yesterday()", &ctx()).is_err());
    }

    #[test]
    fn rejects_relative_dates_past_the_calendar_instead_of_panicking() {
        for query in ["updated > 999999w", "updated > -999999w"] {
            let error = to_sql(query, &ctx()).err().expect("an error, not a panic");
            assert!(error.message.contains("out of range"), "{}", error.message);
        }
    }
}
