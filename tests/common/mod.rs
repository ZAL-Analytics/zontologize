use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use zontologize::db;

pub const TEST_URL: &str = "ZONTOLOGIZE_TEST_DATABASE_URL";
const SCHEMA_PREFIX: &str = "zontologize_test_";
static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn test_url() -> String {
    std::env::var(TEST_URL).unwrap_or_else(|_| panic!("{TEST_URL} is not set"))
}

pub fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

pub struct Env {
    pub schema: String,
    pub dir: PathBuf,
    pub pool: PgPool,
}

impl Env {
    pub async fn new(name: &str) -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let schema = format!("{SCHEMA_PREFIX}{name}_{}_{n}", std::process::id());
        let dir = std::env::temp_dir().join(&schema);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        drop_schema(&schema).await;
        let mut conn = db::connect(Some(&test_url()), &schema).await.unwrap();
        db::migrate::run(&mut conn, &schema).await.unwrap();
        db::close(conn).await;
        let options = PgConnectOptions::from_str(&test_url())
            .unwrap()
            .options([("search_path", schema.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        Env { schema, dir, pool }
    }

    pub fn config(&self) -> PathBuf {
        self.dir.join("config")
    }

    pub fn data(&self) -> PathBuf {
        self.dir.join("data")
    }

    pub fn write_config(&self, extra: &str) {
        let ontology = repo("examples/acme/ontology/acme.ttl");
        let text = format!(
            "[database]\nschema = \"{}\"\nmax_connections = 4\n\
             [extract]\nthreads = 2\ntimeout_secs = 60\n\
             [ontology]\nfiles = [\"{}\"]\nlanguages = [\"en\", \"nl\"]\n\
             include = [\"acme:Organisation\", \"acme:Site\", \"acme:Project\", \
             \"acme:Equipment\", \"acme:Document\"]\n\
             document_class = \"acme:Document\"\n{extra}",
            self.schema,
            ontology.display()
        );
        std::fs::write(self.config().join("zontologize.toml"), text).unwrap();
    }

    pub fn write_rules(&self, text: &str) {
        std::fs::write(self.config().join("rules.toml"), text).unwrap();
    }

    pub fn copy_acme_rules(&self) {
        std::fs::copy(
            repo("examples/acme/rules.toml"),
            self.config().join("rules.toml"),
        )
        .unwrap();
        let gazetteer = self.config().join("gazetteer");
        std::fs::create_dir_all(&gazetteer).unwrap();
        std::fs::copy(
            repo("examples/acme/gazetteer/customers.csv"),
            gazetteer.join("customers.csv"),
        )
        .unwrap();
    }

    pub fn file(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.data().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    pub async fn scan(&self) -> serde_json::Value {
        self.scan_sources(&[]).await
    }

    pub async fn scan_sources(&self, sources: &[&str]) -> serde_json::Value {
        let sources: Vec<String> = sources.iter().map(|s| s.to_string()).collect();
        zontologize::scan::run(Some(&self.config()), &sources, Some(&test_url()))
            .await
            .unwrap();
        self.last_stats().await
    }

    pub async fn last_stats(&self) -> serde_json::Value {
        let stats: sqlx::types::Json<serde_json::Value> =
            sqlx::query_scalar("select stats from runs order by id desc limit 1")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        stats.0
    }

    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn finish(self) {
        self.pool.close().await;
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let schema = self.schema.clone();
        let dir = self.dir.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                drop_schema(&schema).await;
                drop_schema(&format!("{schema}_store")).await;
            });
            let _ = std::fs::remove_dir_all(&dir);
        })
        .join()
        .unwrap();
    }
}

pub async fn drop_schema(schema: &str) {
    let mut conn = db::connect(Some(&test_url()), "public").await.unwrap();
    let sql = format!(
        "drop schema if exists {} cascade",
        db::quote_identifier(schema)
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(&mut conn)
        .await
        .unwrap();
    db::close(conn).await;
}

pub fn folder_source(name: &str, root: &Path, dataset: &str) -> String {
    format!(
        "[[sources]]\nname = \"{name}\"\nroot = \"{}\"\ndataset = \"{dataset}\"\n",
        root.display()
    )
}

pub fn stage(stats: &serde_json::Value, stage: &str) -> u64 {
    stats[stage]["items"].as_u64().unwrap()
}

pub fn source(stats: &serde_json::Value, name: &str, field: &str) -> u64 {
    stats["sources"][name][field].as_u64().unwrap()
}
