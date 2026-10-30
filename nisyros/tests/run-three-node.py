import os
from pathlib import Path
import secrets
import selectors
import socket
import subprocess
import sys
import tempfile


repo = Path(__file__).resolve().parents[2]
simulator = sys.argv[1] == "simulator"
nonparticipant = len(sys.argv) > 2 and sys.argv[2] == "--nonparticipant"
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
    binary = temporary / "communicate-three"
    subprocess.run(
        ["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-I", "nisyros/include",
         "nisyros/tests/communicate_nonparticipant.c" if nonparticipant else
         "nisyros/tests/communicate_three.c", "-L", "target/debug", "-lnisyros",
         f"-Wl,-rpath,{repo / 'target/debug'}", "-o", str(binary)],
        cwd=repo, check=True,
    )
    listeners = [socket.socket() for _ in range(3)]
    try:
        for listener in listeners:
            listener.bind(("127.0.0.1", 0))
        addresses = [f"127.0.0.1:{listener.getsockname()[1]}" for listener in listeners]
    finally:
        for listener in listeners:
            listener.close()
    session_id = str(secrets.randbits(63) or 1)
    env = os.environ.copy()
    env["NISYROS_DISABLE_BACKENDS"] = "rocm"
    if simulator:
        env["LD_LIBRARY_PATH"] = f"{temporary}:{env.get('LD_LIBRARY_PATH', '')}"
        env["NISYROS_SIM_DEVICES"] = "1"
        env.pop("VK_INSTANCE_LAYERS", None)
    else:
        env["VK_DRIVER_FILES"] = str(Path(sys.argv[1]).resolve())
        env["VK_INSTANCE_LAYERS"] = "VK_LAYER_KHRONOS_validation"
    processes = [subprocess.Popen(
        [str(binary), str(node), session_id, *addresses],
        cwd=repo, env=env, text=True,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ) for node in range(3)]
    try:
        with selectors.DefaultSelector() as selector:
            for node, process in enumerate(processes):
                selector.register(process.stdout, selectors.EVENT_READ, node)
            ready = set()
            while len(ready) < 3:
                events = selector.select(timeout=30)
                if not events:
                    raise RuntimeError("three-node completion barrier timed out")
                for key, _ in events:
                    node = key.data
                    line = key.fileobj.readline()
                    expected = (f"node {node} completed nonparticipant " +
                                ("bootstrap" if node == 0 else "group transfer") + "\n") if nonparticipant else \
                               f"node {node} completed three-node transfer and reduction\n"
                    if line != expected:
                        raise RuntimeError(f"node {node} did not reach completion: {line!r}")
                    ready.add(node)
                    selector.unregister(key.fileobj)
        for process in processes:
            process.stdin.write("\n")
            process.stdin.flush()
        results = [process.communicate(timeout=30) for process in processes]
    finally:
        for process in processes:
            if process.poll() is None:
                process.kill()
                process.communicate()
    failures = []
    for node, (process, (output, errors)) in enumerate(zip(processes, results)):
        if process.returncode:
            failures.append(f"node {node}: {process.returncode}\n{output}\n{errors}")
    if failures:
        raise RuntimeError("\n".join(failures))
    print("nonparticipant node skipped group work" if nonparticipant else
          "three nodes completed direct transfers and integer reduction")
