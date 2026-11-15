#!/usr/bin/env python3
"""Build Jevlint with TIR's Rust grammar compatibility patch."""

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path


def go_output(*args):
    return subprocess.check_output(["go", *args], text=True).strip()


def download(module, destination):
    metadata = json.loads(go_output("mod", "download", "-json", module))
    shutil.copytree(metadata["Dir"], destination, copy_function=shutil.copyfile)
    for directory, _, _ in os.walk(destination):
        Path(directory).chmod(0o755)


def main():
    patch = Path(__file__).with_name("jevlint-rust.patch").resolve()
    version = "v0.4.0-tir.1"
    with tempfile.TemporaryDirectory(prefix="tir-jevlint-") as directory:
        root = Path(directory)
        grammar = root / "rust"
        jevlint = root / "jevlint"
        download("github.com/tree-sitter/tree-sitter-rust@v0.24.2", grammar)
        download("github.com/codegirl-007/jevlint@v0.4.0", jevlint)
        generator = shutil.which("tree-sitter")
        if (
            not generator
            or subprocess.check_output([generator, "--version"], text=True).split()[1]
            != "0.26.9"
        ):
            subprocess.run(
                [
                    "cargo",
                    "install",
                    "tree-sitter-cli",
                    "--version",
                    "=0.26.9",
                    "--locked",
                    "--root",
                    str(root / "tools"),
                ],
                check=True,
            )
            generator = str(root / "tools/bin/tree-sitter")
        subprocess.run(["patch", "-p1", "-i", str(patch)], cwd=grammar, check=True)
        subprocess.run([generator, "generate"], cwd=grammar, check=True)
        subprocess.run(
            [
                "go",
                "mod",
                "edit",
                f"-replace=github.com/tree-sitter/tree-sitter-rust={grammar}",
            ],
            cwd=jevlint,
            check=True,
        )
        binary = root / "jevlint-bin"
        subprocess.run(
            [
                "go",
                "build",
                "-a",
                "-buildvcs=false",
                "-trimpath",
                "-ldflags",
                f"-X github.com/codegirl-007/jevlint/internal/cli.version={version}",
                "-o",
                str(binary),
                "./cmd/jevlint",
            ],
            cwd=jevlint,
            check=True,
        )
        gobin = go_output("env", "GOBIN")
        bindir = (
            Path(gobin)
            if gobin
            else Path(go_output("env", "GOPATH").split(os.pathsep)[0]) / "bin"
        )
        bindir.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=bindir, delete=False) as staged:
            temporary = Path(staged.name)
        try:
            shutil.copyfile(binary, temporary)
            temporary.chmod(0o755)
            os.replace(temporary, bindir / "jevlint")
        finally:
            temporary.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
