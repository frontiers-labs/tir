import pathlib
import subprocess
import sys
import tempfile

with tempfile.TemporaryDirectory() as directory:
    output = pathlib.Path(directory)
    obj = output / "conversion.o"
    obj.write_bytes(sys.stdin.buffer.read())
    executable = output / "check"
    harness = pathlib.Path(__file__).with_suffix(".c")
    subprocess.run(["gcc", "-O2", str(harness), str(obj), "-o", str(executable)], check=True)
    subprocess.run([str(executable)], check=True)
