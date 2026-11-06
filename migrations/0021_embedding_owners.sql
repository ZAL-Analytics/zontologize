delete from embeddings e
where (e.owner_kind = 'chunk' and not exists (select 1 from chunks k where k.id = e.owner_id))
    or (e.owner_kind = 'entity' and not exists (select 1 from entities n where n.id = e.owner_id));

create function delete_owned_embeddings() returns trigger
language plpgsql
set search_path from current
as $$
begin
    delete from embeddings e using gone g, embedding_models m
    where e.model_id = m.id and e.owner_kind = tg_argv[0] and e.owner_id = g.id;
    return null;
end
$$;

create trigger chunks_delete_embeddings after delete on chunks
referencing old table as gone
for each statement execute function delete_owned_embeddings('chunk');

create trigger entities_delete_embeddings after delete on entities
referencing old table as gone
for each statement execute function delete_owned_embeddings('entity');

delete from llm_job_items i
where i.chunk_id is not null and not exists (select 1 from chunks k where k.id = i.chunk_id);

alter table llm_job_items
    add constraint llm_job_items_chunk foreign key (chunk_id) references chunks (id) on delete cascade;

create index llm_job_items_chunk on llm_job_items (chunk_id);

alter table relations drop constraint relations_provenance;

alter table relations add constraint relations_provenance check (
    case when property_iri = 'http://www.w3.org/2002/07/owl#sameAs'
        then document_id is null or chunk_id is null
        else (document_id is null) <> (chunk_id is null)
    end
);
