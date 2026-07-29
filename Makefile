# BuildKit metrics agent

BUILDKIT_REF ?= v0.31.2
BUILDKIT_RAW_BASE ?= https://raw.githubusercontent.com/moby/buildkit/$(BUILDKIT_REF)
GOOGLEAPIS_REF ?= 0a38d04e5f6c265e74a994240b762c22666329a5
GOOGLEAPIS_RAW_BASE ?= https://raw.githubusercontent.com/googleapis/googleapis/$(GOOGLEAPIS_REF)
PROTOBUF_REF ?= 69f97e01f74d7c0bc7b429f3aa471a2c22855379
PROTOBUF_RAW_BASE ?= https://raw.githubusercontent.com/protocolbuffers/protobuf/$(PROTOBUF_REF)/src

.PHONY: help update-protos generate build run test coverage docker docker-multi
.DEFAULT_GOAL := help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

update-protos: ## Update local proto snapshot from BuildKit; run before generate
	BUILDKIT_REF="$(BUILDKIT_REF)" \
	BUILDKIT_RAW_BASE="$(BUILDKIT_RAW_BASE)" \
	GOOGLEAPIS_REF="$(GOOGLEAPIS_REF)" \
	GOOGLEAPIS_RAW_BASE="$(GOOGLEAPIS_RAW_BASE)" \
	PROTOBUF_REF="$(PROTOBUF_REF)" \
	PROTOBUF_RAW_BASE="$(PROTOBUF_RAW_BASE)" \
	sh tools/update-protos.sh

generate: ## Generate Rust code from protos into src/generated
	cargo run -p codegen

build: ## Build release binary
	cargo build --release

run: build ## Build and run the agent
	cargo run --release --

test: ## Run tests and clippy lints
	cargo test
	cargo clippy -- -D warnings

coverage: ## Run tests under cargo-llvm-cov; writes codecov.json; excludes src/generated and main.rs
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { echo "cargo-llvm-cov not in PATH (use nix develop)"; exit 1; }
	cargo llvm-cov test -p buildkit-metrics-agent \
		--ignore-filename-regex '.*/src/(generated/.*|main\.rs)' \
		--codecov --output-path codecov.json \
		--fail-under-lines 90

docker: ## Build Docker image (single arch)
	docker build -t buildkit-metrics-agent .

docker-multi: ## Build Docker image (linux/amd64 + linux/arm64)
	docker buildx build --platform linux/amd64,linux/arm64 -t buildkit-metrics-agent .
