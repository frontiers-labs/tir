"""Link and execute an FCC object supplied on stdin."""

import subprocess
import sys
import tempfile
from pathlib import Path

with tempfile.TemporaryDirectory(prefix="fcc-execute-") as directory:
    obj = Path(directory) / "input.o"
    executable = Path(directory) / "program"
    obj.write_bytes(sys.stdin.buffer.read())
    subprocess.run(["cc", str(obj), "-o", str(executable)], check=True, timeout=30)
    subprocess.run([str(executable)], check=True, timeout=30)
