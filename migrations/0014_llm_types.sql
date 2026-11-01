create table llm_types (
    content_id bigint not null references contents (id) on delete cascade,
    page_from integer not null,
    page_to integer not null,
    doc_type text,
    method text not null,
    confidence double precision not null,
    cache_key text not null,
    primary key (content_id, page_from)
);

alter table documents add column type_method text;
