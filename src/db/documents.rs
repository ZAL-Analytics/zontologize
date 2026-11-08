use anyhow::Result;
use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

#[derive(Debug, Clone, FromRow)]
pub struct Stored {
    pub id: i64,
    pub path: String,
    pub external_id: Option<String>,
    pub file: Option<String>,
    pub sha256: Option<String>,
    pub size: i64,
    pub mtime: DateTime<Utc>,
    pub kind: Option<String>,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: Option<i64>,
    pub path: String,
    pub external_id: Option<String>,
    pub file: Option<String>,
    pub sha256: Option<String>,
    pub size: i64,
    pub mtime: DateTime<Utc>,
    pub kind: Option<String>,
    pub status: &'static str,
    pub reason: Option<String>,
}

const STORED: &str = "select id, path, external_id, file, sha256, size, mtime, kind, status, reason \
                      from documents where source_id = $1";

pub async fn upsert_source<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    name: &str,
    kind: &str,
    root: &str,
    dataset: &str,
    view: Option<&str>,
) -> Result<i64> {
    let id = sqlx::query_scalar(
        "insert into sources (name, kind, root, dataset, view) values ($1, $2, $3, $4, $5) \
         on conflict (name) do update set kind = excluded.kind, root = excluded.root, \
         dataset = excluded.dataset, view = excluded.view returning id",
    )
    .bind(name)
    .bind(kind)
    .bind(root)
    .bind(dataset)
    .bind(view)
    .fetch_one(executor)
    .await?;
    Ok(id)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relocation {
    pub from_source: String,
    pub from_path: String,
    pub to_source: String,
    pub to_path: String,
}

pub async fn relocate(
    conn: &mut sqlx::PgConnection,
    from_source: i64,
    from_path: &str,
    to_source: i64,
    to_path: &str,
) -> Result<u64> {
    let moved = sqlx::query(
        "update documents set source_id = $3, \
         path = $4 || substr(path, char_length($2) + 1), rules_key = null \
         where source_id = $1 and (path = $2 or starts_with(path, $2 || '/'))",
    )
    .bind(from_source)
    .bind(from_path)
    .bind(to_source)
    .bind(to_path)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(moved)
}

pub async fn drop_empty_sources(
    conn: &mut sqlx::PgConnection,
    keep: &[String],
) -> Result<Vec<String>> {
    let names = sqlx::query_scalar(
        "delete from sources s where s.name <> all($1) \
         and not exists (select 1 from documents d where d.source_id = s.id) returning name",
    )
    .bind(keep)
    .fetch_all(conn)
    .await?;
    Ok(names)
}

pub async fn source_ids(pool: &PgPool, names: &[String]) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar("select id from sources where name = any($1)")
        .bind(names)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

pub async fn by_paths(pool: &PgPool, source_id: i64, paths: &[String]) -> Result<Vec<Stored>> {
    let sql = format!("{STORED} and path = any($2)");
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(source_id)
        .bind(paths)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn by_external_ids(pool: &PgPool, source_id: i64, ids: &[String]) -> Result<Vec<Stored>> {
    let sql = format!("{STORED} and external_id = any($2)");
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(source_id)
        .bind(ids)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn touch(pool: &PgPool, ids: &[i64], run_id: i64) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("update documents set last_seen_run_id = $2 where id = any($1)")
        .bind(ids)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

struct Columns {
    ids: Vec<i64>,
    paths: Vec<String>,
    external_ids: Vec<Option<String>>,
    files: Vec<Option<String>>,
    shas: Vec<Option<String>>,
    sizes: Vec<i64>,
    mtimes: Vec<DateTime<Utc>>,
    kinds: Vec<Option<String>>,
    statuses: Vec<String>,
    reasons: Vec<Option<String>>,
}

impl Columns {
    fn of<'a>(rows: impl Iterator<Item = &'a Row>) -> Columns {
        let mut c = Columns {
            ids: Vec::new(),
            paths: Vec::new(),
            external_ids: Vec::new(),
            files: Vec::new(),
            shas: Vec::new(),
            sizes: Vec::new(),
            mtimes: Vec::new(),
            kinds: Vec::new(),
            statuses: Vec::new(),
            reasons: Vec::new(),
        };
        for row in rows {
            c.ids.push(row.id.unwrap_or(0));
            c.paths.push(row.path.clone());
            c.external_ids.push(row.external_id.clone());
            c.files.push(row.file.clone());
            c.shas.push(row.sha256.clone());
            c.sizes.push(row.size);
            c.mtimes.push(row.mtime);
            c.kinds.push(row.kind.clone());
            c.statuses.push(row.status.to_string());
            c.reasons.push(row.reason.clone());
        }
        c
    }
}

pub async fn write(pool: &PgPool, source_id: i64, run_id: i64, rows: &[Row]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let shas: Vec<&str> = rows.iter().filter_map(|r| r.sha256.as_deref()).collect();
    if !shas.is_empty() {
        sqlx::query(
            "insert into contents (sha256) select distinct unnest($1::text[]) \
             on conflict (sha256) do nothing",
        )
        .bind(&shas)
        .execute(&mut *tx)
        .await?;
    }
    let new = Columns::of(rows.iter().filter(|r| r.id.is_none()));
    if !new.paths.is_empty() {
        sqlx::query(
            "insert into documents (source_id, path, external_id, file, sha256, size, mtime, kind, \
             status, reason, content_id, first_run_id, last_seen_run_id) \
             select $1, n.path, n.external_id, n.file, n.sha256, n.size, n.mtime, n.kind, n.status, \
             n.reason, c.id, $2, $2 \
             from unnest($3::text[], $4::text[], $5::text[], $6::text[], $7::bigint[], \
             $8::timestamptz[], $9::text[], $10::text[], $11::text[]) \
             as n(path, external_id, file, sha256, size, mtime, kind, status, reason) \
             left join contents c on c.sha256 = n.sha256",
        )
        .bind(source_id)
        .bind(run_id)
        .bind(&new.paths)
        .bind(&new.external_ids)
        .bind(&new.files)
        .bind(&new.shas)
        .bind(&new.sizes)
        .bind(&new.mtimes)
        .bind(&new.kinds)
        .bind(&new.statuses)
        .bind(&new.reasons)
        .execute(&mut *tx)
        .await?;
    }
    let old = Columns::of(rows.iter().filter(|r| r.id.is_some()));
    if !old.ids.is_empty() {
        sqlx::query(
            "update documents d set path = u.path, external_id = u.external_id, file = u.file, \
             sha256 = u.sha256, size = u.size, mtime = u.mtime, kind = u.kind, status = u.status, \
             reason = u.reason, content_id = c.id, last_seen_run_id = $1, deleted_run_id = null, \
             rules_key = case when d.path = u.path and d.status = u.status then d.rules_key end \
             from unnest($2::bigint[], $3::text[], $4::text[], $5::text[], $6::text[], \
             $7::bigint[], $8::timestamptz[], $9::text[], $10::text[], $11::text[]) \
             as u(id, path, external_id, file, sha256, size, mtime, kind, status, reason) \
             left join contents c on c.sha256 = u.sha256 \
             where d.id = u.id",
        )
        .bind(run_id)
        .bind(&old.ids)
        .bind(&old.paths)
        .bind(&old.external_ids)
        .bind(&old.files)
        .bind(&old.shas)
        .bind(&old.sizes)
        .bind(&old.mtimes)
        .bind(&old.kinds)
        .bind(&old.statuses)
        .bind(&old.reasons)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn mark_deleted(pool: &PgPool, source_id: i64, run_id: i64) -> Result<u64> {
    let count = sqlx::query(
        "update documents set status = 'deleted', deleted_run_id = $2 \
         where source_id = $1 and last_seen_run_id < $2 and status <> 'deleted'",
    )
    .bind(source_id)
    .bind(run_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(count)
}

#[derive(FromRow)]
struct Move {
    old_id: i64,
    new_id: i64,
}

pub async fn merge_moves(pool: &PgPool, source_id: i64, run_id: i64) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let moves: Vec<Move> = sqlx::query_as(
        "with gone as (select id, sha256, \
             row_number() over (partition by sha256 order by id) as rn from documents \
             where source_id = $1 and deleted_run_id = $2 and sha256 is not null), \
         came as (select id, sha256, \
             row_number() over (partition by sha256 order by id) as rn from documents \
             where source_id = $1 and first_run_id = $2 and sha256 is not null \
             and status <> 'deleted') \
         select gone.id as old_id, came.id as new_id from gone join came using (sha256, rn)",
    )
    .bind(source_id)
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await?;
    if moves.is_empty() {
        return Ok(0);
    }
    let old_ids: Vec<i64> = moves.iter().map(|m| m.old_id).collect();
    let new_ids: Vec<i64> = moves.iter().map(|m| m.new_id).collect();
    let rows: Vec<(i64, Stored)> = {
        let stored: Vec<Stored> = sqlx::query_as(
            "select id, path, external_id, file, sha256, size, mtime, kind, status, reason \
             from documents where id = any($1)",
        )
        .bind(&new_ids)
        .fetch_all(&mut *tx)
        .await?;
        moves
            .iter()
            .filter_map(|m| {
                stored
                    .iter()
                    .find(|s| s.id == m.new_id)
                    .map(|s| (m.old_id, s.clone()))
            })
            .collect()
    };
    sqlx::query("delete from documents where id = any($1)")
        .bind(&new_ids)
        .execute(&mut *tx)
        .await?;
    let moved: Vec<Row> = rows
        .into_iter()
        .map(|(old_id, s)| Row {
            id: Some(old_id),
            path: s.path,
            external_id: s.external_id,
            file: s.file,
            sha256: s.sha256,
            size: s.size,
            mtime: s.mtime,
            kind: s.kind,
            status: match s.status.as_str() {
                "skipped" => "skipped",
                "error" => "error",
                _ => "present",
            },
            reason: s.reason,
        })
        .collect();
    let c = Columns::of(moved.iter());
    sqlx::query(
        "update documents d set path = u.path, size = u.size, mtime = u.mtime, kind = u.kind, \
         status = u.status, reason = u.reason, last_seen_run_id = $1, deleted_run_id = null, \
         rules_key = null \
         from unnest($2::bigint[], $3::text[], $4::bigint[], $5::timestamptz[], $6::text[], \
         $7::text[], $8::text[]) as u(id, path, size, mtime, kind, status, reason) \
         where d.id = u.id",
    )
    .bind(run_id)
    .bind(&c.ids)
    .bind(&c.paths)
    .bind(&c.sizes)
    .bind(&c.mtimes)
    .bind(&c.kinds)
    .bind(&c.statuses)
    .bind(&c.reasons)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(old_ids.len() as u64)
}

pub fn parse_plan(text: &str) -> Result<Vec<Relocation>> {
    let mut plan = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [from_source, from_path, to_source, to_path, ..] = fields[..] else {
            anyhow::bail!("line {}: needs four tab-separated columns", number + 1);
        };
        if [from_source, from_path, to_source, to_path]
            .iter()
            .any(|field| field.is_empty() || field.ends_with('/'))
        {
            anyhow::bail!("line {}: an empty column or a path ending in /", number + 1);
        }
        plan.push(Relocation {
            from_source: from_source.to_string(),
            from_path: from_path.to_string(),
            to_source: to_source.to_string(),
            to_path: to_path.to_string(),
        });
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_skips_comments_and_extra_columns() {
        let plan = parse_plan(
            "# source\tpath\tto_source\tto_path\n\n\
             old\tP-1 Acme\tprojects\tP-1 Acme\tarchive/old\tarchive/new\n\
             old\tloose.pdf\tgeneral\tLoose/loose.pdf\n",
        )
        .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[1].to_path, "Loose/loose.pdf");
        assert_eq!(plan[0].to_source, "projects");
    }

    #[test]
    fn a_plan_line_needs_four_columns() {
        assert!(parse_plan("old\tP-1\tprojects\n").is_err());
        assert!(parse_plan("old\tP-1/\tprojects\tP-1\n").is_err());
    }
}
