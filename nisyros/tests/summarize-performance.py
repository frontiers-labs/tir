import csv
from pathlib import Path
import statistics
import sys


def read(path):
    with Path(path).open() as file:
        return list(csv.DictReader(line for line in file if not line.startswith("#")))


def summary(rows, metric):
    selected = [row for row in rows if row["metric"] == metric]
    assert selected
    wall = sorted(int(row["wall_ns"]) for row in selected)
    cpu = sorted(int(row["cpu_ns"]) for row in selected)
    return len(selected), statistics.median(wall), wall[(95 * len(wall) - 1) // 100], statistics.median(cpu)


local = read(sys.argv[1])
results = {}
for metric in ("empty", "segmented", "transfer"):
    results[metric] = summary(local, metric)
    count, median, p95, cpu = results[metric]
    print(f"{metric}: n={count} median={median:.0f} ns p95={p95} ns submit_cpu={cpu:.0f} ns")

empty = results["empty"]
segmented = results["segmented"]
print(f"event_boundary_delta={segmented[1] - empty[1]:.0f} ns")
transfer_bytes = int(next(row["bytes"] for row in local if row["metric"] == "transfer"))
print(f"transfer_bandwidth={transfer_bytes / results['transfer'][1]:.3f} GB/s")

if len(sys.argv) > 2:
    for node in (0, 1):
        count, median, p95, cpu = summary(read(Path(sys.argv[2]) / f"node{node}.csv"), "transport")
        print(f"tcp_node{node}: n={count} median={median:.0f} ns p95={p95} ns process_cpu={cpu:.0f} ns")
