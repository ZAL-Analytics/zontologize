create table runs (
    id bigint generated always as identity primary key,
    started_at timestamptz not null,
    finished_at timestamptz,
    config_hash text not null,
    stats jsonb not null default '{}'
);
