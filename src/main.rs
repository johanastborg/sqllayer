pub mod engine;
pub mod morton;
pub mod schema;
pub mod sql;

use clap::{Parser, Subcommand};
use engine::{format_query_result, RedisSqlEngine};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "sqllayer",
    version = "0.1.0",
    about = "SQL layer on top of Redis using Morton codes (Z-order curves) for multidimensional indexing"
)]
struct Cli {
    /// Local Redis connection URL
    #[arg(long, default_value = "redis://127.0.0.1:6379/")]
    redis_url: String,

    /// Path to customers CSV dataset
    #[arg(long, default_value = "customers-2000000.csv")]
    csv: PathBuf,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Load `customers-2000000.csv` into Redis and build Morton Z-order curve indexes
    Load {
        /// Maximum number of rows to load (omit to load all 2,000,000 rows, or e.g. --limit 100000)
        #[arg(short, long)]
        limit: Option<usize>,

        /// Flush existing `sqllayer:customers:*` keys before loading
        #[arg(long, default_value_t = true)]
        flush: bool,
    },
    /// Execute a single SQL query (SELECT, EXPLAIN SELECT, DESCRIBE, SHOW TABLES, SHOW INDEXES)
    Query {
        /// SQL statement to execute
        sql: String,
    },
    /// Start an interactive SQL REPL shell connected to local Redis
    Repl,
    /// Run a demonstration suite of SELECT and multidimensional Morton range queries
    Demo {
        /// Number of rows to ensure are loaded for the demo (defaults to 100,000 for quick start; pass 2000000 for full CSV)
        #[arg(short, long, default_value_t = 100_000)]
        rows: usize,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let engine = RedisSqlEngine::connect(&cli.redis_url)?;

    match cli.command.unwrap_or(Commands::Demo { rows: 100_000 }) {
        Commands::Load { limit, flush } => {
            println!(
                "Loading '{}' into Redis ({}) with Morton Z-order indexing...",
                cli.csv.display(),
                cli.redis_url
            );
            let (loaded, secs) = engine.ingest_csv(&cli.csv, limit, flush)?;
            println!(
                "Done! Indexed {} rows in {:.2}s ({:.0} rows/sec).",
                loaded,
                secs,
                loaded as f64 / secs.max(0.001)
            );
        }
        Commands::Query { sql } => {
            let res = engine.execute_sql(&sql)?;
            print!("{}", format_query_result(&res));
        }
        Commands::Repl => {
            run_repl(&engine)?;
        }
        Commands::Demo { rows } => {
            run_demo(&engine, &cli.csv, rows)?;
        }
    }

    Ok(())
}

fn run_demo(
    engine: &RedisSqlEngine,
    csv_path: &PathBuf,
    target_rows: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let current_rows = engine.indexed_row_count()? as usize;
    if current_rows < target_rows {
        println!(
            "=== Step 1: Ingesting {} rows from '{}' into Redis with Morton Z-Order Indexes ===",
            target_rows,
            csv_path.display()
        );
        let (loaded, secs) = engine.ingest_csv(csv_path, Some(target_rows), true)?;
        println!(
            "Successfully indexed {} rows in {:.2}s ({:.0} rows/sec).\n",
            loaded,
            secs,
            loaded as f64 / secs.max(0.001)
        );
    } else {
        println!(
            "=== Redis already contains {} Morton-indexed rows from '{}' ===\n",
            current_rows,
            csv_path.display()
        );
    }

    let demo_queries = [
        (
            "1. Inspect Schema & Morton Index Architecture",
            "SHOW INDEXES",
        ),
        (
            "2. Point Lookup via Morton Index",
            "SELECT index, customer_id, first_name, last_name, country, city, subscription_date \
             FROM customers WHERE index = 2",
        ),
        (
            "3. 1D Range Query on `index` (BETWEEN)",
            "SELECT index, first_name, last_name, company, country, subscription_date \
             FROM customers WHERE index BETWEEN 10 AND 18 ORDER BY index ASC",
        ),
        (
            "4. 2D Range Query on `(index, subscription_date)` via 2D Morton Curve",
            "SELECT index, first_name, last_name, country, subscription_date \
             FROM customers \
             WHERE index BETWEEN 1 AND 500 \
               AND subscription_date BETWEEN '2021-01-01' AND '2021-06-30' \
             ORDER BY subscription_date ASC LIMIT 10",
        ),
        (
            "5. 2D Spatial/Lexical Range Query on `(country, city)` with Prefix `LIKE`",
            "SELECT index, first_name, last_name, country, city, subscription_date \
             FROM customers \
             WHERE country = 'Costa Rica' AND city LIKE 'East%' \
             ORDER BY index ASC LIMIT 10",
        ),
        (
            "6. 3D Multi-Attribute Range Query on `(index, subscription_date, country)` via 3D Morton Octree",
            "SELECT index, customer_id, first_name, last_name, country, city, subscription_date \
             FROM customers \
             WHERE index BETWEEN 1 AND 50000 \
               AND subscription_date BETWEEN '2020-01-01' AND '2020-12-31' \
               AND country BETWEEN 'Canada' AND 'Costa Rica' \
             ORDER BY country ASC, subscription_date ASC LIMIT 10",
        ),
        (
            "7. Fast Morton-Accelerated Aggregation (`COUNT(*)`, `MIN`, `MAX`)",
            "SELECT COUNT(*) AS matching_customers, MIN(subscription_date) AS earliest, MAX(subscription_date) AS latest \
             FROM customers \
             WHERE index BETWEEN 1000 AND 20000 \
               AND subscription_date BETWEEN '2021-01-01' AND '2021-12-31'",
        ),
        (
            "8. EXPLAIN Query Execution Plan (BIGMIN / LITMAX Z-Order Range Decomposition)",
            "EXPLAIN SELECT index, first_name, country, subscription_date \
             FROM customers \
             WHERE index BETWEEN 1000 AND 10000 \
               AND subscription_date BETWEEN '2021-01-01' AND '2021-06-30' \
               AND country = 'Costa Rica'",
        ),
    ];

    for (title, sql) in demo_queries {
        println!("--------------------------------------------------------------------------------");
        println!("### {title}");
        println!("SQL> {sql}\n");
        let res = engine.execute_sql(sql)?;
        print!("{}\n", format_query_result(&res));
    }

    Ok(())
}

fn run_repl(engine: &RedisSqlEngine) -> Result<(), Box<dyn std::error::Error>> {
    println!("================================================================================");
    println!("  Redis + Morton Code SQL Layer — Interactive REPL");
    println!("  Connected to local Redis. Indexed rows: {}", engine.indexed_row_count()?);
    println!("  Commands: SELECT ..., EXPLAIN SELECT ..., DESCRIBE customers, SHOW INDEXES, exit");
    println!("================================================================================");

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        print!("sql> ");
        io::stdout().flush()?;
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("exit") || trimmed.eq_ignore_ascii_case("quit") {
            break;
        }
        match engine.execute_sql(trimmed) {
            Ok(res) => print!("{}", format_query_result(&res)),
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    Ok(())
}
