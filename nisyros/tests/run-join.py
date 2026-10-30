import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import time


repo = Path(__file__).resolve().parents[2]
icd = Path(sys.argv[1]).resolve()
subprocess.run(["cargo", "build", "--locked", "-p", "nisyros"], cwd=repo, check=True)

with tempfile.TemporaryDirectory() as directory:
    temporary = Path(directory)
    data = json.loads(icd.read_text())
    library = data["ICD"]["library_path"]
    if not os.path.isabs(library) and "/" in library:
        data["ICD"]["library_path"] = str((icd.parent / library).resolve())
    duplicate = temporary / "duplicate.json"
    duplicate.write_text(json.dumps(data))
    binary = temporary / "join"
    subprocess.run(
        [
            "cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
            "-I", "nisyros/include", "nisyros/tests/join.c",
            "-L", "target/debug", "-lnisyros",
            f"-Wl,-rpath,{repo / 'target/debug'}", "-o", str(binary),
        ],
        cwd=repo,
        check=True,
    )
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    address = f"127.0.0.1:{port}"
    session_id = str(secrets.randbits(63) or 1)
    env0 = os.environ.copy()
    env1 = os.environ.copy()
    env0["NISYROS_DISABLE_BACKENDS"] = "rocm,xdna"
    env1["NISYROS_DISABLE_BACKENDS"] = "rocm,xdna"
    env0["VK_DRIVER_FILES"] = f"{icd}:{duplicate}"
    env1["VK_DRIVER_FILES"] = str(icd)
    node0 = subprocess.Popen(
        [str(binary), "0", session_id, address, "2"],
        cwd=repo, env=env0, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        time.sleep(0.1)
        node1 = subprocess.Popen(
            [str(binary), "1", session_id, address, "1"],
            cwd=repo, env=env1, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            output1, errors1 = node1.communicate(timeout=20)
            output0, errors0 = node0.communicate(timeout=20)
        finally:
            if node1.poll() is None:
                node1.kill()
                node1.communicate()
    finally:
        if node0.poll() is None:
            node0.kill()
            node0.communicate()
    if node0.returncode or node1.returncode:
        raise RuntimeError(
            f"node 0: {node0.returncode}\n{errors0}\n"
            f"node 1: {node1.returncode}\n{errors1}"
        )
    lines0 = output0.strip().splitlines()
    lines1 = output1.strip().splitlines()
    assert len(lines0) == len(lines1) == 3
    for first, second in zip(lines0, lines1):
        a, b = first.split(":"), second.split(":")
        assert a[:4] == b[:4]
        assert a[5] == b[5]
        assert a[4] != b[4]
    print("two-node group: three stable device IDs, two local ranks on node 0")
