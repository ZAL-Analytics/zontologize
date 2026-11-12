alter table llm_job_items
    add column entity_id bigint references entities (id) on delete cascade;

create index llm_job_items_entity on llm_job_items (entity_id);
