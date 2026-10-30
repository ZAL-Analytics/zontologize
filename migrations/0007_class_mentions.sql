create table class_mentions (
    id bigint generated always as identity primary key,
    class_iri text not null,
    document_id bigint references documents (id) on delete cascade,
    chunk_id bigint references chunks (id) on delete cascade,
    span_start integer not null,
    span_end integer not null,
    method text not null,
    check ((document_id is null) <> (chunk_id is null))
);

create index class_mentions_class on class_mentions (class_iri);
create index class_mentions_document on class_mentions (document_id);
create index class_mentions_chunk on class_mentions (chunk_id);
