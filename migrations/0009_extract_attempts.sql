alter table contents add column attempts integer not null default 0;

alter table contents drop constraint contents_status_check;
alter table contents add constraint contents_status_check
    check (status in ('pending', 'running', 'done', 'error'));
