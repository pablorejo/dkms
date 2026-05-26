FROM python:3.12-slim-bookworm

ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 PIP_NO_CACHE_DIR=1

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates coreutils \
    && rm -rf /var/lib/apt/lists/*

# pycares wheel embeds c-ares — no extra system lib needed.
RUN pip install --no-cache-dir aiohttp==3.13.5 aiodns==4.0.4

WORKDIR /app

COPY etsi014_roundtrip_loadtest.py /app/

CMD ["python3", "/app/etsi014_roundtrip_loadtest.py"]
