#[path = "../../../../fcc/benches/support/coremark.rs"]
mod coremark;
#[path = "../../../../fcc/benches/support/dhrystone.rs"]
mod dhrystone;
#[path = "../../../../fcc/benches/support/whetstone.rs"]
mod whetstone;

#[test]
fn coremark_repetitions_validate_every_crc() {
    let output = "2K performance run parameters for coremark.\nIterations : 600000\nTotal time (secs): 10.00\nseedcrc : 0xe9f5\n[0]crclist : 0xe714\n[0]crcmatrix : 0x1fd7\n[0]crcstate : 0x8e3a\n[0]crcfinal : 0xa14c\nCorrect operation validated.\nCoreMark 1.0 : 123.45\n";
    coremark::validate(&output.repeat(2));
    coremark::validate(
        &output
            .replace("Total time (secs): 10.00", "Total time (secs): 1.00")
            .replace(
                "Correct operation validated.\nCoreMark 1.0 : 123.45\n",
                "ERROR! Must execute for at least 10 secs for a valid result!\nErrors detected\n",
            ),
    );
    for invalid in [
        format!("{output}{}", output.replace("0xa14c", "0x1234")),
        format!("runtime error\n{output}"),
        output.replace("Iterations : 600000", "Iterations : 1"),
    ] {
        assert!(std::panic::catch_unwind(|| coremark::validate(&invalid)).is_err());
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
