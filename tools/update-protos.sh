#!/bin/sh
set -eu

: "${BUILDKIT_REF:=v0.32.0}"
: "${BUILDKIT_RAW_BASE:=https://raw.githubusercontent.com/moby/buildkit/${BUILDKIT_REF}}"
: "${GOOGLEAPIS_REF:=0a38d04e5f6c265e74a994240b762c22666329a5}"
: "${GOOGLEAPIS_RAW_BASE:=https://raw.githubusercontent.com/googleapis/googleapis/${GOOGLEAPIS_REF}}"
: "${PROTOBUF_REF:=69f97e01f74d7c0bc7b429f3aa471a2c22855379}"
: "${PROTOBUF_RAW_BASE:=https://raw.githubusercontent.com/protocolbuffers/protobuf/${PROTOBUF_REF}/src}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl not found in PATH" >&2
  exit 1
fi

repo_root="$(CDPATH= cd "$(dirname "$0")/.." && pwd)"

fetch() {
  base="$1"
  path="$2"
  out="$3"
  url="${base}/${path}"
  printf 'Fetching %s\n' "$url"
  mkdir -p "$(dirname "$out")"
  curl -fsSL "$url" -o "$out"
}

require_in_file() {
  file="$1"
  needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "Expected upstream proto to contain: $needle" >&2
    echo "Source file: $file" >&2
    exit 1
  fi
}

buildkit_proto_dir="$repo_root/proto/github.com/moby/buildkit"
google_rpc_dir="$repo_root/proto/google/rpc"
google_protobuf_dir="$repo_root/proto/google/protobuf"

fetch "$BUILDKIT_RAW_BASE" \
  "api/services/control/control.proto" \
  "$buildkit_proto_dir/api/services/control/control.proto"
fetch "$BUILDKIT_RAW_BASE" \
  "api/types/worker.proto" \
  "$buildkit_proto_dir/api/types/worker.proto"
fetch "$BUILDKIT_RAW_BASE" \
  "solver/pb/ops.proto" \
  "$buildkit_proto_dir/solver/pb/ops.proto"
fetch "$BUILDKIT_RAW_BASE" \
  "sourcepolicy/pb/policy.proto" \
  "$buildkit_proto_dir/sourcepolicy/pb/policy.proto"
fetch "$GOOGLEAPIS_RAW_BASE" \
  "google/rpc/status.proto" \
  "$google_rpc_dir/status.proto"
fetch "$PROTOBUF_RAW_BASE" \
  "google/protobuf/any.proto" \
  "$google_protobuf_dir/any.proto"
fetch "$PROTOBUF_RAW_BASE" \
  "google/protobuf/timestamp.proto" \
  "$google_protobuf_dir/timestamp.proto"

control_proto="$buildkit_proto_dir/api/services/control/control.proto"
worker_proto="$buildkit_proto_dir/api/types/worker.proto"

require_in_file "$control_proto" "rpc Info(InfoRequest) returns (InfoResponse);"
require_in_file "$control_proto" "rpc ListWorkers(ListWorkersRequest) returns (ListWorkersResponse);"
require_in_file "$control_proto" "rpc DiskUsage(DiskUsageRequest) returns (DiskUsageResponse);"
require_in_file "$control_proto" "rpc ListenBuildHistory(BuildHistoryRequest) returns (stream BuildHistoryEvent);"
require_in_file "$control_proto" "message UsageRecord"
require_in_file "$control_proto" "google.rpc.Status error = 5;"
require_in_file "$control_proto" "int32 numCachedSteps = 15;"
require_in_file "$control_proto" "int32 numTotalSteps = 16;"
require_in_file "$control_proto" "int32 numCompletedSteps = 17;"
require_in_file "$worker_proto" "message WorkerRecord"
require_in_file "$worker_proto" "repeated CDIDevice CDIDevices = 6;"
require_in_file "$worker_proto" "string dockerfileVersion = 4;"

printf 'Updated proto snapshot from BuildKit %s\n' "$BUILDKIT_REF"
