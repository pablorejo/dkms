"""Submit a loadtest to the orchestator and capture its output.

Flow (matches the orchestator built-in loadtest from
``api_orchestator.py:2650-2730``):

1. ``client.submit_loadtest(sim_id, params)`` → ``{test_id,
   deployment_name, ...}``. The orchestator creates a Deployment in
   the sim namespace (``sim-<id>-ns``) that runs the
   ``dkms-loadtest:v1`` image with the SAE ramp described in
   ``params`` (``start_saes``, ``end_saes``, ``step_saes``,
   ``interval_seconds``, ``warmup_seconds``, ``per_sae_lambda``...).
2. ``kubectl wait --for=condition=Available --timeout=...
   deploy/<name>`` to ensure the loadtest pod is up.
3. Tail ``kubectl logs -f deploy/<name>`` into ``loadtest.log``.
4. At the deadline (or when ``stop_event`` fires), scrape Prometheus
   metrics from inside the pod via ``kubectl exec`` and save them as
   ``loadtest-metrics.txt``.
5. Returns ``{test_id, deployment_name, log_path, metrics_path,
   submitted_response, scrape_ok, elapsed_seconds}``.

Compatible with R-005: only ``kubectl`` (via ``subprocess``) and
HTTP (via ``OrchestatorClient``). All shell-outs are routed through
``kubectl_runner`` so tests can inject a fake.
"""

from __future__ import annotations

import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Callable

from tests.cli.log_capture import (
    DEFAULT_KUBECTL,
    KubectlRunner,
    PopenFactory,
    _default_kubectl_runner,
)

DEFAULT_METRICS_PORT: int = 9095
DEFAULT_LOADTEST_TOTAL_TIMEOUT_S: float = 600.0
DEFAULT_WAIT_AVAILABLE_TIMEOUT_S: float = 120.0


def _kubectl_wait_available(
    namespace: str,
    deployment_name: str,
    timeout_seconds: float,
    *,
    kubectl_runner: KubectlRunner,
    kubectl_path: str,
) -> None:
    args = [
        kubectl_path,
        "wait",
        "-n",
        namespace,
        "--for=condition=Available",
        f"--timeout={int(timeout_seconds)}s",
        f"deploy/{deployment_name}",
    ]
    result = kubectl_runner(args)
    if result.returncode != 0:
        raise RuntimeError(
            f"kubectl wait failed (rc={result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )


def _resolve_loadtest_pod(
    namespace: str,
    deployment_name: str,
    *,
    kubectl_runner: KubectlRunner,
    kubectl_path: str,
) -> str | None:
    """Find a Pod backing the given Deployment in ``namespace``.

    Returns the bare pod name (e.g. ``loadtest-ramp-x-abc-defgh``) or
    ``None`` if no Pod was found (Deployment scaled to 0 or already
    deleted).
    """
    args = [
        kubectl_path,
        "get",
        "pods",
        "-n",
        namespace,
        "-o",
        "jsonpath={.items[*].metadata.name}",
        "-l",
        f"app={deployment_name}",
    ]
    result = kubectl_runner(args)
    if result.returncode != 0:
        return None
    names = result.stdout.split()
    if not names:
        # Fallback: filter by name prefix in case the Deployment's `app=`
        # selector convention changes.
        args2 = [
            kubectl_path,
            "get",
            "pods",
            "-n",
            namespace,
            "-o",
            "jsonpath={.items[*].metadata.name}",
        ]
        result2 = kubectl_runner(args2)
        if result2.returncode != 0:
            return None
        for cand in result2.stdout.split():
            if cand.startswith(deployment_name):
                return cand
        return None
    return names[0]


def _kubectl_cp_from_pod(
    namespace: str,
    pod: str,
    remote_path: str,
    local_path: Path,
    *,
    kubectl_runner: KubectlRunner,
    kubectl_path: str,
) -> bool:
    """``kubectl cp <ns>/<pod>:<remote> <local>``. Returns False on failure."""
    local_path.parent.mkdir(parents=True, exist_ok=True)
    args = [
        kubectl_path,
        "cp",
        f"{namespace}/{pod}:{remote_path}",
        str(local_path),
    ]
    result = kubectl_runner(args)
    if result.returncode != 0:
        return False
    return local_path.exists() and local_path.stat().st_size > 0


def _try_extract_loadtest_outputs(
    namespace: str,
    deployment_name: str,
    output_dir: Path,
    *,
    kubectl_runner: KubectlRunner,
    kubectl_path: str,
) -> dict[str, Path | None]:
    """Best-effort copy of ``/var/loadtest-output/{requests,sae_timeline}.csv``
    to ``<output_dir>/data/``.

    The loadtest pod stays alive after its ramp finishes ("manteniendo
    /metrics hasta SIGTERM" in the runner log), so as long as we call
    this before the Deployment is deleted, ``kubectl cp`` works.

    Returns ``{"requests_csv": Path or None, "sae_timeline_csv": Path or
    None}`` — keys are always present, values are ``None`` for any file
    that failed to copy (pod gone, file missing, kubectl error).
    """
    out: dict[str, Path | None] = {
        "requests_csv": None,
        "sae_timeline_csv": None,
    }
    pod = _resolve_loadtest_pod(
        namespace, deployment_name,
        kubectl_runner=kubectl_runner, kubectl_path=kubectl_path,
    )
    if pod is None:
        return out
    data_dir = output_dir / "data"
    targets = [
        ("requests_csv", "/var/loadtest-output/requests.csv", "loadtest_requests.csv"),
        ("sae_timeline_csv", "/var/loadtest-output/sae_timeline.csv", "loadtest_sae_timeline.csv"),
    ]
    for key, remote, local_name in targets:
        local = data_dir / local_name
        if _kubectl_cp_from_pod(
            namespace, pod, remote, local,
            kubectl_runner=kubectl_runner, kubectl_path=kubectl_path,
        ):
            out[key] = local
    return out


def _scrape_metrics(
    namespace: str,
    deployment_name: str,
    *,
    kubectl_runner: KubectlRunner,
    kubectl_path: str,
    metrics_port: int,
) -> str:
    """Use ``kubectl exec`` + python3 urllib to pull the /metrics body.

    The loadtest pod doesn't ship ``curl``; the orchestator's
    ``dkms-loadtest`` image only has python3. Fetched documented in
    CLAUDE.md (section "Schemas for POST /orch/simulations are
    non-trivial").
    """
    url = f"http://localhost:{metrics_port}/metrics"
    inline = (
        "import urllib.request,sys;"
        f"sys.stdout.write(urllib.request.urlopen({url!r}).read().decode())"
    )
    args = [
        kubectl_path,
        "exec",
        "-n",
        namespace,
        f"deploy/{deployment_name}",
        "--",
        "python3",
        "-c",
        inline,
    ]
    result = kubectl_runner(args)
    if result.returncode != 0:
        raise RuntimeError(
            f"kubectl exec metrics scrape failed (rc={result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()[:200]}"
        )
    return result.stdout


def _spawn_logs_tail(
    namespace: str,
    deployment_name: str,
    log_path: Path,
    stop_event: threading.Event,
    *,
    kubectl_path: str,
    popen_factory: PopenFactory,
) -> tuple[threading.Thread, Callable[[], None]]:
    """Background thread that runs ``kubectl logs -f deploy/<name>``."""
    args = [
        kubectl_path,
        "logs",
        "-f",
        "-n",
        namespace,
        f"deploy/{deployment_name}",
    ]
    log_path.parent.mkdir(parents=True, exist_ok=True)
    file_handle = open(log_path, "w", buffering=1, encoding="utf-8")
    proc = popen_factory(args, stdout=file_handle, stderr=subprocess.STDOUT)

    def terminator() -> None:
        try:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        pass
        finally:
            try:
                file_handle.flush()
                file_handle.close()
            except Exception:
                pass

    def worker() -> None:
        try:
            while not stop_event.is_set():
                if proc.poll() is not None:
                    break
                stop_event.wait(timeout=0.5)
        finally:
            terminator()

    thread = threading.Thread(
        target=worker, name=f"loadtest-tail-{deployment_name}", daemon=True
    )
    thread.start()
    return thread, terminator


def submit_and_wait_loadtest(
    client: Any,
    sim_id: int,
    params: dict[str, Any],
    output_dir: str | Path,
    *,
    total_timeout_seconds: float = DEFAULT_LOADTEST_TOTAL_TIMEOUT_S,
    wait_available_seconds: float = DEFAULT_WAIT_AVAILABLE_TIMEOUT_S,
    metrics_port: int = DEFAULT_METRICS_PORT,
    namespace: str | None = None,
    stop_event: threading.Event | None = None,
    kubectl_runner: KubectlRunner | None = None,
    popen_factory: PopenFactory = subprocess.Popen,
    kubectl_path: str = DEFAULT_KUBECTL,
) -> dict[str, Any]:
    """Submit a loadtest, wait until done, capture logs + Prometheus metrics.

    Args:
        client: ``OrchestatorClient`` instance (already logged in).
        sim_id: target simulation id.
        params: forwarded as JSON body to
            ``POST /orch/web/simulations/<sim_id>/tests`` —
            ``start_saes``, ``end_saes``, ``step_saes``,
            ``interval_seconds``, ``warmup_seconds``, ``per_sae_lambda``,
            ``key_size_bits``, ``request_timeout_seconds``.
        output_dir: per-run directory; ``loadtest.log`` and
            ``loadtest-metrics.txt`` land here.
        total_timeout_seconds: max wall-clock time before stopping.
        wait_available_seconds: how long to wait for the Deployment
            to become ``Available``.
        metrics_port: ``METRICS_PORT`` env in the loadtest pod (default
            9095, per orchestator). Override only if the pod
            spec changes.
        namespace: target namespace. ``None`` → ``sim-<sim_id>-ns``
            (orchestator convention).
        stop_event: external cancellation handle.
        kubectl_runner / popen_factory: test seams.

    Returns:
        ``{test_id, deployment_name, log_path, metrics_path,
        submitted_response, scrape_ok, scrape_error, elapsed_seconds}``.
    """
    out_dir = Path(output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    log_path = out_dir / "loadtest.log"
    metrics_path = out_dir / "loadtest-metrics.txt"
    runner = kubectl_runner or _default_kubectl_runner
    event = stop_event if stop_event is not None else threading.Event()
    ns = namespace if namespace is not None else f"sim-{sim_id}-ns"

    started = time.monotonic()
    submitted = client.submit_loadtest(sim_id, params)
    deployment_name = submitted["deployment_name"]
    test_id = submitted["test_id"]

    _kubectl_wait_available(
        ns,
        deployment_name,
        wait_available_seconds,
        kubectl_runner=runner,
        kubectl_path=kubectl_path,
    )

    tail_thread, terminator = _spawn_logs_tail(
        ns,
        deployment_name,
        log_path,
        event,
        kubectl_path=kubectl_path,
        popen_factory=popen_factory,
    )

    deadline = started + total_timeout_seconds
    # Scrape periodically while the pod is still alive. The loadtest pod
    # terminates as soon as the ramp finishes, so a single scrape *after*
    # the deadline often hits "container not found". Strategy:
    # poll every `scrape_interval` seconds, keep the last successful body.
    scrape_interval = max(15.0, min(60.0, total_timeout_seconds / 5))
    last_good_body: str | None = None
    scrape_error: str | None = None
    next_scrape = started + scrape_interval
    extracted: dict[str, Path | None] = {
        "requests_csv": None,
        "sae_timeline_csv": None,
    }
    try:
        while not event.is_set() and time.monotonic() < deadline:
            event.wait(timeout=1.0)
            if time.monotonic() >= next_scrape:
                try:
                    body = _scrape_metrics(
                        ns,
                        deployment_name,
                        kubectl_runner=runner,
                        kubectl_path=kubectl_path,
                        metrics_port=metrics_port,
                    )
                    last_good_body = body
                    scrape_error = None
                except Exception as exc:  # noqa: BLE001
                    scrape_error = str(exc)
                # Whenever a scrape succeeds the pod is reachable; this
                # is also when ``kubectl cp`` can pull the per-request
                # CSVs from /var/loadtest-output. We only attempt it
                # once we have something to extract (after warmup +
                # first ramp step), and only if we haven't got the file
                # already.
                if last_good_body is not None:
                    fresh = _try_extract_loadtest_outputs(
                        ns,
                        deployment_name,
                        Path(output_dir),
                        kubectl_runner=runner,
                        kubectl_path=kubectl_path,
                    )
                    for key, val in fresh.items():
                        if val is not None:
                            extracted[key] = val
                next_scrape = time.monotonic() + scrape_interval
    finally:
        event.set()
        terminator()
        tail_thread.join(timeout=10)

    # One last attempt after the loop; some metrics may have accrued.
    try:
        body = _scrape_metrics(
            ns,
            deployment_name,
            kubectl_runner=runner,
            kubectl_path=kubectl_path,
            metrics_port=metrics_port,
        )
        last_good_body = body
        scrape_error = None
    except Exception as exc:  # noqa: BLE001
        # Keep last_good_body if we already had one; only record error if not.
        if last_good_body is None:
            scrape_error = str(exc)

    # Final cp attempt — the pod may still be alive (it waits for SIGTERM
    # after the ramp finishes, before the orchestator stops the sim).
    if last_good_body is not None:
        fresh = _try_extract_loadtest_outputs(
            ns,
            deployment_name,
            Path(output_dir),
            kubectl_runner=runner,
            kubectl_path=kubectl_path,
        )
        for key, val in fresh.items():
            if val is not None:
                extracted[key] = val

    scrape_ok = last_good_body is not None
    if scrape_ok:
        metrics_path.write_text(last_good_body or "", encoding="utf-8")
    else:
        metrics_path.write_text("", encoding="utf-8")

    elapsed = time.monotonic() - started
    return {
        "test_id": test_id,
        "deployment_name": deployment_name,
        "namespace": ns,
        "log_path": log_path,
        "metrics_path": metrics_path,
        "submitted_response": submitted,
        "scrape_ok": scrape_ok,
        "scrape_error": scrape_error,
        "elapsed_seconds": elapsed,
        "requests_csv": extracted["requests_csv"],
        "sae_timeline_csv": extracted["sae_timeline_csv"],
    }


__all__ = [
    "DEFAULT_LOADTEST_TOTAL_TIMEOUT_S",
    "DEFAULT_METRICS_PORT",
    "DEFAULT_WAIT_AVAILABLE_TIMEOUT_S",
    "submit_and_wait_loadtest",
]
