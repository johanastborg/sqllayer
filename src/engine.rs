use crate::morton::{
    decompose_morton_2d, decompose_morton_3d, encode_city_21, encode_country_21, encode_date_21,
    encode_index_21, format_morton_hex, morton_decode_2d, morton_decode_3d, morton_encode_2d,
    morton_encode_3d, parse_morton_member, MortonInterval,
};
use crate::schema::{Column, CustomerRow};
use crate::sql::{
    AggFunc, MortonQueryBounds, ProjectionItem, SelectQuery, SqlCommand, SqlError,
};
use redis::{Client, Commands, Connection};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Instant;
use thiserror::Error;

pub const KEY_ROWS: &str = "sqllayer:customers:rows";
pub const KEY_MORTON_3D: &str = "sqllayer:customers:morton3d";
pub const KEY_MORTON_2D_ID: &str = "sqllayer:customers:morton2d_id";
pub const KEY_MORTON_2D_CC: &str = "sqllayer:customers:morton2d_cc";
pub const KEY_META: &str = "sqllayer:customers:meta";

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQL error: {0}")]
    Sql(#[from] SqlError),
}

/// Which Morton Z-order curve index in Redis was chosen by the query planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MortonIndexChoice {
    /// 3D Morton Code `(index, subscription_date, country)` (`u64`, 21 bits × 3)
    Morton3D,
    /// 2D Morton Code `(index, subscription_date)` (`u64`, staggered 21-bit coordinates)
    Morton2DIndexDate,
    /// 2D Morton Code `(country, city)` (`u64`, staggered 21-bit coordinates)
    Morton2DCountryCity,
}

impl MortonIndexChoice {
    pub fn redis_key(&self) -> &'static str {
        match self {
            MortonIndexChoice::Morton3D => KEY_MORTON_3D,
            MortonIndexChoice::Morton2DIndexDate => KEY_MORTON_2D_ID,
            MortonIndexChoice::Morton2DCountryCity => KEY_MORTON_2D_CC,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            MortonIndexChoice::Morton3D => {
                "3D Morton Z-Order Curve (index, subscription_date, country)"
            }
            MortonIndexChoice::Morton2DIndexDate => {
                "2D Morton Z-Order Curve (index, subscription_date)"
            }
            MortonIndexChoice::Morton2DCountryCity => "2D Morton Z-Order Curve (country, city)",
        }
    }
}

/// Detailed query plan produced by the Morton Query Planner.
#[derive(Debug, Clone)]
pub struct MortonQueryPlan {
    pub index_choice: MortonIndexChoice,
    pub bounds: MortonQueryBounds,
    pub intervals: Vec<MortonInterval>,
    pub max_intervals_budget: usize,
    pub zspace_pruned_pct: f64,
}

/// Telemetry from executing a SQL query over Redis using Morton codes.
#[derive(Debug, Clone, Default)]
pub struct ExecutionStats {
    pub index_used: String,
    pub redis_key: String,
    pub intervals_count: usize,
    pub zspace_pruned_pct: f64,
    pub zset_candidates_scanned: usize,
    pub morton_prefilter_passed: usize,
    pub rows_fetched_hmget: usize,
    pub rows_matched: usize,
    pub elapsed_ms: f64,
}

/// Result of executing any SQL command.
#[derive(Debug)]
pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub stats: Option<ExecutionStats>,
}

pub struct RedisSqlEngine {
    client: Client,
    max_intervals: usize,
}

impl RedisSqlEngine {
    pub fn connect(redis_url: &str) -> Result<Self, EngineError> {
        let client = Client::open(redis_url)?;
        // Verify connectivity immediately
        let mut conn = client.get_connection()?;
        let _: String = redis::cmd("PING").query(&mut conn)?;
        Ok(Self {
            client,
            max_intervals: 512,
        })
    }

    pub fn get_connection(&self) -> Result<Connection, EngineError> {
        Ok(self.client.get_connection()?)
    }

    /// Returns the number of rows currently indexed in Redis, or 0 if not loaded.
    pub fn indexed_row_count(&self) -> Result<u32, EngineError> {
        let mut conn = self.get_connection()?;
        let count: Option<u32> = conn.hget(KEY_META, "row_count")?;
        Ok(count.unwrap_or(0))
    }

    /// Returns the maximum `index` value stored in Redis metadata.
    pub fn max_table_index(&self) -> Result<u32, EngineError> {
        let mut conn = self.get_connection()?;
        let max_idx: Option<u32> = conn.hget(KEY_META, "max_index")?;
        Ok(max_idx.unwrap_or(2_000_000))
    }

    /// Ingests `customers-2000000.csv` (or a prefix up to `limit` rows) into local Redis,
    /// building the row store and Morton code ZSET indexes using batched Redis pipelines.
    pub fn ingest_csv<P: AsRef<Path>>(
        &self,
        csv_path: P,
        limit: Option<usize>,
        flush_existing: bool,
    ) -> Result<(usize, f64), EngineError> {
        let start = Instant::now();
        let mut conn = self.get_connection()?;

        if flush_existing {
            let _: () = redis::pipe()
                .del(KEY_ROWS)
                .del(KEY_MORTON_3D)
                .del(KEY_MORTON_2D_ID)
                .del(KEY_MORTON_2D_CC)
                .del(KEY_META)
                .query(&mut conn)?;
        }

        let file = File::open(csv_path)?;
        let reader = BufReader::with_capacity(4 * 1024 * 1024, file);
        let mut csv_reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(reader);

        let batch_size = 5_000usize;
        let mut pipe = redis::pipe();
        let mut in_batch = 0usize;
        let mut total_loaded = 0usize;
        let mut max_index = 0u32;

        let mut record = csv::StringRecord::new();
        while csv_reader.read_record(&mut record)? {
            if let Some(max_rows) = limit {
                if total_loaded >= max_rows {
                    break;
                }
            }

            let Some(row) = CustomerRow::from_string_record(&record) else {
                continue;
            };

            let x_idx = encode_index_21(row.index);
            let y_date = encode_date_21(&row.subscription_date);
            let z_country = encode_country_21(&row.country, false);
            let w_city = encode_city_21(&row.city, false);

            let m3d = morton_encode_3d(x_idx, y_date, z_country);
            let m2d_id = morton_encode_2d(x_idx, y_date);
            let m2d_cc = morton_encode_2d(z_country, w_city);

            let member_3d = format!("{}:{}" , format_morton_hex(m3d), row.index);
            let member_2d_id = format!("{}:{}", format_morton_hex(m2d_id), row.index);
            let member_2d_cc = format!("{}:{}", format_morton_hex(m2d_cc), row.index);

            pipe.hset(KEY_ROWS, row.index, row.to_compact_storage())
                .zadd(KEY_MORTON_3D, member_3d, 0)
                .zadd(KEY_MORTON_2D_ID, member_2d_id, 0)
                .zadd(KEY_MORTON_2D_CC, member_2d_cc, 0);

            max_index = max_index.max(row.index);
            in_batch += 1;
            total_loaded += 1;

            if in_batch >= batch_size {
                let _: () = pipe.query(&mut conn)?;
                pipe = redis::pipe();
                in_batch = 0;
                if total_loaded % 50_000 == 0 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let rate = total_loaded as f64 / elapsed.max(0.001);
                    println!(
                        "  [Ingest] Loaded {:>9} rows ({:>8.0} rows/sec)...",
                        total_loaded, rate
                    );
                }
            }
        }

        if in_batch > 0 {
            let _: () = pipe.query(&mut conn)?;
        }

        let elapsed = start.elapsed().as_secs_f64();
        let _: () = redis::pipe()
            .hset(KEY_META, "row_count", total_loaded as u32)
            .hset(KEY_META, "max_index", max_index)
            .hset(KEY_META, "ingest_seconds", format!("{:.3}", elapsed))
            .query(&mut conn)?;

        Ok((total_loaded, elapsed))
    }

    /// Builds a `MortonQueryPlan` for a `SelectQuery`.
    pub fn plan_query(&self, query: &SelectQuery, max_table_index: u32) -> MortonQueryPlan {
        let bounds = MortonQueryBounds::from_predicate(query.filter.as_ref(), max_table_index);

        let has_id_or_date = bounds.constrained_index || bounds.constrained_date;
        let has_country_or_city = bounds.constrained_country || bounds.constrained_city;

        // Choose the optimal Morton Z-Order Curve index:
        // 1. If both (index/date) AND (country) are constrained -> 3D Morton Index
        // 2. If only (country/city) are constrained -> 2D Morton (country, city) Index
        // 3. Otherwise -> 2D Morton (index, subscription_date) Index
        let index_choice = if has_id_or_date && bounds.constrained_country {
            MortonIndexChoice::Morton3D
        } else if has_country_or_city && !has_id_or_date {
            MortonIndexChoice::Morton2DCountryCity
        } else if has_id_or_date && bounds.constrained_city {
            // Compare selectivity: if index/date is tight, use 2D_ID or 3D; if city+country, use 2D_CC
            MortonIndexChoice::Morton2DIndexDate
        } else {
            MortonIndexChoice::Morton2DIndexDate
        };

        let intervals = match index_choice {
            MortonIndexChoice::Morton3D => decompose_morton_3d(
                bounds.min_index,
                bounds.max_index,
                bounds.min_date,
                bounds.max_date,
                bounds.min_country,
                bounds.max_country,
                self.max_intervals,
            ),
            MortonIndexChoice::Morton2DIndexDate => decompose_morton_2d(
                bounds.min_index,
                bounds.max_index,
                bounds.min_date,
                bounds.max_date,
                self.max_intervals,
            ),
            MortonIndexChoice::Morton2DCountryCity => decompose_morton_2d(
                bounds.min_country,
                bounds.max_country,
                bounds.min_city,
                bounds.max_city,
                self.max_intervals,
            ),
        };

        // Compute the fraction of the table's total Morton domain pruned by BIGMIN/LITMAX
        let full_bounds = MortonQueryBounds::unconstrained(max_table_index);
        let full_span: u128 = match index_choice {
            MortonIndexChoice::Morton3D => {
                let z0 = morton_encode_3d(
                    full_bounds.min_index,
                    full_bounds.min_date,
                    full_bounds.min_country,
                );
                let z1 = morton_encode_3d(
                    full_bounds.max_index,
                    full_bounds.max_date,
                    full_bounds.max_country,
                );
                (z1.saturating_sub(z0) as u128).max(1)
            }
            MortonIndexChoice::Morton2DIndexDate => {
                let z0 = morton_encode_2d(full_bounds.min_index, full_bounds.min_date);
                let z1 = morton_encode_2d(full_bounds.max_index, full_bounds.max_date);
                (z1.saturating_sub(z0) as u128).max(1)
            }
            MortonIndexChoice::Morton2DCountryCity => {
                let z0 = morton_encode_2d(full_bounds.min_country, full_bounds.min_city);
                let z1 = morton_encode_2d(full_bounds.max_country, full_bounds.max_city);
                (z1.saturating_sub(z0) as u128).max(1)
            }
        };

        let scanned_span: u128 = intervals.iter().map(|iv| iv.span() as u128).sum();
        let coverage = (scanned_span as f64 / full_span as f64).clamp(0.0, 1.0);
        let zspace_pruned_pct = (1.0 - coverage) * 100.0;

        MortonQueryPlan {
            index_choice,
            bounds,
            intervals,
            max_intervals_budget: self.max_intervals,
            zspace_pruned_pct,
        }
    }

    /// Parses and executes a SQL string against local Redis.
    pub fn execute_sql(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let cmd = crate::sql::parse_sql(sql)?;
        match cmd {
            SqlCommand::Select(q) => self.execute_select(&q),
            SqlCommand::Explain(q) => self.execute_explain(&q),
            SqlCommand::Describe(table) => self.execute_describe(&table),
            SqlCommand::ShowTables => self.execute_show_tables(),
            SqlCommand::ShowIndexes => self.execute_show_indexes(),
        }
    }

    fn execute_show_tables(&self) -> Result<QueryResult, EngineError> {
        let row_count = self.indexed_row_count()?;
        Ok(QueryResult {
            headers: vec![
                "table_name".to_string(),
                "engine".to_string(),
                "indexed_rows".to_string(),
                "morton_indexes".to_string(),
            ],
            rows: vec![vec![
                "customers".to_string(),
                "Redis + Morton Z-Order Curve".to_string(),
                row_count.to_string(),
                "morton3d, morton2d_id, morton2d_cc".to_string(),
            ]],
            stats: None,
        })
    }

    fn execute_show_indexes(&self) -> Result<QueryResult, EngineError> {
        let mut conn = self.get_connection()?;
        let c3d: usize = conn.zcard(KEY_MORTON_3D).unwrap_or(0);
        let c2d_id: usize = conn.zcard(KEY_MORTON_2D_ID).unwrap_or(0);
        let c2d_cc: usize = conn.zcard(KEY_MORTON_2D_CC).unwrap_or(0);

        Ok(QueryResult {
            headers: vec![
                "index_name".to_string(),
                "redis_zset_key".to_string(),
                "dimensions".to_string(),
                "bit_layout".to_string(),
                "entries".to_string(),
            ],
            rows: vec![
                vec![
                    "morton3d".to_string(),
                    KEY_MORTON_3D.to_string(),
                    "(index, subscription_date, country)".to_string(),
                    "63-bit Z-curve (21b x 21b x 21b)".to_string(),
                    c3d.to_string(),
                ],
                vec![
                    "morton2d_id".to_string(),
                    KEY_MORTON_2D_ID.to_string(),
                    "(index, subscription_date)".to_string(),
                    "42-bit Staggered 2D Z-curve (21b x 21b)".to_string(),
                    c2d_id.to_string(),
                ],
                vec![
                    "morton2d_cc".to_string(),
                    KEY_MORTON_2D_CC.to_string(),
                    "(country, city)".to_string(),
                    "42-bit Staggered 2D Z-curve (bits 20..6 x 14..0)".to_string(),
                    c2d_cc.to_string(),
                ],
            ],
            stats: None,
        })
    }

    fn execute_describe(&self, table: &str) -> Result<QueryResult, EngineError> {
        if table != "customers" {
            return Err(EngineError::Sql(SqlError::UnknownTable(table.to_string())));
        }
        let rows = Column::ALL
            .iter()
            .map(|c| {
                vec![
                    c.name().to_string(),
                    c.data_type().to_string(),
                    c.morton_indexed_info().to_string(),
                ]
            })
            .collect();

        Ok(QueryResult {
            headers: vec![
                "column".to_string(),
                "type".to_string(),
                "morton_index".to_string(),
            ],
            rows,
            stats: None,
        })
    }

    fn execute_explain(&self, query: &SelectQuery) -> Result<QueryResult, EngineError> {
        let max_idx = self.max_table_index()?;
        let plan = self.plan_query(query, max_idx);

        let sample_intervals: String = plan
            .intervals
            .iter()
            .take(4)
            .map(|iv| {
                format!(
                    "[{}..{}]",
                    format_morton_hex(iv.start),
                    format_morton_hex(iv.end)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");

        let rows = vec![
            vec![
                "Selected Morton Index".to_string(),
                plan.index_choice.description().to_string(),
            ],
            vec![
                "Redis Sorted Set Key".to_string(),
                plan.index_choice.redis_key().to_string(),
            ],
            vec![
                "Index Bound (d0: index)".to_string(),
                format!(
                    "[{} .. {}] (constrained: {})",
                    plan.bounds.min_index, plan.bounds.max_index, plan.bounds.constrained_index
                ),
            ],
            vec![
                "Date Bound (d1: subscription_date)".to_string(),
                format!(
                    "[0x{:05x} .. 0x{:05x}] (constrained: {})",
                    plan.bounds.min_date, plan.bounds.max_date, plan.bounds.constrained_date
                ),
            ],
            vec![
                "Country Bound (d2: country)".to_string(),
                format!(
                    "[0x{:05x} .. 0x{:05x}] (constrained: {})",
                    plan.bounds.min_country,
                    plan.bounds.max_country,
                    plan.bounds.constrained_country
                ),
            ],
            vec![
                "City Bound (d3: city)".to_string(),
                format!(
                    "[0x{:05x} .. 0x{:05x}] (constrained: {})",
                    plan.bounds.min_city, plan.bounds.max_city, plan.bounds.constrained_city
                ),
            ],
            vec![
                "Z-Order Intervals (BIGMIN/LITMAX)".to_string(),
                format!(
                    "{} intervals (budget: {})",
                    plan.intervals.len(),
                    plan.max_intervals_budget
                ),
            ],
            vec![
                "Morton Z-Space Pruned".to_string(),
                format!("{:.4}%", plan.zspace_pruned_pct),
            ],
            vec![
                "Sample ZRANGEBYLEX Intervals".to_string(),
                if plan.intervals.len() > 4 {
                    format!("{} ... (+{} more)", sample_intervals, plan.intervals.len() - 4)
                } else {
                    sample_intervals
                },
            ],
            vec![
                "Execution Pipeline".to_string(),
                "1. ZRANGEBYLEX pipeline -> 2. CPU morton_decode pre-filter -> 3. HMGET rows -> 4. SQL filter/sort/project".to_string(),
            ],
        ];

        Ok(QueryResult {
            headers: vec!["plan_property".to_string(), "value".to_string()],
            rows,
            stats: None,
        })
    }

    fn execute_select(&self, query: &SelectQuery) -> Result<QueryResult, EngineError> {
        let start = Instant::now();
        let max_idx = self.max_table_index()?;
        let plan = self.plan_query(query, max_idx);
        let mut conn = self.get_connection()?;

        let is_unconstrained = !plan.bounds.constrained_index
            && !plan.bounds.constrained_date
            && !plan.bounds.constrained_country
            && !plan.bounds.constrained_city;

        // Can we stop scanning early if there is a LIMIT and no ORDER BY (or ORDER BY index ASC)
        // and no aggregate functions?
        let has_aggregates = query.projections.iter().any(|p| {
            matches!(
                p,
                ProjectionItem::CountStar { .. } | ProjectionItem::Aggregate { .. }
            )
        });

        // Step 1: Execute pipelined ZRANGEBYLEX across the decomposed Morton intervals
        let zset_key = plan.index_choice.redis_key();
        let mut pipe = redis::pipe();

        // If completely unconstrained with a small LIMIT and no non-indexed filter, cap ZRANGEBYLEX count
        let per_interval_limit = if is_unconstrained
            && query.filter.is_none()
            && query.order_by.is_empty()
            && !has_aggregates
        {
            query.limit.map(|l| (l + query.offset).max(1))
        } else {
            None
        };

        for iv in &plan.intervals {
            let min_lex = format!("[{}:", format_morton_hex(iv.start));
            let max_lex = format!("[{}:\u{ffff}", format_morton_hex(iv.end));
            let mut cmd = redis::cmd("ZRANGEBYLEX");
            cmd.arg(zset_key).arg(min_lex).arg(max_lex);
            if let Some(lim) = per_interval_limit {
                cmd.arg("LIMIT").arg(0).arg(lim);
            }
            pipe.add_command(cmd);
        }

        let interval_results: Vec<Vec<String>> = if plan.intervals.is_empty() {
            Vec::new()
        } else {
            pipe.query(&mut conn)?
        };

        // Step 2: In-register Morton coordinate decoding & bounding-box pre-filter
        // This eliminates Z-curve false positives BEFORE fetching rows from Redis!
        let mut zset_candidates_scanned = 0usize;
        let mut candidate_row_ids: Vec<u32> = Vec::new();
        let mut seen_ids: HashSet<u32> = HashSet::new();

        for members in interval_results {
            zset_candidates_scanned += members.len();
            for member in members {
                let Some((z, row_id)) = parse_morton_member(&member) else {
                    continue;
                };

                let in_bounds = match plan.index_choice {
                    MortonIndexChoice::Morton3D => {
                        let (x, y, z_c) = morton_decode_3d(z);
                        x >= plan.bounds.min_index
                            && x <= plan.bounds.max_index
                            && y >= plan.bounds.min_date
                            && y <= plan.bounds.max_date
                            && z_c >= plan.bounds.min_country
                            && z_c <= plan.bounds.max_country
                    }
                    MortonIndexChoice::Morton2DIndexDate => {
                        let (x, y) = morton_decode_2d(z);
                        x >= plan.bounds.min_index
                            && x <= plan.bounds.max_index
                            && y >= plan.bounds.min_date
                            && y <= plan.bounds.max_date
                    }
                    MortonIndexChoice::Morton2DCountryCity => {
                        let (x, y) = morton_decode_2d(z);
                        x >= plan.bounds.min_country
                            && x <= plan.bounds.max_country
                            && y >= plan.bounds.min_city
                            && y <= plan.bounds.max_city
                    }
                };

                if in_bounds && seen_ids.insert(row_id) {
                    candidate_row_ids.push(row_id);
                }
            }
        }

        let morton_prefilter_passed = candidate_row_ids.len();

        // Optimization: If the query is `SELECT COUNT(*) FROM customers WHERE ...` and the WHERE
        // clause ONLY constrains `index` and/or `subscription_date` (which are 100% lossless in
        // 21-bit Morton coordinates!), then `morton_prefilter_passed` is already the exact count!
        let is_exact_morton_only_filter = Self::is_lossless_morton_predicate(query.filter.as_ref());
        if query.projections.len() == 1
            && matches!(query.projections[0], ProjectionItem::CountStar { .. })
            && is_exact_morton_only_filter
        {
            let alias = match &query.projections[0] {
                ProjectionItem::CountStar { alias } => {
                    alias.clone().unwrap_or_else(|| "COUNT(*)".to_string())
                }
                _ => unreachable!(),
            };
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            return Ok(QueryResult {
                headers: vec![alias],
                rows: vec![vec![morton_prefilter_passed.to_string()]],
                stats: Some(ExecutionStats {
                    index_used: plan.index_choice.description().to_string(),
                    redis_key: zset_key.to_string(),
                    intervals_count: plan.intervals.len(),
                    zspace_pruned_pct: plan.zspace_pruned_pct,
                    zset_candidates_scanned,
                    morton_prefilter_passed,
                    rows_fetched_hmget: 0,
                    rows_matched: morton_prefilter_passed,
                    elapsed_ms,
                }),
            });
        }

        // If ordering by `index` and filter is lossless on (index, date), we can sort `candidate_row_ids`
        // and apply LIMIT/OFFSET before HMGET!
        if is_exact_morton_only_filter && !has_aggregates {
            if query.order_by.is_empty() {
                candidate_row_ids.sort_unstable();
                if let Some(lim) = query.limit {
                    candidate_row_ids = candidate_row_ids
                        .into_iter()
                        .skip(query.offset)
                        .take(lim)
                        .collect();
                }
            } else if query.order_by.len() == 1 && query.order_by[0].col == Column::Index {
                if query.order_by[0].ascending {
                    candidate_row_ids.sort_unstable();
                } else {
                    candidate_row_ids.sort_unstable_by(|a, b| b.cmp(a));
                }
                if let Some(lim) = query.limit {
                    candidate_row_ids = candidate_row_ids
                        .into_iter()
                        .skip(query.offset)
                        .take(lim)
                        .collect();
                }
            }
        }

        // Step 3: Batch-fetch candidate rows from Redis Hash `sqllayer:customers:rows` via HMGET
        let mut matched_rows: Vec<CustomerRow> = Vec::with_capacity(candidate_row_ids.len());
        let rows_fetched_hmget = candidate_row_ids.len();

        for chunk in candidate_row_ids.chunks(2_000) {
            let payloads: Vec<Option<String>> = redis::cmd("HMGET")
                .arg(KEY_ROWS)
                .arg(chunk)
                .query(&mut conn)?;

            for payload in payloads.into_iter().flatten() {
                if let Some(row) = CustomerRow::from_compact_storage(&payload) {
                    if query
                        .filter
                        .as_ref()
                        .map(|pred| pred.matches(&row))
                        .unwrap_or(true)
                    {
                        matched_rows.push(row);
                    }
                }
            }
        }

        let rows_matched = matched_rows.len();

        // Step 4: Apply ORDER BY
        if !query.order_by.is_empty() {
            matched_rows.sort_by(|a, b| {
                for ob in &query.order_by {
                    let va = a.get(ob.col);
                    let vb = b.get(ob.col);
                    let cmp = va.cmp(&vb);
                    if cmp != Ordering::Equal {
                        return if ob.ascending { cmp } else { cmp.reverse() };
                    }
                }
                a.index.cmp(&b.index)
            });
        } else {
            matched_rows.sort_by_key(|r| r.index);
        }

        // Step 5: Compute aggregates or project columns + apply OFFSET / LIMIT
        let (headers, result_rows) = if has_aggregates {
            Self::evaluate_aggregates(&query.projections, &matched_rows)
        } else {
            // Note: If we already applied offset/limit before HMGET, don't double-skip
            let already_sliced = is_exact_morton_only_filter
                && (query.order_by.is_empty()
                    || (query.order_by.len() == 1 && query.order_by[0].col == Column::Index))
                && query.limit.is_some();

            let iter: Box<dyn Iterator<Item = &CustomerRow>> = if already_sliced {
                Box::new(matched_rows.iter())
            } else if let Some(lim) = query.limit {
                Box::new(matched_rows.iter().skip(query.offset).take(lim))
            } else {
                Box::new(matched_rows.iter().skip(query.offset))
            };

            Self::project_rows(&query.projections, iter)
        };

        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        Ok(QueryResult {
            headers,
            rows: result_rows,
            stats: Some(ExecutionStats {
                index_used: plan.index_choice.description().to_string(),
                redis_key: zset_key.to_string(),
                intervals_count: plan.intervals.len(),
                zspace_pruned_pct: plan.zspace_pruned_pct,
                zset_candidates_scanned,
                morton_prefilter_passed,
                rows_fetched_hmget,
                rows_matched,
                elapsed_ms,
            }),
        })
    }

    /// Returns true if the predicate only uses exact range/equality comparisons on `index`
    /// and/or `subscription_date` (which are losslessly encoded in our 21-bit Morton coordinates).
    fn is_lossless_morton_predicate(pred: Option<&crate::sql::Predicate>) -> bool {
        use crate::sql::{CmpOp, Predicate};
        let Some(p) = pred else {
            return true;
        };
        match p {
            Predicate::And(a, b) => {
                Self::is_lossless_morton_predicate(Some(a))
                    && Self::is_lossless_morton_predicate(Some(b))
            }
            Predicate::Comparison { col, op, .. } => {
                matches!(col, Column::Index | Column::SubscriptionDate)
                    && matches!(
                        op,
                        CmpOp::Eq | CmpOp::Lt | CmpOp::Lte | CmpOp::Gt | CmpOp::Gte
                    )
            }
            Predicate::Between { col, negated, .. } => {
                !*negated && matches!(col, Column::Index | Column::SubscriptionDate)
            }
            _ => false,
        }
    }

    fn evaluate_aggregates(
        projections: &[ProjectionItem],
        rows: &[CustomerRow],
    ) -> (Vec<String>, Vec<Vec<String>>) {
        let mut headers = Vec::with_capacity(projections.len());
        let mut out_row = Vec::with_capacity(projections.len());

        for item in projections {
            match item {
                ProjectionItem::CountStar { alias } => {
                    headers.push(alias.clone().unwrap_or_else(|| "COUNT(*)".to_string()));
                    out_row.push(rows.len().to_string());
                }
                ProjectionItem::Aggregate { func, col, alias } => {
                    let hdr = alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({})", func.name(), col.name()));
                    headers.push(hdr);
                    let val_str = match func {
                        AggFunc::Count => rows.len().to_string(),
                        AggFunc::Min => rows
                            .iter()
                            .map(|r| r.get(*col))
                            .min()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "NULL".to_string()),
                        AggFunc::Max => rows
                            .iter()
                            .map(|r| r.get(*col))
                            .max()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "NULL".to_string()),
                    };
                    out_row.push(val_str);
                }
                ProjectionItem::Column { col, alias } => {
                    headers.push(alias.clone().unwrap_or_else(|| col.name().to_string()));
                    out_row.push(
                        rows.first()
                            .map(|r| r.get(*col).to_string())
                            .unwrap_or_else(|| "NULL".to_string()),
                    );
                }
                ProjectionItem::Wildcard => {
                    headers.push("*".to_string());
                    out_row.push(rows.len().to_string());
                }
            }
        }

        (headers, vec![out_row])
    }

    fn project_rows<'a>(
        projections: &[ProjectionItem],
        rows: impl Iterator<Item = &'a CustomerRow>,
    ) -> (Vec<String>, Vec<Vec<String>>) {
        let mut expanded_cols: Vec<(Column, String)> = Vec::new();
        for item in projections {
            match item {
                ProjectionItem::Wildcard => {
                    for col in Column::ALL {
                        expanded_cols.push((col, col.name().to_string()));
                    }
                }
                ProjectionItem::Column { col, alias } => {
                    expanded_cols.push((
                        *col,
                        alias.clone().unwrap_or_else(|| col.name().to_string()),
                    ));
                }
                _ => {}
            }
        }

        let headers = expanded_cols.iter().map(|(_, name)| name.clone()).collect();
        let mut out_rows = Vec::new();
        for r in rows {
            let vals = expanded_cols
                .iter()
                .map(|(col, _)| r.get(*col).to_string())
                .collect();
            out_rows.push(vals);
        }
        (headers, out_rows)
    }
}

/// Renders a `QueryResult` as an aligned ASCII table along with Morton index telemetry.
pub fn format_query_result(res: &QueryResult) -> String {
    let mut out = String::new();
    if res.headers.is_empty() {
        return out;
    }

    let mut widths: Vec<usize> = res.headers.iter().map(|h| h.len()).collect();
    for row in &res.rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let make_sep = |widths: &[usize]| -> String {
        let mut s = String::from("+");
        for w in widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s.push('\n');
        s
    };

    let sep = make_sep(&widths);
    out.push_str(&sep);

    // Header row
    out.push('|');
    for (i, h) in res.headers.iter().enumerate() {
        out.push_str(&format!(" {:<width$} |", h, width = widths[i]));
    }
    out.push('\n');
    out.push_str(&sep);

    // Data rows
    for row in &res.rows {
        out.push('|');
        for (i, cell) in row.iter().enumerate() {
            out.push_str(&format!(" {:<width$} |", cell, width = widths[i]));
        }
        out.push('\n');
    }
    out.push_str(&sep);

    if let Some(stats) = &res.stats {
        out.push_str(&format!(
            "{} row(s) returned in {:.2} ms\n",
            res.rows.len(),
            stats.elapsed_ms
        ));
        out.push_str(&format!(
            "  [Morton Index] {} ({})\n",
            stats.index_used, stats.redis_key
        ));
        out.push_str(&format!(
            "  [Z-Curve Scan] {} ZRANGEBYLEX interval(s) | {:.4}% Z-space pruned | {} ZSET entries scanned -> {} passed Morton decode -> {} HMGET fetched -> {} matched\n",
            stats.intervals_count,
            stats.zspace_pruned_pct,
            stats.zset_candidates_scanned,
            stats.morton_prefilter_passed,
            stats.rows_fetched_hmget,
            stats.rows_matched
        ));
    }

    out
}

