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
simulator = sys.argv[1] == "simulator"
icd = None if simulator else Path(sys.argv[1]).resolve()
peer_loss = len(sys.argv) > 2 and sys.argv[2] == "--peer-loss"
mismatch = len(sys.argv) > 2 and sys.argv[2] == "--mismatch"
replay = len(sys.argv) > 2 and sys.argv[2] == "--replay"
collective_mismatch = len(sys.argv) > 2 and sys.argv[2] == "--collective-mismatch"
collective_timeout = len(sys.argv) > 2 and sys.argv[2] == "--collective-timeout"
subprocess.run(["cargo", "build", "--locked", "-p", "nisyros"], cwd=repo, check=True)

with tempfile.TemporaryDirectory() as directory:
    temporary = Path(directory)
    if simulator:
        subprocess.run(
            ["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-fPIC", "-shared",
             "nisyros/tests/vulkan_simulator.c", "-ldl", "-pthread",
             "-o", str(temporary / "libvulkan.so.1")],
            cwd=repo, check=True,
        )
    else:
        data = json.loads(icd.read_text())
        library = data["ICD"]["library_path"]
        if not os.path.isabs(library) and "/" in library:
            data["ICD"]["library_path"] = str((icd.parent / library).resolve())
        duplicate = temporary / "duplicate.json"
        duplicate.write_text(json.dumps(data))
    binary = temporary / "communicate"
    subprocess.run(
        [
            "cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
            "-I", "nisyros/include", "nisyros/tests/communicate.c",
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
    if simulator:
        for env, count in ((env0, "2"), (env1, "1")):
            env["LD_LIBRARY_PATH"] = f"{temporary}:{env.get('LD_LIBRARY_PATH', '')}"
            env["NISYROS_SIM_DEVICES"] = count
            env.pop("VK_INSTANCE_LAYERS", None)
    else:
        env0["VK_DRIVER_FILES"] = f"{icd}:{duplicate}"
        env1["VK_DRIVER_FILES"] = str(icd)
        env0["VK_INSTANCE_LAYERS"] = "VK_LAYER_KHRONOS_validation"
        env1["VK_INSTANCE_LAYERS"] = "VK_LAYER_KHRONOS_validation"
    extra = (["peer-loss"] if peer_loss else ["mismatch"] if mismatch else
             ["replay"] if replay else ["collective-mismatch"] if collective_mismatch else
             ["collective-timeout"] if collective_timeout else [])
    node0 = subprocess.Popen(
        [str(binary), "0", session_id, address, *extra],
        cwd=repo, env=env0, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        time.sleep(0.1)
        node1 = subprocess.Popen(
            [str(binary), "1", session_id, address, *extra],
            cwd=repo, env=env1, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            output1, errors1 = node1.communicate(timeout=30)
            output0, errors0 = node0.communicate(timeout=30)
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
    if replay:
        assert "node 1 replayed five bound communication and reduction sequences" in output1
        print(output1.strip())
    elif collective_mismatch or collective_timeout:
        expected = "collective mismatch" if collective_mismatch else "collective timeout"
        assert f"{expected} terminated the collective wait" in output0
        if collective_mismatch:
            assert f"{expected} terminated the collective wait" in output1
        print(output0.strip())
    elif peer_loss or mismatch:
        expected = "peer loss" if peer_loss else "byte-count mismatch"
        assert f"{expected} terminated the pending wait" in output0
        if mismatch:
            assert f"{expected} terminated the pending wait" in output1
        print(output0.strip())
    else:
        assert "node 1 received two matched messages" in output1
        assert "three ranks completed integer reductions" in output1
        print(output1.strip())
