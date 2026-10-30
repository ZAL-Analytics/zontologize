create table ontology_versions (
    id bigint generated always as identity primary key,
    sha256 text not null unique,
    files jsonb not null,
    loaded_run_id bigint references runs (id)
);

create table classes (
    ontology_version_id bigint not null references ontology_versions (id) on delete cascade,
    iri text not null,
    parents text[] not null,
    types text[] not null,
    labels jsonb not null,
    alt_labels jsonb not null,
    definition jsonb not null,
    selected boolean not null,
    primary key (ontology_version_id, iri)
);

create table properties (
    ontology_version_id bigint not null references ontology_versions (id) on delete cascade,
    iri text not null,
    kind text not null,
    domain text[] not null,
    range text[] not null,
    parents text[] not null,
    labels jsonb not null,
    selected boolean not null,
    primary key (ontology_version_id, iri)
);
