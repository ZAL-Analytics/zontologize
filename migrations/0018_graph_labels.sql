create table graph_labels (
    kind text not null check (kind in ('vertex', 'edge')),
    iri text not null,
    label text not null,
    primary key (kind, iri)
);
