pub mod common;

use common::{Env, source, stage};

async fn exec(env: &Env, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&env.pool)
        .await
        .unwrap();
}

async fn store(env: &Env) -> String {
    let store = format!("{}_store", env.schema);
    exec(
        env,
        &format!(
            "create schema {store};
             create table {store}.folders (id int primary key, parent_id int, name text not null);
             create table {store}.files (id int primary key, folder_id int not null, name text not null,
                 stored text not null, sha256 text, size bigint not null,
                 uploaded timestamptz not null);
             create view {store}.zontologize_files as
                 with recursive tree as (
                     select id, name::text as path from {store}.folders where parent_id is null
                     union all
                     select f.id, tree.path || '/' || f.name from {store}.folders f
                     join tree on tree.id = f.parent_id)
                 select 'doc-' || files.id as id, tree.path || '/' || files.name as path,
                     files.stored as file, files.sha256, files.size, files.uploaded as modified
                 from {store}.files join tree on tree.id = files.folder_id;
             insert into {store}.folders values (1, null, 'Projects'), (2, 1, 'P-2024-0001 Delta Works');"
        ),
    )
    .await;
    store
}

async fn upload(
    env: &Env,
    store: &str,
    id: i32,
    folder: i32,
    name: &str,
    stored: &str,
    text: &str,
) {
    let path = env.file(stored, text);
    let sha = sha256(text.as_bytes());
    exec(
        env,
        &format!(
            "insert into {store}.files values ({id}, {folder}, '{name}', '{stored}', '{sha}', {}, now())
             on conflict (id) do update set stored = excluded.stored, sha256 = excluded.sha256,
             size = excluded.size, uploaded = excluded.uploaded",
            std::fs::metadata(path).unwrap().len()
        ),
    )
    .await;
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
#[ignore]
async fn table_source_follows_renames_versions_and_removals() {
    let env = Env::new("table").await;
    let store = store(&env).await;
    env.write_config(&format!(
        "[[sources]]\nname = \"store\"\nkind = \"table\"\nview = \"{store}.zontologize_files\"\n\
         root = \"{}\"\ndataset = \"current\"\n",
        env.data().display()
    ));
    env.copy_acme_rules();
    upload(
        &env,
        &store,
        1,
        2,
        "Report P-2024-0001 a.txt",
        "files/2024/05/a1b2.txt",
        "first report",
    )
    .await;
    upload(
        &env,
        &store,
        2,
        2,
        "Letter P-2024-0001 b.txt",
        "files/2024/05/c3d4.txt",
        "a letter",
    )
    .await;
    upload(
        &env,
        &store,
        3,
        2,
        "Note P-2024-0001 c.txt",
        "files/2024/06/e5f6.txt",
        "a note",
    )
    .await;
    let stats = env.scan().await;
    assert_eq!(source(&stats, "store", "new"), 3);
    assert_eq!(source(&stats, "store", "hashed"), 0);
    assert_eq!(stage(&stats, "extract"), 3);
    assert_eq!(
        env.count(
            "select count(*) from documents where external_id = 'doc-1' \
             and path = 'Projects/P-2024-0001 Delta Works/Report P-2024-0001 a.txt'"
        )
        .await,
        1
    );
    exec(
        &env,
        &format!("update {store}.folders set name = 'P-2024-0001 Delta Works BV' where id = 2"),
    )
    .await;
    upload(
        &env,
        &store,
        2,
        2,
        "Letter P-2024-0001 b.txt",
        "files/2024/07/g7h8.txt",
        "a letter, second version",
    )
    .await;
    exec(&env, &format!("delete from {store}.files where id = 3")).await;
    let stats = env.scan().await;
    assert_eq!(source(&stats, "store", "changed"), 2);
    assert_eq!(source(&stats, "store", "deleted"), 1);
    assert_eq!(stage(&stats, "extract"), 1);
    assert_eq!(
        env.count(
            "select count(*) from documents where external_id = 'doc-1' and status = 'present' \
             and path = 'Projects/P-2024-0001 Delta Works BV/Report P-2024-0001 a.txt'"
        )
        .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from documents where external_id = 'doc-3' and status = 'deleted'"
        )
        .await,
        1
    );
    assert_eq!(
        env.count(
            "select count(*) from documents where external_id = 'doc-2' \
             and file = 'files/2024/07/g7h8.txt'"
        )
        .await,
        1
    );
    env.finish().await;
}

#[tokio::test]
#[ignore]
async fn missing_column_names_view_and_column() {
    let env = Env::new("table_column").await;
    let store = format!("{}_store", env.schema);
    exec(
        &env,
        &format!(
            "create schema {store};
             create view {store}.files as select 'x'::text as id, 'p'::text as path, 'f'::text as file"
        ),
    )
    .await;
    env.write_config(&format!(
        "[[sources]]\nname = \"store\"\nkind = \"table\"\nview = \"{store}.files\"\nroot = \"/\"\n"
    ));
    let error = zontologize::scan::run(Some(&env.config()), &[], Some(&common::test_url()))
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(&format!("{store}.files")), "{message}");
    assert!(message.contains("no column sha256"), "{message}");
    assert_eq!(env.count("select count(*) from runs").await, 0);
    env.finish().await;
}
