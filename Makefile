# BuildKit metrics agent

.PHONY: generate build run test docker docker-multi

# Generate Rust code from protos into src/generated (checked in as source).
generate:
	cargo run -p codegen

build:
	cargo build --release

run: build
	cargo run --release --

test:
	cargo test
	cargo clippy -- -D warnings

# Build Docker image (requires src/generated/ from make generate).
# Use buildx for multi-arch: make docker-multi or docker buildx build --platform linux/amd64,linux/arm64 -t buildkit-metrics-agent .
docker:
	docker build -t buildkit-metrics-agent .
docker-multi:
	docker buildx build --platform linux/amd64,linux/arm64 -t buildkit-metrics-agent .
