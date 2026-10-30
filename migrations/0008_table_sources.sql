alter table sources add column view text;

alter table documents
    add column external_id text,
    add column file text;

alter table documents drop constraint documents_source_id_path_key;

create unique index documents_source_path on documents (source_id, path)
    where external_id is null;
create index documents_path on documents (source_id, path);
create unique index documents_external_id on documents (source_id, external_id)
    where external_id is not null;
