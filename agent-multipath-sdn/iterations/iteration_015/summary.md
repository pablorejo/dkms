# Iteración 015 — Trabajo preparatorio OBJ-018 (con bloqueo activo en OBJ-016)

**Fecha:** 2026-05-18

## Contexto

OBJ-016/017/018 siguen `[ ]` (el usuario no añadió OBJ-021 wiring). Para
evitar iterar sin avanzar, hago trabajo **offline preparatorio** para
OBJ-018: extender `bench_multipath.py` con la gráfica comparativa
baseline-vs-post. Cuando llegue post-data real, el script estará listo.

## Qué se hizo

### `tests/cli/bench_multipath.py`

- Añadida `_import_matplotlib()` (lazy import, silent skip).
- Añadida **función pública** `plot_compare(baseline_run, post_run, output_path)` que genera una gráfica de 2 paneles (lado a lado) con líneas `enc` por commodity para baseline y post. Eje Y compartido; threshold de saturación marcado.
- Robusta: retorna `False` (no crashea) si matplotlib falta o si los CSVs no existen.
- Nuevo flag CLI `--plot-compare PATH` que invoca la función cuando `--baseline` también está.

### `tests/cli/test_bench_multipath.py`

- `test_plot_compare_writes_png_when_matplotlib_present` — verifica que `plot_compare` produce un PNG > 5 KB con CSVs sintéticos.
- `test_plot_compare_returns_false_on_missing_csvs` — retorna False sin crashear cuando faltan CSVs.

### Smoke test ejecutado

```
python3 -m tests.cli.bench_multipath <run> --baseline <run> --plot-compare out.png
```

→ generó `iter_015/baseline_vs_baseline_smoke.png` (247 KB, 2 paneles idénticos, como esperado al comparar baseline contra sí mismo).

## Verificación

- `python3 -m pytest tests/cli/` → **192 passed** (era 190 + 2 nuevos).
- Sin tocar Rust en esta iter (cargo no aplica).

## Decisiones

- **NO marco OBJ-018 [x]** porque sigue requiriendo post-data real (que está bloqueada por OBJ-016 → OBJ-021). El script ESTÁ listo, faltan datos.
- Hago trabajo preparatorio en lugar de spin-loop sobre OBJ-016. Mejor: la próxima iter del cron, viendo que OBJ-016 sigue `[ ]` y no hay OBJ-021, podría:
  - Lanzar OBJ-016 igualmente y registrar baseline==post (honesto).
  - O hacer más trabajo offline (refinar bench output, mejorar tests, etc.).

## Bloqueos persistentes

- **OBJ-016/017/018** dependen de wiring real + imagen Docker. Sin OBJ-021 (decisión del usuario), Fase D queda 1/5.

## Próximo paso si el cron sigue corriendo

Si la próxima iter no encuentra OBJ-021, probablemente intentará OBJ-016 y registrará baseline-equivalent. Eso al menos cierra OBJ-016/017 con datos honestos y permite generar la gráfica final con `plot_compare`.
