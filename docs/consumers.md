# Reading the database

Apps read the views in `zontologize_api` (`[database] api_schema`). The views only gain columns
between versions; `select zontologize_api.contract_version()` says which contract you have.

A run after new files arrive, from cron or by hand:

```sh
zontologize scan && zontologize extract && zontologize embed \
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
| `pages`, `chunks` | Text per content (`content_id`); `chunks.tsv` for full-text search |
| `entities` | Things found; `canonical_id` groups duplicates |
| `aliases` | Other names of an entity |
| `mentions`, `class_mentions` | Where an entity or class is named: chunk or document, span, method, confidence |
| `relations` | Facts between entities, without rejected ones; `status` is `auto`, `proposed` or `confirmed` |
| `classes`, `properties` | The selected ontology the facts use |
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
entity with `id`, `class`, `key`, `name`, `canonical_id` and `dataset`, one edge per relation that
is not rejected with `id`, `iri`, `status`, `confidence` and `method`. Labels are the class and
property labels in camel case (`Project`, `describes`); `graph_labels` maps them to IRIs.

Each connection that runs Cypher needs:

```sql
load 'age';
set search_path = ag_catalog, "$user", public;
```

A role that is not superuser cannot `load 'age'`; a superuser runs
`alter role reader set session_preload_libraries = 'age';` once instead, and grants
`usage` on `ag_catalog` and the graph schema.

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
