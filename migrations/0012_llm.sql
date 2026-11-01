create table llm_cache (
    key text primary key,
    provider text not null,
    model text not null,
    response jsonb not null,
    input_tokens bigint not null,
    output_tokens bigint not null,
    cost_usd double precision not null,
    created_at timestamptz not null default now()
);

create table llm_jobs (
    id bigint generated always as identity primary key,
    task text not null,
    provider text not null,
    model text not null,
    ontology_version_id bigint references ontology_versions (id),
    params jsonb not null default '{}',
    status text not null default 'running'
        check (status in ('running', 'paused', 'finished', 'abandoned')),
    pause_reason text,
    resume_after timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table llm_job_items (
    job_id bigint not null references llm_jobs (id) on delete cascade,
    ord integer not null,
    chunk_id bigint not null,
    cache_key text,
    status text not null default 'pending'
        check (status in ('pending', 'done', 'invalid', 'error')),
    error text,
    primary key (job_id, ord)
);

create table llm_calls (
    id bigint generated always as identity primary key,
    job_id bigint references llm_jobs (id) on delete set null,
    provider text not null,
    model text not null,
    task text not null,
    cache_key text,
    input_chars bigint not null,
    input_tokens bigint not null default 0,
    cached_input_tokens bigint not null default 0,
    output_tokens bigint not null default 0,
    cost_usd double precision not null default 0,
    subscription boolean not null default false,
    five_hour_use double precision,
    five_hour_resets_at timestamptz,
    seven_day_use double precision,
    seven_day_resets_at timestamptz,
    seconds double precision not null,
    status text not null check (status in ('ok', 'invalid', 'error', 'pause')),
    error text,
    created_at timestamptz not null default now()
);

create index llm_calls_model on llm_calls (provider, model, task, id);
create index llm_calls_job on llm_calls (job_id);
