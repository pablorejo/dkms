# DKMS container images.
#
# Targets:
#   make help              show this list
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

.PHONY: help images push

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
