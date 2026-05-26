"""Agrega los CSVs de los N workers en un solo `requests.csv` global +
summary.json + plots.

Uso: python3 aggregate_workers.py <distributed_run_dir>
"""
from __future__ import annotations
import argparse, csv, json, pathlib, statistics, re, collections


def main(run_dir: pathlib.Path):
    workers = sorted(run_dir.glob("worker-*"))
    print(f"Found {len(workers)} workers")

    # Concatenar requests.csv de todos los workers, añadiendo worker_id.
    out_csv = run_dir / "requests.csv"
    header = None
    all_rows = []
    for wd in workers:
        csv_p = wd / "requests.csv"
        if not csv_p.exists():
            print(f"  WARN: {wd.name} sin requests.csv")
            continue
        wid = int(re.search(r"worker-(\d+)", wd.name).group(1))
        with csv_p.open() as fp:
            r = csv.reader(fp)
            h = next(r)
            if header is None:
                header = ["worker_id"] + h
            for row in r:
                all_rows.append([wid] + row)

    print(f"Total rows: {len(all_rows)}")
    with out_csv.open("w") as fp:
        w = csv.writer(fp)
        w.writerow(header)
        w.writerows(all_rows)
    print(f"wrote {out_csv}")

    # Summary.
    cols = {c: i for i, c in enumerate(header)}
    n = len(all_rows)
    enc_ok = sum(1 for r in all_rows if r[cols["ok_enc"]] == "1")
    dec_ok = sum(1 for r in all_rows if r[cols["ok_dec"]] == "1")
    match = sum(1 for r in all_rows if r[cols["match"]] == "1")
    enc_lats = sorted(float(r[cols["enc_ms"]]) for r in all_rows if r[cols["ok_enc"]] == "1")
    dec_lats = sorted(float(r[cols["dec_ms"]]) for r in all_rows if r[cols["ok_dec"]] == "1")

    def pcts(lats):
        if not lats: return {}
        return {"p50": round(lats[len(lats) // 2], 2),
                "p95": round(lats[int(len(lats) * 0.95)], 2),
                "p99": round(lats[int(len(lats) * 0.99)], 2),
                "max": round(lats[-1], 2)}

    # HTTP codes
    http_counter = collections.Counter()
    client_counter = collections.Counter()
    for r in all_rows:
        if r[cols["ok_enc"]] == "1" and r[cols["ok_dec"]] == "1":
            continue
        err = r[cols["err"]]
        m = re.match(r"(enc|dec) HTTP (\d+)", err)
        if m:
            http_counter[(m.group(1), m.group(2))] += 1
        elif "Cannot connect" in err:
            client_counter["Cannot connect"] += 1
        elif err.strip() in ("enc exc:", "dec exc:", ""):
            client_counter["empty exc"] += 1
        elif "Timeout" in err or "timed out" in err:
            client_counter["timeout"] += 1
        else:
            client_counter[err[:60]] += 1

    summary = {
        "n_workers": len(workers),
        "total_requests": n,
        "enc_ok": enc_ok, "dec_ok": dec_ok, "match": match,
        "enc_ok_pct": round(100 * enc_ok / n, 2) if n else 0,
        "dec_ok_pct": round(100 * dec_ok / n, 2) if n else 0,
        "match_pct": round(100 * match / n, 2) if n else 0,
        "enc_latency_ms": pcts(enc_lats),
        "dec_latency_ms": pcts(dec_lats),
        "http_errors_from_server": {f"{op} HTTP {code}": cnt for (op, code), cnt in http_counter.items()},
        "client_errors": dict(client_counter),
    }
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    args = p.parse_args()
    main(pathlib.Path(args.run_dir))
