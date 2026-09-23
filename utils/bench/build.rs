fn main() {
    for name in [
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        let value = std::env::var(name).unwrap_or_default();
        println!("cargo:rustc-env=TIR_BENCH_{name}={value}");
    }
    let compiler = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .output()
        .expect("read rustc identity");
    assert!(compiler.status.success(), "rustc -vV failed");
    println!(
        "cargo:rustc-env=TIR_BENCH_RUSTC={}",
        String::from_utf8(compiler.stdout)
            .unwrap()
            .trim()
            .replace('\n', ";")
    );
    println!("cargo:rerun-if-changed=build.rs");
}
