use std::collections::BTreeSet;

use anyhow::Result;
use sqlx::{FromRow, PgPool};

use crate::db::entity_classes::{self, Override};
use crate::db::resolve::{BUILT_IN, NewLink, SAME_AS};

pub const SNIPPETS: i64 = 3;
pub const RELATIONS: i64 = 10;
pub const METHOD: &str = "curate:";

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Proposed {
    pub src_id: i64,
    pub dst_id: i64,
    pub method: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, FromRow)]
pub struct MemberRow {
    pub id: i64,
    pub name: String,
    pub class_iri: String,
    pub aliases: Vec<String>,
    pub mentions: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct SnippetRow {
    pub entity_id: i64,
    pub text: String,
    pub span_start: i32,
    pub span_end: i32,
}

#[derive(Debug, Clone, FromRow)]
pub struct RelationRow {
    pub entity_id: i64,
    pub property_iri: String,
    pub outgoing: bool,
    pub name: String,
    pub class_iri: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct OverrideRow {
    pub entity_id: i64,
    pub name: String,
    pub class_iri: String,
    pub new_class: Option<String>,
    pub new_name: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Root {
    pub id: i64,
    pub name: String,
    pub class_iri: String,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct End {
    pub property_iri: String,
    pub src_id: i64,
    pub dst_id: i64,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct NameRow {
    pub root_id: i64,
    pub name: String,
    pub aliases: Vec<String>,
}

pub async fn names(pool: &PgPool) -> Result<Vec<NameRow>> {
    let rows = sqlx::query_as(
        "select coalesce(e.canonical_id, e.id) as root_id, e.name, \
         array(select a.alias from aliases a where a.entity_id = e.id and a.method <> $1 \
         order by a.alias) as aliases from entities e where e.document_id is null \
         and not exists (select 1 from entity_classes x where x.entity_id = e.id \
         and x.status in ('auto', 'confirmed') and x.class_iri is null) order by e.id",
    )
    .bind(crate::resolve::METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn roots(pool: &PgPool) -> Result<Vec<Root>> {
    let rows = sqlx::query_as(
        "select e.id, coalesce(o.name, e.name) as name, \
         coalesce(o.class_iri, e.class_iri) as class_iri from entities e \
         left join entity_classes o on o.entity_id = e.id and o.status in ('auto', 'confirmed') \
         where (e.canonical_id is null or e.canonical_id = e.id) and e.document_id is null \
         and not (o.entity_id is not null and o.class_iri is null) order by e.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn ends(pool: &PgPool) -> Result<Vec<End>> {
    let rows = sqlx::query_as(
        "select distinct r.property_iri, coalesce(a.canonical_id, a.id) as src_id, \
         coalesce(b.canonical_id, b.id) as dst_id from relations r \
         join entities a on a.id = r.src_id join entities b on b.id = r.dst_id \
         where r.status <> 'rejected' and r.property_iri <> all($1) \
         and not exists (select 1 from entity_classes x where x.entity_id in (a.id, b.id) \
         and x.status in ('auto', 'confirmed') and x.class_iri is null) order by 1, 2, 3",
    )
    .bind(&BUILT_IN[..])
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn proposed(pool: &PgPool) -> Result<Vec<Proposed>> {
    let rows = sqlx::query_as(
        "select src_id, dst_id, method, confidence from relations \
         where property_iri = $1 and status = 'proposed' order by src_id, dst_id, method",
    )
    .bind(SAME_AS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn rejected(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    let rows = sqlx::query_as(
        "select src_id, dst_id from relations where property_iri = $1 and status = 'rejected' \
         and src_id = any($2) and dst_id = any($2)",
    )
    .bind(SAME_AS)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn members(pool: &PgPool, ids: &[i64]) -> Result<Vec<MemberRow>> {
    let rows = sqlx::query_as(
        "select e.id, e.name, coalesce((select o.class_iri from entity_classes o \
         where o.entity_id = e.id and o.status in ('auto', 'confirmed')), e.class_iri) as class_iri, \
         array(select a.alias from aliases a where a.entity_id = e.id and a.method <> $2 \
         order by a.alias) as aliases, \
         (select count(*) from mentions m where m.entity_id = e.id) as mentions \
         from entities e where e.id = any($1) order by e.id",
    )
    .bind(ids)
    .bind(crate::resolve::METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn snippets(pool: &PgPool, ids: &[i64], around: usize) -> Result<Vec<SnippetRow>> {
    let rows = sqlx::query_as(
        "select m.entity_id, substr(k.text, w.first + 1, m.span_end - w.first + $3) as text, \
         m.span_start - w.first as span_start, m.span_end - w.first as span_end \
         from entities e cross join lateral (select x.id, x.entity_id, x.chunk_id, x.span_start, \
         x.span_end from mentions x where x.entity_id = e.id and x.chunk_id is not null \
         order by x.id limit $2) m \
         join chunks k on k.id = m.chunk_id \
         cross join lateral (select greatest(m.span_start - $3, 0) as first) w \
         where e.id = any($1) order by m.entity_id, m.id",
    )
    .bind(ids)
    .bind(SNIPPETS)
    .bind(around as i32)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn relations(pool: &PgPool, ids: &[i64]) -> Result<Vec<RelationRow>> {
    let rows = sqlx::query_as(
        "select e.id as entity_id, r.property_iri, r.outgoing, r.name, r.class_iri \
         from entities e cross join lateral (select distinct x.property_iri, \
         x.src_id = e.id as outgoing, o.name, o.class_iri from relations x \
         join entities o on o.id = case when x.src_id = e.id then x.dst_id else x.src_id end \
         where (x.src_id = e.id or x.dst_id = e.id) and x.property_iri <> $2 \
         and not starts_with(x.method, $3) and not starts_with(x.method, $4) \
         and x.status <> 'rejected' order by 1, 2 desc, 3, 4 limit $5) r \
         where e.id = any($1) order by 1, 2, 3 desc, 4, 5",
    )
    .bind(ids)
    .bind(SAME_AS)
    .bind(crate::resolve::method(""))
    .bind(METHOD)
    .bind(RELATIONS)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

type LinkKey = (i64, i64, String, String, f64);
type RowKey = (i64, Option<String>, Option<String>, String, String, f64);

pub async fn write(
    pool: &PgPool,
    members: &[i64],
    links: &[NewLink],
    rows: &[Override],
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let existing: Vec<(i64, i64, i64, String, String, f64)> = sqlx::query_as(
        "select id, src_id, dst_id, method, status, confidence from relations \
         where property_iri = $1 and starts_with(method, $2) and status in ('auto', 'proposed') \
         and src_id = any($3) and dst_id = any($3)",
    )
    .bind(SAME_AS)
    .bind(METHOD)
    .bind(members)
    .fetch_all(&mut *tx)
    .await?;
    let settled: BTreeSet<(i64, i64)> = sqlx::query_as::<_, (i64, i64)>(
        "select least(src_id, dst_id), greatest(src_id, dst_id) from relations \
         where property_iri = $1 and status in ('confirmed', 'rejected') \
         and src_id = any($2) and dst_id = any($2)",
    )
    .bind(SAME_AS)
    .bind(members)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let stored: Vec<RowKey> = sqlx::query_as(
        "select entity_id, class_iri, name, method, status, confidence from entity_classes \
         where entity_id = any($1)",
    )
    .bind(members)
    .fetch_all(&mut *tx)
    .await?;
    let kept: BTreeSet<i64> = stored
        .iter()
        .filter(|r| r.4 == "confirmed" || r.4 == "rejected")
        .map(|r| r.0)
        .collect();
    let links: Vec<&NewLink> = links
        .iter()
        .filter(|l| !settled.contains(&(l.src_id.min(l.dst_id), l.src_id.max(l.dst_id))))
        .collect();
    let rows: Vec<&Override> = rows
        .iter()
        .filter(|r| !kept.contains(&r.entity_id))
        .collect();
    let mut have: Vec<LinkKey> = existing
        .iter()
        .map(|l| (l.1, l.2, l.3.clone(), l.4.clone(), l.5))
        .collect();
    let mut want: Vec<LinkKey> = links
        .iter()
        .map(|l| {
            (
                l.src_id,
                l.dst_id,
                l.method.clone(),
                l.status.to_string(),
                l.confidence,
            )
        })
        .collect();
    let mut have_rows: Vec<RowKey> = stored
        .into_iter()
        .filter(|r| r.3.starts_with(METHOD) && !kept.contains(&r.0))
        .collect();
    let mut want_rows: Vec<RowKey> = rows
        .iter()
        .map(|r| {
            (
                r.entity_id,
                r.class_iri.clone(),
                r.name.clone(),
                r.method.clone(),
                r.status.clone(),
                r.confidence,
            )
        })
        .collect();
    let order = |a: &LinkKey, b: &LinkKey| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2));
    have.sort_by(order);
    want.sort_by(order);
    have_rows.sort_by_key(|r| r.0);
    want_rows.sort_by_key(|r| r.0);
    if have == want && have_rows == want_rows {
        return Ok(false);
    }
    let ids: Vec<i64> = existing.iter().map(|l| l.0).collect();
    crate::db::resolve::delete(&mut *tx, &ids).await?;
    entity_classes::delete(&mut tx, members, METHOD).await?;
    let links: Vec<NewLink> = links.into_iter().cloned().collect();
    crate::db::resolve::insert(&mut *tx, &links).await?;
    let rows: Vec<Override> = rows.into_iter().cloned().collect();
    entity_classes::upsert(&mut tx, &rows).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn link_counts(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query_as(
        "select status, count(*) from relations where property_iri = $1 \
         and starts_with(method, $2) group by 1 order by 1",
    )
    .bind(SAME_AS)
    .bind(METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn overrides(pool: &PgPool) -> Result<Vec<OverrideRow>> {
    let rows = sqlx::query_as(
        "select e.id as entity_id, e.name, e.class_iri, c.class_iri as new_class, \
         c.name as new_name, c.status from entity_classes c join entities e on e.id = c.entity_id \
         where starts_with(c.method, $1) order by e.name, e.id",
    )
    .bind(METHOD)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn calls(pool: &PgPool, job: i64) -> Result<(i64, f64)> {
    let row = sqlx::query_as(
        "select count(*), coalesce(sum(cost_usd), 0)::float8 from llm_calls where job_id = $1",
    )
    .bind(job)
    .fetch_one(pool)
    .await?;
    Ok(row)
}
