create table chunks (
    id bigint generated always as identity primary key,
    content_id bigint not null references contents (id) on delete cascade,
    ord integer not null,
    page_from integer,
    page_to integer,
    text text not null,
    ts_config regconfig not null,
    tsv tsvector generated always as (to_tsvector(ts_config, text)) stored,
    unique (content_id, ord)
);

create index chunks_tsv on chunks using gin (tsv);
