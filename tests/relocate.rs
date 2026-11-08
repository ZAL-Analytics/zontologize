pub mod common;

use common::{Env, folder_source, source, stage};

#[tokio::test]
#[ignore]
async fn relocated_documents_keep_their_ids_and_text() {
    let env = Env::new("relocate").await;
    let old = env.data().join("old");
    env.write_config(&folder_source("old", &old, "legacy"));
    env.file("old/P-1 Acme/report.txt", "pump report");
    env.file("old/Misc/notes.txt", "loose notes");
    env.file("old/loose.txt", "a loose file");
    env.scan().await;
    let mut ids = Vec::new();
    for path in ["Misc/notes.txt", "loose.txt", "P-1 Acme/report.txt"] {
        let id: i64 = sqlx::query_scalar("select id from documents where path = $1")
            .bind(path)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        ids.push(id);
    }
    let chunks = env.count("select count(*) from chunks").await;

    let new = env.data().join("new");
    std::fs::create_dir_all(new.join("projects")).unwrap();
    std::fs::create_dir_all(new.join("general/Loose")).unwrap();
    std::fs::rename(old.join("P-1 Acme"), new.join("projects/P-1 Acme")).unwrap();
    std::fs::rename(old.join("Misc"), new.join("general/Misc")).unwrap();
    std::fs::rename(old.join("loose.txt"), new.join("general/Loose/loose.txt")).unwrap();
    env.write_config(&format!(
        "{}{}",
        folder_source("projects", &new.join("projects"), "current"),
        folder_source("general", &new.join("general"), "current")
    ));
    let plan = env.dir.join("plan.tsv");
    std::fs::write(
        &plan,
        "# from_source\tfrom_path\tto_source\tto_path\n\
         old\tP-1 Acme\tprojects\tP-1 Acme\n\
         old\tMisc\tgeneral\tMisc\n\
         old\tloose.txt\tgeneral\tLoose/loose.txt\n",
    )
    .unwrap();
    let text = std::fs::read_to_string(&plan).unwrap();
    let plan = zontologize::db::documents::parse_plan(&text).unwrap();
    let config = zontologize::config::load(Some(&env.config())).unwrap();
    let mut conn = zontologize::db::connect(Some(&common::test_url()), &env.schema)
        .await
        .unwrap();
    let (count, dropped) = zontologize::relocate(&mut conn, &config, Some(&env.config()), &plan)
        .await
        .unwrap();
    zontologize::db::close(conn).await;
    assert_eq!(count, 3);
    assert_eq!(dropped, vec!["old".to_string()]);

    let moved: Vec<(i64, String, String)> = sqlx::query_as(
        "select d.id, s.name, d.path from documents d join sources s on s.id = d.source_id \
         where d.rules_key is null order by d.id",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(moved.iter().map(|m| m.0).collect::<Vec<_>>(), expected);
    assert!(moved.contains(&(ids[2], "projects".into(), "P-1 Acme/report.txt".into())));
    assert!(moved.contains(&(ids[0], "general".into(), "Misc/notes.txt".into())));
    assert!(moved.contains(&(ids[1], "general".into(), "Loose/loose.txt".into())));
    assert_eq!(
        env.count("select count(*) from sources where name = 'old'")
            .await,
        0
    );

    let stats = env.scan().await;
    assert_eq!(source(&stats, "projects", "new"), 0);
    assert_eq!(source(&stats, "general", "new"), 0);
    assert_eq!(source(&stats, "general", "deleted"), 0);
    assert_eq!(stage(&stats, "extract"), 0);
    assert_eq!(env.count("select count(*) from chunks").await, chunks);
    assert_eq!(
        env.count("select count(*) from documents where rules_key is null")
            .await,
        0
    );
    env.finish().await;
}
