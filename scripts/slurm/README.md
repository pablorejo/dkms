# Binaries-only DKMS deployment on CESGA Slurm

Run the five Rust binaries (`sdn`, `qkc`, `orr`, `dkms`, `quditto`) directly on a
CESGA OpenHPC Slurm cluster — **no Python orchestrator, no database, no k8s**.
A topology graph (from `tests/cli/topology_builders.py`) is turned into a full
local deployment: per-entity SDN `topology_dir` JSON, per-binary configs, a PKI,
and a launch plan that comes up across one or many nodes, then a load ramp drives
ETSI-014 round-trips against it.

Model: **one "site" per graph node** = co-located `{dkms + orr + qkc}`; **one
quditto per edge** (shared by the two endpoint QKCs); the **SDN is a singleton**.

---

## TL;DR — deploy one test

```bash
# from the repo root, on a login node inside tmux
REPO=$PWD
LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
RUN=$LUSTRE/runs/er-n40

sbatch -J er-n40 -N 7 -c 64 --mem=180G -t 01:00:00 -p short \
    -o "$LUSTRE/runs/sbatch-er-n40-%j.out" \
    --export="ALL,REPO=$REPO,RUN=$RUN,\
GEN_ARGS=--topo er --n 40 --degree 4 --pairs 8000 --key-bits 256 --qd-alpha 0,\
SDN_SOLVER=clarabel,\
RT=1,RT_WORKERS=25,RT_LAMBDA=1,RT_START=10,RT_STEP=10,RT_INTERVAL=10,RT_HOLD=30,RT_POISSON=1,RT_SIZE=256,RT_WARMUP=5,\
WARMUP=30,SMOKE_RETRIES=10,SMOKE_GAP=30" \
    "$REPO/scripts/slurm/deploy.sbatch"
```

Results land in `tests/results/er-n40/` (plots, CSVs, `summary.json`, `ANALYSIS.md`).
**To change what you test, change only `GEN_ARGS` (`--topo`, `--n`, `--degree`),
`-N` (node count), and `SDN_SOLVER`.** Everything else is the fixed recipe that
makes cells comparable.

`launch_clarabel_er40.sh` is exactly this command wrapped in a script — copy it
as a template for one-off cells.

---

## Build (once)

The toolchain on the box is too old and `$HOME` has a small inode quota, so the
build env lives on `$LUSTRE`:

```bash
source "$LUSTRE/dkms-build/buildenv.sh"   # gcc/12.3.0 + rust/1.88.0 + PROTOC + CARGO_HOME on $LUSTRE
cargo fetch                                # on a login node (has internet)
srun -p short -c32 --mem=32G bash -lc '
  source "$LUSTRE/dkms-build/buildenv.sh"; cargo build --release --offline'
```

Gotchas baked into `buildenv.sh`: default `gcc 10.1` has the memcmp bug
`aws-lc-sys` rejects (→ `gcc/12.3.0`); system `protoc 3.14` is too old for the
`proto3 optional` in `sdn.proto` (→ downloaded `protoc 25.x`); the cargo registry
blows the `$HOME` 100k-inode quota (→ `CARGO_HOME` on `$LUSTRE`). Binaries land in
`$LUSTRE/dkms-build/target/release/`, which is the deploy default — rebuild there
and every deployment picks up the new binaries automatically.

---

## The deploy pipeline (what one cell does)

The `sbatch` above launches `deploy.sbatch`, which on the allocation:

1. discovers the real node IPs and exports the SDN env (`SDN_SOLVER`,
   `SDN_DISABLE_LEX_REFINEMENT=1`, `TOKIO_WORKER_THREADS`, fd limits);
2. `srun`s **one `node_agent.sh` per node**, coordinated by barrier files on `$LUSTRE`.

Inside, each cell runs this chain:

| Stage | Script | What it does |
|---|---|---|
| generate | `gen_deploy.py` | graph (`topology_builders.py`) → `topology_dir` JSON + per-binary TOML + ports + **PKI** (CA, per-DKMS/SAE certs) + `plan.json` + round-trip pairs |
| launch | `launch.py` | phased bring-up with readiness gates: **0 SDN → 1 quditto+QKC → 2 ORR → 3 DKMS** |
| smoke-gate | `smoke.py` | 50 ETSI-014 enc→dec→match round-trips; **if it fails, the ramp is skipped** (a STALL is itself the datum) |
| load | `roundtrip.py` | the SAE ramp 500→16000 (Poisson λ=1) → `requests.csv` |
| collect | `collect.py` | copy key artifacts to `tests/results/<cell>/`, compress logs/configs (inode budget) |
| analyze | `analyze_run.py` + `plot_run.py` | CSVs + 9 PNG plots |
| aggregate | `summarize.py` | refresh `tests/results/SUMMARY.md` + `summary.csv` |

### The knobs you set via `--export`

| Var | Meaning |
|---|---|
| `GEN_ARGS` | **topology selector**: `--topo {ba\|er\|ba2\|rgg\|secoqc\|star\|line\|ring\|mesh} --n <N> --degree <d> --pairs 8000 --key-bits 256 --qd-alpha 0` |
| `SDN_SOLVER` | LP backend: `microlp` (default, exact simplex — stalls at N≥40 dense) or `clarabel` (interior-point, scales to N=70 dense / N=100 tree) |
| `-N <nodes>` | physical Slurm nodes. Rule of thumb: `ceil(N/6.7) + 1` (one dedicated RT client node). N=40→7, N=60→10, N=80→13, N=100→16 |
| `RT=1, RT_*` | ramp params (per-worker): `RT_WORKERS=25 RT_LAMBDA=1 RT_START=10 RT_STEP=10 RT_INTERVAL=10 RT_HOLD=30 RT_POISSON=1` ⇒ 500→16000 SAEs, +500/10s |
| `SMOKE_RETRIES`/`SMOKE_GAP` | smoke-gate window. `10×30s` (5 min) for N≤60; **`30×60s` (30 min) for N≥70** — the first dense solve can exceed 5 min |
| `WARMUP` | seconds to let buffers fill before smoke |

`--degree 2` ⇒ BA is a scale-free **tree** (m=1, E=N−1). `--degree 4` ⇒ dense
(`er`/`rgg`/`secoqc`/`ba2`, E≈2N). `--qd-alpha 0` ⇒ uniform edge cap = R0 = 2000.

---

## Running a whole campaign (many cells)

Don't call `sbatch` by hand for a sweep — use a **driver**. Three exist, one per
campaign; copy one as a template:

| Driver | Cells |
|---|---|
| `matrix_campaign.sh` | microlp baseline: 5 topos × N{20,30,40,50,60} = 25 cells |
| `clarabel_campaign.sh` | the 14 N≥40 cells that STALLed on microlp, re-run with Clarabel |
| `big_campaign.sh` | large-topology probe: er-n70, ba-n80, er-n80, ba-n100, rgg-n70 |

A driver loops over a `cells=("topo deg N nodes" ...)` array doing `sbatch --wait`
per cell. It is:

- **idempotent** — skips any cell with a `tests/results/<cell>/DONE` sentinel, so
  re-running resumes where it left off;
- **self-cleaning** — `rm -rf` each `$LUSTRE` run dir after collection (~32k inodes
  per cell, quota ~250k);
- with an outer **retry loop** for infrastructure failures (no `smoke_rc` file ⇒
  node death / gen crash ⇒ retried; a genuine smoke STALL is a valid result, stamped DONE).

**Launch it detached or it dies with your session** (the driver blocks on
`sbatch --wait` for hours):

```bash
LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
setsid nohup bash scripts/slurm/big_campaign.sh \
    > $LUSTRE/runs/big-campaign/driver-nohup.log 2>&1 < /dev/null &
# verify it reparented to init (survives SSH/tmux close):
ps -o pid,ppid -p $(cat $LUSTRE/runs/big-campaign/driver.pid)
```

Monitor: `tail -f $LUSTRE/runs/<campaign>/progress.log`, `squeue -u $USER`,
`ls -d tests/results/<prefix>-*/DONE | wc -l`. The driver prints
`=== ... FINISHED: N/N cells DONE ===` when done.

---

## Files

| File | Role |
|------|------|
| `gen_deploy.py` | graph → `topology_dir` + per-binary TOML/config + PKI + `plan.json` + RT pairs |
| `launch.py` | phased bring-up / `--stop`; `--only-host`/`--phase` for multinode |
| `node_agent.sh` | per-node agent, cross-node barriers on `$LUSTRE` |
| `deploy.sbatch` | discover node IPs → export SDN env → regen plan → `srun` one agent/node → collect |
| `run_local.sh` | single-node loopback bring-up + smoke (quick local check) |
| `smoke.py` | mTLS ETSI-014 round-trip gate (50 pairs) |
| `roundtrip.py` / `load.py` | SAE ramp / sustained-load drivers → CSV |
| `collect.py` | copy results to `tests/results/<cell>/` + compress logs/configs |
| `analyze_run.py` / `analyze_sae_ramp.py` / `plot_run.py` | CSVs + PNG plots (needs **system** python3 + matplotlib) |
| `summarize.py` / `matrix_table.py` / `clarabel_table.py` / `compare_rt.py` | cross-run aggregation tables |
| `*_campaign.sh` | sequential, idempotent campaign drivers |

---

## Results layout

Live runs execute under `$LUSTRE/runs/<cell>/` (deleted after collection). The kept
results are in the gitignored `tests/results/<cell>/`:

- **plots/** — 9 PNGs: `match_vs_429`, `buffer_fill`, `match_over_time`,
  `rps_over_time`, `latency_hist`, `latency_split_time`, `errors_over_time`,
  `outcome_breakdown`, `keys_per_dkms`;
- **CSVs**: `buffer_fill.csv`, `*_timeseries.csv`; `summary.json` (machine-readable);
- `ANALYSIS.md`, `smoke.txt`, `plan.json`, `nodes/hosts.txt`;
- `logs.tar.gz` + `configs.tar.gz` (one inode each).

Campaign-level synthesis:
- `tests/results/SUMMARY.md` / `summary.csv` — every run (refreshed by `summarize.py`);
- `tests/results/matrix-n20-60/{MATRIX.md,matrix.csv}` — the microlp 5×5 baseline;
- `tests/results/clarabel-n40-60/{COMPARISON.md,ANALYSIS.md}` — microlp-vs-Clarabel;
- `tests/results/big-n70-100/ANALYSIS.md` — the N=70..100 probe.

The pre-Slurm **reference campaign** (orchestrator + EKS deployment, N=20, 5
topologies) lives in `resultados_definitivos_n_20/<topo>/plots/` — the "antes"
baseline these binaries-only Slurm runs were reproducing.

---

## Key findings (full story)

- **Data plane scales to N=100+ with zero infra errors**: DKMS↔DKMS ETSI-020 mTLS,
  QKC multi-hop routing, ORR onion + O(N²) ML-KEM bootstrap (9900 handshakes at
  N=100), per-edge quditto. Never the bottleneck.
- **The bottleneck is the SDN MCMCF-λ rate LP** (`N(N-1)` commodities × `2E` arcs):
  - `microlp` (simplex): STALLs at N≥40 on dense topologies (no solve completes).
  - `clarabel` (interior-point, `SDN_SOLVER=clarabel`): solves dense to **N=70**
    (~430 s, rgg-n70 ~880 s), tree to **N=100** (~280 s). Ceiling between N=70/80 dense.
  - Under Clarabel, phase-2 lex-refinement is forced off (interior-point puts
    circulation flow on slack cycles → would corrupt WCMP tables); forwarding falls
    back to shortest-path, same as production's `SDN_DISABLE_LEX_REFINEMENT=1`.
- **Debouncer guard** (`sdn/src/debounce.rs`): at most one MCF solve in flight;
  overlapping triggers coalesce to one re-run. Without it, multi-minute solves stack
  concurrently and starve the SDN node (the er-n60 404 cascade).
- **Per-allocation cert wipe** (`deploy.sbatch` clears `$RUN/tls`): DKMS peer mTLS
  validates the server cert by node IP, so SANs must match the current allocation.
- **Saturation is topology physics, not a fault**: with uniform edge cap (R0=2000)
  and fixed 16000-SAE demand, sparse/hub topologies (BA tree) funnel through a small
  min-cut → clean `429` backpressure that worsens with N (96%→69% match N=50→100);
  dense topologies (er/rgg/secoqc) have a high min-cut → 100% match, 0×429.

**Max N supported across ALL five topologies: 60** (the complete 5×5 matrix). Higher
works per-topology (er→70, ba-tree→100) but the dense-solver ceiling (~N=80) is the
limit for "every topology".
