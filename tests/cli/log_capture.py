"""Capture ``kubectl logs -f`` from a set of pods to local files.

Cumple **R-005** (`subprocess` → ``kubectl`` ÚNICAMENTE; no `kube-rs`,
no Python K8s client). Cumple **R-006** (sin dependencias externas).

Diseño:

* ``list_pods(namespace, ...)``: helper que delega en ``kubectl get pods``
  para resolver el conjunto de pods objetivo. Acepta substring de
  nombre y/o label selector. Pure stdlib.
* ``PodTail``: encapsula un único ``kubectl logs -f <pod>`` corriendo
  en background; redirige stdout/stderr al fichero
  ``<output_dir>/<pod>.log`` y termina el subproceso cuando
  ``stop_event`` se activa.
* ``tail_pods(...)``: arranca un ``PodTail`` por pod, devuelve un
  ``LogCapture`` agregador con ``.stop()`` y ``.join(timeout)``.

Para tests sin red, el caller puede inyectar ``popen_factory`` y
``kubectl_runner`` para mockear los binarios.
"""

from __future__ import annotations

import os
import shlex
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterable

# Type aliases
PopenFactory = Callable[..., subprocess.Popen]
KubectlRunner = Callable[[list[str]], "subprocess.CompletedProcess[str]"]

DEFAULT_KUBECTL = "kubectl"
DEFAULT_TERM_GRACE_SECONDS: float = 5.0


def _default_kubectl_runner(args: list[str]) -> subprocess.CompletedProcess[str]:
    """Run ``kubectl <args>`` and return the CompletedProcess (stdout captured)."""
    return subprocess.run(
        args,
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )


def list_pods(
    namespace: str,
    *,
    name_substring: str | None = None,
    label_selector: str | None = None,
    kubectl_runner: KubectlRunner | None = None,
    kubectl_path: str = DEFAULT_KUBECTL,
) -> list[str]:
    """Return the list of pod names in ``namespace`` matching the filter.

    Either or both filters may be provided. ``name_substring`` is matched
    case-sensitively against the pod name (``kubectl`` returns
    ``pod/<name>``; the ``pod/`` prefix is stripped before matching).
    """
    runner = kubectl_runner or _default_kubectl_runner
    args = [kubectl_path, "get", "pods", "-n", namespace, "-o", "name"]
    if label_selector:
        args.extend(["-l", label_selector])
    result = runner(args)
    if result.returncode != 0:
        raise RuntimeError(
            f"kubectl get pods failed (rc={result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )
    names: list[str] = []
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith("pod/"):
            line = line[len("pod/") :]
        if name_substring and name_substring not in line:
            continue
        names.append(line)
    return names


# -----------------------------------------------------------------------------
# Per-pod tail
# -----------------------------------------------------------------------------


@dataclass
class PodTail:
    """Tail a single pod via ``kubectl logs -f``."""

    pod: str
    namespace: str
    output_path: Path
    stop_event: threading.Event
    kubectl_path: str = DEFAULT_KUBECTL
    popen_factory: PopenFactory = subprocess.Popen
    term_grace_seconds: float = DEFAULT_TERM_GRACE_SECONDS
    container: str | None = None

    _proc: subprocess.Popen | None = field(init=False, default=None, repr=False)
    _thread: threading.Thread | None = field(init=False, default=None, repr=False)
    _file: Any = field(init=False, default=None, repr=False)

    def _build_args(self) -> list[str]:
        args = [self.kubectl_path, "logs", "-f", self.pod, "-n", self.namespace]
        if self.container:
            args.extend(["-c", self.container])
        return args

    def start(self) -> None:
        if self._thread is not None:
            return
        self.output_path.parent.mkdir(parents=True, exist_ok=True)
        self._file = open(self.output_path, "w", buffering=1, encoding="utf-8")
        try:
            self._proc = self.popen_factory(
                self._build_args(),
                stdout=self._file,
                stderr=subprocess.STDOUT,
            )
        except Exception:
            self._file.close()
            self._file = None
            raise
        self._thread = threading.Thread(
            target=self._supervise,
            name=f"tail-{self.pod}",
            daemon=True,
        )
        self._thread.start()

    def _supervise(self) -> None:
        try:
            while not self.stop_event.is_set():
                # Process may exit on its own (e.g., pod terminates); we still
                # honor stop_event for clean shutdown.
                if self._proc is None or self._proc.poll() is not None:
                    break
                self.stop_event.wait(timeout=0.5)
            self._terminate()
        finally:
            if self._file is not None:
                try:
                    self._file.flush()
                    self._file.close()
                except Exception:
                    pass
                self._file = None

    def _terminate(self) -> None:
        if self._proc is None:
            return
        if self._proc.poll() is not None:
            return
        try:
            self._proc.terminate()
        except Exception:
            return
        try:
            self._proc.wait(timeout=self.term_grace_seconds)
        except subprocess.TimeoutExpired:
            try:
                self._proc.kill()
            except Exception:
                pass
            try:
                self._proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass

    def join(self, timeout: float | None = None) -> None:
        if self._thread is not None:
            self._thread.join(timeout=timeout)

    def is_alive(self) -> bool:
        return self._thread is not None and self._thread.is_alive()


# -----------------------------------------------------------------------------
# Aggregator
# -----------------------------------------------------------------------------


@dataclass
class LogCapture:
    """Manages a fan-out of ``PodTail`` workers under one ``stop_event``."""

    namespace: str
    output_dir: Path
    pods: list[str]
    stop_event: threading.Event
    tails: list[PodTail] = field(default_factory=list)

    def stop(self) -> None:
        self.stop_event.set()

    def join(self, timeout: float | None = None) -> None:
        deadline = None if timeout is None else time.monotonic() + timeout
        for tail in self.tails:
            remaining = None
            if deadline is not None:
                remaining = max(0.0, deadline - time.monotonic())
            tail.join(timeout=remaining)

    def files(self) -> dict[str, Path]:
        return {tail.pod: tail.output_path for tail in self.tails}

    def __enter__(self) -> "LogCapture":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.stop()
        self.join(timeout=10)


def tail_pods(
    namespace: str,
    pod_pattern: str | Iterable[str],
    output_dir: str | os.PathLike[str],
    *,
    stop_event: threading.Event | None = None,
    label_selector: str | None = None,
    container: str | None = None,
    kubectl_path: str = DEFAULT_KUBECTL,
    kubectl_runner: KubectlRunner | None = None,
    popen_factory: PopenFactory = subprocess.Popen,
    term_grace_seconds: float = DEFAULT_TERM_GRACE_SECONDS,
) -> LogCapture:
    """Tail every pod in ``namespace`` matching ``pod_pattern`` to disk.

    Args:
        namespace: Kubernetes namespace.
        pod_pattern: Either (a) a substring of the pod name, OR (b) an
            iterable of pod names. With (b), no ``kubectl get pods`` is
            performed; the iterable is used as-is.
        output_dir: Local directory. ``<output_dir>/<pod>.log`` is
            written per pod. Created if missing.
        stop_event: External cancellation handle. If ``None``, a fresh
            ``threading.Event`` is created (still reachable via
            ``capture.stop_event``).
        label_selector: Forwarded to ``kubectl get pods -l ...`` when
            ``pod_pattern`` is a string.
        container: ``kubectl logs -c <container>`` (multi-container pods).
        kubectl_path: Path to ``kubectl`` binary (default ``"kubectl"``).
        kubectl_runner / popen_factory: Test seams. Defaults call
            ``subprocess.run`` / ``subprocess.Popen``.

    Returns:
        A ``LogCapture`` whose ``.stop()`` activates the event and whose
        ``.join(timeout)`` waits for every worker to exit.
    """
    out_dir = Path(output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    event = stop_event if stop_event is not None else threading.Event()

    if isinstance(pod_pattern, str):
        pods = list_pods(
            namespace,
            name_substring=pod_pattern or None,
            label_selector=label_selector,
            kubectl_runner=kubectl_runner,
            kubectl_path=kubectl_path,
        )
    else:
        pods = list(pod_pattern)

    capture = LogCapture(
        namespace=namespace,
        output_dir=out_dir,
        pods=pods,
        stop_event=event,
    )

    for pod in pods:
        tail = PodTail(
            pod=pod,
            namespace=namespace,
            output_path=out_dir / f"{pod}.log",
            stop_event=event,
            kubectl_path=kubectl_path,
            popen_factory=popen_factory,
            term_grace_seconds=term_grace_seconds,
            container=container,
        )
        tail.start()
        capture.tails.append(tail)

    return capture


def kubectl_command_preview(
    namespace: str, pod: str, *, container: str | None = None
) -> str:
    """Return the shell-quoted command that ``PodTail`` would run.

    Useful for ``--dry-run`` listings and tests.
    """
    parts = [DEFAULT_KUBECTL, "logs", "-f", pod, "-n", namespace]
    if container:
        parts.extend(["-c", container])
    return " ".join(shlex.quote(p) for p in parts)


__all__ = [
    "DEFAULT_KUBECTL",
    "DEFAULT_TERM_GRACE_SECONDS",
    "KubectlRunner",
    "LogCapture",
    "PodTail",
    "PopenFactory",
    "kubectl_command_preview",
    "list_pods",
    "tail_pods",
]
