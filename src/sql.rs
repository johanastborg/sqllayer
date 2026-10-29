use crate::morton::{
    encode_city_21, encode_country_21, encode_date_21, encode_index_21, MAX_COORD_21,
};
use crate::schema::{Column, CustomerRow, Value};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, Ident, ObjectName, Offset, OrderByExpr,
    Query, SelectItem, SetExpr, Statement, Value as SqlValue,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SqlError {
    #[error("SQL syntax error: {0}")]
    ParseError(String),
    #[error("Unsupported SQL statement: {0}")]
    Unsupported(String),
    #[error("Unknown column: '{0}'")]
    UnknownColumn(String),
    #[error("Unknown table: '{0}' (expected 'customers')")]
    UnknownTable(String),
}

/// Top-level parsed SQL command.
#[derive(Debug, Clone)]
pub enum SqlCommand {
    Select(SelectQuery),
    Explain(SelectQuery),
    Describe(String),
    ShowTables,
    ShowIndexes,
}

/// Projection item in a `SELECT` clause.
#[derive(Debug, Clone)]
pub enum ProjectionItem {
    Wildcard,
    Column { col: Column, alias: Option<String> },
    CountStar { alias: Option<String> },
    Aggregate {
        func: AggFunc,
        col: Column,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Min,
    Max,
}

impl AggFunc {
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
        }
    }
}

/// Sort specification in `ORDER BY`.
#[derive(Debug, Clone)]
pub struct OrderByField {
    pub col: Column,
    pub ascending: bool,
}

/// Parsed `SELECT` query.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub table: String,
    pub projections: Vec<ProjectionItem>,
    pub filter: Option<Predicate>,
    pub order_by: Vec<OrderByField>,
    pub limit: Option<usize>,
    pub offset: usize,
}

/// Compiled `WHERE` predicate tree.
#[derive(Debug, Clone)]
pub enum Predicate {
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
    Comparison {
        col: Column,
        op: CmpOp,
        val: Value,
    },
    Between {
        col: Column,
        low: Value,
        high: Value,
        negated: bool,
    },
    Like {
        col: Column,
        pattern: String,
        negated: bool,
    },
    InList {
        col: Column,
        list: Vec<Value>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl Predicate {
    /// Evaluates the predicate against a deserialized `CustomerRow`.
    pub fn matches(&self, row: &CustomerRow) -> bool {
        match self {
            Predicate::And(a, b) => a.matches(row) && b.matches(row),
            Predicate::Or(a, b) => a.matches(row) || b.matches(row),
            Predicate::Not(inner) => !inner.matches(row),
            Predicate::Comparison { col, op, val } => {
                let actual = row.get(*col);
                match op {
                    CmpOp::Eq => actual.cmp(val).is_eq(),
                    CmpOp::Neq => actual.cmp(val).is_ne(),
                    CmpOp::Lt => actual < *val,
                    CmpOp::Lte => actual <= *val,
                    CmpOp::Gt => actual > *val,
                    CmpOp::Gte => actual >= *val,
                }
            }
            Predicate::Between {
                col,
                low,
                high,
                negated,
            } => {
                let actual = row.get(*col);
                let inside = actual >= *low && actual <= *high;
                if *negated { !inside } else { inside }
            }
            Predicate::Like {
                col,
                pattern,
                negated,
            } => {
                let actual = row.get(*col).as_str();
                let matched = like_match(&actual, pattern);
                if *negated { !matched } else { matched }
            }
            Predicate::InList {
                col,
                list,
                negated,
            } => {
                let actual = row.get(*col);
                let found = list.iter().any(|v| actual.cmp(v).is_eq());
                if *negated { !found } else { found }
            }
        }
    }
}

/// Case-insensitive SQL `LIKE` pattern matcher supporting `%` and `_` wildcards.
fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

/// Multi-dimensional bounding box in 21-bit Morton coordinate space extracted from a SQL `WHERE` clause.
#[derive(Debug, Clone)]
pub struct MortonQueryBounds {
    pub min_index: u32,
    pub max_index: u32,
    pub constrained_index: bool,

    pub min_date: u32,
    pub max_date: u32,
    pub constrained_date: bool,

    pub min_country: u32,
    pub max_country: u32,
    pub constrained_country: bool,

    pub min_city: u32,
    pub max_city: u32,
    pub constrained_city: bool,
}

impl MortonQueryBounds {
    /// Creates an unconstrained bounding box spanning the active table domain.
    pub fn unconstrained(max_table_index: u32) -> Self {
        Self {
            min_index: 1,
            max_index: encode_index_21(max_table_index.max(1)),
            constrained_index: false,

            min_date: encode_date_21("2020-01-01"),
            max_date: encode_date_21("2022-05-31"),
            constrained_date: false,

            min_country: 0,
            max_country: encode_country_21("zzzz", true),
            constrained_country: false,

            min_city: 0,
            max_city: encode_city_21("zzzz", true),
            constrained_city: false,
        }
    }

    /// Extracts tight Morton coordinate bounds from a predicate tree.
    pub fn from_predicate(pred: Option<&Predicate>, max_table_index: u32) -> Self {
        let mut bounds = Self::unconstrained(max_table_index);
        if let Some(p) = pred {
            bounds.apply_predicate(p, max_table_index);
        }
        bounds
    }

    fn apply_predicate(&mut self, pred: &Predicate, max_table_index: u32) {
        match pred {
            Predicate::And(a, b) => {
                self.apply_predicate(a, max_table_index);
                self.apply_predicate(b, max_table_index);
            }
            Predicate::Or(a, b) => {
                let mut left = Self::unconstrained(max_table_index);
                left.apply_predicate(a, max_table_index);
                let mut right = Self::unconstrained(max_table_index);
                right.apply_predicate(b, max_table_index);

                if left.constrained_index && right.constrained_index {
                    self.min_index = self.min_index.max(left.min_index.min(right.min_index));
                    self.max_index = self.max_index.min(left.max_index.max(right.max_index));
                    self.constrained_index = true;
                }
                if left.constrained_date && right.constrained_date {
                    self.min_date = self.min_date.max(left.min_date.min(right.min_date));
                    self.max_date = self.max_date.min(left.max_date.max(right.max_date));
                    self.constrained_date = true;
                }
                if left.constrained_country && right.constrained_country {
                    self.min_country = self
                        .min_country
                        .max(left.min_country.min(right.min_country));
                    self.max_country = self
                        .max_country
                        .min(left.max_country.max(right.max_country));
                    self.constrained_country = true;
                }
                if left.constrained_city && right.constrained_city {
                    self.min_city = self.min_city.max(left.min_city.min(right.min_city));
                    self.max_city = self.max_city.min(left.max_city.max(right.max_city));
                    self.constrained_city = true;
                }
            }
            Predicate::Not(_) => {}
            Predicate::Comparison { col, op, val } => match col {
                Column::Index => {
                    if let Some(v) = val.as_u32() {
                        let c = encode_index_21(v);
                        match op {
                            CmpOp::Eq => {
                                self.min_index = self.min_index.max(c);
                                self.max_index = self.max_index.min(c);
                                self.constrained_index = true;
                            }
                            CmpOp::Gt => {
                                self.min_index =
                                    self.min_index.max(c.saturating_add(1).min(MAX_COORD_21));
                                self.constrained_index = true;
                            }
                            CmpOp::Gte => {
                                self.min_index = self.min_index.max(c);
                                self.constrained_index = true;
                            }
                            CmpOp::Lt => {
                                self.max_index = self.max_index.min(c.saturating_sub(1));
                                self.constrained_index = true;
                            }
                            CmpOp::Lte => {
                                self.max_index = self.max_index.min(c);
                                self.constrained_index = true;
                            }
                            CmpOp::Neq => {}
                        }
                    }
                }
                Column::SubscriptionDate => {
                    let s = val.as_str();
                    let d = encode_date_21(&s);
                    match op {
                        CmpOp::Eq => {
                            self.min_date = self.min_date.max(d);
                            self.max_date = self.max_date.min(d);
                            self.constrained_date = true;
                        }
                        CmpOp::Gt | CmpOp::Gte => {
                            self.min_date = self.min_date.max(d);
                            self.constrained_date = true;
                        }
                        CmpOp::Lt | CmpOp::Lte => {
                            self.max_date = self.max_date.min(d);
                            self.constrained_date = true;
                        }
                        CmpOp::Neq => {}
                    }
                }
                Column::Country => {
                    let s = val.as_str();
                    let low = encode_country_21(&s, false);
                    let high = encode_country_21(&s, true);
                    match op {
                        CmpOp::Eq => {
                            self.min_country = self.min_country.max(low);
                            self.max_country = self.max_country.min(high);
                            self.constrained_country = true;
                        }
                        CmpOp::Gt | CmpOp::Gte => {
                            self.min_country = self.min_country.max(low);
                            self.constrained_country = true;
                        }
                        CmpOp::Lt | CmpOp::Lte => {
                            self.max_country = self.max_country.min(high);
                            self.constrained_country = true;
                        }
                        CmpOp::Neq => {}
                    }
                }
                Column::City => {
                    let s = val.as_str();
                    let low = encode_city_21(&s, false);
                    let high = encode_city_21(&s, true);
                    match op {
                        CmpOp::Eq => {
                            self.min_city = self.min_city.max(low);
                            self.max_city = self.max_city.min(high);
                            self.constrained_city = true;
                        }
                        CmpOp::Gt | CmpOp::Gte => {
                            self.min_city = self.min_city.max(low);
                            self.constrained_city = true;
                        }
                        CmpOp::Lt | CmpOp::Lte => {
                            self.max_city = self.max_city.min(high);
                            self.constrained_city = true;
                        }
                        CmpOp::Neq => {}
                    }
                }
                _ => {}
            },
            Predicate::Between {
                col,
                low,
                high,
                negated,
            } => {
                if !*negated {
                    self.apply_predicate(
                        &Predicate::Comparison {
                            col: *col,
                            op: CmpOp::Gte,
                            val: low.clone(),
                        },
                        max_table_index,
                    );
                    self.apply_predicate(
                        &Predicate::Comparison {
                            col: *col,
                            op: CmpOp::Lte,
                            val: high.clone(),
                        },
                        max_table_index,
                    );
                }
            }
            Predicate::Like {
                col,
                pattern,
                negated,
            } => {
                if !*negated {
                    // Extract constant prefix before any '%' or '_' wildcard
                    let prefix: String = pattern
                        .chars()
                        .take_while(|&c| c != '%' && c != '_')
                        .collect();
                    if !prefix.is_empty() {
                        match col {
                            Column::Country => {
                                let low = encode_country_21(&prefix, false);
                                let high = encode_country_21(&prefix, true);
                                self.min_country = self.min_country.max(low);
                                self.max_country = self.max_country.min(high);
                                self.constrained_country = true;
                            }
                            Column::City => {
                                let low = encode_city_21(&prefix, false);
                                let high = encode_city_21(&prefix, true);
                                self.min_city = self.min_city.max(low);
                                self.max_city = self.max_city.min(high);
                                self.constrained_city = true;
                            }
                            Column::SubscriptionDate => {
                                if prefix.len() >= 4 {
                                    let low_date = match prefix.len() {
                                        4 => format!("{}-01-01", prefix),
                                        7 => format!("{}-01", prefix),
                                        _ => prefix.clone(),
                                    };
                                    let high_date = match prefix.len() {
                                        4 => format!("{}-12-31", prefix),
                                        7 => format!("{}-31", prefix),
                                        _ => prefix,
                                    };
                                    self.min_date = self.min_date.max(encode_date_21(&low_date));
                                    self.max_date = self.max_date.min(encode_date_21(&high_date));
                                    self.constrained_date = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Predicate::InList {
                col,
                list,
                negated,
            } => {
                if !*negated && !list.is_empty() {
                    if let (Some(min_v), Some(max_v)) = (list.iter().min(), list.iter().max()) {
                        self.apply_predicate(
                            &Predicate::Between {
                                col: *col,
                                low: min_v.clone(),
                                high: max_v.clone(),
                                negated: false,
                            },
                            max_table_index,
                        );
                    }
                }
            }
        }
    }
}

/// Parses a raw SQL string into a `SqlCommand`.
pub fn parse_sql(sql: &str) -> Result<SqlCommand, SqlError> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let upper = trimmed.to_ascii_uppercase();

    if upper == "SHOW TABLES" {
        return Ok(SqlCommand::ShowTables);
    }
    if upper == "SHOW INDEXES" || upper == "SHOW INDEX" {
        return Ok(SqlCommand::ShowIndexes);
    }
    if let Some(rest) = upper.strip_prefix("DESCRIBE ").or_else(|| upper.strip_prefix("DESC ")) {
        let table = rest.trim().to_ascii_lowercase();
        return Ok(SqlCommand::Describe(table));
    }

    let dialect = GenericDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, trimmed).map_err(|e| SqlError::ParseError(e.to_string()))?;

    if statements.len() != 1 {
        return Err(SqlError::Unsupported(
            "Expected exactly one SQL statement".to_string(),
        ));
    }

    match statements.pop().unwrap() {
        Statement::Query(query) => Ok(SqlCommand::Select(compile_query(*query)?)),
        Statement::Explain { statement, .. } => match *statement {
            Statement::Query(query) => Ok(SqlCommand::Explain(compile_query(*query)?)),
            other => Err(SqlError::Unsupported(format!(
                "EXPLAIN only supports SELECT queries, got: {other}"
            ))),
        },
        Statement::ShowTables { .. } => Ok(SqlCommand::ShowTables),
        other => Err(SqlError::Unsupported(format!("{other}"))),
    }
}

fn compile_query(query: Query) -> Result<SelectQuery, SqlError> {
    let select = match *query.body {
        SetExpr::Select(s) => s,
        other => {
            return Err(SqlError::Unsupported(format!(
                "Only simple SELECT is supported, got: {other}"
            )))
        }
    };

    if select.from.len() != 1 {
        return Err(SqlError::Unsupported(
            "Expected exactly one table in FROM clause".to_string(),
        ));
    }

    let table_name = extract_table_name(&select.from[0].relation)?;
    if table_name.to_ascii_lowercase() != "customers" {
        return Err(SqlError::UnknownTable(table_name));
    }

    let mut projections = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => projections.push(ProjectionItem::Wildcard),
            SelectItem::UnnamedExpr(expr) => {
                projections.push(compile_projection_expr(expr, None)?);
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                projections.push(compile_projection_expr(expr, Some(alias.value.clone()))?);
            }
            other => {
                return Err(SqlError::Unsupported(format!(
                    "Unsupported projection: {other}"
                )))
            }
        }
    }

    let filter = match &select.selection {
        Some(expr) => Some(compile_predicate(expr)?),
        None => None,
    };

    let mut order_by = Vec::new();
    if let Some(ob) = query.order_by {
        for OrderByExpr { expr, asc, .. } in ob.exprs {
            let col = expr_to_column(&expr)?;
            order_by.push(OrderByField {
                col,
                ascending: asc.unwrap_or(true),
            });
        }
    }

    let limit = match query.limit {
        Some(expr) => Some(
            expr_to_value(&expr)?
                .as_u32()
                .ok_or_else(|| SqlError::Unsupported("LIMIT must be an integer".to_string()))?
                as usize,
        ),
        None => None,
    };

    let offset = match query.offset {
        Some(Offset { value, .. }) => expr_to_value(&value)?
            .as_u32()
            .ok_or_else(|| SqlError::Unsupported("OFFSET must be an integer".to_string()))?
            as usize,
        None => 0,
    };

    Ok(SelectQuery {
        table: "customers".to_string(),
        projections,
        filter,
        order_by,
        limit,
        offset,
    })
}

fn extract_table_name(factor: &sqlparser::ast::TableFactor) -> Result<String, SqlError> {
    match factor {
        sqlparser::ast::TableFactor::Table { name, .. } => Ok(object_name_to_string(name)),
        other => Err(SqlError::Unsupported(format!(
            "Unsupported table reference: {other}"
        ))),
    }
}

fn object_name_to_string(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|ident| ident.value.as_str())
        .collect::<Vec<_>>()
        .join("_")
}

fn compile_projection_expr(
    expr: &Expr,
    alias: Option<String>,
) -> Result<ProjectionItem, SqlError> {
    match expr {
        Expr::Function(func) => {
            let fname = object_name_to_string(&func.name).to_ascii_uppercase();
            let args = match &func.args {
                sqlparser::ast::FunctionArguments::List(list) => &list.args,
                sqlparser::ast::FunctionArguments::None => &[][..],
                sqlparser::ast::FunctionArguments::Subquery(_) => {
                    return Err(SqlError::Unsupported(
                        "Subquery in function args not supported".to_string(),
                    ))
                }
            };

            if fname == "COUNT" {
                if args.is_empty()
                    || matches!(
                        args.first(),
                        Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard))
                    )
                {
                    return Ok(ProjectionItem::CountStar { alias });
                }
            }

            let agg_func = match fname.as_str() {
                "COUNT" => AggFunc::Count,
                "MIN" => AggFunc::Min,
                "MAX" => AggFunc::Max,
                _ => {
                    return Err(SqlError::Unsupported(format!(
                        "Unsupported function: {fname}"
                    )))
                }
            };

            if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) = args.first() {
                let col = expr_to_column(arg_expr)?;
                Ok(ProjectionItem::Aggregate {
                    func: agg_func,
                    col,
                    alias,
                })
            } else {
                Err(SqlError::Unsupported(format!(
                    "Invalid arguments to {fname}"
                )))
            }
        }
        _ => {
            let col = expr_to_column(expr)?;
            Ok(ProjectionItem::Column { col, alias })
        }
    }
}

fn compile_predicate(expr: &Expr) -> Result<Predicate, SqlError> {
    match expr {
        Expr::Nested(inner) => compile_predicate(inner),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Not,
            expr: inner,
        } => Ok(Predicate::Not(Box::new(compile_predicate(inner)?))),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => Ok(Predicate::And(
                Box::new(compile_predicate(left)?),
                Box::new(compile_predicate(right)?),
            )),
            BinaryOperator::Or => Ok(Predicate::Or(
                Box::new(compile_predicate(left)?),
                Box::new(compile_predicate(right)?),
            )),
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq => {
                // Support both `col OP val` and `val OP col`
                if let Ok(col) = expr_to_column(left) {
                    let val = expr_to_value(right)?;
                    let cmp_op = match op {
                        BinaryOperator::Eq => CmpOp::Eq,
                        BinaryOperator::NotEq => CmpOp::Neq,
                        BinaryOperator::Lt => CmpOp::Lt,
                        BinaryOperator::LtEq => CmpOp::Lte,
                        BinaryOperator::Gt => CmpOp::Gt,
                        BinaryOperator::GtEq => CmpOp::Gte,
                        _ => unreachable!(),
                    };
                    Ok(Predicate::Comparison {
                        col,
                        op: cmp_op,
                        val,
                    })
                } else if let Ok(col) = expr_to_column(right) {
                    let val = expr_to_value(left)?;
                    let cmp_op = match op {
                        BinaryOperator::Eq => CmpOp::Eq,
                        BinaryOperator::NotEq => CmpOp::Neq,
                        BinaryOperator::Lt => CmpOp::Gt,
                        BinaryOperator::LtEq => CmpOp::Gte,
                        BinaryOperator::Gt => CmpOp::Lt,
                        BinaryOperator::GtEq => CmpOp::Lte,
                        _ => unreachable!(),
                    };
                    Ok(Predicate::Comparison {
                        col,
                        op: cmp_op,
                        val,
                    })
                } else {
                    Err(SqlError::Unsupported(format!(
                        "Expected column reference in comparison: {expr}"
                    )))
                }
            }
            other => Err(SqlError::Unsupported(format!(
                "Unsupported operator: {other}"
            ))),
        },
        Expr::Between {
            expr: target,
            negated,
            low,
            high,
        } => {
            let col = expr_to_column(target)?;
            let low_val = expr_to_value(low)?;
            let high_val = expr_to_value(high)?;
            Ok(Predicate::Between {
                col,
                low: low_val,
                high: high_val,
                negated: *negated,
            })
        }
        Expr::Like {
            negated,
            expr: target,
            pattern,
            ..
        }
        | Expr::ILike {
            negated,
            expr: target,
            pattern,
            ..
        } => {
            let col = expr_to_column(target)?;
            let pat_str = expr_to_value(pattern)?.as_str();
            Ok(Predicate::Like {
                col,
                pattern: pat_str,
                negated: *negated,
            })
        }
        Expr::InList {
            expr: target,
            list,
            negated,
        } => {
            let col = expr_to_column(target)?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                vals.push(expr_to_value(item)?);
            }
            Ok(Predicate::InList {
                col,
                list: vals,
                negated: *negated,
            })
        }
        other => Err(SqlError::Unsupported(format!(
            "Unsupported WHERE expression: {other}"
        ))),
    }
}

fn expr_to_column(expr: &Expr) -> Result<Column, SqlError> {
    match expr {
        Expr::Identifier(Ident { value, .. }) => {
            Column::from_name(value).ok_or_else(|| SqlError::UnknownColumn(value.clone()))
        }
        Expr::CompoundIdentifier(idents) => {
            let last = idents
                .last()
                .map(|i| i.value.as_str())
                .unwrap_or_default();
            Column::from_name(last).ok_or_else(|| SqlError::UnknownColumn(last.to_string()))
        }
        other => Err(SqlError::UnknownColumn(other.to_string())),
    }
}

fn expr_to_value(expr: &Expr) -> Result<Value, SqlError> {
    match expr {
        Expr::Value(v) => match v {
            SqlValue::Number(n, _) => {
                if let Ok(i) = n.parse::<i64>() {
                    Ok(Value::Int(i))
                } else {
                    Ok(Value::Str(n.clone()))
                }
            }
            SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => {
                Ok(Value::Str(s.clone()))
            }
            other => Err(SqlError::Unsupported(format!("Unsupported literal: {other}"))),
        },
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr: inner,
        } => {
            let v = expr_to_value(inner)?;
            if let Some(i) = v.as_i64() {
                Ok(Value::Int(-i))
            } else {
                Err(SqlError::Unsupported("Invalid negative literal".to_string()))
            }
        }
        Expr::Identifier(Ident { value, .. }) => Ok(Value::Str(value.clone())),
        other => Err(SqlError::Unsupported(format!(
            "Expected literal value, got: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_select_and_bounds() {
        let cmd = parse_sql(
            "SELECT index, first_name, country, subscription_date FROM customers \
             WHERE index BETWEEN 100 AND 500 \
               AND subscription_date >= '2021-01-01' \
               AND country = 'Costa Rica' \
             ORDER BY index ASC LIMIT 10",
        )
        .unwrap();

        match cmd {
            SqlCommand::Select(q) => {
                assert_eq!(q.table, "customers");
                assert_eq!(q.projections.len(), 4);
                assert_eq!(q.limit, Some(10));
                let bounds = MortonQueryBounds::from_predicate(q.filter.as_ref(), 2_000_000);
                assert!(bounds.constrained_index);
                assert_eq!(bounds.min_index, 100);
                assert_eq!(bounds.max_index, 500);
                assert!(bounds.constrained_date);
                assert!(bounds.constrained_country);
            }
            _ => panic!("Expected Select"),
        }
    }
}

