use std::{collections::BTreeMap, path::Path, process::Command};

#[test]
fn workspace_registers_each_benchmark_source_once() -> anyhow::Result<()> {
    let output = Command::new("cargo")
        .current_dir(xtask::utils::project_root())
        .args(["metadata", "--no-deps", "--locked", "--format-version", "1"])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut owners = BTreeMap::new();
    for package in metadata["packages"].as_array().unwrap() {
        for target in package["targets"].as_array().unwrap() {
            if !target["kind"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind == "bench")
            {
                continue;
            }
            let source = Path::new(target["src_path"].as_str().unwrap()).canonicalize()?;
            let owner = format!(
                "{}/{}",
                package["name"].as_str().unwrap(),
                target["name"].as_str().unwrap()
            );
            if let Some(previous) = owners.insert(source.clone(), owner.clone()) {
                anyhow::bail!("{previous} and {owner} both run {}", source.display());
            }
        }
    }
    Ok(())
}
