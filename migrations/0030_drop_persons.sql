delete from mentions where method = 'gliner';

delete from entities e
where e.method = 'gliner'
  and not exists (select 1 from mentions m where m.entity_id = e.id);

update entities e
set method = (
    select m.method from mentions m
    where m.entity_id = e.id
    order by m.confidence desc, m.id
    limit 1
)
where e.method = 'gliner';

alter table contents drop column persons_key;
