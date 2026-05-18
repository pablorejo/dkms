# DKMS deployment orchestration.
#
# Targets:
#   make help              show this list
#   make deploy            full rebuild + redeploy (calls `clean` first)
#   make deploy-fast       build/push only what changed since the previous git
#                          tag, rollout selectively. Does NOT touch the DB or
#                          delete simulation namespaces.
#   make deploy-restore    `clean` + `deploy-fast`. Like fast, but the cluster
#                          and DB are wiped before redeploy.
#   make clean             wipe simulation namespaces + control plane + DROP
#                          DATABASE + recreate schema + seed.
#   make images            build all images locally, no push.
#   make images-rust       build the 5 Rust images.
#   make images-python     build orchestrator + authz + web.
#   make push              push images already tagged with $(TAG).
#   make seed              run seed_db_from_configs.py against the current DB.
#   make verify            smoke checks against the live deployment.
#   make detect-changes    print components that changed since $(PREVIOUS_TAG).
#
# Variables (override on the command line):
#   TAG=v2                 mutable image tag pushed to every component
#   IMAGE_PREFIX=pablopio
#   PREVIOUS_TAG           git ref used as baseline by fast/restore
#                          (default: latest tag; falls back to HEAD~1)
#   KUBECTL=kubectl

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
.ONESHELL:

TAG             ?= v2
IMAGE_PREFIX    ?= pablopio
KUBECTL         ?= kubectl
GIT_SHA         := $(shell git rev-parse --short HEAD 2>/dev/null || echo nogit)
PREVIOUS_TAG    ?= $(shell git describe --tags --abbrev=0 2>/dev/null || echo HEAD~1)
IMMUTABLE_TAG   ?= $(TAG)-$(GIT_SHA)

ALL_RUST        := dkms orr qkc sdn quditto
ALL_PYTHON      := orchestrator authz web
ALL_COMPONENTS  := $(ALL_RUST) $(ALL_PYTHON)

.PHONY: help deploy deploy-fast deploy-restore clean images images-rust \
        images-python push seed verify detect-changes _preflight _push-only \
        _build-changed _apply-changed

# ──────────────────────────────────────────────────────────────────────
# help
# ──────────────────────────────────────────────────────────────────────
help:
	@awk 'BEGIN{FS=":.*##"; printf "DKMS deployment\n\nTargets:\n"} \
	  /^[a-zA-Z_-]+:.*?##/ {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}' \
	  $(MAKEFILE_LIST)
	@echo
	@echo "Variables:"
	@echo "  TAG=$(TAG)"
	@echo "  PREVIOUS_TAG=$(PREVIOUS_TAG)"
	@echo "  IMMUTABLE_TAG=$(IMMUTABLE_TAG)"
	@echo "  IMAGE_PREFIX=$(IMAGE_PREFIX)"
	@echo "  GIT_SHA=$(GIT_SHA)"

# ──────────────────────────────────────────────────────────────────────
# Preflight: validate tooling and env.
# ──────────────────────────────────────────────────────────────────────
_preflight: ## (internal) validate kubectl/docker context + env
	@command -v $(KUBECTL) >/dev/null || { echo "kubectl missing"; exit 1; }
	@command -v docker     >/dev/null || { echo "docker missing"; exit 1; }
	@ctx=$$($(KUBECTL) config current-context 2>/dev/null); \
	  [[ -n "$$ctx" ]] || { echo "no kubectl context"; exit 1; }; \
	  echo "[preflight] kubectl context: $$ctx"

# ──────────────────────────────────────────────────────────────────────
# Build (local, no push).
# ──────────────────────────────────────────────────────────────────────
images: images-rust images-python ## build every image locally

images-rust: ## build the 5 Rust images
	@TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) bash scripts/build-images.sh

images-python: ## build orchestrator + authz + web
	@TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) IMMUTABLE_TAG=$(IMMUTABLE_TAG) \
	  bash scripts/deploy-images.sh orchestrator authz web

# ──────────────────────────────────────────────────────────────────────
# Push (after build).
# ──────────────────────────────────────────────────────────────────────
push: ## push every $(TAG) image to the registry
	@TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) IMMUTABLE_TAG=$(IMMUTABLE_TAG) \
	  bash scripts/deploy-images.sh --push $(ALL_COMPONENTS)

# ──────────────────────────────────────────────────────────────────────
# Clean: cluster + DB.
# ──────────────────────────────────────────────────────────────────────
clean: _preflight ## delete all DKMS k8s resources + DROP/CREATE DB + seed
	@echo "════════ make clean ════════"
	@bash scripts/k8s-clean.sh
	@bash scripts/db-reset.sh

# ──────────────────────────────────────────────────────────────────────
# Full deploy: clean + build all + push all + apply manifests.
# ──────────────────────────────────────────────────────────────────────
deploy: _preflight ## clean + full rebuild + redeploy
	@echo "════════ make deploy (full) ════════"
	@$(MAKE) clean
	@TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) IMMUTABLE_TAG=$(IMMUTABLE_TAG) \
	  bash scripts/deploy-images.sh --push $(ALL_COMPONENTS)
	@TAG=$(TAG) bash scripts/k8s-apply.sh --rollout
	@$(MAKE) verify

# ──────────────────────────────────────────────────────────────────────
# Fast deploy: only what changed.
# ──────────────────────────────────────────────────────────────────────
detect-changes: ## print changed components since $(PREVIOUS_TAG)
	@bash scripts/detect-changes.sh $(PREVIOUS_TAG) HEAD

deploy-fast: _preflight ## build/push only changed components since $(PREVIOUS_TAG)
	@echo "════════ make deploy-fast (baseline=$(PREVIOUS_TAG)) ════════"
	@changed=$$(bash scripts/detect-changes.sh $(PREVIOUS_TAG) HEAD 2>/dev/null || true); \
	  if [[ -z "$$changed" ]]; then \
	    echo "[deploy-fast] nothing changed; refusing to rebuild"; \
	    exit 0; \
	  fi; \
	  echo "[deploy-fast] components: $$(echo $$changed | tr '\n' ' ')"; \
	  components=$$(echo "$$changed" | grep -vx manifests || true); \
	  if [[ -n "$$components" ]]; then \
	    TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) IMMUTABLE_TAG=$(IMMUTABLE_TAG) \
	      bash scripts/deploy-images.sh --push $$components; \
	  fi; \
	  control_plane=""; \
	  echo "$$changed" | grep -qx orchestrator && control_plane="$$control_plane orchestrator"; \
	  echo "$$changed" | grep -qx authz        && control_plane="$$control_plane authz"; \
	  echo "$$changed" | grep -qx web          && control_plane="$$control_plane web"; \
	  echo "$$changed" | grep -qx manifests    && control_plane="$$control_plane manifests"; \
	  if [[ -n "$$control_plane" ]]; then \
	    TAG=$(TAG) bash scripts/k8s-apply.sh --rollout $$control_plane; \
	  fi
	@$(MAKE) verify

# ──────────────────────────────────────────────────────────────────────
# Restore deploy: clean + build-changed + apply EVERYTHING.
#
# After `clean` the cluster is empty, so we cannot rely on detect-changes
# to decide what to apply: it would skip components whose source did not
# change between tags but which still need to be redeployed. We rebuild
# only the changed images (fast-ish) but the kubectl apply step covers
# the full control plane.
# ──────────────────────────────────────────────────────────────────────
deploy-restore: _preflight ## clean + build-changed + full apply
	@echo "════════ make deploy-restore ════════"
	@$(MAKE) clean
	@changed=$$(bash scripts/detect-changes.sh $(PREVIOUS_TAG) HEAD 2>/dev/null || true); \
	  if [[ -z "$$changed" ]]; then \
	    echo "[deploy-restore] no diff vs $(PREVIOUS_TAG); rebuilding everything"; \
	    components="$(ALL_COMPONENTS)"; \
	  else \
	    components=$$(echo "$$changed" | grep -vx manifests || true); \
	  fi; \
	  if [[ -n "$$components" ]]; then \
	    TAG=$(TAG) IMAGE_PREFIX=$(IMAGE_PREFIX) IMMUTABLE_TAG=$(IMMUTABLE_TAG) \
	      bash scripts/deploy-images.sh --push $$components; \
	  fi
	@TAG=$(TAG) bash scripts/k8s-apply.sh --rollout
	@$(MAKE) verify

# ──────────────────────────────────────────────────────────────────────
# Misc.
# ──────────────────────────────────────────────────────────────────────
seed: ## run seed_db_from_configs.py against the current DB
	@bash scripts/db-reset.sh --no-seed
	@echo "[seed] running seed_db_from_configs.py..."
	@cd orchestrator && python3 seed_db_from_configs.py

verify: ## smoke checks against the live deployment
	@echo "── pods ──"
	@$(KUBECTL) get pods -n dkms-main-ns -n web-dkms 2>/dev/null || true
	@$(KUBECTL) get pods -n web-dkms 2>/dev/null || true
	@echo "── ingress ──"
	@$(KUBECTL) get ingress -A 2>/dev/null | grep -E "dkms2|authz|orchestator|dkms-web" || true
	@echo "── HTTP smoke ──"
	@curl -s -o /dev/null -w "web /web/login   → %{http_code}\n" https://dkms2.pablopiorejoiglesias.es/web/login || true
	@curl -s -o /dev/null -w "orch /orch/health → %{http_code}\n" https://dkms2.pablopiorejoiglesias.es/orch/health || true

# ──────────────────────────────────────────────────────────────────────
# dkms-topo CLI (tests/cli/) — local Python CLI for EKS smoke tests.
# Append-only block added by agent-dkms-topo-cli iter 024 (2026-05-18).
# ──────────────────────────────────────────────────────────────────────

.PHONY: topo-cli-install topo-cli-test topo-cli-test-integration

topo-cli-install: ## create tests/cli/.venv and install Python deps
	@python3 -m venv tests/cli/.venv
	@tests/cli/.venv/bin/pip install -U pip
	@tests/cli/.venv/bin/pip install -r tests/cli/requirements.txt
	@echo "[topo-cli-install] venv ready at tests/cli/.venv/"

topo-cli-test: ## run dkms-topo unit tests (no EKS, no port-forwards)
	@python3 -m pytest tests/cli/

topo-cli-test-integration: ## run dkms-topo integration tests (needs EKS port-forwards)
	@python3 -m pytest tests/cli/ -m integration
