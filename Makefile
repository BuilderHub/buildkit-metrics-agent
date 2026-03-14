# BuildKit metrics agent

.PHONY: help generate build run test docker docker-multi
.DEFAULT_GOAL := help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

generate: ## Generate Rust code from protos into src/generated
	cargo run -p codegen

build: ## Build release binary
	cargo build --release

run: build ## Build and run the agent
	cargo run --release --

test: ## Run tests and clippy lints
	cargo test
	cargo clippy -- -D warnings

docker: ## Build Docker image (single arch)
	docker build -t buildkit-metrics-agent .

docker-multi: ## Build Docker image (linux/amd64 + linux/arm64)
	docker buildx build --platform linux/amd64,linux/arm64 -t buildkit-metrics-agent .
