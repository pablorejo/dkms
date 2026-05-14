# Docker image for `web/`

## Purpose

This directory contains the Docker build assets for the Next.js web
application.

## What it contains

- `Dockerfile`: multi-stage production image build
- `build-and-push.sh`: helper script for `build`, `push`, and
  `build-and-push`

## Key inputs

Loaded from the repository root `.env` when present:

- `DOCKER_HUB_USERNAME`
- `DOCKER_HUB_TOKEN`

Optional overrides:

- `WEB_IMAGE_REPO`
- `WEB_IMAGE_TAG_STABLE`
- `WEB_IMAGE_TAG_LATEST`
- `DOCKER`

## Usage

```bash
bash web/docker/build-and-push.sh build
bash web/docker/build-and-push.sh push
bash web/docker/build-and-push.sh build-and-push
```

## Related docs

- [../README.md](../README.md)
- [../k8s/README.md](../k8s/README.md)
- [../../docs/OPERATIONS.md](../../docs/OPERATIONS.md)
