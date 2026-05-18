"""Unit tests for ``tests/cli/log_capture``.

All ``kubectl`` and ``subprocess.Popen`` calls are mocked via the
dependency-injection seams (``kubectl_runner``, ``popen_factory``);
no network or real ``kubectl`` is invoked.
"""

from __future__ import annotations

import subprocess
import tempfile
import threading
import time
from pathlib import Path

import pytest

from tests.cli.log_capture import (
    LogCapture,
    PodTail,
    kubectl_command_preview,
    list_pods,
    tail_pods,
)


# -----------------------------------------------------------------------------
# Helpers
# -----------------------------------------------------------------------------


def _fake_kubectl_factory(stdout: str, *, returncode: int = 0, stderr: str = ""):
    """Return a callable mimicking ``subprocess.run``."""

    def runner(args):
        return subprocess.CompletedProcess(
            args, returncode=returncode, stdout=stdout, stderr=stderr
        )

    return runner


class FakePopen:
    """Minimal Popen replacement that writes one marker line and exits."""

    def __init__(self, args, stdout=None, stderr=None):
        self.args = args
        if stdout is not None:
            stdout.write(f"mock-line for {args[3]}\n")
            stdout.flush()
        self._returncode = 0
        self.terminated = False
        self.killed = False

    def poll(self):
        return self._returncode

    def terminate(self):
        self.terminated = True

    def wait(self, timeout=None):
        return self._returncode

    def kill(self):
        self.killed = True


class StubbornPopen(FakePopen):
    """Popen that ignores SIGTERM and only dies on SIGKILL."""

    def __init__(self, args, stdout=None, stderr=None):
        super().__init__(args, stdout, stderr)
        self._returncode = None  # still running
        self._term_wait_calls = 0

    def poll(self):
        return self._returncode

    def terminate(self):
        # Do nothing — caller must escalate to kill.
        self.terminated = True

    def wait(self, timeout=None):
        self._term_wait_calls += 1
        if self.killed:
            self._returncode = -9
            return self._returncode
        raise subprocess.TimeoutExpired(self.args, timeout or 1.0)

    def kill(self):
        self.killed = True
        self._returncode = -9


# -----------------------------------------------------------------------------
# kubectl_command_preview / list_pods
# -----------------------------------------------------------------------------


def test_preview_command_basic() -> None:
    assert kubectl_command_preview("ns", "pod-a") == "kubectl logs -f pod-a -n ns"


def test_preview_command_with_container() -> None:
    cmd = kubectl_command_preview("ns", "pod-a", container="dkms")
    assert "-c dkms" in cmd


def test_list_pods_filters_by_substring() -> None:
    runner = _fake_kubectl_factory(
        "pod/dkms-1\npod/dkms-2\npod/loadtest-x\n"
    )
    pods = list_pods("ns", name_substring="dkms", kubectl_runner=runner)
    assert pods == ["dkms-1", "dkms-2"]


def test_list_pods_returns_all_when_no_filter() -> None:
    runner = _fake_kubectl_factory("pod/a\npod/b\n")
    assert list_pods("ns", kubectl_runner=runner) == ["a", "b"]


def test_list_pods_passes_label_selector() -> None:
    seen_args: list[list[str]] = []

    def capturing_runner(args):
        seen_args.append(args)
        return subprocess.CompletedProcess(args, returncode=0, stdout="", stderr="")

    list_pods("ns", label_selector="app=dkms", kubectl_runner=capturing_runner)
    assert seen_args[0][:7] == ["kubectl", "get", "pods", "-n", "ns", "-o", "name"]
    assert "-l" in seen_args[0] and "app=dkms" in seen_args[0]


def test_list_pods_raises_on_nonzero_rc() -> None:
    runner = _fake_kubectl_factory("", returncode=1, stderr="forbidden")
    with pytest.raises(RuntimeError) as ei:
        list_pods("ns", kubectl_runner=runner)
    assert "forbidden" in str(ei.value)


# -----------------------------------------------------------------------------
# tail_pods
# -----------------------------------------------------------------------------


def test_tail_pods_with_explicit_list_writes_files() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        cap = tail_pods(
            "ns",
            ["pod-a", "pod-b"],
            tmpdir,
            popen_factory=FakePopen,
            term_grace_seconds=1.0,
        )
        # Give threads a moment to flush
        time.sleep(0.1)
        cap.stop()
        cap.join(timeout=5)
        for pod, log_path in cap.files().items():
            text = Path(log_path).read_text()
            assert f"mock-line for {pod}" in text


def test_tail_pods_creates_output_dir() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "subdir-that-doesnt-exist"
        cap = tail_pods("ns", ["x"], out, popen_factory=FakePopen)
        cap.stop()
        cap.join(timeout=5)
        assert out.exists()


def test_tail_pods_resolves_pattern_via_kubectl_runner() -> None:
    runner = _fake_kubectl_factory("pod/dkms-7\npod/dkms-8\n")
    with tempfile.TemporaryDirectory() as tmpdir:
        cap = tail_pods(
            "ns",
            "dkms",
            tmpdir,
            popen_factory=FakePopen,
            kubectl_runner=runner,
        )
        cap.stop()
        cap.join(timeout=5)
    assert sorted(cap.pods) == ["dkms-7", "dkms-8"]


def test_context_manager_auto_stops() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        with tail_pods(
            "ns", ["x"], tmpdir, popen_factory=FakePopen
        ) as cap:
            time.sleep(0.05)
    assert cap.stop_event.is_set()


def test_stubborn_subprocess_is_killed_after_grace() -> None:
    """SIGTERM ignored → fall back to SIGKILL after term_grace_seconds."""
    stop = threading.Event()
    with tempfile.TemporaryDirectory() as tmpdir:
        out = Path(tmpdir) / "x.log"
        tail = PodTail(
            pod="x",
            namespace="ns",
            output_path=out,
            stop_event=stop,
            popen_factory=StubbornPopen,
            term_grace_seconds=0.05,
        )
        tail.start()
        time.sleep(0.05)
        stop.set()
        tail.join(timeout=3)
    assert not tail.is_alive()


def test_files_dict_maps_pod_to_path() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        cap = tail_pods(
            "ns", ["a", "b"], tmpdir, popen_factory=FakePopen
        )
        cap.stop()
        cap.join(timeout=5)
        files = cap.files()
        assert set(files.keys()) == {"a", "b"}
        for path in files.values():
            assert path.suffix == ".log"
            assert path.parent == Path(tmpdir)
