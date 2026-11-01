create table fact_reviews (
    id bigint generated always as identity primary key,
    kind text not null check (kind in ('mention', 'relation')),
    fact_id bigint not null,
    class_iri text not null,
    property_iri text,
    method text not null,
    verdict text not null check (verdict in ('right', 'wrong')),
    snapshot text not null,
    created_at timestamptz not null default now()
);

create index fact_reviews_fact on fact_reviews (kind, fact_id);
