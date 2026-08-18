use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fully-resolved invocation of the system `cc` used as a linker driver.
pub(crate) struct LinkCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

/// Build `cc -o <output> <objects...> -L<dir>... -l<lib>...` (objects before libs).
pub(crate) fn link_command(
    objects: &[PathBuf],
    output: &Path,
    lib_dirs: &[PathBuf],
    libs: &[String],
) -> LinkCommand {
    let mut args: Vec<OsString> = vec![OsString::from("-o"), output.into()];
    args.extend(objects.iter().map(OsString::from));
    for dir in lib_dirs {
        let mut flag = OsString::from("-L");
        flag.push(dir);
        args.push(flag);
    }
    args.extend(libs.iter().map(|lib| OsString::from(format!("-l{lib}"))));
    LinkCommand {
        program: PathBuf::from("cc"),
        args,
    }
}

impl LinkCommand {
    /// The command as an argv (program first), for `-###` display.
    pub(crate) fn display_argv(&self) -> Vec<String> {
        let mut argv = vec![self.program.display().to_string()];
        argv.extend(self.args.iter().map(|a| a.to_string_lossy().into_owned()));
        argv
    }

    /// Run the linker, propagating a spawn failure or nonzero exit (with the
    /// linker's stderr) as an error string.
    pub(crate) fn run(&self) -> Result<(), String> {
        let output = Command::new(&self.program)
            .args(&self.args)
            .output()
            .map_err(|e| format!("failed to run linker '{}': {e}", self.program.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("linking failed:\n{stderr}"));
        }
        Ok(())
    }
}
