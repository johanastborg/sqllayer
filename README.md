# `sqllayer` — SQL Layer on Top of Redis Using Morton Codes (Z-Order Curves)

`sqllayer` is a high-performance multidimensional SQL query engine written in Rust on top of Redis. It maps multidimensional table attributes into **Morton codes (Z-order space-filling curves)** stored inside Redis Sorted Sets (`ZSET`s) and executes 1D, 2D, and 3D `SELECT` and range queries in milliseconds over **2,000,000 customer records** (`customers-2000000.csv`).

---

## How It Works

### 1. Multidimensional Morton Code Indexing in Redis
When all members of a Redis Sorted Set (`ZSET`) share a score of `0`, Redis orders members lexicographically and supports $O(\log N + K)$ range scans via `ZRANGEBYLEX`.

`sqllayer` encodes each row's indexed attributes into order-preserving 21-bit unsigned integers (`0..=2,097,151`) and bit-interleaves them into 64-bit Morton codes formatted as zero-padded 16-character hexadecimal strings (`<16-hex-morton>:<row_id>`):

| Coordinate | Column | Encoding (21-bit Order-Preserving) |
| :--- | :--- | :--- |
| `d0` | `index` | Lossless 21-bit integer (`1..=2,000,000`) |
| `d1` | `subscription_date` | Civil calendar days since `2020-01-01` (`0..=1023`), shifted to bits `19..10` (`days << 10`) |
| `d2` | `country` | 15-bit (3-character, 5-bit radix-32) case-insensitive alphabetical prefix at bits `20..6` |
| `d3` | `city` | 15-bit (3-character, 5-bit radix-32) case-insensitive alphabetical prefix at bits `14..0` |

### 2. Redis Data Layout
The engine maintains four keys in your local Redis instance (`redis://127.0.0.1:6379/`):
- `sqllayer:customers:rows` (`HASH`): Maps `<index>` to a compact tab-separated row payload.
- `sqllayer:customers:morton3d` (`ZSET`): 63-bit **3D Morton curve** interleaving `(index, subscription_date, country)`.
- `sqllayer:customers:morton2d_id` (`ZSET`): Staggered **2D Morton curve** interleaving `(index, subscription_date)`.
- `sqllayer:customers:morton2d_cc` (`ZSET`): Staggered **2D Morton curve** interleaving `(country, city)`.
- `sqllayer:customers:meta` (`HASH`): Table metadata (`row_count`, `max_index`, `ingest_seconds`).

### 3. Query Execution Pipeline (`BIGMIN / LITMAX` + In-Register Pre-Filtering)
1. **SQL Parsing & Bounding Box Extraction**: Parses the SQL AST (`sqlparser`) and extracts a multidimensional query hyper-rectangle `[min_d ..= max_d]` from `WHERE` predicates (`=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `LIKE 'prefix%'`, `IN`, `AND`, `OR`).
2. **Adaptive Morton Index Selection**: Selects the optimal Z-order curve index (`morton3d`, `morton2d_id`, or `morton2d_cc`) based on the constrained dimensions.
3. **`BIGMIN / LITMAX` Z-Order Range Decomposition**: Uses $O(1)$ bitwise quadtree/octree splitting and exact dyadic Z-block recognition to decompose the query hyper-rectangle into disjoint contiguous 1D Morton intervals `[z_start .. z_end]`, pruning **99.8%–99.99%+** of the Z-curve search space.
4. **Pipelined `ZRANGEBYLEX`**: Sends all decomposed `[z_start .. z_end]` lexicographical range scans to Redis in a single pipeline round-trip.
5. **CPU `morton_decode` Pre-Filter**: Before touching the row store, each candidate ZSET key `<16-hex-morton>:<row_id>` is decoded in CPU registers (`morton_decode_2d` / `morton_decode_3d`) and verified against the exact coordinate bounding box, discarding Z-curve false positives at zero network cost.
6. **Batched `HMGET` & SQL Evaluation**: Fetches surviving rows from `sqllayer:customers:rows`, applies any residual SQL predicates, sorts (`ORDER BY`), computes aggregates (`COUNT`, `MIN`, `MAX`), and applies `LIMIT` / `OFFSET`.

---

## Project Structure

```text
.
├── Cargo.toml
├── customers-2000000.csv   # 2,000,000 sample customer records (~349 MB)
└── src/
    ├── main.rs             # CLI entrypoint (load, query, repl, demo)
    ├── morton.rs           # 2D/3D Morton encode/decode, domain codecs, BIGMIN/LITMAX decomposition
    ├── schema.rs           # Table schema, column types, and compact row serialization
    ├── sql.rs              # SQL parser, WHERE predicate evaluator, and Morton bounding-box extractor
    └── engine.rs           # Redis connection, pipelined CSV loader, Morton query planner & executor
```

---

## Quick Start

### Prerequisites
- Rust (`cargo`)
- Local Redis instance running on `127.0.0.1:6379` (`redis-cli ping` -> `PONG`)

### Build
```bash
cargo build --release
```

### 1. Load `customers-2000000.csv` into Redis
Load all 2,000,000 rows (or pass `--limit <N>` for a smaller subset):
```bash
./target/release/sqllayer load
```
*(Loads and indexes all 2,000,000 rows across all 3 Morton ZSETs + row store in ~22 seconds at ~91,000 rows/sec.)*

### 2. Run the Built-In Demo Suite
```bash
./target/release/sqllayer demo --rows 2000000
```

### 3. Execute One-Off SQL Queries
```bash
# 1D Range Query on `index`
./target/release/sqllayer query "
  SELECT index, first_name, last_name, company, country, subscription_date
  FROM customers
  WHERE index BETWEEN 1999990 AND 2000000
  ORDER BY index ASC
"

# 2D Range Query on `(index, subscription_date)`
./target/release/sqllayer query "
  SELECT index, first_name, last_name, country, subscription_date
  FROM customers
  WHERE index BETWEEN 1 AND 500
    AND subscription_date BETWEEN '2021-01-01' AND '2021-06-30'
  ORDER BY subscription_date ASC
  LIMIT 10
"

# 2D Lexical Range + Prefix Query on `(country, city)`
./target/release/sqllayer query "
  SELECT index, first_name, last_name, country, city, subscription_date
  FROM customers
  WHERE country = 'Costa Rica' AND city LIKE 'East%'
  ORDER BY index ASC
  LIMIT 10
"

# 3D Multi-Attribute Range Query on `(index, subscription_date, country)`
./target/release/sqllayer query "
  SELECT index, customer_id, first_name, last_name, country, city, subscription_date
  FROM customers
  WHERE index BETWEEN 1 AND 50000
    AND subscription_date BETWEEN '2020-01-01' AND '2020-12-31'
    AND country BETWEEN 'Canada' AND 'Costa Rica'
  ORDER BY country ASC, subscription_date ASC
  LIMIT 10
"

# Inspect the Morton Z-Order Execution Plan
./target/release/sqllayer query "
  EXPLAIN SELECT index, first_name, country, subscription_date
  FROM customers
  WHERE index BETWEEN 1000 AND 10000
    AND subscription_date BETWEEN '2021-01-01' AND '2021-06-30'
    AND country = 'Costa Rica'
"
```

### 4. Interactive SQL REPL
```bash
./target/release/sqllayer repl
```
Inside the REPL, you can run:
- `SHOW TABLES`
- `SHOW INDEXES`
- `DESCRIBE customers`
- `SELECT ... FROM customers WHERE ... ORDER BY ... LIMIT ... OFFSET ...`
- `EXPLAIN SELECT ...`
- `exit` / `quit`

---

## Supported SQL Syntax

- **Projections**: `*`, column names (`index`, `customer_id`, `first_name`, `last_name`, `company`, `city`, `country`, `phone_1`, `phone_2`, `email`, `subscription_date`, `website`), `AS <alias>`
- **Aggregates**: `COUNT(*)`, `COUNT(col)`, `MIN(col)`, `MAX(col)`
- **Filters (`WHERE`)**:
  - Comparison operators: `=`, `!=` / `<>`, `<`, `<=`, `>`, `>=`
  - Range operator: `BETWEEN <low> AND <high>`
  - Pattern matching: `LIKE '<pattern>'` / `ILIKE '<pattern>'` (prefixes automatically push down into Morton coordinate bounds)
  - Set membership: `IN (<v1>, <v2>, ...)`
  - Logical operators: `AND`, `OR`, `NOT`, parenthesized expressions
- **Sorting & Pagination**: `ORDER BY <col> [ASC|DESC], ...`, `LIMIT <n>`, `OFFSET <m>`
- **Introspection**: `EXPLAIN SELECT ...`, `DESCRIBE customers`, `SHOW TABLES`, `SHOW INDEXES`

---

## Running Tests

```bash
cargo test
```
Runs unit tests covering 2D and 3D Morton bit-interleaving roundtrips, chronological and alphabetical monotonicity, `BIGMIN / LITMAX` range decomposition completeness, and SQL query compilation.

