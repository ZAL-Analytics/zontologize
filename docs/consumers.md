# Reading the database

Apps read the views in `zontologize_api` (`[database] api_schema`). The views only gain columns
between versions; `select zontologize_api.contract_version()` says which contract you have.

## Quick start

Find text, with the file and page it is on:

```sql
select d.path, c.page_from, left(c.text, 200) as snippet
from zontologize_api.chunks c
join zontologize_api.documents d on d.content_id = c.content_id,
     websearch_to_tsquery('english', 'seal leak') q
where c.tsv_stem @@ q
order by ts_rank(c.tsv_stem, q) desc
limit 10;
```

Find an entity, typos allowed, and every page that names it:

```sql
select e.name, e.class_iri, d.path, p.page_no
from zontologize_api.canonical_entities e
join zontologize_api.entities x on e.id in (x.id, x.canonical_id)
join zontologize_api.mention_pages p on p.entity_id = x.id
join zontologize_api.documents d on d.content_id = p.content_id
where similarity(lower(e.name), lower('Nortside Pumping Station')) > 0.5;
```

Follow the graph: everything directly linked to one project.

```sql
load 'age';
set search_path = ag_catalog, "$user", public;
select * from cypher('zontologize_graph', $$
  match (p:Project {key: 'P-2021-0042'})-[r]-(n) return type(r), n.name
$$) as (relation agtype, name agtype);
```

## Running

A run after new files arrive, from cron or by hand:

```sh
zontologize scan && zontologize extract && zontologize attributes \
  && zontologize embed \
  && zontologize resolve && zontologize graph sync
```

Only one zontologize command writes at a time. While a run is going, some documents are updated
and others not yet; each document is always complete. `select * from zontologize_api.state`
shows `running`, `running_since`, `last_scan_finished_at`, `ontology_version` and
`contract_version`.

## Views

| View | Holds |
|---|---|
| `sources` | Configured sources |
| `documents` | Files that are not deleted; `status` is `present`, `skipped` (not read) or `error` |
| `pages`, `chunks` | Text per content (`content_id`); `chunks.tsv` for full-text search without stemming (IDs), `chunks.tsv_stem` with Dutch and English stemming (since contract 3). For PDF and image pages, `pages.width` and `pages.height` give the page size (PDF points after `/Rotate`, image pixels) and `pages.words` the words as `[[start, end, x0, y0, x1, y1], ...]`: char offsets into `pages.text` and a 0-1 box with a top-left origin; all three are null for other pages (since contract 6) |
| `entities` | Things found; `canonical_id` groups duplicates |
| `aliases` | Other names of an entity |
| `canonical_entities` | One row per cluster of merged entities (since contract 4): the root's `id`, `class_iri`, `key`, `name`, `props`, `dataset`, `document_id`, `members` (rows in the cluster) and `aliases` (names of the other members and all their aliases). `class_iri` and `name` come from the root's `entity_classes` row when it takes effect; a cluster whose root is not an entity is left out, and so are members that are not an entity |
| `entity_classes` | Corrections per entity (since contract 5): `class_iri` (null: not an entity), `name` (display name of the cluster, null: keep), `method`, `status`, `confidence`. `status` is `auto`, `proposed` (waits for `review classes`), `confirmed` or `rejected` (by a person); only `auto` and `confirmed` rows take effect |
| `canonical_relations` | `relations` with both ends mapped to their cluster root (since contract 4); self-loops and relations of entities or clusters that are not an entity dropped, one row per `src_id`, `property_iri`, `dst_id` (the confirmed one first, then the highest confidence), `support` counts the relations behind it |
| `mentions`, `class_mentions` | Where an entity or class is named: chunk or document, span, method, confidence |
| `mention_pages` | Chunk mentions on their page: `mention_id`, `entity_id`, `content_id`, `page_no`, and `span_start`/`span_end` as char offsets into that page's `pages.text`, end clipped to the page (since contract 6) |
| `relations` | Facts between entities, without rejected ones; `status` is `auto`, `proposed` or `confirmed` |
| `attributes` | Values of entities: `value_text` as found, `unit_text`, `unit_iri`, `low`/`high` as written and `si_low`/`si_high` in SI units, chunk, span, method, confidence; `superseded` when a newer revision replaces the document |
| `units` | Units of the ontology: symbols, labels, `dimension`, `conversion_multiplier`, `conversion_offset` |
| `classes`, `properties` | The selected ontology the facts use; attribute properties have `value_kind`, `quantity_kind`, `dimension`; `classes.privacy_category` (since contract 3) says how a class is hidden, from `zontologize:privacyCategory` |
| `class_ancestors` | `iri`, `ancestor`, `depth` (0 is the class itself) |
| `embedding_models`, `embeddings`, `class_embeddings` | Vectors, see [embeddings.md](embeddings.md) |
| `graph_labels` | Graph label of each class and property |

`zontologize ontology export --out ontology.ttl` writes the same ontology as Turtle.

A reading role needs:

```sql
grant usage on schema zontologize, zontologize_api to reader;
grant select on all tables in schema zontologize, zontologize_api to reader;
```

## Graph

`graph sync` rebuilds the AGE graph `zontologize_graph` (`[database] graph`): one vertex per
cluster of merged entities (`canonical_entities`) with `id`, `class`, `key`, `name`, `dataset`,
`members` and `aliases`, and one edge per row of `canonical_relations` between cluster roots with
`id`, `iri`, `status`, `confidence`, `method` and `support`. Same-as links inside a cluster are
not edges. A vertex takes its label from the cluster's effective class (`entity_classes`). Labels are the class and
property labels in camel case (`Project`, `describes`); `graph_labels` maps them to IRIs.

Each connection that runs Cypher needs:

```sql
load 'age';
set search_path = ag_catalog, "$user", public;
```

A role that is not superuser cannot `load 'age'`; a superuser runs
`alter role reader set session_preload_libraries = 'age';` once instead, and grants
`usage` on `ag_catalog` and the graph schema.
zontologize itself skips `load 'age'` when AGE is in `shared_preload_libraries` or
`session_preload_libraries`, so it can run as a role that is not superuser (for example with
`PGOPTIONS='-c role=zontologize_writer'`); that role needs `pg_read_all_settings` to see them.

Entity names and aliases have trigram indexes on `lower(name)` and `lower(alias)` (`pg_trgm`),
so `similarity(lower(name), lower($1))` finds names with typos.

Every vertex label has an index on its properties, so a lookup by `id`, `key` or `name` is fast,
fastest with a label. While `graph sync` runs, Cypher queries wait a few seconds. Fixed patterns
are fast:

```sql
select * from cypher('zontologize_graph', $$
  match (d)-[:describes]->(p:Project {key: 'P-2021-0042'}) return d.name
$$) as (document agtype);
```

Walks of several hops are faster in SQL on the views:

```sql
with recursive parts (id, depth) as (
  select 42, 0
  union
  select r.src_id, p.depth + 1
  from zontologize_api.relations r
  join parts p on r.dst_id = p.id
  where r.property_iri = 'https://example.org/acme#partOf' and p.depth < 5
)
select e.name, p.depth from parts p join zontologize_api.entities e on e.id = p.id;
```

## Attributes

A value is a range: `si_low` and `si_high` are equal for one number, and one is null for a limit
("at most 50 bar"). SI is `(value + conversion_offset) * conversion_multiplier`, so pressure is in
absolute pascal and temperature in kelvin. A question in bar converts with the `units` view:

```sql
with q as (
  select (10 + conversion_offset) * conversion_multiplier as lo,
         (15 + conversion_offset) * conversion_multiplier as hi
  from zontologize_api.units where iri = 'http://qudt.org/vocab/unit/BAR'
)
select e.name, a.value_text
from zontologize_api.attributes a
join zontologize_api.entities e on e.id = a.entity_id, q
where a.property_iri = 'https://example.org/acme#dischargePressure' and not a.superseded
  and coalesce(a.si_low, a.si_high) <= q.hi and coalesce(a.si_high, a.si_low) >= q.lo;
```

`resolve` links stored entities of one class whose values are close with `similarTo` relations
(always `proposed`, the score as `confidence`). To find stored equipment like a new request,
score at question time the same way: 1 when the ranges overlap, otherwise falling with the
ratio of the nearest values until the tolerance (default 0.2), averaged over the properties:

```sql
with request (property_iri, lo, hi) as (values
  ('https://example.org/acme#ratedPower', 250000.0, 250000.0),
  ('https://example.org/acme#dischargePressure', 1101325.0, 1101325.0)),
scores as (
  select coalesce(e.canonical_id, e.id) as id, r.property_iri,
    max(case when coalesce(a.si_low, a.si_high) <= r.hi and coalesce(a.si_high, a.si_low) >= r.lo
      then 1.0 else greatest(0, 1 - abs(ln(greatest(r.lo, coalesce(a.si_low, a.si_high))
        / least(r.hi, coalesce(a.si_high, a.si_low)))) / ln(1.2)) end) as score
  from request r
  join zontologize_api.attributes a on a.property_iri = r.property_iri and not a.superseded
  join zontologize_api.entities e on e.id = a.entity_id
  where coalesce(a.si_low, a.si_high) > 0
  group by 1, 2)
select e.name, avg(s.score) as score, count(*) as shared
from scores s join zontologize_api.entities e on e.id = s.id
group by e.name having count(*) >= 2 order by score desc limit 10;
```
