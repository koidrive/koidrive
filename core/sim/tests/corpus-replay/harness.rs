#[test]
#[ignore = "replays the corpus under tests/corpus case by case"]
fn replay_corpus() {
    use proptest::test_runner::{Config, FileFailurePersistence};
    let dir = std::env::var("KOI_CORPUS")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus").to_owned());
    let scratch = std::env::temp_dir().join(format!("koidrive-corpus-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    let properties: [(&str, Losses, fn(usize, &[Vec<(usize, Step)>]) -> Result<World, Failure>); 3] = [
        ("devices_converge_across_restores_and_rewinds", Losses::RestoresAndRewinds, converge),
        ("devices_lose_no_write_across_restores_and_rewinds", Losses::RestoresAndRewinds, keep),
        ("devices_converge_across_server_restores", Losses::Restores, converge),
    ];
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let seed = text.split('#').next().unwrap().trim().to_owned();
        let one = scratch.join("one.txt");
        std::fs::write(&one, format!("{seed}\n")).unwrap();
        for (name, losses, check) in properties {
            let config = Config {
                failure_persistence: Some(Box::new(FileFailurePersistence::Direct(Box::leak(one.to_string_lossy().into_owned().into_boxed_str())))),
                max_shrink_iters: if std::env::var("KOI_SHRINK").is_ok() { 100_000 } else { 0 },
                ..Config::with_cases(0)
            };
            let mut runner = TestRunner::new(config);
            let result = runner.run(&(2..=3usize, phases(losses)), |(devices, phases)| {
                check(devices, &phases).map(drop).map_err(|failure| TestCaseError::fail(failure.to_string()))
            });
            let verdict = match result {
                Ok(()) => "pass".to_owned(),
                Err(error) if std::env::var("KOI_FULL").is_ok() => format!("FAIL: {error}"),
                Err(error) => format!("FAIL: {}", error.to_string().lines().next().unwrap_or("").replace('\t', " ")),
            };
            let verdict: String = verdict.chars().take(if std::env::var("KOI_FULL").is_ok() { 10_000_000 } else { 300 }).collect();
            println!("REPLAY\t{}\t{name}\t{verdict}", &seed[3..]);
        }
    }
    panic!("corpus replayed");
}
