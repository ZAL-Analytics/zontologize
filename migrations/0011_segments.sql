create table segments (
    content_id bigint not null references contents (id) on delete cascade,
    ord integer not null,
    page_from integer not null,
    page_to integer not null,
    doc_type text,
    rules_key text not null,
    primary key (content_id, ord)
);
