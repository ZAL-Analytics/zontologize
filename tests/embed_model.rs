use zontologize::embed::{self, Embed};

#[test]
#[ignore]
fn real_model_gives_stable_normalised_vectors() {
    let Ok(names) = std::env::var("ZONTOLOGIZE_TEST_EMBED_MODEL") else {
        return;
    };
    let config = zontologize::config::load(None).unwrap();
    for name in names.split(',') {
        let model = embed::model::parse(name.trim(), &config.embed.models).unwrap();
        let files = embed::local::fetch(&model, &config.embed.cache_dir).unwrap();
        let mut local = embed::local::Local::new(model.clone(), files, 4);
        let texts = vec![
            embed::passage(&model, "The pump P-101 was serviced in March."),
            embed::passage(&model, "De klant vroeg om een nieuwe offerte."),
        ];
        let first = local.embed(&texts).unwrap();
        let again = local.embed(&texts[..1]).unwrap();
        let dimensions = local.dimensions().unwrap();
        assert_eq!(first.vectors.len(), 2, "{name}");
        for vector in &first.vectors {
            assert_eq!(vector.len(), dimensions, "{name}");
            let length: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((length - 1.0).abs() < 1e-3, "{name}: length {length}");
        }
        let drift: f32 = first.vectors[0]
            .iter()
            .zip(&again.vectors[0])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(
            drift < 1e-3,
            "{name}: the same text gave another vector ({drift})"
        );
        let cosine: f32 = first.vectors[0]
            .iter()
            .zip(&first.vectors[1])
            .map(|(a, b)| a * b)
            .sum();
        println!(
            "{name}: {dimensions} dimensions, revision {}, cosine of two texts {cosine:.3}",
            local.revision()
        );
    }
}
