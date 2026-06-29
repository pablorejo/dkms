#!/usr/bin/env bash
# Large-topology security-level test campaign.
# Per cell: gen static-SAE deployment, srun {launch + warmup + seclevels_e2e + stop},
# record PASS/FAIL. Tolerant of per-cell failure; preserves logs of failing cells.
set -u
REPO="/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust"
cd "$REPO"
source "$LUSTRE/dkms-build/buildenv.sh" 2>/dev/null
OUT="$REPO/tests/results/seclevels-campaign"
mkdir -p "$OUT"
mkdir -p "$LUSTRE/dkms-build/camp"
RES="$OUT/campaign.tsv"
echo -e "cell\ttopo\tN\tpqc\tdual\tsolver\tedges_qkd\tedges_pqc\tqkd_comps\tlaunch\te2e_pass\te2e_total\tverdict" > "$RES"

warmup_for() { local n=$1; if [ "$n" -le 12 ]; then echo 45; elif [ "$n" -le 22 ]; then echo 65; elif [ "$n" -le 35 ]; then echo 85; else echo 115; fi; }

# cells: topo N degree pqc dualgrade(0/1) seed
CELLS=(
  "er 20 4 0.3 1 11"
  "er 20 4 0.5 1 11"
  "er 20 4 0.7 1 11"
  "ba 20 4 0.3 1 12"
  "ba 20 4 0.5 1 12"
  "ba 20 4 0.7 1 12"
  "rgg 20 4 0.3 1 13"
  "rgg 20 4 0.5 1 13"
  "rgg 20 4 0.7 1 13"
  "er 20 4 0.0 1 14"
  "er 20 4 1.0 1 14"
  "er 20 4 0.5 0 11"
  "er 30 4 0.5 1 21"
  "ba 30 4 0.5 1 22"
  "rgg 30 4 0.5 1 23"
  "er 50 4 0.5 1 31"
)

i=0
for cell in "${CELLS[@]}"; do
  set -- $cell; topo=$1; N=$2; deg=$3; pqc=$4; dual=$5; seed=$6
  i=$((i+1)); name="c${i}_${topo}_n${N}_pqc${pqc}_d${dual}"
  D="$LUSTRE/dkms-build/camp/$name"; rm -rf "$D"
  solver=microlp; [ "$N" -ge 30 ] && solver=clarabel
  W=$(warmup_for "$N")
  echo "===== [$i/${#CELLS[@]}] $name (solver=$solver warmup=${W}s) ====="
  # 1) generate (static SAE: no --pairs)
  if ! python3 scripts/slurm/gen_deploy.py --topo "$topo" --n "$N" --degree "$deg" --seed "$seed" \
        --hosts 127.0.0.1 --pqc-fraction "$pqc" --security-level qkd_prefer \
        --out "$D" > "$D.gen.log" 2>&1; then
    echo -e "$name\t$topo\t$N\t$pqc\t$dual\t$solver\t?\t?\t?\tGEN_FAIL\t0\t0\tGEN_FAIL" >> "$RES"; continue
  fi
  # topology stats
  read EQ EP QC < <(python3 - "$D" <<'PY'
import re,glob,os,sys
d=sys.argv[1]; adj={}; eq=ep=0; seen=set()
for f in glob.glob(os.path.join(d,"sites","site-*","qkc.toml")):
    me=re.search(r"site-(\d+)",f).group(1); cur=None;pqc=False
    def fl():
        global eq,ep
        if cur is None: return
        k=tuple(sorted((me,cur)))
        if k in seen: return
        seen.add(k)
        if pqc: ep+=1
        else: eq+=1; adj.setdefault(me,set()).add(cur); adj.setdefault(cur,set()).add(me)
    for ln in open(f):
        m=re.match(r"\s*neighbor_id\s*=\s*(\d+)",ln)
        if m: fl(); cur=m.group(1); pqc=False
        elif 'link_type = "pqc"' in ln: pqc=True
    fl()
nodes={x for k in seen for x in k}
comp={}
for n in nodes:
    if n in comp: continue
    st=[n]; comp[n]=n
    while st:
        x=st.pop()
        for y in adj.get(x,()):
            if y not in comp: comp[y]=n; st.append(y)
print(eq,ep,len(set(comp.values())) if nodes else 0)
PY
)
  # 2) run on a compute node
  LOG="$D/run.log"
  timeout 700 srun -p short -c16 --mem=16G -t 14 bash -lc "
    source \"\$LUSTRE/dkms-build/buildenv.sh\" 2>/dev/null
    cd $REPO
    export SDN_SOLVER=$solver
    $([ "$dual" = "1" ] && echo 'export SDN_DUAL_GRADE_TABLES=1')
    python3 scripts/slurm/launch.py --plan $D/plan.json --logs $D/logs --timeout 120 || { echo LAUNCH_FAIL; exit 9; }
    echo READY; sleep $W
    python3 scripts/slurm/seclevels_e2e.py $D; rc=\$?
    python3 scripts/slurm/launch.py --logs $D/logs --stop >/dev/null 2>&1
    exit \$rc
  " > "$LOG" 2>&1
  rc=$?
  launch=OK; [ "$rc" = "9" ] && launch=LAUNCH_FAIL
  grep -q "LAUNCH_FAIL" "$LOG" && launch=LAUNCH_FAIL
  ep_pass=$(grep -oE "=== [0-9]+/[0-9]+ passed ===" "$LOG" | grep -oE "[0-9]+/[0-9]+" | head -1)
  pp=${ep_pass%/*}; tt=${ep_pass#*/}; pp=${pp:-0}; tt=${tt:-0}
  verdict=FAIL
  [ "$launch" = "OK" ] && [ "$tt" -gt 0 ] && [ "$pp" = "$tt" ] && verdict=PASS
  [ "$rc" = "124" ] && verdict=TIMEOUT
  echo -e "$name\t$topo\t$N\t$pqc\t$dual\t$solver\t$EQ\t$EP\t$QC\t$launch\t$pp\t$tt\t$verdict" >> "$RES"
  echo "  -> $verdict (e2e $pp/$tt, launch=$launch, rc=$rc)"
done
echo "===== campaign done. results: $RES ====="
column -t -s$'\t' "$RES"
