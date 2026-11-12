create table entity_classes (
    entity_id bigint primary key references entities (id) on delete cascade,
    class_iri text,
    name text,
    method text not null,
    status text not null check (status in ('auto', 'proposed', 'confirmed', 'rejected')),
    confidence double precision not null,
    ontology_version_id bigint not null references ontology_versions (id),
    created_at timestamptz not null default now()
);
