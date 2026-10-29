pub mod config;
pub mod db;
pub mod scan;

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

pub async fn db_migrate(config_dir: Option<&Path>) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let schema = &config.database.schema;
    let mut conn = db::connect(database_url().as_deref(), schema).await?;
    let result = db::migrate::run(&mut conn, schema).await;
    db::close(conn).await;
    let applied = result?;
    if applied.is_empty() {
        println!("schema {schema}: nothing to do");
    }
    for version in applied {
        println!("schema {schema}: applied {version:04}");
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn db_check(config_dir: Option<&Path>) -> Result<ExitCode> {
    let config = config::load(config_dir)?;
    let schema = &config.database.schema;
    let mut conn = db::connect(database_url().as_deref(), schema).await?;
    let result = check(&mut conn, schema).await;
    db::close(conn).await;
    let complete = result?;
    Ok(if complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

async fn check(conn: &mut sqlx::PgConnection, schema: &str) -> Result<bool> {
    let mut complete = true;
    println!("server: {}", db::server_version(conn).await?);
    for name in db::EXTENSIONS {
        match db::extension_version(conn, name).await? {
            Some(version) => println!("extension {name}: {version}"),
            None => {
                complete = false;
                println!(
                    "extension {name}: missing, run as database owner: {}",
                    db::migrate::create_extension_sql(name)
                );
            }
        }
    }
    if db::schema_exists(conn, schema).await? {
        println!("schema {schema}: present");
    } else {
        complete = false;
        println!("schema {schema}: missing, run zontologize db migrate");
    }
    for state in db::migrate::states(conn, schema).await? {
        let status = if state.applied { "applied" } else { "pending" };
        if !state.applied {
            complete = false;
        }
        println!(
            "migration {:04} {}: {status}",
            state.version, state.description
        );
    }
    Ok(complete)
}
