# iter_034 — sim 58 random K=2 cierra + lanzar bridge K=2

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo:** seguir OBJ-028 (K=2).

## sim 58 random K=2 — RESULTADOS FINALES

| Métrica | Baseline | sim 52 (K=3 0.70) | sim 55 (K=3 0.50) | **sim 58 (K=2 0.70)** | R-017 |
|---|---|---|---|---|---|
| Saturated | 48/380 | 30/380 | 18/380 | 18/380 | — |
| **M1 CV** | 0.603 | 1.187 | 1.589 | **0.517** | **-14.2%** ❌ (req -30%) |
| M3 sum | 12917 | 12940 | 14690 | 14210 | +10.0% ✓ |
| M5 starv | 12.6% | 7.9% | 4.7% | **4.7%** | -62.5% ✓ |
| max rate | 99.9 | 469.8 | 767.6 | **151.3** | — |
| Loadtest | n/a | 2240/2240 | 2299/2299 | 2305/2305=100% | ✓ |

**Sorpresa muy positiva**: con K=2, el CV cae **BAJO el baseline** (0.517 < 0.603) — el algoritmo ya no degrada M1 (a -14.2% del baseline). Aún así no alcanza el umbral R-017 de -30%.

M3 está borderline (+10.0%, justo no cae). M5 cumple holgadamente (-62.5%). max rate cae drásticamente (151 vs 470/767 con K=3) — confirma que K=2 evita concentrar tráfico en commodities con paths disjuntos.

## Estado agregado OBJ-028 (K=2 + OVERLAP=0.70)

- **mesh3x3 (sim 57)**: CUMPLE ✓ (M1 -47%, M3 +143%, M5 -57%).
- **random (sim 58)**: NO CUMPLE — solo M1 falla (-14.2%, no -30%), pero M3 y M5 OK.

R-017 estricto requiere para CADA criterio:
- M1 ≥-30 % en ≥2/3: mesh ✓, random ❌ → necesita bridge ✓.
- M3 no caer >10 % en 3/3: mesh ✓, random ✓ → necesita bridge ✓.
- M5 ≥-50 % en ≥2/3: mesh ✓, random ✓ → 2/3 ya cumple.

**M1 es el cuello**. Si bridge cumple M1 ≥-30 % → 2/3 → R-017 global ✓.

## Lanzar bridge --cluster-n 4 --cluster-count 2 con K=2

```bash
python3 -m tests.cli.dkms_topo bridge --cluster-n 4 --cluster-count 2 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 142 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_034/post/cuello_bridge
```

PID **3103150**. Output `/tmp/iter034-bridge-sdnv83.out`. Offset 142 (node_ids 143-150, local_qkc_id 100143-100150 libres, max ocupado=100141).

ETA fin sim ~13 min.

### Hipótesis para bridge con K=2

En bridge K=3+OVERLAP=0.70 (sim 53): M1 +6.8%, M5 0%, M3 +0.1%. Algoritmo degenera a single-path porque sólo hay 1 enlace inter-cluster.

Con K=2: la única diferencia es que el SDN busca 2 paths en vez de 3. En bridge no hay paths alternativos para cross-cluster → comportamiento similar a sim 53 esperado (M1 ~0%, M5 ~0%).

**Predicción**: bridge K=2 probablemente NO cumple M1 → R-017 global falla → arrancar OBJ-029 con cap por commodity.

## Bloqueos

Ninguno. Próximo cron evalúa bridge.
