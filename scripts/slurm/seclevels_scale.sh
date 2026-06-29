#!/usr/bin/env bash
# Scale batch: push N large (70/100) + high PQC at scale. clarabel solver,
# bigger allocation + longer warmup. Same per-cell flow as seclevels_campaign.sh.
set -u
REPO="/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust"
cd "$REPO"; source "$LUSTRE/dkms-build/buildenv.sh" 2>/dev/null
OUT="$REPO/tests/results/seclevels-campaign"; mkdir -p "$OUT" "$LUSTRE/dkms-build/camp"
RES="$OUT/scale.tsv"
echo -e "cell\ttopo\tN\tpqc\tsolver\tedges_qkd\tedges_pqc\tqkd_comps\tlaunch\te2e_pass\te2e_total\tverdict" > "$RES"

# topo N degree pqc seed warmup cores mem
CELLS=(
  "er 70 4 0.5 71 170 32 48G"
  "er 100 4 0.5 72 220 48 64G"
  "er 50 4 0.8 73 130 32 48G"
  "ba 70 4 0.5 74 170 32 48G"
)
i=0
for cell in "${CELLS[@]}"; do
  set -- $cell; topo=$1; N=$2; deg=$3; pqc=$4; seed=$5; W=$6; C=$7; MEM=$8
  i=$((i+1)); name="s${i}_${topo}_n${N}_pqc${pqc}"
  D="$LUSTRE/dkms-build/camp/$name"; rm -rf "$D"
  echo "===== [$i/${#CELLS[@]}] $name (clarabel warmup=${W}s -c$C --mem=$MEM) ====="
  if ! python3 scripts/slurm/gen_deploy.py --topo "$topo" --n "$N" --degree "$deg" --seed "$seed" \
        --hosts 127.0.0.1 --pqc-fraction "$pqc" --security-level qkd_prefer \
        --out "$D" > "$D.gen.log" 2>&1; then
    echo -e "$name\t$topo\t$N\t$pqc\tclarabel\t?\t?\t?\tGEN_FAIL\t0\t0\tGEN_FAIL" >> "$RES"; continue
  fi
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
nodes={x for k in seen for x in k}; comp={}
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
  LOG="$D/run.log"
  timeout 900 srun -p short -c"$C" --mem="$MEM" -t 16 bash -lc "
    source \"\$LUSTRE/dkms-build/buildenv.sh\" 2>/dev/null; cd $REPO
    export SDN_SOLVER=clarabel SDN_DUAL_GRADE_TABLES=1
    python3 scripts/slurm/launch.py --plan $D/plan.json --logs $D/logs --timeout 180 || { echo LAUNCH_FAIL; exit 9; }
    echo READY; sleep $W
    python3 scripts/slurm/seclevels_e2e.py $D; rc=\$?
    python3 scripts/slurm/launch.py --logs $D/logs --stop >/dev/null 2>&1
    exit \$rc
  " > "$LOG" 2>&1
  rc=$?
  launch=OK; { [ "$rc" = "9" ] || grep -q LAUNCH_FAIL "$LOG"; } && launch=LAUNCH_FAIL
  ep_pass=$(grep -oE "=== [0-9]+/[0-9]+ passed ===" "$LOG" | grep -oE "[0-9]+/[0-9]+" | head -1)
  pp=${ep_pass%/*}; tt=${ep_pass#*/}; pp=${pp:-0}; tt=${tt:-0}
  verdict=FAIL
  [ "$launch" = OK ] && [ "$tt" -gt 0 ] && [ "$pp" = "$tt" ] && verdict=PASS
  [ "$rc" = 124 ] && verdict=TIMEOUT
  echo -e "$name\t$topo\t$N\t$pqc\tclarabel\t$EQ\t$EP\t$QC\t$launch\t$pp\t$tt\t$verdict" >> "$RES"
  echo "  -> $verdict (e2e $pp/$tt, launch=$launch, rc=$rc)"
done
echo "===== scale done ====="; column -t -s$'\t' "$RES"
