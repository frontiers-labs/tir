fn runs<'a>(log: &'a str, header: &'a str) -> impl Iterator<Item = &'a str> {
    let (prefix, output) = log.split_once(header).expect("missing benchmark output");
    assert!(
        prefix.trim().is_empty(),
        "unexpected benchmark output: {prefix}"
    );
    output.split(header)
}

pub fn validate(log: &str) {
    for output in runs(log, "Dhrystone Benchmark, Version 2.1 (Language: C)") {
        assert!(output.contains("Execution starts, 100000000 runs through Dhrystone"));
        let lower = output.to_ascii_lowercase();
        assert!(
            ![
                "error",
                "failed",
                "failure",
                "runtime",
                "insufficient duration"
            ]
            .iter()
            .any(|word| lower.contains(word)),
            "Dhrystone failed: {output}"
        );
        let values = output
            .split_once("Final values of the variables used in the benchmark:")
            .expect("missing Dhrystone final values")
            .1;
        let values = values
            .split("Measured time")
            .next()
            .unwrap()
            .split("Microseconds")
            .next()
            .unwrap();
        let mut pointers = Vec::new();
        let normalized = values
            .trim()
            .lines()
            .map(|line| {
                let line = line.trim_end();
                if let Some((_, address)) = line.split_once("Ptr_Comp:") {
                    let address = address.trim();
                    let pointer = usize::from_str_radix(address.trim_start_matches("0x"), 16)
                        .expect("invalid Dhrystone pointer");
                    assert_ne!(pointer, 0);
                    pointers.push(pointer);
                    line.replace(address, "<pointer>")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(pointers.len(), 2);
        assert_eq!(pointers[0], pointers[1]);
        assert_eq!(normalized, include_str!("../Inputs/dhrystone.out").trim());
    }
}
