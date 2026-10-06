#!/usr/bin/env python3
"""Fail CI when required jobs fail or elapsed CI time reaches ten minutes."""

import datetime
import json
import os
import subprocess
import sys


results = json.loads(os.environ["RESULTS"])
failed = [name for name, job in results.items()
          if name != "peak-rss" and job["result"] != "success"]
run = f"repos/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"


def api(*args):
    return json.loads(subprocess.check_output(["gh", "api", *args]))


def timestamp(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


# A re-run lists the jobs it reuses with the timestamps of the attempt that ran
# them, so only jobs started in this attempt are measured.
attempt_start = timestamp(api(run)["run_started_at"])
jobs = [job for page in api("--paginate", "--slurp", f"{run}/jobs?per_page=100")
        for job in page["jobs"]
        if job["name"] != "build" and job.get("started_at")
        and job.get("completed_at")
        and timestamp(job["started_at"]) >= attempt_start]
elapsed = (max(timestamp(job["completed_at"]) for job in jobs)
           - min(timestamp(job["started_at"]) for job in jobs)).total_seconds() if jobs else 0
print(f"CI elapsed: {elapsed:.0f}s / 600s, from first job start to last job completion")
if failed:
    print(f"Required jobs did not succeed: {', '.join(failed)}", file=sys.stderr)
if elapsed >= 600:
    print("CI exceeded its ten-minute budget", file=sys.stderr)
sys.exit(bool(failed) or elapsed >= 600)
