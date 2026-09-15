# DKMS container images.
#
# Targets:
#   make help              show this list
#   make check             fmt --check + clippy -D warnings + doc + linkcheck + test (the CI gate)
#   make fmt / clippy / doc / linkcheck / test   the pieces of `check`, one at a time
#   make doc-open          build the rustdoc and open it in the browser
#   make images            build the 5 Rust images locally, no push.
#   make push              push images already tagged with $(TAG).
#
# Multi-host deployment (one `docker compose up` per institution) is
# documented in docker/README.md. The same images can be built for several
# platforms at once with:
#
#   docker buildx bake -f docker/docker-bake.hcl --push
#
# Variables (override on the command line):
#   TAG=v2                 mutable image tag applied to every image
#   IMAGE_PREFIX=pablopio

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
.ONESHELL:

TAG             ?= v2
IMAGE_PREFIX    ?= pablopio
GIT_SHA         := $(shell git rev-parse --short HEAD 2>/dev/null || echo nogit)
IMMUTABLE_TAG   ?= $(TAG)-$(GIT_SHA)

ALL_RUST        := dkms orr qkc sdn quditto

.PHONY: help check fmt clippy doc doc-open linkcheck test rendercheck deny images push

# ──────────────────────────────────────────────────────────────────────
# help
# ──────────────────────────────────────────────────────────────────────
help:
	@awk 'BEGIN{FS=":.*##"; printf "DKMS images\n\nTargets:\n"} \
	  /^[a-zA-Z_-]+:.*?##/ {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}' \
	  $(MAKEFILE_LIST)
	@echo
	@echo "Variables:"
	@echo "  TAG=$(TAG)"
	@echo "  IMMUTABLE_TAG=$(IMMUTABLE_TAG)"
	@echo "  IMAGE_PREFIX=$(IMAGE_PREFIX)"
	@echo "  GIT_SHA=$(GIT_SHA)"

# ──────────────────────────────────────────────────────────────────────
# Quality gate. `check` is what CI runs (.github/workflows/ci.yml); run it
# locally before pushing. `check` exports DKMS_NO_TEST_SKIPS=1 itself, which
# turns every environment-dependent test skip (openssl < 3.5, non-default LP
# solver) into a failure, so a green run means everything actually ran. A bare
# `make test` stays permissive (skips allowed) for boxes without openssl 3.5.
# ──────────────────────────────────────────────────────────────────────
fmt: ## cargo fmt --all -- --check
	@cargo fmt --all -- --check

clippy: ## cargo clippy --workspace --all-targets -- -D warnings
	@cargo clippy --workspace --all-targets -- -D warnings

# rustdoc is part of the gate: a broken intra-doc link or a `<T>` read as
# HTML is a warning here and a dead link in the published docs. Private
# items are documented on purpose — four of the five crates are binaries,
# so their "public API" is not the interesting part.
DOC_FLAGS := --workspace --no-deps --document-private-items

doc: ## cargo doc (warnings are errors) + landing page at <target>/doc/index.html
	@RUSTDOCFLAGS="-D warnings" cargo doc $(DOC_FLAGS)
	@python3 scripts/rustdoc-index.py

doc-open: doc ## make doc + open it in the browser
	@d="$$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/doc/index.html"; \
	  xdg-open "$$d" 2>/dev/null || open "$$d"

test: ## cargo test --workspace
	@cargo test --workspace

deny: ## cargo audit + cargo deny (cadena de suministro; NECESITA RED, fuera de `check`)
	@cargo audit
	@cargo deny check advisories bans sources licenses

rendercheck: ## tests of the node.yml -> TOML renderer (docker/render_config.py)
	@python3 -m unittest discover -s docker -p "test_*.py"

linkcheck: ## relative links and anchors of every tracked Markdown file resolve
	@python3 scripts/check-md-links.py

check: export DKMS_NO_TEST_SKIPS = 1
check: fmt clippy doc linkcheck rendercheck test ## fmt + clippy + doc + linkcheck + rendercheck + test (no skips)

# ──────────────────────────────────────────────────────────────────────
# Build (local, no push).
# ──────────────────────────────────────────────────────────────────────
images: ## build the 5 Rust images
	@TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) bash scripts/build-images.sh

# ──────────────────────────────────────────────────────────────────────
# Push (after build).
# ──────────────────────────────────────────────────────────────────────
push: ## push every $(TAG) image to the registry
	@TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) IMMUTABLE_TAG=$(IMMUTABLE_TAG) \
	  bash scripts/deploy-images.sh --push $(ALL_RUST)
