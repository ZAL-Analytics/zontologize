alter table runs
    add column status text not null default 'running'
        check (status in ('running', 'finished', 'failed', 'aborted'));

create table sources (
    id bigint generated always as identity primary key,
    name text not null unique,
    kind text not null check (kind in ('folder', 'table')),
    root text not null,
    dataset text not null default ''
);

create table documents (
    id bigint generated always as identity primary key,
    source_id bigint not null references sources (id),
    path text not null,
    sha256 text,
    size bigint not null,
    mtime timestamptz not null,
    kind text,
    status text not null check (status in ('present', 'skipped', 'error', 'deleted')),
    reason text,
    first_run_id bigint not null references runs (id),
    last_seen_run_id bigint not null references runs (id),
    deleted_run_id bigint references runs (id),
    unique (source_id, path)
);

create index documents_sha256 on documents (sha256);
create index documents_last_seen on documents (source_id, last_seen_run_id);
