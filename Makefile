.PHONY: all format lint check test e2e build run release

IMAGE_NAME = lnurlcash/lnurl-mint-rust
CONTAINER_NAME = lnurl-mint-rust

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

ENV_FILE ?= $(wildcard .env)

# The container runs as whoever runs `make run` (--user), as lnurl-mint's does:
# run it as the user bitcoind runs as, and it reads bitcoind's cookie with
# bitcoind's own permissions, no rpccookieperms needed. Its state is a host
# directory owned by that same user (DATA, default ./data), not a volume: the
# image's own user (uid 1000) is someone else.
RUN_UID := $(shell id -u)
RUN_GID := $(shell id -g)
DATA ?= $(CURDIR)/data

# bitcoind's RPC cookie, for BITCOIND_RPC_COOKIE. Its directory is mounted
# read-only, not the file: bitcoind replaces the cookie on every restart, and a
# single-file mount would keep showing the old one. --group-add also gives the
# container the cookie's group, for a cookie readable by group
# (`rpccookieperms=group`). Override with BITCOIN_COOKIE=/path/to/.cookie.
BITCOIN_COOKIE ?= $(HOME)/.bitcoin/.cookie
COOKIE_FILE := $(wildcard $(BITCOIN_COOKIE))
COOKIE_ARGS := $(if $(COOKIE_FILE),\
	-v $(dir $(COOKIE_FILE)):/bitcoin:ro \
	--group-add $(shell stat -c %g $(COOKIE_FILE)) \
	-e BITCOIND_RPC_COOKIE=/bitcoin/$(notdir $(COOKIE_FILE)),)

# the container (this user, or the cookie's group) must read the cookie and
# enter its directory: say how before the mint fails on it
define CHECK_COOKIE
f='$(COOKIE_FILE)'; d=$$(dirname "$$f"); \
can() { set -- $$(stat -c '%u %a' "$$1") "$$2"; \
  [ "$$1" = $(RUN_UID) ] || [ $$(( $$(echo "$$2" | rev | cut -c2) & $$3 )) -ne 0 ] || [ $$(( $$(echo "$$2" | rev | cut -c1) & $$3 )) -ne 0 ]; }; \
can "$$f" 4 || echo "WARNING: uid $(RUN_UID) can't read $$f ($$(stat -c '%a %U:%G' "$$f")): run make as its owner, or give bitcoind rpccookieperms=group"; \
can "$$d" 1 || echo "WARNING: uid $(RUN_UID) can't enter $$d ($$(stat -c '%a %U:%G' "$$d")): run make as its owner, or chmod g+x $$d"
endef

# host networking reaches a bitcoind on the host. -t 60: the node writes its
# channel state on SIGTERM. DATA_DIR is pinned to the mount, whatever .env says.
run:
	@echo "Restarting container..."
	docker pull $(IMAGE_NAME) 2>/dev/null || true
	@$(if $(COOKIE_FILE),echo "Mounting bitcoind cookie $(COOKIE_FILE)",echo "No bitcoind cookie at $(BITCOIN_COOKIE) - set BITCOIN_COOKIE=... or use BITCOIND_RPC_USER/PASSWORD in .env")
	@$(if $(COOKIE_FILE),$(CHECK_COOKIE),true)
	mkdir -p $(DATA)
	docker stop -t 60 $(CONTAINER_NAME) 2>/dev/null || true
	docker rm $(CONTAINER_NAME) 2>/dev/null || true
	docker run --restart always -d --name $(CONTAINER_NAME) \
		--network host \
		--stop-timeout 60 \
		--user $(RUN_UID):$(RUN_GID) \
		$(if $(ENV_FILE),--env-file $(ENV_FILE),) \
		$(COOKIE_ARGS) \
		-v $(DATA):/data \
		-e DATA_DIR=/data \
		$(IMAGE_NAME)
	@echo "Container $(CONTAINER_NAME) is running"

# tags v$(VERSION) and pushes the tag: CI's release workflow stamps that
# version into the build and pushes lnurlcash/lnurl-mint-rust to Docker Hub
release:
	@test -n "$(VERSION)" || { echo "usage: make release VERSION=x.y.z"; exit 1; }
	git tag "v$(VERSION)" && git push origin "v$(VERSION)"
