# Plan de pruebas — escalado de DKMSs, SAEs y topología

**Versión**: borrador 2026-05-17 · `pablorejo`
**Estado**: para discusión antes de ejecutar la primera serie.

## 1. Propósito

Caracterizar el sistema DKMS Rust completo (DKMS + ORR + QKC + SDN + quditto)
en tres ejes que afectan a la planificación de capacidad y a la dinámica del
solver MCF del SDN:

1. **Número de DKMSs** en la red — afecta al número de flujos, al coste de
   bootstrap O(N²) ML-KEM y al tamaño del problema MCF.
2. **Número de SAEs activos** — controla la demanda agregada y el régimen
   (subutilizado → equilibrio → saturado).
3. **Topología** de los enlaces QKD — fija la conectividad, los caminos y la
   contención por enlace.

El objetivo no es un *benchmark* puntual, sino observar cómo varía el
comportamiento (tasa de llenado, fairness, latencia de la primera clave,
desvío entre buffers que deberían recibir la misma rate) al variar cada eje.

## 2. Resumen de la propuesta original

> - DKMSs: 5, 10, 30, 50, 100
> - SAEs: rampa start=100, step=100, end=1000 por topología
> - Topologías: malla cuadrada, estrella 4 puntas, anillo
> - Antes de los SAEs: verificar la tasa de llenado media de buffers y que
>   no haya desbalances grandes por prioridades.

Esto suma **150 corridas nominales** (5 × 3 × 10). Hay dos problemas que
hay que resolver antes de pegar al botón de arrancar — uno aritmético y
otro de tiempo de pared. Ambos abajo.

## 3. Análisis crítico — antes de aceptar la matriz tal cual

### 3.1 Los N propuestos no encajan en todas las topologías

| N propuesto | Anillo | Estrella 4-puntas (1 centro + 4×k) | Malla cuadrada (n×n) |
|------------:|:------:|:----------------------------------:|:--------------------:|
|   5         |   ok   | ok (k=1, 1+4·1)                    | imposible (√5∉ℕ)     |
|  10         |   ok   | **no es 1+4k** — asimétrica         | imposible            |
|  30         |   ok   | **no es 1+4k**                     | imposible            |
|  50         |   ok   | **no es 1+4k**                     | imposible            |
| 100         |   ok   | **no es 1+4k**                     | ok (10×10)           |

- **Anillo** admite cualquier N ≥ 3 → la serie propuesta es válida tal cual.
- **Estrella de 4 puntas** con `k` DKMSs por brazo: total `1 + 4k`. Solo
  encaja con N ∈ {5, 9, 13, 17, 21, 25, 29, 33, 37, 41, 45, 49, 53, 57,
  61, 65, 69, 73, 77, 81, 85, 89, 93, 97, 101, …}.
- **Malla cuadrada** n×n: N debe ser cuadrado perfecto: {1, 4, 9, 16, 25,
  36, 49, 64, 81, 100, 121, …}.

**Decisión adoptada (2026-05-17)**: serie limpia válida en las 3
topologías → **N ∈ {9, 25, 49, 81, 121}**.

- Cuadrados perfectos → malla 3×3 / 5×5 / 7×7 / 9×9 / 11×11.
- 1+4k con k ∈ {2, 6, 12, 20, 30} → estrella con 2/6/12/20/30 DKMS por
  brazo.
- Anillo: cualquier N ≥ 3 vale, esta serie también.

Reemplaza la serie original {5, 10, 30, 50, 100} que no era válida en
malla (excepto 100) ni en estrella (excepto 5). Los N "grandes" (81,
121) prueban los caminos largos en malla (diámetro 2(n−1) = 16 y 20
respectivamente).

### 3.2 Tiempo de pared y RAM — `capacity_per_peer` fijo a 10 000

**Decisión adoptada**: `capacity_per_peer = 10 000` constante para
todas las corridas, independiente de N. Justificación: simplicidad y
comparabilidad — el mismo buffer en todas las pruebas significa que
las diferencias observadas vienen del sistema (topología, fairness,
SDN) y no de un parámetro que cambia.

**Crítica explícita (sé crítico)**:

- **10 000 es arbitrario**. No está alineado con:
  - El default del Rust (`4 096` en `dkms/src/config.rs:168`).
  - El override del orquestador (`65 536` en `orchestator.py:~920`).
  - Ninguna métrica derivada del workload (burst SAE típico,
    latencia ORR, etc.).
  Es un valor *redondo* elegido para tener tiempos manejables en N
  grande sin perder demasiada amortiguación en N pequeño.

- **Tiempo a Saturated no es homogéneo entre N**. Con B fijo y rate
  steady ≈ R0/K (K = nº flujos en edge crítico), el tiempo crece como
  N² en anillo, como N^{3/2} en malla, como N² en estrella. Concretos
  con R0=2000 (ver tabla 5.3 recalculada):
  - Anillo N=9: 10 s. Anillo N=121: ~30 min.
  - Malla N=9: 23 s. Malla N=121: **6 h 46 min**.
  - Estrella N=9: 12 s. Estrella N=121: 45 min.
  → la corrida más larga del baseline (malla N=121) **no cabe en una
  jornada de trabajo**. Hay que decidir si se acepta o se ajusta R0
  (subirlo) sólo para esa celda.

- **El "valor correcto" depende de la rate SAE objetivo**. Si los SAEs
  piden a `λ_sae = 0.5 req/s` por SAE y hay r SAEs/DKMS, la demanda
  agregada por buffer (i→j) en steady state es ~`r · λ_sae · (S/(S−1))`.
  Para que el buffer absorba un burst transitorio de duración δ, B
  debe cubrir `r · λ_sae · δ`. Para r=10 SAEs/DKMS, λ=0.5, δ=10 s:
  B≈50. **B=10 000 está sobredimensionado** para amortiguación pura
  → la mayor parte de la capacidad es "stockpile", no "buffer". Si lo
  único que se mide es throughput steady state, B grande sólo retrasa
  la transición a Saturated sin cambiar el régimen final.

- **Dependiente de N sería más limpio**. Una alternativa formal:
  `B(N) = c · R0 / K_max(N)` donde K_max es nº flujos en edge crítico.
  Esto haría que el tiempo de llenado fuera ≈ constante en todas las
  celdas. Pero rompe la comparabilidad de la métrica "fill_ratio"
  (10 % de B distinto en cada celda). El usuario aceptó este coste:
  **B fijo, tiempos heterogéneos, comparabilidad de fill_ratio
  preservada**.

**RAM proyectada por DKMS** con B=10 000, ks=32 B, N=121:
`10 000 × 32 × 2 (ENC+DEC) × 120 peers ≈ 77 MB` por DKMS solo en
buffers. Total cluster: `77 MB × 121 ≈ 9.3 GB`. Cabe en EKS con
límites razonables (cada pod ≤ 256 MB). Para N=49: `≈ 30 MB / DKMS`,
total 1.5 GB. Trivial.

→ **No hay bloqueo de RAM** con esta capacity en ningún N de la
matriz.

### 3.3 Bootstrap O(N²) — fact-check

ORR↔ORR hace ML-KEM-768 entre cada par ordenado al arranque
(documentado en CLAUDE.md §SDN phantom-Priority). Para N DKMS son
`N(N−1)` handshakes. Para N=121: 14 520 handshakes. Cada uno es PQC
encap/decap (~10 µs) + un round-trip gRPC en la red K8s (~1-5 ms). Con
paralelismo via `tokio::spawn` el wall-clock es algunos segundos en una
red sana. **Es viable**, pero hay que validarlo: el bug del
"phantom-Priority" (CLAUDE.md) demostró que cuando el bootstrap **no**
se completa para algún par, el solver SDN degrada a default
`TrafficPriority::Priority` (peso 100) y se carga la asignación real.
Pre-flight obligatorio: `GET sdn:3002/priority` devuelve `N(N−1)`
entradas. Si faltan, no se arranca la rampa.

### 3.4 Rampa SAE absoluta 100 → 10 000 step 100

**Decisión adoptada**: rampa **absoluta independiente de N**, de 100
hasta 10 000 SAEs totales, paso 100, con **early stop** si la corrida
muestra signos de fallo (criterios en 6.2). Es la propuesta original
del usuario, mantenida porque el objetivo es "ver el comportamiento
general de la topología", no comparar el mismo régimen entre N.

**Crítica explícita (sé crítico)**:

- **Régimen muy distinto entre N**. Con S=10 000 SAEs totales:
  - N=9: 1111 SAEs/DKMS. **Régimen muy sobrecargado** — el HTTP
    server del DKMS y el `SaeBufferBucket` por SAE pueden ser cuello
    de botella *antes* que el SDN. Estamos midiendo la cola SAE-side,
    no la dinámica de buffers DKMS↔DKMS.
  - N=49: 204 SAEs/DKMS. Régimen alto pero manejable.
  - N=121: 82 SAEs/DKMS. Régimen normal.
  → la comparación entre N en el extremo alto de la rampa **no es
  apples-to-apples**. Los baseline (rampa baja, ej. 100 SAEs) sí lo
  son: 11 SAEs/DKMS en N=9, ~1 SAE/DKMS en N=121.

- **El límite hard del orquestador es 5 000**, no 10 000
  (`api_orchestator.py:82`: `end_saes: int = Field(..., le=5000)`).
  La rampa hasta 10 000 requiere **subir ese límite** a 20 000 o
  similar. Es un cambio trivial en una línea, pero es **un cambio
  que hay que hacer antes** de poder correr el plan. Lo añado a la
  sección 7 (pre-flight).

- **100 escalones × 30 s = 50 min por corrida** sólo en la rampa.
  Sumando warmup (30 s baseline) y settle (30 s en cada escalón ya
  contabilizado) son 50-55 min por corrida con SAEs. Por 15 corridas
  (3 topologías × 5 N) = **12-14 h de wall-clock total**. Aceptable
  si el early-stop dispara en celdas extremas.

- **`per_sae_lambda` default es 0.5 req/s** (no 1.0 que asumí
  inicialmente). Demanda total a S=10 000: `5 000 req/s` agregadas
  sobre la red. Repartidas como matriz uniforme inter-DKMS, por
  flujo (i,j): `5 000 / (N² − N)`. Para N=9: 69 req/s/flujo. Para
  N=121: 0.34 req/s/flujo. La demanda es perfectamente absorbible
  por la rate steady en cada caso. Lo que va a saturar primero es
  la cola SAE-side y el procesamiento HTTP del DKMS.

**Early-stop**: la corrida se aborta cuando se cumpla **cualquiera**
de estos criterios sostenido durante 2 escalones consecutivos
(60 s):

1. **HTTP error rate > 10 %** (códigos ≠ 200, ≠ 429). Indica que
   el DKMS está roto o saturado, no rate-limited.
2. **p99 latencia /enc_keys > 5 s**. Indica encolado descontrolado.
3. **σ throughput inter-SAE > 80 %** sostenido. Indica algún flujo
   completamente starved (probable bug bootstrap).
4. **RSS de algún pod > 80 % del límite del container**.

El runner anota el escalón en que se abortó y el motivo. Los datos
de los escalones previos se guardan para análisis parcial.

### 3.5 La verificación "buffer fill antes de los SAEs" se va a pelear con la histéresis del clasificador

`dkms/src/control/priority.rs` clasifica el buffer en 5 + 1 clases con
**histéresis del 5%** entre transiciones (10% en Saturated). Lo que el
usuario observa como "todos los buffers deberían llenarse igual" es
verdad en el límite, pero la dinámica real es:

1. **0 → 10% (Priority, peso 100)**: si las prioridades son strict en
   el SDN (mira `sdn/src/mcf.rs:6` "strict priority within class"), un
   flujo en Priority recibe **toda** la capacidad del edge que el solver
   le da. Los flujos no-Priority en el mismo edge reciben ≈ 0.
2. **10 → 30% (Important, peso 30)**: si un flujo sigue solo (no hay
   coactivos), sigue recibiendo todo. Si otro flujo del mismo edge
   también pasó a Important, comparten proporcional al peso (que es
   igual entre ambos: 30).
3. **30 → 50%, etc.**

→ Esperado en arranque homogéneo (todos los flujos parten de 0,
mismo R0, mismo nº de hops): **los buffers suben en escalera** —
todos cruzan 10% más o menos a la vez, luego 30% a la vez, etc.
**Las prioridades no causan desbalance entre buffers homogéneos.**
Causan el desbalance cuando los flujos son heterogéneos (multi-hop,
edges asimétricos, contención asimétrica).

→ "no debería haber mucha diferencia debido a las prioridades" es
correcto en topologías simétricas (anillo, grid de Manhattan). En
**estrella** sí habrá diferencia: edge centro—brazo lleva más flujos
que edge a_i — a_(i+1) lejos del centro → buffers que usan ese edge
crítico se llenan más lento que los flujos enteros intra-brazo.

Hay que medir **tres patrones distintos**:
- **σ tiempo-a-Priority→Important** entre buffers que "deberían"
  llenarse igual (mismo path length).
- **σ tiempo-a-Saturated** entre todos los buffers (incluyendo asimetría
  natural por topología).
- **kurtosis / cola** del histograma de rates observadas — si hay
  outliers significa que el bootstrap quedó cojo (CLAUDE.md §race).

## 4. Plan revisado

### 4.0 Modelo de comportamiento de los SAEs

**Asignación SAE → DKMS (al instanciar)**:

- Cada SAE se asigna a un DKMS elegido **uniformemente al azar** del
  conjunto de N DKMSs en el instante en que se crea.
- La asignación es **fija para la vida del SAE** (no migra).
- Con S SAEs totales y N DKMSs, cada DKMS recibe ~S/N SAEs en media.
  Desviación estándar `≈ √(S·(N−1)/N²)` → para S=10·N y N=49:
  σ ≈ √(490·48/2401) ≈ 3.13. La carga por DKMS varía entre
  aprox. (10−2·3.13) y (10+2·3.13) → 4 a 16 SAEs.

**Patrón de peticiones (durante la corrida)**:

- Cada SAE elige **un SAE peer aleatorio** del conjunto global de S
  SAEs, excluyéndose a sí mismo, con distribución **uniforme**.
- El SAE peer **puede estar en el mismo DKMS** (en ese caso es una
  petición intra-DKMS, no requiere flujo inter-DKMS y consume del
  buffer "local") o en otro distinto.
- Probabilidad de que el peer esté en el mismo DKMS:
  `(S/N − 1) / (S − 1)`. Para S=10·N grande: `≈ 1/N`. Tiende a 0
  cuando N crece → en N=121 sólo ~0.8 % de las peticiones son
  intra-DKMS.

**Frecuencia de petición por SAE**:

- Baseline: **1.0 req/s por SAE** (sube respecto al default 0.5 del
  orquestador en `api_orchestator.py:91`), con intervalos
  exponenciales (Poisson) para evitar sincronización accidental.
- **Justificación del cambio**: con λ=0.5 la demanda por flujo
  inter-DKMS en N=49 es `S·λ/N² ≈ 2.1 req/s` para S=10 000, frente
  a una capacidad steady de `4·R0/N² ≈ 3.33 keys/s`. **El margen
  1.5× hace que prácticamente nunca se vean 429** — el sistema está
  permanentemente por debajo de saturación SAE-side. Con λ=1.0 la
  demanda iguala/excede la capacidad en escalones altos de la
  rampa, así se observa la transición operativa → saturada.
- **Eje secundario sugerido**: hacer un sweep λ ∈ {0.5, 1.0, 2.0}
  en una sola celda (N=49 anillo) antes de fijar λ para el resto
  de la matriz. Confirma el sweet spot donde aparecen 429s.
- Configurable: env `SAE_RATE_HZ` en el binario Rust `sae-sim`
  (sección 9.2). El runner del orquestador (`dkms-loadtest`)
  reexpone el flag como query param.

**Matriz de demanda inter-DKMS implícita**:

- Con muestreo uniforme y `≈ S/N` SAEs por DKMS, la rate de
  peticiones del DKMS *i* hacia el DKMS *j ≠ i* es:
  ```
  D(i,j) ≈ (S/N) · λ_sae · (S/N − ε) / (S − 1)
        ≈ λ_sae · S / (N² − N)    keys/s
  ```
  con `λ_sae = 0.5 req/s` (default).
- Es **simétrica y uniforme** sobre todos los pares ordenados (i,j).
  → en una topología simétrica el SDN debería repartir capacidad
  homogéneamente, salvo asimetría natural de la topología
  (estrella).
- Demanda total agregada: `S · λ_sae`. Para la rampa absoluta hasta
  S=10 000: pico de **5 000 req/s** agregadas. Por flujo (i,j) con
  N=49: 5 000 / 2 352 ≈ **2.1 req/s**. Con N=121: 5 000 / 14 520 ≈
  **0.34 req/s**.

### 4.1 Matriz definitiva

| Eje | Valores | Comentario |
|-----|---------|------------|
| **N (DKMSs)** | 9, 25, 49, 81, 121 | Cuadrados perfectos y 1+4k simultáneos |
| **Topología** | anillo, malla cuadrada, estrella 4-puntas | 3 patrones |
| **R0 (keys/s por edge)** | 2 000 | Default quditto. Subir a 8 000 solo en malla N=121 si el wall-clock no cabe |
| **B (capacity_per_peer)** | **10 000 fijo** | Constante en toda la matriz (decisión 3.2) |
| **Rampa SAE** | 100 → 10 000 step 100 | Absoluta. Early-stop si dispara criterio 3.4 |
| **`per_sae_lambda`** | **1.0 req/s** | Subido vs default 0.5 para forzar régimen saturado (§4.0) |
| **`interval_seconds`** (escalón) | 30 s | Default actual |
| **`warmup_seconds`** | 30 s | Baseline antes de la rampa |

**Corridas**: `5 N × 3 topo = 15 corridas`. Cada corrida cubre la
rampa entera en un solo deployment (no se reinicia entre escalones).

**Fases de una corrida**:

1. **Crear** sim con la topología (POST /orch/web/simulations).
2. **Arrancar** (POST /orch/web/simulations/<id>/run). Esperar pods
   Ready y bootstrap ML-KEM completado (verificar
   `GET sdn:3002/priority` devuelve N(N−1) entradas).
3. **Baseline pre-SAE** (30 s): observar fill_rate, transiciones QoS,
   tiempo a Saturated por buffer. **Verificar simetría aquí** (sec.
   5.4/5.5) antes de empezar la rampa — si los buffers no se llenan
   uniformemente, el bug está antes de los SAEs.
4. **Rampa SAE absoluta** 100 → 10 000 step 100, 30 s/escalón.
   Lanzar via `POST /orch/web/simulations/<id>/tests` con
   `start_saes=100, end_saes=10000, step_saes=100, interval_seconds=30`.
   Aplicar early-stop (sec. 3.4).
5. **Stop** (DELETE namespace).

**Tiempo por corrida**:
- Setup + baseline: ~3-5 min.
- Rampa completa: 100 escalones × 30 s = 50 min.
- Stop: ~1 min.
- Total por corrida: ~55 min (sin early-stop).
- Total 15 corridas: **~14 h wall-clock**.

**Cómputo total estimado**: 1-2 días con supervisión humana ligera,
o 1 jornada si el runner está bien instrumentado.

## 5. Tasa teórica de llenado de buffers

### 5.1 Lo que limita la tasa por flujo

Para un buffer `enc[src→dst]` la tasa de llenado en steady state está
limitada por el mínimo de:

1. **Asignación del SDN**: `rate_sdn(src,dst)` keys/s, salida del
   solver MCF — proportional-fair dentro de cada clase QoS, strict
   priority entre clases. Depende de qué clase está ahora el buffer.
2. **Cap del generator local**:
   `max_tokens_per_peer_per_tick × (1000 / tick_ms)` =
   `400 × 10 = 4000 keys/s` con override del orquestador
   (`32 × 10 = 320 keys/s` sin override).
3. **Capacidad del edge cuello de botella en el path**:
   `min over edges en el path de (R0_e − tráfico de otros flujos en e)`.
4. **ack_pending**: el token bucket no emite si hay >
   `bucket_cap_seconds × rate` claves sin ACK. Con override 2.0 s y
   rate 4000 → bucket cap 8000 keys "en vuelo". No limita hasta tasas
   muy altas.

En la práctica, para R0 = 2000 y los overrides actuales:

- **Edge sin contención + 1 flujo**: 2000 keys/s, capped por el SDN
  (no por el generator que admite 4000).
- **Edge con K flujos coactivos en la misma clase**: ~`R0/K` keys/s
  por flujo.
- **Flujo multi-hop (m edges)**: `R0 / max_K_en_el_path`.

### 5.2 Fórmulas por topología (steady state, todos los flujos activos)

Notación: N = nº de DKMSs. Asumimos shortest-path routing simple.

**Anillo de N nodos** (N edges):
- Flujos totales ordenados: `N(N−1)`.
- Camino entre i y j: arco más corto, longitud `min(|i−j|, N−|i−j|)`.
- Flujos pasando por cada edge: `⌊N²/4⌋` ordenados (anillo simétrico).
- Rate por flujo: `R0 · 4 / N²`.
- Tiempo llenado: `B · N² / (4·R0)`.

**Malla cuadrada n×n (N=n²)**:
- Edges: `2n(n−1)`.
- Camino Manhattan entre dos celdas. Distancia media: `2n/3`.
- Flujos máximos en edge central: `n³/2` ordenados (cota superior;
  con routing balanceado es algo menor).
- Rate por flujo (aprox, asumiendo XY-routing por filas-columnas):
  `R0 · 2 / n³` = `R0 · 2 / N^{3/2}`.
- Tiempo llenado: `B · N^{3/2} / (2·R0)`.

**Estrella 4 puntas (1 centro + 4·k DKMS, N=1+4k)**:
- 4 brazos lineales de k nodos. Edges: `4k`.
- Edge centro—a₁ del brazo A: lleva todos los flujos brazo-A↔resto y
  brazo-A↔centro. Total ordenados = `2k(3k+1)` ≈ `6k²` para k grande.
- Edge a_i—a_{i+1} (interior del brazo): lleva flujos para los k−i
  nodos a partir de a_{i+1}: `2(k−i)(3k+1)` ordenados.
- Cuello de botella: edge centro—a₁. Rate por flujo:
  `R0 / 6k²` = `R0 · 16 / (3(N−1)²)`.
- Tiempo llenado: `B · 6k² / R0`.

### 5.3 Tabla — rate teórica y tiempo de llenado, R0=2 000, B=10 000

| Topo     | N   | k/n   | Flujos | Rate/flujo aprox | t_fill = B/rate |
|----------|-----|-------|--------|------------------|-----------------|
| Anillo   | 9   |   —   |   72   | 988 keys/s       |     10.1 s      |
| Anillo   | 25  |   —   |  600   | 128 keys/s       |     78 s ≈ 1 m 18 s |
| Anillo   | 49  |   —   |  2 352 | 33.3 keys/s      |    300 s ≈ 5 m 0 s |
| Anillo   | 81  |   —   |  6 480 | 12.2 keys/s      |    820 s ≈ 13 m 40 s |
| Anillo   | 121 |   —   | 14 520 | 5.50 keys/s      |  1 818 s ≈ 30 m 18 s |
| Malla    | 9   | 3×3   |   72   | 444 keys/s       |     22.5 s      |
| Malla    | 25  | 5×5   |  600   | 32 keys/s        |    313 s ≈ 5 m 13 s |
| Malla    | 49  | 7×7   |  2 352 | 5.83 keys/s      |  1 715 s ≈ 28 m 35 s |
| Malla    | 81  | 9×9   |  6 480 | 1.37 keys/s      |  7 299 s ≈ 2 h 1 m 39 s |
| Malla    | 121 | 11×11 | 14 520 | 0.41 keys/s      | 24 390 s ≈ **6 h 46 m 30 s** |
| Estrella | 9   | k=2   |   72   | 833 keys/s       |     12 s        |
| Estrella | 25  | k=6   |  600   | 92.6 keys/s      |    108 s ≈ 1 m 48 s |
| Estrella | 49  | k=12  |  2 352 | 23.1 keys/s      |    433 s ≈ 7 m 13 s |
| Estrella | 81  | k=20  |  6 480 | 8.33 keys/s      |  1 201 s ≈ 20 m 0 s |
| Estrella | 121 | k=30  | 14 520 | 3.70 keys/s      |  2 703 s ≈ 45 m 3 s |

**Cómo leer esta tabla**:

- "Rate/flujo aprox" es la rate en steady state cuando el solver SDN
  ha llegado a estado estable y todos los buffers están en la misma
  clase QoS (todos en Priority, o todos en Important, etc).
- "t_fill" es el tiempo desde buffer vacío hasta `Saturated` (95 %
  de B), pero **subestima** el tiempo real porque ignora la
  histéresis del clasificador y el strict priority entre clases.
- Con strict priority bien implementado, durante el tramo Priority
  → Important el flujo único en clase Priority recibe **todo** el R0
  del edge — efectivamente acelera, no la rate de la tabla. La tabla
  es la rate del **régimen final** donde todos los buffers están en
  la misma clase y empieza la fairness intra-clase.

**Nota**: la columna `t_fill` es **informativa**, no operativa. El
baseline pre-SAE de cada corrida (sec. 6.2) no espera a Saturated,
sino que mide el `fill_rate` observado en una ventana de 60 s y lo
compara con la rate teórica de esta tabla. La validación tarda lo
mismo en N=9 que en N=121.

### 5.4 Cómo opera el strict priority — per-edge, no global

Antes de las predicciones por topología, una nota crítica sobre el
solver MCF (`sdn/src/mcf.rs:382-394`):

> Strict priority compara clases **por enlace**. Sólo si en un edge
> hay flujos en clase superior consumiendo capacidad, los flujos en
> clase inferior **que pasen por ese mismo edge** se ven frenados.
> Dos flujos en edges completamente distintos son independientes,
> aunque estén en clases distintas.

Consecuencias:

1. **Las prioridades equilibran flujos que compiten en el mismo
   edge.** No equilibran flujos en edges distintos.
2. **Si un flujo de borde tiene su edge sin compañía**, se llena a
   ~R0 keys/s aunque haya flujos centrales en clase superior
   consumiendo otros edges.
3. **La sincronización de llenado sólo es global cuando todos los
   flujos pasan por un único cuello de botella común** (anillo).

### 5.5 Anillo — único caso totalmente sincronizado

Por simetría rotacional, en anillo todos los edges llevan el mismo
nº de flujos (≈ N²/4). Cada flujo está limitado por todos sus edges
por igual. El strict priority sí sincroniza globalmente:

1. **Fase 1 (0 → ~10%)** — todos los flujos en Priority. Cada uno
   recibe `R0 / K = 4·R0 / N²`. Suben en bloque.
2. **Transición** — el primero que cruza 10 % pasa a Important, se
   frena a 0 mientras los demás (aún en Priority) absorben más rate
   y lo alcanzan en milisegundos. Re-sincronización.
3. **Repetir** en 30 %, 50 %, 70 %, 95 %.

→ **Métrica de pase**: σ(t_a_Saturated) / media < **5 %**. Si σ es
mayor, sospechar bootstrap incompleto o bug del solver.

### 5.6 Malla — asimetría posicional natural

Los edges centrales llevan más flujos que los de borde
(aproximadamente `n³/4` vs `n²/2`). Flujos cortos en el borde
compiten contra ~`n` flujos en su edge; flujos largos que cruzan el
centro compiten contra ~`n²` en al menos un edge.

Los flujos de borde **no se ven frenados** por strict priority
aunque los centrales estén en clase superior — sus edges no se
solapan. Cada flujo evoluciona limitado por su propio cuello.

Predicción:
- Buffers de flujos cortos (borde a borde adyacente) saturan rápido.
- Buffers de flujos largos (esquina a esquina opuesta) saturan
  lento, en proporción al K_max de su path.
- Los cortos llegan a Saturated → peso 0 → liberan capacidad sólo en
  los edges que cruzaban (no en los centrales, salvo flujos del
  centro al borde).
- En el límite quedan los flujos largos activos compartiendo los
  edges centrales.

→ **Métrica de pase**: ordenar buffers por `K_max(path)` y verificar
correlación con orden de saturación (Spearman ρ > 0.7). σ
sobre todos los buffers será grande (30-50 %) **por construcción**,
no por bug.

### 5.7 Estrella — asimetría por hops

Hay **dos clases naturales** de flujos:
- **Inter-brazo** (≥ 2 hops, pasan por centro): cuello de botella es
  el edge centro—a₁ que comparten con muchos flujos.
- **Intra-brazo** (1 a k hops, no salen del brazo): edges menos
  cargados, cuello de botella mucho más relajado.

Igual que en malla, strict priority no transfiere capacidad entre
estas dos familias (sus edges no se solapan en su mayoría). Los
intra-brazo del extremo lejano (a_k → a_{k−1}) avanzan libres
aunque haya tráfico Priority en centro—a₁.

Predicción:
- Buffers intra-brazo saturan **mucho antes** que inter-brazo.
- Los intra-brazo Saturated se quitan del MCF y liberan capacidad
  del **edge interior del brazo** (no del edge centro—brazo) para
  los inter-brazo que también lo cruzan.
- En el límite: solo quedan flujos inter-brazo activos compartiendo
  `R0 / (6k²)` por flujo.

→ **Métrica de pase**: ordenar buffers por nº de hops y verificar
que el orden de saturación coincide con el orden por hops
creciente. Tolerar inversiones menores (asimetría de bootstrap).

## 6. Métricas y criterios de pase

### 6.1 Métricas mínimas a recoger

Recoger via parser de logs (DKMS no expone Prometheus aún, ver CLAUDE.md
§DKMS Rust /metrics returns 200 with empty body):

| Métrica | Fuente | Cadencia |
|---------|--------|----------|
| `enc`, `dec` (keys en buffer) | log `generator.state` | 5 s |
| `emit_total` (acumulado) | mismo | 5 s |
| `observed_keys_per_s` | mismo | 5 s |
| `sdn_rate_keys_per_s` | mismo | 5 s |
| `ack_pending` | mismo | 5 s |
| Clase QoS por buffer | `priority.classify` log | en transición |
| SDN priority registry | `GET sdn:3002/priority` | 30 s |
| SDN allocations | `GET sdn:3002/allocations` | 30 s |
| RAM del pod DKMS / ORR / QKC | `kubectl top pod` | 30 s |

**Nota sobre cadencia**: el frenado strict priority en transiciones
de clase dura ~0.1-1 s. Con muestreo a 5 s sólo se observa el
**promedio** de la ventana, no la dinámica fina. Suficiente para
validar steady-state y criterios de §6.2, pero **insuficiente para
detectar bugs sub-segundo del solver** (p. ej. un buffer congelado
3 s en vez de 0.3 s en cada umbral). Si los resultados parecen
correctos pero quieres validar el solver en serio, hace falta
añadir un scrape Prometheus a 1 Hz (futuro trabajo).

### 6.2 Criterios de pase

Por corrida sin SAEs (baseline, demanda 100 % del generator):

**Procedimiento**: con buffers vacíos al t=0, esperar 60 s y medir
el `fill_rate` observado de cada buffer (slope de `enc` en logs
`generator.state`). Comparar con la rate teórica de la tabla 5.3.
No se espera a Saturated.

- **PASE rate**: para cada buffer, `|observed_rate − theoretical_rate|
  / theoretical_rate < 10 %`. La rate teórica es R0/K_max donde
  K_max es el nº de flujos en el edge cuello del path de ese buffer.
- **PASE σ — anillo**: σ(observed_rate)/media < **5 %** (único caso
  donde todos los flujos comparten el mismo K, §5.5).
- **PASE correlación — malla**: Spearman ρ entre `K_max(path)` y
  `observed_rate` < **−0.7** (mayor K → menor rate). σ sobre todos
  los buffers puede ser 30-50 % por construcción posicional (§5.6),
  no se evalúa directamente.
- **PASE correlación — estrella**: Spearman ρ entre nº de hops y
  `observed_rate` < **−0.8** (más hops → menor rate, §5.7). Tolerar
  inversiones < 10 % de buffers.
- **FAIL** si: algún buffer tiene `observed_rate ≈ 0` mientras los
  demás emiten normal (sospechar bug `ORR sin master_secret`,
  CLAUDE.md §Bootstrap race).

**Coste temporal**: 60 s por corrida, independiente de N y de la
topología. Sustituye al criterio "tiempo a Saturated" que tardaba
hasta 7 h en malla N=121.

Por corrida con SAEs (demanda guiada por sae_sim aleatorio):

- **PASE token bucket**: `observed_keys_per_s` por buffer ≥
  0.9 · `sdn_rate_keys_per_s` en steady state. Verificado
  históricamente (token bucket respeta la asignación SDN).
- **PASE fairness intra-clase**: σ del throughput por SAE / media <
  **tolerancia por muestreo** (ver más abajo).
- **PASE distribución SAE**: el histograma de SAEs/DKMS no rechaza
  la hipótesis de uniformidad (test χ² a α=0.05 con N−1 g.d.l.).
- **FAIL** si: edge con flujos en Important pero alguno en Priority
  recibe ≠ 0 (strict priority violado — bug del solver).
- **FAIL** si: la matriz de demanda inter-DKMS observada se aleja
  > 3σ de uniformidad (muestreo del peer no es uniforme — bug en
  `sae_sim.py`).

**Tolerancia por muestreo aleatorio**: el modelo SAE aleatorio (4.0)
introduce dos fuentes de varianza no atribuibles al sistema. Como la
rampa es absoluta, S y por tanto S/N varían a lo largo de la corrida:

1. **Carga por DKMS**: con S SAEs y N DKMSs, σ(SAEs/DKMS)/media =
   `√((N−1)/S)`. Para S=100 (inicio de rampa) y N=9: `√(8/100) ≈ 28 %`.
   Para S=10 000 (final) y N=121: `√(120/10000) ≈ 11 %`. La varianza
   relativa **decae con S creciente** → los escalones altos son los
   más comparables; los bajos llevan ruido grande.
2. **Matriz de demanda inter-DKMS**: peticiones por par (i,j) tienen
   rate media `D̄ = λ_sae·S/(N²−N)`. Para S=10 000, λ=0.5, N=49:
   D̄ ≈ 2.1 req/s. Varianza Poisson:
   `σ²(D)/D̄ = 1/(D̄·T_obs)`. Para T_obs=30 s y D̄=2.1:
   `σ(D)/D̄ ≈ 13 %`.

Tolerancia σ(throughput) sugerida en pase con SAEs (aplica **por
escalón de la rampa**, después de 15 s de settle):

- Por SAE individual: < 40 % (incluye Poisson + asignación variable).
- Por DKMS agregado: < `100·√((N−1)/S)` % + 5 % (margen).
- Por flujo inter-DKMS: < 20 % en escalones con D̄ > 1 req/s; sin
  evaluar en escalones con D̄ < 1 (ruido domina).

Si σ excede estos umbrales, **no asumir bug automáticamente**:
- Repetir la corrida con otra semilla aleatoria.
- Si la varianza es **persistente** (mismo flujo siempre lento) →
  bug. Si es **ruido** (flujos lentos cambian entre corridas) →
  muestreo.

**Semilla aleatoria reproducible**: el binario `sae-sim` (sec. 9.2)
acepta `--seed` o env `SAE_SEED` para fijar la PRNG. Reportarlo en
cada corrida para poder rejugar la misma asignación si aparece
anomalía.

## 7. Procedimiento operativo

### 7.1 Pre-flight (obligatorio antes de cada corrida)

1. **Imagen orquestator con `end_saes` ≥ 20 000**
   (`api_orchestator.py:82`). Sin este cambio el POST de loadtest
   rechaza la rampa hasta 10 000 SAEs con 422.
2. **Imagen orquestator con fix psycopg2** — tag `:v3` o
   equivalente. Probar `/orch/health` y `/orch/web/simulations` en
   un GET autenticado.
3. **Override del DKMS `[buffer]` con `capacity_per_peer = 10 000`**
   en `orchestator.py:~920` (sustituye al 65 536 actual). Recordar:
   config-rs reemplaza la sección entera, así que el override debe
   incluir **todos** los campos:
   ```python
   "DKMS__buffer__capacity_per_peer":   "10000",
   "DKMS__buffer__refill_low_watermark":"2500",
   "DKMS__buffer__refill_batch":         "500",
   ```
4. **Override del DKMS `[generator]`** sin cambios respecto al
   actual (sec. 9.1 punto 3).
5. **Imagen `sae-sim:v1` publicada** y el deployment
   `dkms-loadtest` actualizado para usarla.
6. **`K8S_DKMS_RUST_SIDECARS=true`** en deploy `orchestator`
   (CLAUDE.md §Orchestrator Rust split).
7. **Cluster con presupuesto de memoria** — con B=10 000 ninguna
   celda requiere ajustes especiales (RAM por DKMS ≤ 77 MB en N=121,
   sec. 3.2).

### 7.2 Arranque

1. `POST /orch/api/sim/<id>/run` (no `kubectl rollout restart` — ver
   CLAUDE.md §Bootstrap race).
2. Esperar `kubectl get ns sim-<id>` Active y todos los pods Ready.
3. Verificar `GET sdn:3002/priority` devuelve `N(N−1)` entradas. Si
   faltan, **abortar** — el solver va a degradar a phantom-Priority.
4. Marcar t=0.

### 7.3 Durante la corrida

- Logger paralelo capturando `generator.state` de todos los DKMSs.
- Cada 30 s: snapshot SDN `/priority` y `/allocations`.
- Watchdog RSS: si algún pod supera 70 % del límite → parar y revisar.

### 7.4 Postmortem

- Volcar logs por DKMS, parsearlos, computar:
  - histograma de t_a_Saturated por flujo.
  - serie temporal de `observed_keys_per_s` vs `sdn_rate_keys_per_s`.
  - eventos de cambio de clase QoS.
- Compararlo con la fila correspondiente de la tabla 5.3.
- Anotar resultado y desviaciones en `docs/test-results/`.

## 8. Riesgos conocidos antes de empezar

- **Bug ORR sin master_secret** (CLAUDE.md): si algún ORR ↔ ORR
  bootstrap no se completa, ese flujo queda sin emisión y arrastra
  la simetría. Si la frecuencia es > 1 % de pares en N grande, hay que
  parar y arreglar el race antes de seguir.

- **Solver SDN proportional-fair, no max-min** (CLAUDE.md §Future
  features): el plan asume implícitamente max-min. El solver actual
  es proportional-fair con pesos iguales dentro de clase → en
  ausencia de demanda heterogénea, da el mismo resultado que max-min.
  Si se introduce demanda heterogénea por SAE en pruebas grandes, la
  diferencia se notará y hay que documentarla.

- **DKMS Prometheus vacío**: no hay scrape posible. Todo es log
  parsing. Si los logs entrelazan o se trunan, perdemos métricas.

- **Buffer / capacity de los SAE token bucket** (`SaeBufferBucket`):
  cada SAE tiene su propio bucket. Si rampa muy rápida, el SAE se
  queda sin tokens antes de que el SDN le actualice la rate. No es
  un fail del sistema, pero hay que distinguirlo de un fail real.

## 9. Próximos pasos

### 9.1 Pre-requisitos antes de la primera corrida

1. **Subir `end_saes` del orquestador** de 5 000 → 20 000 en
   `orchestrator/api_orchestator.py:82`. Cambio de un valor (el
   validador `_check_ramp` ya cubre el caso). Smoke test:
   `POST /orch/web/simulations/<id>/tests` con `end_saes=10000`
   devuelve 200, no 422.
2. **Confirmar override de `[buffer]` con `capacity_per_peer=10000`**
   en `orchestator.py:~920` (sustituye al 65 536 actual). Recordar
   pasar **todos** los campos de la sección `[buffer]` (config-rs
   reemplaza la sección entera, no merge — CLAUDE.md §DKMS buffer
   defaults).
3. **Confirmar el override de `[generator]`** con
   `max_tokens_per_peer_per_tick=400`, `tick_ms=100`, etc. (sin
   cambios respecto al actual).
4. **Construir y publicar el binario `sae-sim` en Rust** (sección
   9.2) como imagen Docker `pablopio/sae-sim:v1`. El deployment
   `dkms-loadtest` del orquestador pasa a usar esta imagen en vez
   del `runner.py` Python heredado.

### 9.2 Reescritura del simulador SAE — `sae-sim` en Rust

**Motivación**: el `runner.py` Python que vive detrás de
`POST /orch/web/simulations/<id>/tests` (origen: el repo
`code_dkms/src/loadtest/`) tiene los siguientes problemas para esta
serie de pruebas:

- **No soporta DKMS aleatorio**: cada SAE se asocia a un único DKMS
  por línea de comandos.
- **No soporta peer aleatorio uniforme**: cada SAE pide siempre al
  mismo SAE "slave".
- **CPU-bound para S grande**: con 10 000 SAEs simultáneos
  Python+asyncio se ahoga en TLS handshakes y context switches.
  Rust+tokio+rustls maneja 10 000 conexiones HTTPS persistentes con
  un coste de memoria mucho menor (~5-10 KB por conn vs ~80 KB en
  Python).

**Diseño del crate `sae-sim/`** (nuevo *workspace member*, no es
runtime — vive aparte de los 5 módulos del sistema):

```
sae-sim/
├── Cargo.toml
├── README.md
├── Dockerfile
├── src/
│   ├── main.rs        # CLI + tokio runtime + spawn de tasks
│   ├── sampler.rs     # PRNG, asignación DKMS, selección de peer
│   ├── worker.rs      # Loop por-SAE: Poisson + HTTPS request
│   ├── metrics.rs     # Prometheus exporter (counters: req_ok,
│   │                  # req_429, req_err; histogram latencia)
│   └── config.rs      # Parseo de CLI + env + topology JSON
```

**Dependencias** (Cargo.toml resumido):

```toml
[dependencies]
tokio       = { version = "1", features = ["full"] }
reqwest     = { version = "0.12", default-features = false,
                features = ["rustls-tls", "http2", "json"] }
rustls      = "0.23"
clap        = { version = "4", features = ["derive", "env"] }
rand        = "0.8"
rand_distr  = "0.4"    # Exponential
tracing     = "0.1"
tracing-subscriber = "0.3"
prometheus  = "0.13"
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
```

**CLI** (`sae-sim --help`):

```
USAGE: sae-sim [OPTIONS]

  --pool-size <N>               nº de SAEs a simular (default 100)
  --dkms-endpoints <LIST>       lista CSV de DKMSs, formato
                                "https://host1:port1,..." (requerido)
  --sae-cert-dir <DIR>          directorio con sae_NNN.{crt,key} por SAE
  --ca <PATH>                   cert CA del DKMS
  --sae-id-prefix <PREFIX>      default "sae_"
  --rate-hz <FLOAT>             req/s por SAE (default 0.5)
  --seed <U64>                  semilla PRNG (default: clock-derived)
  --duration <SECONDS>          duración total (default 0 = infinito)
  --key-size-bits <N>           default 256
  --log <PATH>                  fichero de log por línea (default stdout)
  --metrics-addr <HOST:PORT>    expose /metrics Prometheus (default :9200)
  --warmup-seconds <S>          espera antes de empezar el loop (default 0)
  --ramp                        si está, recibe escalones via control HTTP
                                en --control-addr (para integración con
                                el orquestador)
  --control-addr <HOST:PORT>    default :19100
```

**Comportamiento**:

1. Al arrancar, lee `dkms-endpoints` y `pool-size`. Construye dos
   pools: `dkms_pool[]` y `sae_pool[]` (IDs `sae_000001` ...).
2. Para cada SAE en `sae_pool`:
   - Elige uniformemente un DKMS de `dkms_pool` → es el "home DKMS"
     del SAE, fijo para su vida.
   - Carga `sae_id.crt` y `sae_id.key` del `sae-cert-dir`.
   - Crea cliente reqwest con `Identity::from_pem(cert+key)` y
     `Certificate::from_pem(ca)` → mTLS configurado.
3. Lanza una task tokio por SAE. Cada task:
   - Espera `Exp(rate-hz)` segundos (Poisson).
   - Elige uniformemente un peer del `sae_pool` excluyéndose a sí
     mismo.
   - `POST /api/v1/keys/{peer_sae}/enc_keys` al home DKMS.
   - Registra resultado en counters/histogram + log.
4. `/metrics` expone:
   - `sae_sim_requests_total{sae_id, status}` counter
   - `sae_sim_latency_seconds{sae_id}` histogram (buckets 1ms-10s)
   - `sae_sim_active_saes` gauge
   - `sae_sim_dkms_assignment{dkms_id}` gauge (cardinality limitada
     — verificable a posteriori)
5. Modo `--ramp`: en lugar de arrancar todos los SAEs de golpe, abre
   un servidor HTTP en `control-addr` con endpoints:
   - `POST /step {"active_saes": N}` activa los N primeros SAEs del
     pool (los siguientes quedan idle).
   - `GET /status` devuelve nº activos, throughput acumulado, p99.
   - `POST /stop` finaliza.
   El orquestador llama a `/step` cada `interval_seconds` con el
   nuevo N.

**Cómo encaja con el orquestador**:

- El deployment actual `dkms-loadtest:v1` se rebautiza
  `dkms-loadtest:v2` con la imagen del nuevo binario Rust.
- El handler `POST /orch/web/simulations/<id>/tests` en
  `api_orchestator.py` traduce los parámetros del POST a flags del
  binario:
  - `start_saes` → no se pasa directamente; el orquestador llama
    `/step {N=start_saes}` después del warmup.
  - `end_saes`, `step_saes`, `interval_seconds` → el orquestador
    hace el loop de escalones llamando `/step` con N creciente.
  - `per_sae_lambda` → CLI `--rate-hz`.
  - `key_size_bits` → CLI `--key-size-bits`.
  - `warmup_seconds` → CLI `--warmup-seconds`.

**Despliegue dentro del cluster** (evita CloudFlare):

El sae-sim corre como **Deployment K8s en el mismo namespace que la
simulación**, NO como cliente externo. Las peticiones HTTPS van por
DNS interno del cluster (`dkms-X.<ns>.svc.cluster.local:8443`) →
nunca tocan el ingress ni CloudFlare → no hay riesgo de rate-limit
del WAF ni de que un proxy intermedio tumbe la conexión por tráfico
"sospechoso".

```yaml
# Deployment generado por el orquestador (PodLoadTest):
apiVersion: apps/v1
kind: Deployment
metadata:
  name: dkms-loadtest
  namespace: sim-<id>           # mismo ns que los DKMSs
spec:
  replicas: 1                   # default; escalable a N réplicas
  template:
    spec:
      containers:
      - name: sae-sim
        image: pablopio/sae-sim:v1
        args:
          - --pool-size=10000
          - --dkms-endpoints=https://dkms-1:8443,https://dkms-2:8443,...
          - --rate-hz=1.0
          - --ramp
          - --control-addr=0.0.0.0:19100
          - --metrics-addr=0.0.0.0:9200
          - --results-dir=/results
        volumeMounts:
          - name: results
            mountPath: /results
        ports:
          - containerPort: 19100  # control
          - containerPort: 9200   # /metrics
      volumes:
        - name: results
          persistentVolumeClaim:
            claimName: loadtest-results
```

**Escalado horizontal**: si una sola réplica no aguanta los 10 000
SAEs (CPU pegado a límite del pod), el orquestador puede arrancar
`replicas: K` con `--pool-size=10000/K` y `--sae-id-offset=k·10000/K`
para que cada réplica simule un subconjunto disjunto de SAEs. El
endpoint `/step` se llama en paralelo a las K réplicas. Probable
necesidad: medir CPU en N=49 con S=10 000 antes de fijar K.

**Conectividad de red**: cada DKMS expone un Service ClusterIP
estándar (gestionado por el orquestador). El sae-sim lee los
endpoints del Service vía DNS interno. No requiere `hostNetwork`,
NetworkPolicy abierta entre pods del namespace (default en
`sim-<id>` según `pods.py`), y mTLS contra `tls.sae_client_ca` igual
que cualquier SAE real.

**Test plan del propio crate** (en `sae-sim/tests/`):

1. Unitario: el sampler genera distribución uniforme verificable
   con χ² al 5 %.
2. Unitario: intervalos exponenciales con media = 1/λ ± 5 %.
3. Integración: arranca contra un mock HTTP server, comprueba que
   con `pool-size=100`, `rate-hz=2`, `duration=30s` el total de
   requests está en `100·2·30 = 6 000 ± 5 %`.
4. Integración: con un DKMS mock que devuelve 429 con probabilidad
   p, la métrica `sae_sim_requests_total{status="429"}` converge a
   p·total ± varianza muestral.

**Estimación**: ~600-800 líneas de Rust + tests + Dockerfile.
1 sesión de implementación + 1 sesión de validación contra un
DKMS real.

### 9.3 Runner de la matriz completa

`scripts/escalado-tests/run-matrix.sh` (bash) que itere:

```bash
for N in 9 25 49 81 121; do
  for TOPO in ring grid star4; do
    ./scripts/escalado-tests/run-one.sh "$N" "$TOPO"
  done
done
```

Cada `run-one.sh`:
1. Crea sim via `POST /orch/web/simulations`.
2. Espera Ready + bootstrap completado (chequea
   `GET sdn:3002/priority` = N(N−1)).
3. Hace baseline de 30 s + verifica simetría (sec. 5.4/5.5).
4. Lanza loadtest con `POST /orch/web/simulations/<id>/tests`
   parámetros = la fila 4.1.
5. Polling `/orch/web/simulations/<id>/tests/<test_id>/status`
   cada 5 s. Aplica early-stop si las métricas Prometheus del
   sae-sim disparan los criterios 3.4.
6. Al terminar, exporta CSVs desde Prometheus + logs DKMS
   (`generator.state`) a `results/<topo>-N<N>/`.
7. DELETE sim.

### 9.4 Extracción de datos del cluster a local

**Objetivo**: tras cada corrida, descargar a la máquina local todos
los CSV y logs para analizarlos con Python sin depender de
infraestructura del cluster.

**Qué genera cada componente**:

| Productor | Output | Ubicación en el pod |
|-----------|--------|---------------------|
| sae-sim   | `requests-<sae_range>-<ts>.csv` (una fila por petición) | `/results/` (PVC `loadtest-results`) |
| sae-sim   | `assignment-<ts>.csv` (mapping SAE→DKMS al arranque) | `/results/` |
| sae-sim   | `metrics-<ts>.prom` (snapshot final /metrics) | `/results/` |
| DKMS pods | logs `generator.state`, transiciones QoS | stdout → recogido por promtail / `kubectl logs` |
| SDN pod   | snapshots de `/priority` y `/allocations` | el runner los descarga vía HTTP |

**Esquema de `requests-*.csv`**:
```
ts_unix_ms,sae_id,peer_sae,dkms_id,status_code,latency_ms,error_kind
1747486800123,sae_001234,sae_005678,dkms-12,200,4.3,
1747486800189,sae_001234,sae_002345,dkms-12,429,1.1,
1747486800210,sae_005678,sae_001234,dkms-7,200,3.7,
```
Append-only desde cada réplica del sae-sim. El runner hace `tar` al
final para deduplicar nombres.

**Flujo de extracción** (al final de cada corrida, ejecutado por
`run-one.sh`):

```bash
RUN_ID="$(date +%Y%m%d-%H%M%S)-${TOPO}-N${N}"
LOCAL_DIR="./results/${RUN_ID}"
mkdir -p "$LOCAL_DIR"

# 1. CSVs del sae-sim — del PVC vía un pod intermedio.
kubectl -n "sim-${SIM_ID}" exec deploy/dkms-loadtest -- \
  tar -C /results -czf - . > "${LOCAL_DIR}/sae-sim.tar.gz"

# 2. Logs DKMS — un fichero por pod, sólo líneas de interés.
for pod in $(kubectl -n "sim-${SIM_ID}" get pods -l app=dkms -o name); do
  kubectl -n "sim-${SIM_ID}" logs "$pod" \
    | grep -E "generator\.state|priority\.transition" \
    > "${LOCAL_DIR}/${pod##*/}.log"
done

# 3. Snapshots SDN.
kubectl -n "sim-${SIM_ID}" exec deploy/sdn -- \
  curl -s http://localhost:3002/priority > "${LOCAL_DIR}/sdn-priority.json"
kubectl -n "sim-${SIM_ID}" exec deploy/sdn -- \
  curl -s http://localhost:3002/allocations > "${LOCAL_DIR}/sdn-alloc.json"

# 4. Manifest de la corrida — versión orquestador, imágenes, semilla.
echo "topo=${TOPO} N=${N} seed=${SEED} ..." > "${LOCAL_DIR}/manifest.txt"
```

Resultado: `./results/<run_id>/` con todo lo necesario para análisis
offline. Total esperado por corrida: ~50-200 MB (la mayoría son las
filas de `requests-*.csv`; con S=10 000 y rampa de 50 min, ~30 M
filas → ~1 GB sin gzip, ~150 MB con gzip).

### 9.5 Plotter — análisis local con Python

Carpeta `scripts/escalado-tests/analyze/` con:

- `requirements.txt`: `pandas`, `numpy`, `matplotlib`, `seaborn`,
  `scipy` (Spearman ρ y χ²).
- `load_run.py`: descomprime el tarball, parsea CSVs y logs DKMS a
  DataFrames, persiste a `parquet` para iteraciones rápidas.
- `figures.py`: genera todas las figuras de la corrida.

Comando típico:
```bash
cd scripts/escalado-tests/analyze/
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt

python load_run.py ../../../results/20260518-1430-ring-N49/
python figures.py ../../../results/20260518-1430-ring-N49/
```

**Figuras producidas por corrida**:

1. `t_a_Saturated_histograma.png` — distribución del tiempo a
   Saturated por buffer. Estrecha en anillo; correlacionada con
   posición en malla; bimodal (intra/inter-brazo) en estrella.
2. `rate_vs_sdn_timeseries.png` — `observed_keys_per_s` vs
   `sdn_rate_keys_per_s` por buffer en el tiempo.
3. `throughput_sae_boxplot.png` — boxplot del throughput por SAE en
   steady state, agrupados por DKMS.
4. `histograma_saes_por_dkms.png` — verifica uniformidad de la
   asignación SAE→DKMS al arranque (test χ²).
5. `heatmap_demanda_inter_dkms.png` — matriz `N×N` de peticiones
   por par DKMS. Esperado: uniforme. Detecta sesgos en el muestreo
   del peer.
6. `429_rate_vs_step.png` — drop rate (% 429) vs nº SAEs activos.
   Para verificar el punto de saturación SAE-side y elegir λ.
7. `latency_p99_vs_step.png` — p99 latencia /enc_keys vs escalón.
   Indicador de salud del DKMS HTTP server.

Notebook resumen `compare_runs.ipynb` para comparar varias corridas
(cross-topología, cross-N) usando los Parquet pre-procesados.
5. `heatmap_demanda_inter_dkms.png` — verifica uniformidad de la
   selección peer SAE.

### 9.6 Validación del modelo teórico antes de escalar

**Primera corrida obligatoria**: N=9, anillo, baseline sin SAEs.

- Tiempo esperado a Saturated por buffer: 10.1 s (tabla 5.3).
- Si la observación cae en `[5, 20] s` → modelo OK, seguir.
- Si cae fuera → depurar **antes** de gastar tiempo de pared en N
  grandes. Hipótesis a chequear:
  - Bootstrap incompleto (sec. 7.1).
  - `[generator]` no se aplicó (override falló — config-rs).
  - SDN tiene phantom-Priority (CLAUDE.md).

## 10. Apéndice — derivación abreviada de "flujos por edge"

**Anillo**: para N par con shortest-path único (rompiendo empates
arbitrariamente), cada edge lleva los pares (i,j) cuya distancia es
mínima cruzando ese edge. Por simetría, cada uno de los N edges lleva
exactamente `N²/4` flujos *ordenados* (`N(N−1)/2N · 2 = (N−1)/2`
flujos por dirección ... — la cuenta limpia da `⌊N²/4⌋`).

**Malla cuadrada n×n con XY-routing**: cada par (a,b) usa primero el
edge horizontal, luego vertical (sin desempate). La carga horizontal
máxima ocurre en el edge central de la fila central, con cota superior
`n · (n²−1)/4` ≈ `n³/4` flujos ordenados. Con routing balanceado
(YX en mitad de pares) la carga máxima baja a ~`n³/8`. Como
aproximación general en el documento se ha usado `n³/4 = N^{3/2}/4`
(conservadora — la rate teórica resultante es **límite inferior**, el
sistema real puede ir algo mejor).

**Estrella k·4+1**: derivado en la sección 5.2. El edge centro—a₁
del brazo A lleva todos los flujos con un endpoint en {a₁,…,a_k} (k
nodos) y otro endpoint en el resto del grafo (1 centro + 3k nodos del
resto de brazos) = k(3k+1). Por ambas direcciones: `2k(3k+1)`. Para
k ≥ 2, `6k² + 2k ≈ 6k²`.

---

**Cambios sugeridos al lector**:
- Aceptar/rechazar la sustitución de N por la serie limpia
  {9, 25, 49, 81, 121}.
- Aceptar/rechazar el cambio de la rampa SAE absoluta (100→1000) a
  relativa (r·N).
- Confirmar el R0 = 2000 keys/s o subirlo para acortar wall-clock.
