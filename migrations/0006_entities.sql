alter table documents
    add column doc_type text,
    add column props jsonb not null default '{}',
    add column rules_key text;

create table entities (
    id bigint generated always as identity primary key,
    class_iri text not null,
    key text not null,
    name text not null,
    props jsonb not null default '{}',
    dataset text not null default '',
    method text not null,
    confidence double precision not null,
    document_id bigint references documents (id) on delete cascade,
    ontology_version_id bigint not null references ontology_versions (id),
    unique (class_iri, key)
);

create table aliases (
    entity_id bigint not null references entities (id) on delete cascade,
    alias text not null,
    lang text not null default '',
    method text not null,
    primary key (entity_id, alias)
);

create table mentions (
    id bigint generated always as identity primary key,
    entity_id bigint not null references entities (id) on delete cascade,
    document_id bigint references documents (id) on delete cascade,
    chunk_id bigint references chunks (id) on delete cascade,
    span_start integer not null,
    span_end integer not null,
    method text not null,
    confidence double precision not null,
    check ((document_id is null) <> (chunk_id is null))
);

create index mentions_entity on mentions (entity_id);
create index mentions_document on mentions (document_id);
create index mentions_chunk on mentions (chunk_id);

create table relations (
    id bigint generated always as identity primary key,
    src_id bigint not null references entities (id) on delete cascade,
    property_iri text not null,
    dst_id bigint not null references entities (id) on delete cascade,
    props jsonb not null default '{}',
    method text not null,
    status text not null default 'auto'
        check (status in ('auto', 'proposed', 'confirmed', 'rejected')),
    confidence double precision not null,
    document_id bigint references documents (id) on delete cascade,
    chunk_id bigint references chunks (id) on delete cascade,
    check ((document_id is null) <> (chunk_id is null))
);

create index relations_src on relations (src_id);
create index relations_dst on relations (dst_id);
create index relations_document on relations (document_id);
create index relations_chunk on relations (chunk_id);
