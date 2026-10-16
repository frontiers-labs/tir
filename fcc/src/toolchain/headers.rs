use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn system_include_dirs() -> Vec<PathBuf> {
    let candidates = if cfg!(target_os = "macos") {
        macos_dirs()
    } else {
        linux_dirs()
    };

    let mut existing = Vec::new();
    for path in candidates {
        if path.is_dir() && !existing.contains(&path) {
            existing.push(path);
        }
    }
    existing
}

fn linux_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = compiler_include_dir() {
        paths.push(path);
    }
    paths.extend([
        PathBuf::from("/usr/local/include"),
        PathBuf::from(format!("/usr/include/{}-linux-gnu", std::env::consts::ARCH)),
        PathBuf::from("/usr/include"),
    ]);
    paths
}

fn compiler_include_dir() -> Option<PathBuf> {
    let output = Command::new("cc")
        .arg("-print-file-name=include")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = Path::new(std::str::from_utf8(&output.stdout).ok()?.trim());
    path.is_absolute().then(|| path.to_path_buf())
}

fn macos_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(sdkroot) = std::env::var("SDKROOT") {
        paths.push(PathBuf::from(sdkroot).join("usr/include"));
    }
    if let Ok(output) = std::process::Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        && output.status.success()
    {
        let sdkroot = String::from_utf8_lossy(&output.stdout);
        paths.push(PathBuf::from(sdkroot.trim()).join("usr/include"));
    }
    paths.extend([
        PathBuf::from("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include"),
        PathBuf::from(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include",
        ),
        PathBuf::from("/usr/include"),
    ]);
    paths
}
