"""Unit tests for ``tests/cli/prom_parser``."""

from __future__ import annotations

import math

from tests.cli.prom_parser import (
    filter_counter,
    parse_metrics,
    sum_counter,
)


SAMPLE_LOADTEST = """\
# HELP loadtest_requests_total Total HTTP requests issued
# TYPE loadtest_requests_total counter
loadtest_requests_total{status_code="200",result="success"} 100
loadtest_requests_total{status_code="429",result="throttled"} 5
loadtest_requests_total{status_code="500",result="server_error"} 1

# HELP loadtest_request_duration_seconds Request duration histogram
# TYPE loadtest_request_duration_seconds histogram
loadtest_request_duration_seconds_bucket{le="0.005"} 10
loadtest_request_duration_seconds_bucket{le="0.01"} 50
loadtest_request_duration_seconds_bucket{le="0.025"} 80
loadtest_request_duration_seconds_bucket{le="0.05"} 95
loadtest_request_duration_seconds_bucket{le="+Inf"} 106
loadtest_request_duration_seconds_count 106
loadtest_request_duration_seconds_sum 1.234
"""


def test_parses_counter_entries() -> None:
    r = parse_metrics(SAMPLE_LOADTEST)
    entries = r["counters"]["loadtest_requests_total"]
    assert len(entries) == 3
    labels = {tuple(sorted(e["labels"].items())): e["value"] for e in entries}
    assert labels[(("result", "success"), ("status_code", "200"))] == 100.0
    assert labels[(("result", "throttled"), ("status_code", "429"))] == 5.0


def test_parses_histogram_buckets() -> None:
    r = parse_metrics(SAMPLE_LOADTEST)
    h = r["histograms"]["loadtest_request_duration_seconds"]
    bucket_keys = [le for le, _ in h["buckets"]]
    bucket_vals = [v for _, v in h["buckets"]]
    assert bucket_keys == [0.005, 0.01, 0.025, 0.05, "+Inf"]
    assert bucket_vals == [10, 50, 80, 95, 106]
    assert h["count"] == 106
    assert h["sum"] == 1.234


def test_sum_counter_aggregates_all_labels() -> None:
    r = parse_metrics(SAMPLE_LOADTEST)
    total = sum_counter(r["counters"], "loadtest_requests_total")
    assert total == 106.0


def test_filter_counter_by_label_subset() -> None:
    r = parse_metrics(SAMPLE_LOADTEST)
    ok = filter_counter(r["counters"], "loadtest_requests_total", status_code="200")
    throttled = filter_counter(
        r["counters"], "loadtest_requests_total", status_code="429"
    )
    assert ok == 100.0
    assert throttled == 5.0


def test_skips_help_and_type_comments() -> None:
    text = """\
# HELP my_metric just a help
# TYPE my_metric counter
my_metric{x="1"} 42.0
"""
    r = parse_metrics(text)
    assert r["unknown"]["my_metric"][0]["value"] == 42.0


def test_skips_blank_lines_and_invalid() -> None:
    text = "\n\n  \nnot a valid line at all\nfoo_total{a=\"b\"} 1\n"
    r = parse_metrics(text)
    assert r["counters"]["foo_total"][0]["value"] == 1.0


def test_parses_special_values() -> None:
    text = (
        "weird_metric{kind=\"nan\"} NaN\n"
        "weird_metric{kind=\"inf\"} +Inf\n"
        "weird_metric{kind=\"ninf\"} -Inf\n"
    )
    r = parse_metrics(text)
    by_kind = {e["labels"]["kind"]: e["value"] for e in r["unknown"]["weird_metric"]}
    assert math.isnan(by_kind["nan"])
    assert math.isinf(by_kind["inf"]) and by_kind["inf"] > 0
    assert math.isinf(by_kind["ninf"]) and by_kind["ninf"] < 0


def test_histogram_buckets_sorted_by_le() -> None:
    text = """\
foo_bucket{le="1.0"} 50
foo_bucket{le="0.1"} 10
foo_bucket{le="+Inf"} 100
foo_bucket{le="0.5"} 30
foo_count 100
foo_sum 0.987
"""
    r = parse_metrics(text)
    keys = [le for le, _ in r["histograms"]["foo"]["buckets"]]
    assert keys == [0.1, 0.5, 1.0, "+Inf"]


def test_count_without_buckets_is_not_treated_as_histogram() -> None:
    text = 'isolated_count 7\n'
    r = parse_metrics(text)
    # No buckets registered, so 'isolated_count' lands in unknown
    assert "isolated" not in r["histograms"]


def test_empty_input_yields_empty_dicts() -> None:
    r = parse_metrics("")
    assert r["counters"] == {}
    assert r["histograms"] == {}
    assert r["unknown"] == {}


def test_labels_with_escaped_quote() -> None:
    text = 'my_total{path="/a\\"b"} 9\n'
    r = parse_metrics(text)
    e = r["counters"]["my_total"][0]
    assert e["labels"]["path"] == '/a"b'
    assert e["value"] == 9.0


def test_metric_without_labels() -> None:
    text = "no_labels_total 17\n"
    r = parse_metrics(text)
    e = r["counters"]["no_labels_total"][0]
    assert e["labels"] == {}
    assert e["value"] == 17.0
