fn runs<'a>(log: &'a str, header: &'a str) -> impl Iterator<Item = &'a str> {
    let (prefix, output) = log.split_once(header).expect("missing benchmark output");
    assert!(
        prefix.trim().is_empty(),
        "unexpected benchmark output: {prefix}"
    );
    output.split(header)
}

pub fn validate(log: &str) {
    for output in runs(log, "2K performance run parameters for coremark.") {
        let field = |name| {
            output.lines().find_map(|line| {
                let (key, value) = line.trim().trim_start_matches("[0]").split_once(':')?;
                (key.trim() == name).then_some(value.trim())
            })
        };
        for (name, expected) in [
            ("Iterations", "600000"),
            ("seedcrc", "0xe9f5"),
            ("crclist", "0xe714"),
            ("crcmatrix", "0x1fd7"),
            ("crcstate", "0x8e3a"),
            ("crcfinal", "0xa14c"),
        ] {
            assert_eq!(field(name), Some(expected), "incorrect CoreMark {name}");
        }
        let seconds: f64 = field("Total time (secs)")
            .expect("missing CoreMark time")
            .parse()
            .unwrap();
        assert!(seconds.is_finite() && seconds >= 0.0);
        let short_error = "ERROR! Must execute for at least 10 secs for a valid result!";
        for line in output.lines().map(str::trim) {
            let lower = line.to_ascii_lowercase();
            if ["error", "failed", "failure", "cannot validate operation"]
                .iter()
                .any(|word| lower.contains(word))
            {
                assert!(
                    line == short_error
                        || (line == "Errors detected"
                            && seconds < 10.0
                            && output.contains(short_error)),
                    "CoreMark failed: {line}"
                );
            }
        }
        if seconds >= 10.0 {
            assert!(output.contains("Correct operation validated."));
            let has_score = output
                .lines()
                .any(|line| line.starts_with("CoreMark 1.0 :"));
            assert!(has_score);
        }
    }
}
