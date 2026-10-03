.PHONY: all format lint check test e2e build run release

IMAGE_NAME = lnurl-mint-rust
CONTAINER_NAME = lnurl-mint-rust
VOLUME_NAME = lnurl-mint-rust

all: format lint

format:
	cargo fmt

lint:
	cargo clippy --all-targets -- -D warnings

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

# the regtest end-to-end test, plus the conformance grader; needs
# BITCOIN_BIN=/path/to/bitcoin/bin
e2e:
	cargo build
	CONFORM=1 MINT_BIN=target/debug/lnurl-mint python3 scripts/regtest_e2e.py

build:
	docker build --pull -t $(IMAGE_NAME) .

ENV_FILE := $(wildcard .env)

# host networking reaches a bitcoind on the host; the named volume keeps the
# seed and the channels. -t 60: the node writes its channel state on SIGTERM
run:
	@echo "Restarting container..."
	docker stop -t 60 $(CONTAINER_NAME) 2>/dev/null || true
	docker rm $(CONTAINER_NAME) 2>/dev/null || true
	docker run --restart always -d --name $(CONTAINER_NAME) \
		--network host \
		--stop-timeout 60 \
		$(if $(ENV_FILE),--env-file $(ENV_FILE),) \
		-v $(VOLUME_NAME):/data \
		$(IMAGE_NAME)
	@echo "Container $(CONTAINER_NAME) is running"

# tags the Cargo.toml version and pushes the tag: CI's release workflow
# builds and pushes lnurlcash/lnurl-mint-rust to Docker Hub from it
release:
	@version=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1); \
	git tag "v$$version" && git push origin "v$$version"
