import Lean

private def run (cmd : String) (args : Array String) : IO Unit := do
  let result ← IO.Process.output {cmd, args}
  unless result.exitCode == 0 do
    throw (IO.userError (result.stdout ++ result.stderr))

def main : IO Unit := do
  let root ← IO.currentDir
  let home ← IO.getEnv "HOME"
  let cache ← IO.getEnv "XDG_CACHE_HOME"
  let cache := System.FilePath.mk (cache.getD ((home.getD "") ++ "/.cache"))
  let tools := cache / "hax/tools"
  let llbc := root / "llbc/cdsl_hm.llbc"
  IO.FS.createDirAll llbc.parent.get!
  run (tools / "charon/nightly-2026.10.31/charon").toString #[
    "rustc", "--preset=aeneas", "--start-from", "cdsl_hm",
    "--start-from", "core::clone::Clone::clone_from,core::cmp::PartialEq::ne,core::cmp::PartialOrd::lt,core::cmp::PartialOrd::le,core::cmp::PartialOrd::gt,core::cmp::PartialOrd::ge",
    "--exclude", "{impl core::fmt::Debug for _}", "--dest-file", llbc.toString,
    "--", (root / "../src/hm.rs").toString, "--crate-type", "lib",
    "--crate-name", "cdsl_hm", "--edition", "2024"]
  run (tools / "aeneas/build-2b9e3ea/aeneas").toString #[
    "-backend", "lean", "-core-models-lib", "-split-files", "-specs", "hax",
    llbc.toString, "-dest", root.toString, "-subdir", "Hm/Extraction"]
  for name in ["TypesExternal", "FunsExternal"] do
    let dir := root / "Hm/Extraction"
    let text ← IO.FS.readFile (dir / (name ++ "_Template.lean"))
    if (text.splitOn "axiom ").length > 1 || (text.splitOn "sorry").length > 1 then
      throw (IO.userError "Extraction requires an unproved external model")
    IO.FS.writeFile (dir / (name ++ ".lean")) text
  IO.FS.writeFile (root / "Hm/Extraction.lean")
    "import Hm.Extraction.Types\nimport Hm.Extraction.Funs\n"
