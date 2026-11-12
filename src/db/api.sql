create or replace function {api}.contract_version() returns integer
    language sql immutable as 'select 6';

create or replace view {api}.sources as
select id, name, kind, root, dataset
from {schema}.sources;

create or replace view {api}.documents as
select d.id, d.source_id, d.path, d.external_id, d.file, d.sha256, d.content_id, d.size,
    d.mtime, d.kind, d.doc_type, d.type_method, d.props, d.status, d.reason, s.dataset,
    d.first_run_id, d.last_seen_run_id
from {schema}.documents d
join {schema}.sources s on s.id = d.source_id
where d.status <> 'deleted';

create or replace view {api}.pages as
select p.content_id, p.page_no, p.method, p.ocr_confidence, p.text, p.width, p.height, p.words
from {schema}.pages p
where exists (select 1 from {schema}.documents d
    where d.content_id = p.content_id and d.status = 'present');

create or replace view {api}.chunks as
select c.id, c.content_id, c.ord, c.page_from, c.page_to, c.text, c.ts_config, c.tsv, c.tsv_stem
from {schema}.chunks c
where exists (select 1 from {schema}.documents d
    where d.content_id = c.content_id and d.status = 'present');

create or replace view {api}.entities as
select id, class_iri, key, name, props, dataset, method, confidence, document_id,
    canonical_id
from {schema}.entities;

create or replace view {api}.entity_classes as
select entity_id, class_iri, name, method, status, confidence, ontology_version_id, created_at
from {schema}.entity_classes;

create or replace view {api}.canonical_entities as
select r.id, coalesce(o.class_iri, r.class_iri) as class_iri, r.key,
    coalesce(o.name, r.name) as name, r.props, r.dataset, r.document_id,
    (select count(*) from {schema}.entities m
        where (m.id = r.id or m.canonical_id = r.id) and not exists (
            select 1 from {schema}.entity_classes x where x.entity_id = m.id
            and x.status in ('auto', 'confirmed') and x.class_iri is null))
        as members,
    coalesce((select array_agg(distinct n.name order by n.name) from (
        select r.name
        union all
        select m.name from {schema}.entities m where m.canonical_id = r.id
        and not exists (select 1 from {schema}.entity_classes x where x.entity_id = m.id
            and x.status in ('auto', 'confirmed') and x.class_iri is null)
        union all
        select a.alias from {schema}.aliases a
        join {schema}.entities m on m.id = a.entity_id
        where (m.id = r.id or m.canonical_id = r.id)
        and not exists (select 1 from {schema}.entity_classes x where x.entity_id = m.id
            and x.status in ('auto', 'confirmed') and x.class_iri is null)
    ) n where n.name <> coalesce(o.name, r.name)), array[]::text[]) as aliases
from {schema}.entities r
left join {schema}.entity_classes o on o.entity_id = r.id and o.status in ('auto', 'confirmed')
where (r.canonical_id is null or r.canonical_id = r.id)
    and not (o.entity_id is not null and o.class_iri is null);

create or replace view {api}.canonical_relations as
select id, src_id, property_iri, dst_id, props, method, status, confidence, document_id,
    chunk_id, span_start, span_end, support
from (
    select r.id, coalesce(a.canonical_id, a.id) as src_id, r.property_iri,
        coalesce(b.canonical_id, b.id) as dst_id, r.props, r.method, r.status, r.confidence,
        r.document_id, r.chunk_id, r.span_start, r.span_end,
        count(*) over w as support,
        row_number() over (w order by r.status = 'confirmed' desc, r.confidence desc, r.id)
            as rank
    from {schema}.relations r
    join {schema}.entities a on a.id = r.src_id
    join {schema}.entities b on b.id = r.dst_id
    where r.status <> 'rejected' and coalesce(a.canonical_id, a.id) <> coalesce(b.canonical_id, b.id)
        and not exists (select 1 from {schema}.entity_classes x
            where x.entity_id in (a.id, b.id, coalesce(a.canonical_id, a.id),
                coalesce(b.canonical_id, b.id))
            and x.status in ('auto', 'confirmed') and x.class_iri is null)
    window w as (partition by coalesce(a.canonical_id, a.id), r.property_iri,
        coalesce(b.canonical_id, b.id))
) g
where rank = 1;

create or replace view {api}.aliases as
select entity_id, alias, lang, method
from {schema}.aliases;

create or replace view {api}.mentions as
select id, entity_id, document_id, chunk_id, span_start, span_end, method, confidence
from {schema}.mentions;

create or replace view {api}.mention_pages as
select m.id as mention_id, m.entity_id, c.content_id, p.page_no,
    c.text_start + m.span_start - p.text_start as span_start,
    least(c.text_start + m.span_end - p.text_start, char_length(p.text)) as span_end
from {schema}.mentions m
join {schema}.chunks c on c.id = m.chunk_id
join {schema}.pages p on p.content_id = c.content_id
    and c.text_start + m.span_start >= p.text_start
    and c.text_start + m.span_start < p.text_start + char_length(p.text)
where m.span_start is not null and m.span_end is not null
    and exists (select 1 from {schema}.documents d
        where d.content_id = c.content_id and d.status = 'present');

create or replace view {api}.class_mentions as
select id, class_iri, document_id, chunk_id, span_start, span_end, method, confidence
from {schema}.class_mentions;

create or replace view {api}.relations as
select id, src_id, property_iri, dst_id, props, method, status, confidence, document_id,
    chunk_id, span_start, span_end
from {schema}.relations
where status <> 'rejected';

create or replace view {api}.classes as
select c.iri, c.parents, c.types, c.labels, c.alt_labels, c.definition, c.privacy_category
from {schema}.classes c
where c.selected and c.ontology_version_id = ({current});

create or replace view {api}.properties as
select p.iri, p.kind, p.domain, p.range, p.parents, p.labels, p.alt_labels, p.value_kind,
    p.quantity_kind, p.dimension, p.tolerance
from {schema}.properties p
where p.selected and p.ontology_version_id = ({current});

create or replace view {api}.units as
select u.iri, u.symbols, u.labels, u.alt_labels, u.dimension, u.conversion_multiplier,
    u.conversion_offset
from {schema}.units u
where u.ontology_version_id = ({current});

create or replace view {api}.attributes as
select a.id, a.entity_id, a.property_iri, a.value_text, a.unit_text, a.unit_iri, a.low, a.high,
    a.si_low, a.si_high, a.dimension, a.chunk_id, a.span_start, a.span_end, a.method,
    a.confidence,
    exists (select 1 from {schema}.documents d
        join {schema}.entities de on de.document_id = d.id and de.key = d.id::text
        join {schema}.relations r on r.dst_id = de.id
            and r.property_iri = 'http://purl.org/dc/terms/replaces' and r.status <> 'rejected'
        where d.content_id = k.content_id and d.status = 'present') as superseded
from {schema}.attributes a
join {schema}.chunks k on k.id = a.chunk_id
where exists (select 1 from {schema}.documents d
    where d.content_id = k.content_id and d.status = 'present');

create or replace view {api}.class_ancestors as
with recursive up (iri, ancestor, depth) as (
    select c.iri, c.iri, 0
    from {schema}.classes c
    where c.selected and c.ontology_version_id = ({current})
    union
    select up.iri, parent, up.depth + 1
    from up
    join {schema}.classes c on c.iri = up.ancestor and c.ontology_version_id = ({current})
    cross join lateral unnest(c.parents || c.types) as parent
    where up.depth < 100
)
select iri, ancestor, min(depth) as depth
from up
group by iri, ancestor;

create or replace view {api}.embedding_models as
select id, name, repo, file, data_files, tokenizer, revision, sha256, dimensions, pooling,
    output, passage_prefix, query_prefix, max_tokens, normalize, current
from {schema}.embedding_models;

create or replace view {api}.embeddings as
select model_id, owner_kind, owner_id, truncated, embedding
from {schema}.embeddings;

create or replace view {api}.class_embeddings as
select model_id, iri, truncated, embedding
from {schema}.class_embeddings;

create or replace view {api}.graph_labels as
select kind, iri, label
from {schema}.graph_labels;

create or replace view {api}.state as
select
    exists (select 1 from pg_locks l
        where l.locktype = 'advisory' and l.granted and l.objsubid = 2
        and l.classid = hashtext('zontologize')::oid
        and l.objid = hashtext({schema_text})::oid) as running,
    (select max(started_at) from {schema}.runs where status = 'running') as running_since,
    (select max(finished_at) from {schema}.runs
        where status = 'finished' and stats ? 'walk') as last_scan_finished_at,
    (select sha256 from {schema}.ontology_versions where id = ({current})) as ontology_version,
    {api}.contract_version() as contract_version;
