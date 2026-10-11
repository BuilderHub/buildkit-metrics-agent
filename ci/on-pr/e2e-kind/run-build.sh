#!/usr/bin/env bash
set -euo pipefail

dir="$(cd "$(dirname "$0")" && pwd)"

docker buildx create --name e2e-remote --driver remote tcp://127.0.0.1:1234 --use
docker buildx build --builder e2e-remote --progress plain \
  --target runtime \
  -t e2e.example/app:ci \
  --output type=oci,dest=/tmp/e2e-app.tar \
  "$dir"
