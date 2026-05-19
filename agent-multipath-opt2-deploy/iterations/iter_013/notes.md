# iter_013 — OBJ-015 lanzado (sim mesh 3x3 post-cambio, multipath ON)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-015 (lanzar; verificación en próxima iter cuando termine).

## Pre-cheques

- kubectl context: `cluster/dkms1` ✓
- BD `kme.local_qkc_id`: ocupados hasta `node_id=120` + huecos varios. **Libre offset 121** → node_ids 122-130 para mesh 3x3 (9 nodos).
- Deploy actual orchestator: `orr:v8.1` + `ORR_MULTIPATH_ENABLED=true` (iter_012).

## Comando lanzado en background

```bash
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 121 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir agent-multipath-opt2-deploy/iterations/iter_013/post/pequena_densa
```

PID 2597119. Output: `/tmp/iter013-mesh3x3-post.out`.

Sim previa con el mismo nombre `mesh-3x3` (sim 49 del agente sdn-multipath, baseline) fue borrada automáticamente (`--force`). La sim post-cambio es nueva.

Sim id real lo asignará el orchestator (visible en próxima iter).

## Esperable

- ~3-5 min: pods Ready, saturate empieza.
- ~10-12 min: saturate cierra (timeout 600s) + CSVs escritos.
- ~15-17 min: loadtest 5→50 SAEs (~225s) + cierre sim.

## Próximo

Iter_014 (siguiente cron en 5 min): verificar progreso. Si terminó, ejecutar `bench_multipath.py` contra baseline iter_009 del agente previo. Si aún corre, esperar.

## Bloqueos

Ninguno. Cron seguirá verificando estado cada 5 min.
