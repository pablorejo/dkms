# iter_028 — OBJ-027 CIERRE (NO cumple R-017) + OBJ-028 INICIADO (K_PATHS=2)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivos:** cerrar OBJ-027 + arrancar OBJ-028.

## OBJ-027 CERRADO — NO cumple R-017

Tras 2 sims con `DEFAULT_OVERLAP_THRESHOLD=0.50`:

### Mesh 3x3 (sim 54)
| Métrica | Baseline | sim 54 (0.50) | Delta | Cumple |
|---|---|---|---|---|
| M1 CV | 1.192 | 0.846 | -29.0% | ❌ (req -30%) |
| M3 sum | 2382 | 5362 | +125% | ✓ |
| M5 starv | 58.3% | 34.7% | -40.5% | ❌ (req -50%) |

### Random n=20 d=3 (sim 55)
| Métrica | Baseline | sim 55 (0.50) | Delta | Cumple |
|---|---|---|---|---|
| M1 CV | 0.603 | **1.589** | +163.5% | ❌ (PEOR aún que sim 52 con 0.70) |
| M3 sum | 12917 | 14690 | +13.7% | ✓ |
| M5 starv | 12.6% | **4.7%** | -62.5% | ✓ |

### Agregado parcial OBJ-027 (mesh + random)

- M1 cumple: 0/2 → con bridge cumple sería 1/3 (req ≥2/3) → **FALLA garantizado**.
- M3 cumple: 2/2 → con bridge → 3/3 ✓ posible.
- M5 cumple: 1/2 (mesh ❌, random ✓) → con bridge ✓ → 2/3 ✓ posible.

**Como M1 ya falla 2/3 garantizado**, NO vale lanzar bridge: OBJ-027 NO PUEDE cumplir R-017 globalmente. Ahorro EKS al saltar sim bridge.

### Hallazgo principal

Threshold 0.50 muestra **trade-off**: en mesh densa degrada, en random sparse mejora M5 dramáticamente pero **empeora aún más M1** (max rate sube de 470 a 767 kps por commodity rico). 0.50 hace que el SDN apile rate en menos commodities con paths realmente disjuntos.

## OBJ-028 INICIADO

### Cambios código

`sdn/src/mcf.rs`:
- **Revertido**: `DEFAULT_OVERLAP_THRESHOLD: 0.50 → 0.70` (test renombrado de vuelta, con comment del intento OBJ-027).
- **Nuevo cambio**: `McfSolver::default()` `k_paths: 3 → 2`.

**Hipótesis**: menos paths por commodity reduce competencia interna por capacidad → mejora M1 en sparse donde con K=3 el SDN apilaba rate en commodities con paths realmente disjuntos. mesh densa probablemente sigue funcionando bien con K=2 porque hay paths cortos suficientes.

### Verificación pre-push R-014

- cargo test sdn 71 passed ✓
- clippy sdn verde ✓
- docker build `pablopio/sdn:v8.3` 138MB ✓
- tag nuevo, no sobrescribe :v8 / :v8.2 ✓
- rollback intacto ✓

### Push autónomo

```
docker push pablopio/sdn:v8.3
digest sha256:ab0c55f748ed2348cb0ef624048b46df87fa1aac30573692cb5d5cb5b5dd1c0c
```

### Deploy

```
kubectl -n dkms-main-ns set env deploy/orchestator SDN_IMAGE=pablopio/sdn:v8.3
```

Rollout OK. Env actual: `SDN_IMAGE=pablopio/sdn:v8.3`, `ORR_IMAGE=pablopio/orr:v8.1`, `QKC_IMAGE=pablopio/qkc:v8`, `ORR_MULTIPATH_ENABLED=true`.

Port-forwards relanzados: orch (PID 3053280), authz (3053281).

### Sim mesh 3x3 LANZADA

```bash
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 40 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_028/post/pequena_densa
```

PID **3053751**. Output `/tmp/iter028-mesh3x3-sdnv83.out`. Offset 40 (node_ids 41-49, local_qkc_id 100041-100049 libres por huecos BD pre-existentes).

ETA fin sim ~17 min.

## Plan

1. Próximo cron: evaluar mesh 3x3 sim ~56 con K=2.
2. Si cumple R-017 → lanzar random.
3. Si random cumple → lanzar bridge → 3/3 ✓ → Fase G.
4. Si mesh O random falla → seguir a OBJ-029 con cap por commodity.

## Bloqueos

Ninguno. Próximo cron evalúa sim mesh 3x3 nueva.
