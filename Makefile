.PHONY: all format lint check test e2e build run release

IMAGE_NAME = lnurl-mint-rust
CONTAINER_NAME = lnurl-mint-rust
VOLUME_NAME = lnurl-mint-rust

all: format lint

# every language in the repo: Rust, the e2e tests' Python, the admin UI's JS
# and CSS. Ruff runs through uvx; Biome is pinned in e2e/package.json.
RUFF = uvx ruff@0.16.10
BIOME = e2e/node_modules/.bin/biome

format: e2e/node_modules
	cargo fmt
	$(RUFF) format e2e
	$(BIOME) check --write

lint: e2e/node_modules
	cargo clippy --all-targets -- -D warnings
	$(RUFF) check e2e
	$(BIOME) lint

check: e2e/node_modules
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	$(RUFF) check e2e
	$(RUFF) format --check e2e
	$(BIOME) ci

test:
	cargo test

e2e/node_modules: e2e/package.json e2e/package-lock.json
	npm ci --prefix e2e
	touch $@

# the regtest end-to-end test with the conformance grader and the admin UI in
# a headless browser; needs BITCOIN_BIN=/path/to/bitcoin/bin
e2e: e2e/node_modules
	cargo build
	npx --prefix e2e playwright install chromium
	CONFORM=1 UI=1 MINT_BIN=target/debug/lnurl-mint python3 e2e/regtest.py

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

# tags v$(VERSION) and pushes the tag: CI's release workflow stamps that
# version into the build and pushes lnurlcash/lnurl-mint-rust to Docker Hub
release:
	@test -n "$(VERSION)" || { echo "usage: make release VERSION=x.y.z"; exit 1; }
	git tag "v$(VERSION)" && git push origin "v$(VERSION)"
