alter table ontology_versions add column last_run_id bigint references runs (id);

update ontology_versions set last_run_id = loaded_run_id;
