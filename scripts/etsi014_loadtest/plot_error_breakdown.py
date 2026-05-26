"""Categoriza outcomes del roundtrip CSV y genera 2 plots:

  - outcome_breakdown.png   pie + barra apilada con el desglose
  - errors_over_time.png    serie temporal de fallos por categoría
"""
from __future__ import annotations

import argparse
import collections
import csv
import pathlib
import re

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def categorize(r: dict) -> str:
    err = r["err"]
    if r["ok_enc"] == "1" and r["ok_dec"] == "1":
        return "OK round-trip"
    if r["ok_enc"] != "1":
        m = re.match(r"enc HTTP (\d+)(?::|$)", err)
        if m:
            return f"enc HTTP {m.group(1)}"
        if "Cannot connect" in err:
            return "enc connect fail (client)"
        if "Timeout" in err or "timed out" in err:
            return "enc timeout"
        return "enc other"
    # enc ok, dec failed
    m = re.match(r"dec HTTP (\d+)(?::|$)", err)
    if m:
        return f"dec HTTP {m.group(1)}"
    if "Cannot connect" in err:
        return "dec connect fail (client)"
    if "Timeout" in err or "timed out" in err:
        return "dec timeout"
    return "dec other"


def main(run_dir: pathlib.Path) -> None:
    rows = list(csv.DictReader((run_dir / "requests.csv").open()))
    plots = run_dir / "plots"
    plots.mkdir(parents=True, exist_ok=True)

    cats = collections.Counter()
    rows_cat = []
    for r in rows:
        c = categorize(r)
        cats[c] += 1
        rows_cat.append((float(r["t_emit"]), c))

    total = sum(cats.values())
    print(f"total: {total}")
    for c, n in cats.most_common():
        print(f"  {c:30s} {n:>7d} ({100*n/total:.2f}%)")

    # 1. Outcome breakdown: pie (con OK gigante) + bar de fallos sin OK.
    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(15, 6))

    ordered = cats.most_common()
    labels = [c for c, _ in ordered]
    counts = [n for _, n in ordered]
    colors = ["seagreen" if c.startswith("OK") else
              ("steelblue" if "HTTP" in c else
               ("orange" if "connect" in c else
                ("firebrick" if "timeout" in c else "grey")))
              for c in labels]
    explode = [0.0] + [0.05] * (len(labels) - 1)
    ax1.pie(counts, labels=labels, autopct="%1.2f%%", colors=colors,
            explode=explode, startangle=90, textprops={"fontsize": 9})
    ax1.set_title(f"Outcome breakdown (n={total})")

    # Bar — solo fallos (sin OK).
    fail_labels, fail_counts, fail_colors = [], [], []
    for c, n in ordered:
        if c.startswith("OK"):
            continue
        fail_labels.append(c)
        fail_counts.append(n)
        fail_colors.append(
            "steelblue" if "HTTP" in c else
            ("orange" if "connect" in c else
             ("firebrick" if "timeout" in c else "grey"))
        )
    if fail_counts:
        ax2.bar(range(len(fail_labels)), fail_counts, color=fail_colors)
        ax2.set_xticks(range(len(fail_labels)))
        ax2.set_xticklabels(fail_labels, rotation=30, ha="right")
        ax2.set_ylabel("count")
        ax2.set_title("Failed requests only — by category")
        ax2.grid(True, alpha=0.3, axis="y")
        for i, v in enumerate(fail_counts):
            ax2.text(i, v, f"{v}\n({100*v/total:.2f}%)",
                     ha="center", va="bottom", fontsize=9)
    else:
        ax2.text(0.5, 0.5, "Zero failures", ha="center", va="center",
                 fontsize=20, transform=ax2.transAxes)
        ax2.set_axis_off()

    fig.suptitle(f"ETSI 014 round-trip outcomes (no 4xx/5xx from server)", fontsize=13)
    fig.tight_layout()
    fig.savefig(plots / "outcome_breakdown.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'outcome_breakdown.png'}")

    # 2. Errors over time stacked area.
    t = np.array([rc[0] for rc in rows_cat])
    t0 = t.min()
    t_rel = t - t0
    duration = float(t_rel.max()) + 1
    window = 10.0
    bins = np.arange(0, duration + 1, window)
    centers = (bins[:-1] + bins[1:]) / 2

    fail_cats = [c for c, _ in cats.most_common() if not c.startswith("OK")]
    series = {c: np.zeros(len(centers)) for c in fail_cats}
    for ts, cat in rows_cat:
        if cat.startswith("OK"):
            continue
        i = min(int((ts - t0) / window), len(centers) - 1)
        series[cat][i] += 1

    fig, ax = plt.subplots(figsize=(13, 5))
    bottom = np.zeros(len(centers))
    for c in fail_cats:
        color = ("steelblue" if "HTTP" in c else
                 ("orange" if "connect" in c else
                  ("firebrick" if "timeout" in c else "grey")))
        ax.fill_between(centers, bottom, bottom + series[c], label=c,
                        color=color, alpha=0.8)
        bottom += series[c]
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("failures / 10s bin")
    ax.set_title("Failures over time (stacked) — every spike is client-side")
    ax.grid(True, alpha=0.3)
    ax.legend(loc="upper left")
    fig.tight_layout()
    fig.savefig(plots / "errors_over_time.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'errors_over_time.png'}")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    main(pathlib.Path(p.parse_args().run_dir))
