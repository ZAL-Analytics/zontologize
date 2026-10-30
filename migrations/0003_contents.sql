create table contents (
    id bigint generated always as identity primary key,
    sha256 text not null unique,
    status text not null default 'pending' check (status in ('pending', 'done', 'error')),
    error text,
    extract_key text,
    extract_seconds double precision,
    chunk_key text,
    rules_key text
);

create table pages (
    content_id bigint not null references contents (id) on delete cascade,
    page_no integer not null,
    method text not null,
    ocr_confidence double precision,
    text text not null,
    primary key (content_id, page_no)
);

alter table documents add column content_id bigint references contents (id);

create index documents_content on documents (content_id);
