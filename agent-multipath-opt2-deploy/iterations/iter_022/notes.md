# iter_022 — OBJ-027 (auto-tune iter 1): OVERLAP_THRESHOLD 0.70→0.50, build+push+deploy+lanzar sim mesh3x3

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197` tras desbloqueo R-016.
**Objetivo:** OBJ-027 (Fase F-bis, auto-tune iter 1).

## Cambio aplicado

`sdn/src/mcf.rs:197`: `DEFAULT_OVERLAP_THRESHOLD: 0.70 → 0.50`.
Test `filter_default_threshold_constant_is_0_70` renombrado a `..._0_50` y comentado con el porqué.

**Hipótesis**: en random/bridge sparse, el filtro 0.70 era demasiado permisivo — paths con 70 % de edges compartidos competían por la misma capacidad, lo que explosionaba la varianza de rate (M1). Bajar a 0.50 exige paths más disjuntos → menos competencia interna por commodity → mejor M1.

## Verificación pre-push (R-014)

- `cargo test -p sdn --release --lib`: **71 passed** ✓ (incluye test renombrado).
- `cargo clippy -p sdn --release --all-targets -- -D warnings`: verde ✓.
- `docker build -t pablopio/sdn:v8.2 -f sdn/Dockerfile .`: OK, 138 MB ✓.
- Tag nuevo `:v8.2`, no sobrescribe `:v8` (R-014/R-010) ✓.
- Rollback path intacto (`:v7`, `:v8` siguen en registry) ✓.

## Push autónomo

`docker push pablopio/sdn:v8.2`:
- exit=0
- digest: `sha256:4ba19f23348295ef7aa4b1fdb5fd7e6bb085e8fa0e59310fe86849d30e2819f5`
- size: 856

Log en `iter_022/logs/docker_push.log`.

## Redeploy EKS

```bash
kubectl -n dkms-main-ns set env deploy/orchestator SDN_IMAGE=pablopio/sdn:v8.2
kubectl -n dkms-main-ns rollout status deploy/orchestator
```

Rollout OK. Estado deploy:
```
SDN_IMAGE=pablopio/sdn:v8.2
ORR_IMAGE=pablopio/orr:v8.1
QKC_IMAGE=pablopio/qkc:v8
ORR_MULTIPATH_ENABLED=true
```

Port-forward orch relanzado tras rotación pod (HTTP 200 verificado).

## Sim mesh 3x3 lanzada

```bash
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 60 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_022/post/pequena_densa
```

- PID **3001586** (nohup, disowned).
- Output `/tmp/iter022-mesh3x3-sdnv82.out`.
- node-id-offset 60 → node_ids 61-69 → local_qkc_id 100061-100069 (libre confirmado BD; max ocupado 100151).
- Sim previa `mesh-3x3` (sim 51) borrada con `--force`.
- ETA fin ~00:55Z (15-17 min).

## Plan post-mesh

Tras completar mesh 3x3:
- Si **mejora M1 sin romper M3/M5** → continuar con random n=20 d=3.
- Si mantiene mesh OK pero falla random → seguir con iter 2 (OBJ-028, probar K=2 o threshold 0.35).
- Si rompe mesh (R-017 baja en mesh) → revertir threshold y probar otro parámetro.

## Bloqueos

Ninguno. Próximo cron evalúa progreso sim 54.
