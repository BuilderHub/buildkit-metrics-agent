#!/usr/bin/env python3
"""Wait for /metrics to show the e2e build counters and step-label series."""

import re
import time
import urllib.request

URL = "http://127.0.0.1:9090/metrics"
WANTED = (
    "buildkit_builds_total",
    "buildkit_builds_succeeded_total",
    "buildkit_builds_failed_total",
)
STEP_LABELS = (("target", "runtime"), ("image", "e2e.example/app"))
DEADLINE_SECS = 120


def unlabeled(body: str, name: str):
    m = re.search(rf"^{re.escape(name)}(?:\{{[^}}]*\}})?\s+([0-9.]+)$", body, re.M)
    return float(m.group(1)) if m else None


def labeled(body: str, name: str, labels):
    parts = "".join(
        rf'(?=[^}}]*{re.escape(key)}="{re.escape(value)}")' for key, value in labels
    )
    m = re.search(
        rf"^{re.escape(name)}\{{{parts}[^}}]*\}}\s+([0-9.]+)$",
        body,
        re.M,
    )
    return float(m.group(1)) if m else None


def main() -> None:
    deadline = time.time() + DEADLINE_SECS
    latest_body = ""
    try:
        while time.time() < deadline:
            latest_body = urllib.request.urlopen(URL, timeout=5).read().decode()
            values = {metric: unlabeled(latest_body, metric) for metric in WANTED}
            cached = labeled(
                latest_body, "buildkit_builds_cached_steps_total", STEP_LABELS
            )
            total_steps = labeled(
                latest_body, "buildkit_builds_total_steps_total", STEP_LABELS
            )
            values["buildkit_builds_cached_steps_total"] = cached
            values["buildkit_builds_total_steps_total"] = total_steps

            if (
                all(values[metric] is not None for metric in WANTED)
                and values["buildkit_builds_total"] >= 1
                and values["buildkit_builds_succeeded_total"] >= 1
                and values["buildkit_builds_failed_total"] == 0
                and cached is not None
                and total_steps is not None
                and total_steps >= 1
            ):
                print("Metrics check passed:", values)
                return

            time.sleep(5)
        raise SystemExit("Expected build metrics were not observed within timeout")
    finally:
        print("----- /metrics payload start -----")
        print(latest_body.rstrip())
        print("----- /metrics payload end -----")


if __name__ == "__main__":
    main()
