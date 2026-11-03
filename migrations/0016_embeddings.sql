create table embedding_models (
    id bigint generated always as identity primary key,
    name text not null,
    fingerprint text not null unique,
    repo text not null,
    file text not null,
    data_files text[] not null default '{}',
    tokenizer text not null,
    revision text not null,
    sha256 text not null,
    dimensions integer not null check (dimensions > 0),
    pooling text not null check (pooling in ('mean', 'cls', 'last', 'none')),
    output text not null,
    passage_prefix text not null,
    query_prefix text not null,
    max_tokens integer not null,
    normalize boolean not null,
    current boolean not null default false,
    created_at timestamptz not null default now()
);

create table embeddings (
    model_id bigint not null references embedding_models (id) on delete cascade,
    owner_kind text not null check (owner_kind in ('chunk', 'entity')),
    owner_id bigint not null,
    text_key text not null,
    truncated boolean not null default false,
    embedding halfvec not null,
    primary key (model_id, owner_kind, owner_id)
);

create index embeddings_text_key on embeddings (model_id, text_key);

create table class_embeddings (
    model_id bigint not null references embedding_models (id) on delete cascade,
    iri text not null,
    text_key text not null,
    truncated boolean not null default false,
    embedding halfvec not null,
    primary key (model_id, iri)
);

alter table entities add column canonical_id bigint references entities (id) on delete set null;

create index entities_canonical on entities (canonical_id);

alter table relations drop constraint relations_check;

alter table relations add constraint relations_provenance check (
    case when property_iri = 'http://www.w3.org/2002/07/owl#sameAs'
        then document_id is null and chunk_id is null
        else (document_id is null) <> (chunk_id is null)
    end
);

create index relations_property on relations (property_iri);
