use std::{fs, path::PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use dotenvy::dotenv;
use mysql2pg_middleware::{
    config::AppConfig,
    executor::build_executor,
    server,
    translator::{translate_sql, TranslationResult},
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "mysql2pg-middleware")]
#[command(about = "Translate MySQL SQL to PostgreSQL SQL and optionally execute it against PostgreSQL")]
struct Cli {
    #[arg(short, long, default_value = "config/example.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Serve,
    Translate {
        #[arg(long)]
        sql: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Execute {
        #[arg(long)]
        sql: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let cfg = AppConfig::load(&cli.config)?;

    match cli.command {
        Commands::Serve => {
            server::serve(cfg).await?;
        }
        Commands::Translate { sql, file, json } => {
            let input = read_input(sql, file)?;
            let result = translate_sql(&input, &cfg.translator)?;
            print_translation(&result, json);
        }
        Commands::Execute { sql, file, json } => {
            let input = read_input(sql, file)?;
            let result = translate_sql(&input, &cfg.translator)?;
            let executor = build_executor(&cfg)?;
            let query_result = executor.execute_sql(&result.translated_sql).await?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "translation": result,
                        "execution": query_result,
                    }))?
                );
            } else {
                print_translation(&result, false);
                println!("\nExecution result:");
                if query_result.columns.is_empty() {
                    println!("rows: {}", query_result.row_count);
                } else {
                    println!("columns: {}", query_result.columns.join(", "));
                    for row in &query_result.rows {
                        println!("{}", row.join(" | "));
                    }
                    println!("rows: {}", query_result.row_count);
                }
            }
        }
    }

    Ok(())
}

fn read_input(sql: Option<String>, file: Option<PathBuf>) -> Result<String> {
    match (sql, file) {
        (Some(sql), None) => Ok(sql),
        (None, Some(path)) => Ok(fs::read_to_string(path)?),
        (Some(_), Some(_)) => anyhow::bail!("provide either --sql or --file, not both"),
        (None, None) => anyhow::bail!("provide one of --sql or --file"),
    }
}

fn print_translation(result: &TranslationResult, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(result).unwrap());
        return;
    }

    println!("Original SQL:\n{}", result.original_sql);
    println!("\nCanonical MySQL AST render:\n{}", result.canonical_mysql_sql);
    println!("\nTranslated PostgreSQL SQL:\n{}", result.translated_sql);
    if !result.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &result.warnings {
            println!("- {warning}");
        }
    }
}
