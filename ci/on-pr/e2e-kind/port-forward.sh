#!/usr/bin/env bash
set -euo pipefail

kubectl port-forward service/buildkitd 1234:1234 >/tmp/pf-buildkitd.log 2>&1 &
kubectl port-forward service/buildkit-metrics-agent 9090:9090 >/tmp/pf-metrics.log 2>&1 &

for _ in $(seq 1 30); do
  if curl -sSf "http://127.0.0.1:9090/metrics" >/dev/null; then
    exit 0
  fi
  sleep 2
done

echo "Port-forward did not become ready" >&2
exit 1
