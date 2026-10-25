pub fn validate(log: &str) {
    let expected = include_str!("../Inputs/whetstone.out")
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut row = 0;
    let mut summary = false;
    let mut completed = 0;
    for line in log.lines() {
        if line.starts_with("C Converted Double Precision Whetstones:") {
            assert_eq!(row, expected.len(), "missing Whetstone module results");
            assert!(summary, "missing Whetstone summary");
            row = 0;
            summary = false;
            completed += 1;
        } else if let Some(duration) =
            line.strip_prefix("Loops: 1000000, Iterations: 1, Duration: ")
        {
            assert!(!summary);
            assert!(
                duration
                    .strip_suffix(" sec.")
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    > 0
            );
            summary = true;
        } else {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() != 7 || fields[0].parse::<i64>().is_err() {
                assert!(
                    line.trim().is_empty(),
                    "unexpected Whetstone output: {line}"
                );
                continue;
            }
            let reference = expected.get(row).expect("extra Whetstone module results");
            assert_eq!(&fields[..3], &reference[..3]);
            for (actual, expected) in fields[3..].iter().zip(&reference[3..]) {
                let value: f64 = actual.parse().unwrap();
                let reference: f64 = expected.parse().unwrap();
                let quantum = |text: &str| {
                    let (mantissa, exponent) = text.split_once('e').unwrap();
                    let digits = mantissa.split_once('.').unwrap().1.len() as i32;
                    10f64.powi(exponent.parse::<i32>().unwrap() - digits)
                };
                assert!(
                    value.is_finite()
                        && (value - reference).abs() <= quantum(actual).min(quantum(expected)),
                    "incorrect Whetstone value: {actual}, expected {expected}"
                );
            }
            row += 1;
        }
    }
    assert!(
        completed > 0 && row == 0 && !summary,
        "incomplete Whetstone output"
    );
}
