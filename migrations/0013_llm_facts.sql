alter table chunks add column llm_key text;

alter table relations
    add column span_start integer,
    add column span_end integer;

create table relation_terms (
    id bigint generated always as identity primary key,
    chunk_id bigint not null references chunks (id) on delete cascade,
    src_class text not null,
    label text not null,
    lang text not null,
    dst_class text not null,
    span_start integer not null,
    span_end integer not null,
    method text not null
);

create index relation_terms_chunk on relation_terms (chunk_id);

alter table llm_job_items
    alter column chunk_id drop not null,
    add column content_id bigint,
    add column page_from integer;
