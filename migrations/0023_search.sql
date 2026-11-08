create index entities_name_trgm on entities using gin (lower(name) gin_trgm_ops);
create index aliases_alias_trgm on aliases using gin (lower(alias) gin_trgm_ops);

alter table chunks add column tsv_stem tsvector generated always as
    (to_tsvector('dutch'::regconfig, text) || to_tsvector('english'::regconfig, text)) stored;
create index chunks_tsv_stem on chunks using gin (tsv_stem);
