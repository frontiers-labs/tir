#[path = "../../../../fcc/benches/support/coremark.rs"]
mod coremark;
#[path = "../../../../fcc/benches/support/dhrystone.rs"]
mod dhrystone;
#[path = "../../../../fcc/benches/support/whetstone.rs"]
mod whetstone;

#[test]
fn coremark_time_per_iteration_requires_a_validated_performance_run() {
    let output = "2K performance run parameters for coremark.\nIterations : 400000\nTotal time (secs): 10.000000\nseedcrc : 0xe9f5\n[0]crclist : 0xe714\n[0]crcmatrix : 0x1fd7\n[0]crcstate : 0x8e3a\n[0]crcfinal : 0xa14c\nCorrect operation validated.\nCoreMark 1.0 : 40000.0\n";
    assert_eq!(
        coremark::time_per_iteration(output),
        std::time::Duration::from_micros(25)
    );
    for invalid in [
        output.replace(
            "Correct operation validated.\n",
            "ERROR! Must execute for at least 10 secs for a valid result!\nErrors detected\n",
        ),
        output.replace("0xe9f5", "0x8a02"),
    ] {
        assert!(std::panic::catch_unwind(|| coremark::time_per_iteration(&invalid)).is_err());
    }
}

#[test]
fn dhrystone_repetitions_validate_values_and_pointers() {
    let values =
        include_str!("../../../../fcc/benches/Inputs/dhrystone.out").replace("<pointer>", "0x1234");
    let output = format!("Dhrystone Benchmark, Version 2.1 (Language: C)\nExecution starts, 100000000 runs through Dhrystone\nFinal values of the variables used in the benchmark:\n{values}Measured time 1 sec\n");
    dhrystone::validate(&output.repeat(2));
    for invalid in [
        format!("{output}{}", output.replace("100000010", "100000011")),
        format!("runtime error\n{output}"),
        output.replacen("0x1234", "0x4321", 1),
        output.replace("0x1234", "0x0"),
    ] {
        assert!(std::panic::catch_unwind(|| dhrystone::validate(&invalid)).is_err());
    }
}

#[test]
fn whetstone_repetitions_require_complete_numeric_results() {
    let values = include_str!("../../../../fcc/benches/Inputs/whetstone.out");
    let score = "C Converted Double Precision Whetstones: 100000.0 MIPS\n";
    let output = format!("{values}Loops: 1000000, Iterations: 1, Duration: 1 sec.\n{score}");
    whetstone::validate(&output.repeat(2));
    for invalid in [
        score.to_owned(),
        format!("{output}{score}"),
        format!("{output}{}", output.replacen("1.0000e+00", "2.0000e+00", 1)),
        output.replace("Duration: 1 sec.", "Duration: 0 sec."),
        format!("runtime error\n{output}"),
    ] {
        assert!(std::panic::catch_unwind(|| whetstone::validate(&invalid)).is_err());
    }
}
