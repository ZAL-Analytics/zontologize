alter table properties
    add column alt_labels jsonb not null default '{}',
    add column value_kind text,
    add column quantity_kind text,
    add column dimension text,
    add column tolerance double precision;

create table units (
    ontology_version_id bigint not null references ontology_versions (id) on delete cascade,
    iri text not null,
    symbols text[] not null,
    labels jsonb not null,
    alt_labels jsonb not null,
    dimension text not null,
    conversion_multiplier double precision not null,
    conversion_offset double precision not null,
    primary key (ontology_version_id, iri)
);

alter table chunks add column attr_key text;

create table attributes (
    id bigint generated always as identity primary key,
    entity_id bigint not null references entities (id) on delete cascade,
    property_iri text not null,
    value_text text not null,
    unit_text text,
    unit_iri text,
    low double precision,
    high double precision,
    si_low double precision,
    si_high double precision,
    dimension text,
    chunk_id bigint not null references chunks (id) on delete cascade,
    span_start integer not null,
    span_end integer not null,
    method text not null,
    confidence double precision not null
);

create index attributes_entity on attributes (entity_id);
create index attributes_chunk on attributes (chunk_id);
create index attributes_property on attributes (property_iri, si_low, si_high);

alter table fact_reviews drop constraint fact_reviews_kind_check;
alter table fact_reviews add constraint fact_reviews_kind_check
    check (kind in ('mention', 'relation', 'attribute'));
