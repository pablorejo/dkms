#!/usr/bin/env python3
"""Genera la página de resultados de la campaña 2026-09 para la web
(`~/Documentos/web_dkms`, Astro): una página estática por idioma, con las
figuras SVG incrustadas y las tablas, en el mismo estilo que
`public/results/scale-laws.html` (se le copia el bloque <style>).

    campaign_web.py --analysis DIR --web DIR [--narrative narrative.json]

  DIR/metrics.json y DIR/figs/*.svg vienen de campaign_analyze.py. El
  narrative.json lleva los textos de lectura (EN/ES) por sección; sin él se
  emiten marcadores «(pendiente)» para que la página siempre sea válida.

Salida: <web>/public/results/campaign-2026-09.html (EN),
        <web>/public/resultados/campana-2026-09.html (ES) y los datos
        descargables en <web>/public/results/campaign-2026-09-data/.
"""
import argparse
import html
import json
import os
import re
import shutil

FAMILIES = ["estrella", "anillo", "puente", "malla", "rgg", "aleatoria"]
LABEL = {
    "en": {"estrella": "Star", "anillo": "Ring (Cₙ)", "puente": "Bridge", "malla": "Mesh (grid)",
           "rgg": "RGG (QKD at distance)", "aleatoria": "Random"},
    "es": {"estrella": "Estrella", "anillo": "Anillo (Cₙ)", "puente": "Puente", "malla": "Malla (rejilla)",
           "rgg": "RGG (QKD a distancia)", "aleatoria": "Aleatoria"},
}
COLORS = {"estrella": "#e6a100", "anillo": "#38d3f0", "puente": "#ff6b6b",
          "malla": "#4cd97b", "rgg": "#9d7bfa", "aleatoria": "#93a1bb"}
NS = list(range(10, 101, 10))
DATA_DIR = "campaign-2026-09-data"

T = {
    "en": {
        "title": "Six topologies, ten sizes, three loads",
        "kicker": "dkms-rust · CESGA FT3 · N=10–100 · 6 topologies × 3 loads · QKD quditto per link · factory defaults",
        "back": "← D-KMS — back to the site",
        "stand": "Every cell is a full deployment on one 64-core node: SDN + N×(QKC, ORR, DKMS) + one simulated QKD link (quditto) per edge — designed with R₀=2000 keys/s, α=0.2 dB/km and 5 km of fibre, or the real geometric distance in the RGG — brought up from empty buffers and driven through rest, paced load and closed-loop saturation. Same factory defaults and the same harness across the 60 cells; the three affected by the replay finding were re-measured with the fixed binary.",
        "toc": "Contents",
        "sec_glossary": "How to read this report",
        "glossary_intro": "Every number on this page is one of the quantities below, measured the same way in the 60 cells. The phases are the three loads the request asked for, plus a recovery window.",
        "glossary": [
            ("Cell", "one topology at one size: a full deployment (SDN + N × QKC/ORR/DKMS + a quditto KME simulator per edge) on a 64-core node, from empty buffers to the end of the recovery window, 22–36 minutes."),
            ("L0 — rest", "no application traffic; the generators fill every ordered pair's transport-key buffer to 4 096 keys. Ends 60 s after the last pair is full or at 600 s."),
            ("L1 — paced load", "one SAE flow per ordered pair, rate-capped so the aggregate offered load is 50 % of the fibre ceiling (at most 30 000 keys/s), for 300 s."),
            ("L2 — closed-loop saturation", "one SAE flow per ordered pair asking for the next key as soon as the previous one arrives, with a 100 ms pause after each 429/503, for 300 s."),
            ("REC — recovery", "no traffic again for up to 180 s: how the buffers refill after the saturation."),
            ("R₀, α, d — the link model", "every QKD link is a quditto simulator minting 256-bit keys at R₀·10^(−αd/10): R₀ is the rate at zero distance (2 000 keys/s here), α the fibre attenuation (0.2 dB/km, standard single-mode fibre at 1 550 nm) and d the link length (5 km in five families; the geometric distance of each edge, 2–47 km, in the RGG). The SDN sizes every edge with the same formula and, since the in-situ estimator exists, with the measured rate when it drifts from it."),
            ("Fibre ceiling Σcap/ħ", "the aggregate rate the fibre allows if every ordered pair received the same share and every key crossed the mean number of hops: the sum of the link capacities divided by the mean path length. Link capacity is R₀·10^(−αd/10)."),
            ("Bottleneck link ceiling", "the aggregate rate at which the most loaded link (under uniform, shortest-path demand) would saturate first."),
            ("Sustained, stock-corrected", "keys served in the last two thirds of L2 minus the key stock drained from the three stores (DKMS buffers, QKC rings, KME) in the same window, divided by the window: what the network actually produced, not what it took from its reserves."),
            ("Fibre utilisation", "QKD keys consumed by the QKCs (Σtaken) over what every link could generate (Σcap) in the window: 1.0 means every link is running dry at its production rate."),
            ("Effective hops", "keys consumed on the fibre per key delivered end to end: the average path length the served traffic actually paid, compared with the uniform mean distance."),
            ("Jain index", "fairness of the keys served per ordered pair in L2: 1.0 when every pair got the same, 1/pairs when a single pair got everything."),
            ("Worst pair / uniform share", "keys served to the pair that got fewest, divided by the average per pair: 1.0 is a perfectly even split, 0.01 means that pair got one percent of its equal share."),
            ("Rejected fraction", "429 (SAE token bucket exhausted) and 503 (no key in the buffer right now) over all L2 requests: back-pressure, not errors — a client that is told to wait instead of being handed a bad key."),
            ("p50 / p90 / p99", "percentiles of the request latency measured by the load client, including its own queueing on the node."),
            ("Integrity round", "an ETSI-014 enc_keys/dec_keys exchange with byte comparison for every ordered pair among 10 nodes, after L1 and at the end of the cell."),
            ("Expired keys", "transport keys a DKMS emitted and dropped because the receiver's acknowledgement never came within 30 s: material lost, never corrupt."),
        ],
        "sec_qkd": "The QKD link model: R₀, α and distance",
        "qkd_cols": ["N", "edges", "distance min / median / max (km)", "link capacity min / median / max (keys/s)", "Σcap (keys/s)", "bottleneck link (keys/s)"],
        "qkd_static_title": "Link capacity R₀·10^(−αd/10) for the campaign's R₀=2000 keys/s and α=0.2 dB/km",
        "qkd_rgg_title": "RGG edges as built, per N: real distances and the capacities they give (the other five families: every edge 5 km, 1 588.7 keys/s)",
        "qkd_static_cols": ["distance (km)", "keys/s", "% of R₀", "who uses it"],
        "sec_topos": "The six topologies",
        "topos_reading": "Drawn at N=20. Every link is QKD: capacity R₀·10^(−αd/10) with R₀=2000 keys/s, α=0.2 dB/km and d=5 km (1588.7 keys/s) — except in the RGG, where each edge carries its own geometric distance (2–47 km → 233–1811 keys/s). Facts at N=100.",
        "sec_families": "Topology by topology",
        "families_intro": "The same cells read per family: how each size filled from empty (left), and what the load did at N=50 and N=100 — served rate, rejections and buffer stock through the paced load, the saturation and the recovery. Each block ends with that family's numbers across N.",
        "fam_cols": ["N", "ceiling keys/s", "fill: t_full s / stock at close", "L1 served/offered", "L2 sustained (×ceiling)", "fibre util.", "eff. hops / mean", "Jain", "worst pair", "rejected", "p99 ms", "expired keys"],
        "sec_theory": "What the fibre allows",
        "sec_l0": "L0 — rest: filling from empty",
        "sec_l1": "L1 — paced load at half the fibre ceiling",
        "sec_l2": "L2 — closed-loop saturation",
        "sec_fair": "Fairness between pairs",
        "sec_lat": "Latency",
        "sec_rec": "Recovery after saturation",
        "sec_findings": "Two things the numbers found in the code",
        "sec_health": "Integrity and health",
        "sec_est": "The rate estimator and the SDN",
        "sec_res": "Resources on the node",
        "sec_data": "Data, provenance and reproducibility",
        "data_reading": "The raw metrics of the 60 cells, the tables and the full analysis are downloadable below. Binary provenance: 57 cells ran the same Rust code (SHAs 4188eca, f315d03, e3464e8 and d60714a differ only in the harness and the analyser); star N=90, star N=100 and bridge N=100 were re-measured with 760b0d1, which carries the per-frame seal fix d40d2fc. Factory defaults in every cell: control plane over mTLS, ACKs over ETSI-020, strict bootstrap trust, per-frame seal required, end-to-end seal between DKMSs, TLS negotiating only X25519MLKEM768 with ML-DSA-65 certificates.",
        "data_files": [("metrics.json", "every metric of every cell (JSON)"), ("TABLES.md", "all tables (Markdown)"), ("ANALYSIS.md", "the full analysis (Spanish, Markdown)"), ("replay_fix.json", "the before/after of the replay fix")],
        "data_repro": "This campaign supersedes the July 2026 scale-laws report (54 configurations, 3 topologies, N ≤ 70, an earlier binary), kept at <a href=\"/results/scale-laws.html\">/results/scale-laws.html</a> for the record only. Reproduce: <code>campaign_sync.sh</code> (with an empty queue) → <code>campaign_submit.sh</code> → pull <code>campaign-2026-09/cells/</code> → <code>campaign_analyze.py</code> → <code>campaign_web.py</code>, all under <code>tests/local-mesh/</code> in the dkms-rust repository. Incidents met on the way, each now a guard in the harness: home space and inode quotas on the cluster (raw CSVs to the node's scratch, toolchain to $STORE), a resubmission of cells still running (guard by queue name), an rsync of the harness while cells were in flight (bash reads scripts in blocks; the cells exited silently — the batch script now runs a scratch copy), and a client that fell back to a curl unable to load ML-DSA certificates (repo root honoured from the copy).",
        "sec_method": "Method",
        "method": "One SLURM job per cell (full 64-core node, 48–200 GB). <b>L0</b>: from empty buffers until every ordered pair reaches 4096 transport keys (+60 s) or 600 s. <b>L1</b>: one flow per ordered pair, rate-capped so the aggregate offered load is 50 % of the fibre ceiling Σcap/ħ (capped at 30 000 keys/s), 300 s. <b>L2</b>: one closed-loop flow per ordered pair with a 100 ms pause after each 429/503, 300 s; the sustained figure is corrected by the buffer stock drained during the window ((served − Δstock)/window over the last 2/3 of L2). <b>REC</b>: up to 180 s for the generator to refill. Integrity: an ETSI-014 enc/dec round with byte comparison on 10 nodes after L1 and at the end; under L2, recv_corrupt and 503 counts. Every module logs its state every 5 s; a sampler aggregates them. Sampler cadence degrades at N ≥ 80 in the dense families (the load client starves it): where fewer than two records fall in the last two thirds of L2, the stock correction uses the last two records of the phase and the value is marked with *.",
        "facts": ["edges", "mean hops", "diameter", "fibre ceiling (keys/s)", "bottleneck link (keys/s)"],
        "pending": "(reading pending)",
    },
    "es": {
        "title": "Seis topologías, diez tamaños, tres cargas",
        "kicker": "dkms-rust · CESGA FT3 · N=10–100 · 6 topologías × 3 cargas · quditto QKD por enlace · defaults de fábrica",
        "back": "← D-KMS — volver a la web",
        "stand": "Cada celda es un despliegue completo en un nodo de 64 cores: SDN + N×(QKC, ORR, DKMS) + un enlace QKD simulado (quditto) por arista — diseñado con R₀=2000 claves/s, α=0,2 dB/km y 5 km de fibra, o la distancia geométrica real en la RGG — levantado desde buffers vacíos y llevado por reposo, carga pautada y saturación en bucle cerrado. Los mismos defaults de fábrica y el mismo arnés en las 60 celdas; las tres afectadas por el hallazgo del replay se volvieron a medir con el binario corregido.",
        "toc": "Índice",
        "sec_glossary": "Cómo leer este informe",
        "glossary_intro": "Cada número de esta página es una de las magnitudes de abajo, medida igual en las 60 celdas. Las fases son las tres cargas que pedía el encargo, más una ventana de recuperación.",
        "glossary": [
            ("Celda", "una topología a un tamaño: un despliegue completo (SDN + N × QKC/ORR/DKMS + un simulador de KME quditto por arista) en un nodo de 64 cores, desde los buffers vacíos hasta el final de la ventana de recuperación, 22–36 minutos."),
            ("L0 — reposo", "sin tráfico de aplicación; los generadores llenan el buffer de claves de transporte de cada par ordenado hasta 4 096 claves. Termina 60 s después de que el último par se llene o a los 600 s."),
            ("L1 — carga pautada", "un flujo SAE por par ordenado, limitado para que la carga ofrecida agregada sea el 50 % del techo de fibra (como mucho 30 000 claves/s), durante 300 s."),
            ("L2 — saturación en bucle cerrado", "un flujo SAE por par ordenado que pide la siguiente clave en cuanto llega la anterior, con una pausa de 100 ms tras cada 429/503, durante 300 s."),
            ("REC — recuperación", "otra vez sin tráfico, hasta 180 s: cómo se rellenan los buffers tras la saturación."),
            ("R₀, α, d — el modelo del enlace", "cada enlace QKD es un simulador quditto que acuña claves de 256 bits a R₀·10^(−αd/10): R₀ es la tasa a distancia cero (2 000 claves/s aquí), α la atenuación de la fibra (0,2 dB/km, fibra monomodo estándar a 1 550 nm) y d la longitud del enlace (5 km en cinco familias; la distancia geométrica de cada arista, 2–47 km, en la RGG). La SDN dimensiona cada arista con la misma fórmula y, desde que existe el estimador in situ, con la tasa medida cuando se aparta de ella."),
            ("Techo de fibra Σcap/ħ", "la tasa agregada que permite la fibra si cada par ordenado recibiera la misma parte y cada clave cruzara el número medio de saltos: la suma de las capacidades de los enlaces entre la longitud media de camino. La capacidad de un enlace es R₀·10^(−αd/10)."),
            ("Techo del enlace cuello", "la tasa agregada a la que el enlace más cargado (con demanda uniforme por camino más corto) saturaría primero."),
            ("Sostenido corregido por stock", "claves servidas en los últimos dos tercios de L2 menos el stock de claves drenado de los tres almacenes (buffers de los DKMS, anillos de los QKC, KME) en la misma ventana, entre la ventana: lo que la red produjo de verdad, no lo que sacó de sus reservas."),
            ("Utilización de la fibra", "claves QKD consumidas por los QKC (Σtaken) entre lo que cada enlace podía generar (Σcap) en la ventana: 1,0 significa que todos los enlaces se vacían a su ritmo de producción."),
            ("Saltos efectivos", "claves consumidas en la fibra por cada clave entregada extremo a extremo: la longitud media de camino que el tráfico servido pagó de verdad, frente a la distancia media uniforme."),
            ("Índice de Jain", "equidad de las claves servidas por par ordenado en L2: 1,0 cuando todos los pares reciben lo mismo, 1/pares cuando un solo par se lo lleva todo."),
            ("Peor par / reparto uniforme", "claves servidas al par que menos recibió, entre la media por par: 1,0 es un reparto perfectamente parejo, 0,01 significa que ese par recibió el uno por ciento de su parte igual."),
            ("Fracción rechazada", "429 (bucket de tokens del SAE agotado) y 503 (ninguna clave en el buffer ahora mismo) sobre todas las peticiones de L2: contrapresión, no errores; a un cliente se le dice que espere en vez de entregarle una clave mala."),
            ("p50 / p90 / p99", "percentiles de la latencia de petición medida por el cliente de carga, incluida su propia cola en el nodo."),
            ("Ronda de integridad", "un intercambio ETSI-014 enc_keys/dec_keys con comparación de bytes para cada par ordenado entre 10 nodos, tras L1 y al final de la celda."),
            ("Claves expiradas", "claves de transporte que un DKMS emitió y descartó porque el acuse del receptor no llegó en 30 s: material perdido, nunca corrupto."),
        ],
        "sec_qkd": "El modelo del enlace QKD: R₀, α y distancia",
        "qkd_cols": ["N", "aristas", "distancia mín / mediana / máx (km)", "capacidad del enlace mín / mediana / máx (claves/s)", "Σcap (claves/s)", "enlace cuello (claves/s)"],
        "qkd_static_title": "Capacidad del enlace R₀·10^(−αd/10) con los R₀=2000 claves/s y α=0,2 dB/km de la campaña",
        "qkd_rgg_title": "Aristas de la RGG tal como se construyeron, por N: distancias reales y capacidades resultantes (las otras cinco familias: toda arista a 5 km, 1 588,7 claves/s)",
        "qkd_static_cols": ["distancia (km)", "claves/s", "% de R₀", "quién la usa"],
        "sec_topos": "Las seis topologías",
        "topos_reading": "Dibujadas a N=20. Todos los enlaces son QKD: capacidad R₀·10^(−αd/10) con R₀=2000 claves/s, α=0.2 dB/km y d=5 km (1588,7 claves/s) — salvo en la RGG, donde cada arista lleva su distancia geométrica (2–47 km → 233–1811 claves/s). Datos a N=100.",
        "sec_families": "Topología a topología",
        "families_intro": "Las mismas celdas leídas por familia: cómo se llenó cada tamaño desde vacío (izquierda) y qué le hizo la carga a N=50 y N=100 — tasa servida, rechazos y stock de buffers a lo largo de la carga pautada, la saturación y la recuperación. Cada bloque termina con los números de esa familia a lo largo de N.",
        "fam_cols": ["N", "techo claves/s", "llenado: t_full s / stock al cierre", "L1 servido/ofrecido", "L2 sostenido (×techo)", "util. fibra", "saltos ef. / medios", "Jain", "peor par", "rechazado", "p99 ms", "claves expiradas"],
        "sec_theory": "Lo que permite la fibra",
        "sec_l0": "L0 — reposo: llenado desde vacío",
        "sec_l1": "L1 — carga pautada a la mitad del techo de fibra",
        "sec_l2": "L2 — saturación en bucle cerrado",
        "sec_fair": "Equidad entre pares",
        "sec_lat": "Latencia",
        "sec_rec": "Recuperación tras la saturación",
        "sec_findings": "Dos cosas que los números encontraron en el código",
        "sec_health": "Integridad y salud",
        "sec_est": "El estimador de tasa y la SDN",
        "sec_res": "Recursos del nodo",
        "sec_data": "Datos, procedencia y reproducibilidad",
        "data_reading": "Las métricas crudas de las 60 celdas, las tablas y el análisis completo se pueden descargar abajo. Procedencia del binario: 57 celdas corrieron el mismo código Rust (los SHA 4188eca, f315d03, e3464e8 y d60714a solo difieren en el arnés y el analizador); estrella N=90, estrella N=100 y puente N=100 se volvieron a medir con 760b0d1, que lleva el fix del sello por frame d40d2fc. Defaults de fábrica en todas: plano de control con mTLS, ACKs por ETSI-020, confianza de arranque estricta, sello por frame obligatorio, sello extremo a extremo entre DKMS, TLS que solo negocia X25519MLKEM768 con certificados ML-DSA-65.",
        "data_files": [("metrics.json", "todas las métricas de todas las celdas (JSON)"), ("TABLES.md", "todas las tablas (Markdown)"), ("ANALYSIS.md", "el análisis completo (Markdown)"), ("replay_fix.json", "el antes/después del fix del replay")],
        "data_repro": "Esta campaña sustituye al informe de leyes de escala de julio de 2026 (54 configuraciones, 3 topologías, N ≤ 70, un binario anterior), que se conserva en <a href=\"/resultados/leyes-de-escala.html\">/resultados/leyes-de-escala.html</a> solo como registro. Reproducir: <code>campaign_sync.sh</code> (con la cola vacía) → <code>campaign_submit.sh</code> → traer <code>campaign-2026-09/cells/</code> → <code>campaign_analyze.py</code> → <code>campaign_web.py</code>, todo en <code>tests/local-mesh/</code> del repositorio dkms-rust. Incidencias por el camino, cada una convertida en una guarda del arnés: cuotas de espacio e inodos del home del clúster (CSV crudos al scratch del nodo, toolchain a $STORE), un resometido de celdas aún en vuelo (guarda por nombre en cola), un rsync del arnés con celdas corriendo (bash lee los guiones por bloques; las celdas salieron en silencio — el sbatch corre ahora una copia en scratch) y un cliente que caía a un curl incapaz de cargar certificados ML-DSA (raíz del repo respetada desde la copia).",
        "sec_method": "Método",
        "method": "Un job SLURM por celda (nodo entero de 64 cores, 48–200 GB). <b>L0</b>: desde buffers vacíos hasta que todos los pares ordenados llegan a 4096 claves de transporte (+60 s) o 600 s. <b>L1</b>: un flujo por par ordenado, pautado para que la oferta agregada sea el 50 % del techo de fibra Σcap/ħ (acotado a 30 000 claves/s), 300 s. <b>L2</b>: un flujo en bucle cerrado por par ordenado con 100 ms de pausa tras cada 429/503, 300 s; la cifra sostenida se corrige con el stock de buffers drenado en la ventana ((servido − Δstock)/ventana sobre los últimos 2/3 de L2). <b>REC</b>: hasta 180 s para que el generador rellene. Integridad: una ronda ETSI-014 enc/dec con comparación de bytes en 10 nodos tras L1 y al final; bajo L2, recv_corrupt y los 503. Cada módulo escribe su estado cada 5 s; un muestreador los agrega. La cadencia del muestreador se degrada a N ≥ 80 en las familias densas (el cliente de carga lo deja sin CPU): donde caen menos de dos registros en los últimos dos tercios de L2, la corrección por stock usa los dos últimos registros de la fase y el valor se marca con *.",
        "facts": ["aristas", "saltos medios", "diámetro", "techo de fibra (claves/s)", "enlace cuello (claves/s)"],
        "pending": "(lectura pendiente)",
    },
}

# (sección, figuras, tablas): el orden de lectura de la página
FIG_SECTIONS = [
    ("sec_theory", ["ceilings", "hops"], ["techo_fibra", "mean_hops", "techo_cuello"]),
    ("sec_l0", ["l0_fill_time", "l0_fill_slope", "l0_pairs_full"], ["t_full_s", "l0_fill_slope", "l0_stock_end", "l0_pairs_full"]),
    ("sec_l1", ["l1_served_vs_offered"], ["l1_served", "l1_429_503"]),
    ("sec_l2", ["l2_sustained", "l2_vs_ceiling", "l2_fibre_utilisation", "l2_effective_hops", "l2_reject"],
     ["l2_sustained", "l2_ratio", "l2_util", "l2_hops", "l2_served", "l2_reject", "l2_429_503"]),
    ("sec_fair", ["fairness_cdf_n50", "fairness_cdf_n100", "l2_jain", "l2_min_pair_share", "l2_pairs_zero"],
     ["l2_jain", "l2_minshare", "l2_p10share", "l2_pair_minmedmax", "l2_pairs_zero"]),
    ("sec_lat", ["l1_latency_p50", "l1_latency", "l2_latency_p50", "l2_latency_p99"],
     ["l1_p50", "l1_p90", "l1_p99", "l2_p50", "l2_p90", "l2_p99"]),
    ("sec_rec", ["rec_time"], ["t_recover_s", "rec_stock"]),
    ("sec_findings", ["replay_fix", "star_hub"], ["expired", "intake"]),
    ("sec_health", [], ["health", "keys_429", "keys_client_err"]),
    ("sec_est", ["estimator"], ["est_ratio", "sdn_zero"]),
    ("sec_res", ["l2_cpu_modules", "rss_peak", "bringup"], ["cpu", "cpu_sae", "rss", "bringup"]),
]

TABLE_TITLES = {
    "en": {"techo_fibra": "Fibre ceiling Σcap/ħ (keys/s)", "mean_hops": "Mean hops (ordered pairs)",
           "techo_cuello": "Bottleneck-link ceiling (keys/s)",
           "t_full_s": "Time until every pair is full (s; — = not within 600 s)",
           "l0_fill_slope": "Fill slope, all pairs (keys/s)",
           "l0_stock_end": "Stock at the close of L0 (% of N·(N−1)·4096)",
           "l0_pairs_full": "Pairs full at the close of L0 (%)",
           "l1_served": "L1 served (keys/s) / offered", "l1_429_503": "L1 rejections 429 / 503 (count)",
           "l1_p50": "L1 latency p50 (ms)", "l1_p90": "L1 latency p90 (ms)", "l1_p99": "L1 latency p99 (ms)",
           "l2_sustained": "L2 sustained, stock-corrected (keys/s; * = window relaxed to the last two sampler records, the sampler starved by the load client at N ≥ 80)",
           "l2_ratio": "L2 sustained / uniform-demand ceiling", "l2_util": "L2 fibre utilisation (Σtaken/Σcap)",
           "l2_hops": "L2 effective hops per delivered key (vs uniform mean)",
           "l2_served": "L2 served, raw (keys/s over the whole 300 s, stock included)",
           "l2_reject": "L2 rejected fraction (429/503)", "l2_429_503": "L2 rejections 429 / 503 (count)",
           "l2_jain": "L2 Jain fairness index (keys per pair)", "l2_minshare": "L2 worst pair / uniform share",
           "l2_p10share": "L2 10th-percentile pair / uniform share",
           "l2_pair_minmedmax": "L2 keys per pair: min / median / max",
           "l2_pairs_zero": "L2 ordered pairs served no key at all",
           "l2_p50": "L2 latency p50 (ms)", "l2_p90": "L2 latency p90 (ms)", "l2_p99": "L2 latency p99 (ms)",
           "t_recover_s": "Recovery to full after L2 (s; — = not within 180 s)",
           "rec_stock": "Stock at the end of the recovery window (%)",
           "health": "recv_corrupt / peel_failed / frame-auth rejects / dead+panics / exchanges with different bytes (429/503 in the rounds are backpressure, not counted here)",
           "keys_429": "Integrity rounds: exchanges refused with 429/503 (back-pressure; both rounds)",
           "keys_client_err": "Integrity rounds the CLIENT could not run (its curl cannot load an ML-DSA certificate) — not a system result",
           "expired": "Keys expired at the sender (emitted, no ACK within 30 s — material discarded, never corrupt)",
           "intake": "Frames dropped by the QKC's bounded intake queue (≥; only nodes 1–2 keep logs; node 1 is the star's hub)",
           "est_ratio": "In-situ QKD rate estimate / quditto formula (mean over links, end of L0)",
           "sdn_zero": "Pairs with a zero SDN rate at the close of L0 (%) — those whose buffers are full, so there is nothing to fill: compare with the pairs-full row of L0",
           "cpu": "Modules CPU under L2 (% of 6400)", "cpu_sae": "Load client CPU under L2 (% of 6400)",
           "rss": "Peak RSS under L2 (MB)", "bringup": "Bring-up of the whole cell (s)"},
    "es": {"techo_fibra": "Techo de fibra Σcap/ħ (claves/s)", "mean_hops": "Saltos medios (pares ordenados)",
           "techo_cuello": "Techo del enlace cuello (claves/s)",
           "t_full_s": "Tiempo hasta todos los pares a tope (s; — = no en 600 s)",
           "l0_fill_slope": "Pendiente de llenado, todos los pares (claves/s)",
           "l0_stock_end": "Stock al cierre de L0 (% de N·(N−1)·4096)",
           "l0_pairs_full": "Pares a tope al cierre de L0 (%)",
           "l1_served": "L1 servido (claves/s) / ofrecido", "l1_429_503": "L1 rechazos 429 / 503 (cuenta)",
           "l1_p50": "L1 latencia p50 (ms)", "l1_p90": "L1 latencia p90 (ms)", "l1_p99": "L1 latencia p99 (ms)",
           "l2_sustained": "L2 sostenido corregido por stock (claves/s; * = ventana relajada a los dos últimos registros del muestreador, que el cliente de carga deja sin CPU a N ≥ 80)",
           "l2_ratio": "L2 sostenido / techo de demanda uniforme", "l2_util": "L2 utilización de la fibra (Σtaken/Σcap)",
           "l2_hops": "L2 saltos efectivos por clave entregada (vs media uniforme)",
           "l2_served": "L2 servido bruto (claves/s sobre los 300 s, stock incluido)",
           "l2_reject": "L2 fracción rechazada (429/503)", "l2_429_503": "L2 rechazos 429 / 503 (cuenta)",
           "l2_jain": "L2 índice de Jain (claves por par)", "l2_minshare": "L2 peor par / reparto uniforme",
           "l2_p10share": "L2 par del percentil 10 / reparto uniforme",
           "l2_pair_minmedmax": "L2 claves por par: mín / mediana / máx",
           "l2_pairs_zero": "L2 pares ordenados sin ninguna clave servida",
           "l2_p50": "L2 latencia p50 (ms)", "l2_p90": "L2 latencia p90 (ms)", "l2_p99": "L2 latencia p99 (ms)",
           "t_recover_s": "Recuperación a tope tras L2 (s; — = no en 180 s)",
           "rec_stock": "Stock al final de la ventana de recuperación (%)",
           "health": "recv_corrupt / peel_failed / rechazos del sello / muertos+panics / intercambios con bytes distintos (los 429/503 de las rondas son contrapresión y no cuentan aquí)",
           "keys_429": "Rondas de integridad: intercambios rechazados con 429/503 (contrapresión; ambas rondas)",
           "keys_client_err": "Rondas de integridad que el CLIENTE no pudo ejecutar (su curl no carga un certificado ML-DSA): no es un resultado del sistema",
           "expired": "Claves expiradas en el emisor (emitidas sin ACK en 30 s: material descartado, nunca corrupto)",
           "intake": "Frames descartados por la cola de entrada acotada del QKC (≥; solo los nodos 1–2 conservan log; el nodo 1 es el hub de la estrella)",
           "est_ratio": "Estimación in situ de tasa QKD / fórmula de quditto (media de los enlaces, fin de L0)",
           "sdn_zero": "Pares con tasa cero de la SDN al cierre de L0 (%): los que tienen el buffer lleno y nada que rellenar; compárese con la fila de pares a tope de L0",
           "cpu": "CPU de los módulos bajo L2 (% de 6400)", "cpu_sae": "CPU del cliente de carga bajo L2 (% de 6400)",
           "rss": "RSS pico bajo L2 (MB)", "bringup": "Arranque de la celda entera (s)"},
}

EXTRA_STYLE = """<style>
.toc{margin:22px 0 8px;padding:14px 18px;border:1px solid #1e2942;border-radius:12px;background:#0d1424}
.toc ol{margin:6px 0 0;padding-left:1.3em;columns:2;column-gap:2.2em}
.toc li{margin:3px 0;font-size:14px}
.toc a{color:#93a1bb;text-decoration:none}
.toc a:hover{color:#38d3f0}
.gloss{display:grid;grid-template-columns:1fr 1fr;gap:10px 22px;margin-top:14px}
.gloss div{border:1px solid #1e2942;border-radius:10px;padding:10px 12px;background:#0d1424}
.gloss b{display:block;color:#e6ebf4;margin-bottom:3px;font-size:14px}
.gloss span{color:#93a1bb;font-size:13.5px;line-height:1.45}
.fam-block{margin-top:34px;padding-top:18px;border-top:1px solid #1e2942}
.fam-block h3{display:flex;align-items:center;gap:10px;font-size:20px;margin:0 0 8px}
.fam-block .sw{display:inline-block;width:12px;height:12px;border-radius:3px}
.chart-row-3{display:grid;grid-template-columns:1fr 1fr;gap:14px;margin-top:14px}
.chart-row-3 > .card:nth-child(3){grid-column:1 / -1}
.dl a{color:#38d3f0;text-decoration:none;font-family:JetBrains Mono,monospace;font-size:13.5px}
.dl li{margin:4px 0}
.mono{font-family:JetBrains Mono,monospace}
@media (max-width:900px){.gloss{grid-template-columns:1fr}.toc ol{columns:1}.chart-row-3{grid-template-columns:1fr}}
</style>"""


def fmt(v, d=0):
    if v is None:
        return "—"
    if isinstance(v, float):
        return ("{:,.%df}" % d).format(v).replace(",", " ")
    if isinstance(v, int):
        return "{:,}".format(v).replace(",", " ")
    return str(v)


def pct(v):
    return fmt(100 * v) if v is not None else "—"


def cell_value(c, key):
    L1, L2 = c.get("L1") or {}, c.get("L2") or {}
    h = c.get("health") or {}
    if key == "techo_fibra":
        return fmt(c["techo_fibra"])
    if key == "techo_cuello":
        return fmt(c.get("techo_cuello"))
    if key == "mean_hops":
        return fmt(c["mean_hops"], 2)
    if key == "t_full_s":
        return fmt(c.get("t_full_s"))
    if key == "l0_fill_slope":
        return fmt(c.get("l0_fill_slope_keys_per_s"))
    if key == "l0_stock_end":
        return pct(c.get("l0_final_stock_frac"))
    if key == "l0_pairs_full":
        return pct(c.get("l0_pairs_full_frac"))
    if key == "l1_served":
        off = c.get("l1_offered_total")
        return "%s / %s" % (fmt(L1.get("served_keys_per_s")), fmt(off)) if L1 else "—"
    if key == "l1_429_503":
        return "%s / %s" % (fmt(L1.get("n429")), fmt(L1.get("n503"))) if L1 else "—"
    if key in ("l1_p50", "l1_p90", "l1_p99"):
        return fmt(L1.get("lat_%s_ms" % key[-3:]), 1) if L1 else "—"
    if key == "l2_sustained":
        v = fmt(L2.get("sustained_corrected_keys_per_s")) if L2 else "—"
        return (v + "*") if (L2 and L2.get("window_relaxed") and v != "—") else v
    if key == "l2_ratio":
        v = L2.get("sustained_corrected_keys_per_s") if L2 else None
        return fmt(v / c["techo_fibra"], 2) if v is not None and c["techo_fibra"] else "—"
    if key == "l2_util":
        return fmt(L2.get("fibre_utilisation"), 2) if L2 else "—"
    if key == "l2_hops":
        v = L2.get("effective_hops") if L2 else None
        return ("%s / %s" % (fmt(v, 2), fmt(c["mean_hops"], 2))) if v is not None else "—"
    if key == "l2_served":
        return fmt(L2.get("served_keys_per_s")) if L2 else "—"
    if key == "l2_reject":
        return fmt(L2.get("reject_frac"), 3) if L2 else "—"
    if key == "l2_429_503":
        return "%s / %s" % (fmt(L2.get("n429")), fmt(L2.get("n503"))) if L2 else "—"
    if key == "l2_jain":
        return fmt(L2.get("jain_index"), 3) if L2 else "—"
    if key == "l2_minshare":
        return fmt(L2.get("min_pair_share"), 2) if L2 else "—"
    if key == "l2_p10share":
        return fmt(L2.get("p10_pair_share"), 2) if L2 else "—"
    if key == "l2_pair_minmedmax":
        return "%s / %s / %s" % (fmt(L2.get("pair_keys_min")), fmt(L2.get("pair_keys_median")), fmt(L2.get("pair_keys_max"))) if L2 else "—"
    if key == "l2_pairs_zero":
        return fmt(L2.get("pairs_zero")) if L2 else "—"
    if key in ("l2_p50", "l2_p90", "l2_p99"):
        return fmt(L2.get("lat_%s_ms" % key[-3:]), 1) if L2 else "—"
    if key == "t_recover_s":
        return fmt(c.get("t_recover_s"))
    if key == "rec_stock":
        return pct(c.get("rec_final_stock_frac"))
    if key == "keys_client_err":
        v = (c.get("keys_L1") or {}).get("client_error", 0) + (c.get("keys_final") or {}).get("client_error", 0)
        return fmt(v) if (c.get("keys_L1") or c.get("keys_final")) else "—"
    if key == "keys_429":
        v = (c.get("keys_L1") or {}).get("throttled", 0) + (c.get("keys_final") or {}).get("throttled", 0)
        return fmt(v) if (c.get("keys_L1") or c.get("keys_final")) else "—"
    if key == "health":
        failed = (c.get("keys_L1") or {}).get("mismatch_or_other", 0) + (c.get("keys_final") or {}).get("mismatch_or_other", 0)
        return "%d / %d / %d / %d / %d" % (h.get("recv_corrupt", 0), h.get("peel_failed", 0) + h.get("dropped_no_secret", 0),
                                          h.get("frame_auth_rejects", 0), h.get("dead_processes", 0) + h.get("panics", 0), failed)
    if key == "expired":
        return fmt(h.get("expired")) if h else "—"
    if key == "intake":
        v = h.get("intake_dropped_frames_min") if h else None
        return ("≥ " + fmt(v)) if v else ("0" if h else "—")
    if key == "est_ratio":
        r = c.get("l0_estimator_rate_mean")
        edges = c.get("edges") or 0
        sigma_cap = (c.get("techo_fibra") or 0) * (c.get("mean_hops") or 0)
        return fmt(r / (sigma_cap / edges), 3) if (r and edges and sigma_cap) else "—"
    if key == "sdn_zero":
        return pct(c.get("l0_sdn_rate_zero_frac"))
    # Con un solo registro del muestreador en L2 (malla N=100) la media de
    # CPU/RSS no es una media: se deja en blanco.
    thin = bool(L2) and (L2.get("n_samples") or 0) < 2
    if key == "cpu":
        cpu = L2.get("cpu_mean_pct") if L2 else None
        return fmt(sum(v for k, v in cpu.items() if k != "sae_load")) if (cpu and not thin) else "—"
    if key == "cpu_sae":
        cpu = L2.get("cpu_mean_pct") if L2 else None
        return fmt(cpu.get("sae_load")) if (cpu and not thin) else "—"
    if key == "rss":
        return fmt(L2.get("rss_total_peak_mb")) if (L2 and not thin) else "—"
    if key == "bringup":
        return fmt(c.get("t_up_s"))
    return "—"


def table(lang, cells, key):
    by = {(c["family"], c["n"]): c for c in cells}
    head = "<tr><th>N</th>" + "".join("<th>%s</th>" % html.escape(LABEL[lang][f]) for f in FAMILIES) + "</tr>"
    rows = []
    for n in NS:
        tds = []
        for f in FAMILIES:
            c = by.get((f, n))
            tds.append("<td class=\"mono\">%s</td>" % (html.escape(cell_value(c, key)) if c else "·"))
        rows.append("<tr><td class=\"mono\">%d</td>%s</tr>" % (n, "".join(tds)))
    return ("<div class=\"card\"><div class=\"reg-h\">%s</div><div class=\"twrap\"><table>%s%s</table></div></div>"
            % (html.escape(TABLE_TITLES[lang][key]), head, "".join(rows)))


def family_table(lang, cells, fam):
    """Una fila por N con las magnitudes clave de UNA familia."""
    by = {c["n"]: c for c in cells if c["family"] == fam}
    cols = T[lang]["fam_cols"]
    head = "<tr>" + "".join("<th>%s</th>" % html.escape(k) for k in cols) + "</tr>"
    rows = []
    for n in NS:
        c = by.get(n)
        if not c:
            continue
        sust = cell_value(c, "l2_sustained")
        ratio = cell_value(c, "l2_ratio")
        vals = [str(n), cell_value(c, "techo_fibra"),
                "%s / %s %%" % (cell_value(c, "t_full_s"), cell_value(c, "l0_stock_end")),
                cell_value(c, "l1_served"),
                "%s (×%s)" % (sust, ratio) if sust != "—" else "—",
                cell_value(c, "l2_util"), cell_value(c, "l2_hops"), cell_value(c, "l2_jain"),
                cell_value(c, "l2_minshare"), cell_value(c, "l2_reject"), cell_value(c, "l2_p99"), cell_value(c, "expired")]
        rows.append("<tr>" + "".join("<td class=\"mono\">%s</td>" % html.escape(v) for v in vals) + "</tr>")
    return "<div class=\"card\"><div class=\"twrap\"><table>%s%s</table></div></div>" % (head, "".join(rows))


def svg_inline(path):
    if not os.path.exists(path):
        return ""
    s = open(path, encoding="utf-8").read()
    s = re.sub(r"<\?xml[^>]*\?>", "", s)
    s = re.sub(r"<!DOCTYPE[^>]*>", "", s)
    # matplotlib pone width/height fijos: que escale con la tarjeta
    s = re.sub(r"<svg([^>]*?)\swidth=\"[^\"]*\"", r"<svg\1", s, count=1)
    s = re.sub(r"<svg([^>]*?)\sheight=\"[^\"]*\"", r"<svg\1", s, count=1)
    s = s.replace("<svg", "<svg style=\"width:100%;height:auto;display:block\"", 1)
    return s


def all_zero(cells, key):
    """Una tabla que solo dice «0» en las 60 celdas es ruido: se omite. Solo
    se aplica a las columnas de incidencias (las métricas valen 0 de pleno
    derecho)."""
    if key not in ("keys_client_err",):
        return False
    return not any(cell_value(c, key) not in ("0", "—") for c in cells)


def fig_path(figs_dir, lang, name):
    """Las gráficas van rotuladas por idioma (figs/en/ para el inglés); las
    figuras neutras (topologías) viven solo en figs/."""
    cand = os.path.join(figs_dir, lang, name + ".svg")
    if lang != "es" and os.path.exists(cand):
        return cand
    return os.path.join(figs_dir, name + ".svg")


def fig_cards(figs_dir, lang, names, cls="chart-row"):
    inner = "".join("<div class=\"card\">%s</div>" % svg_inline(fig_path(figs_dir, lang, f))
                    for f in names if os.path.exists(fig_path(figs_dir, lang, f)))
    return ("<div class=\"%s\">%s</div>" % (cls, inner)) if inner else ""


def topo_cards(lang, cells, figs_dir):
    by = {(c["family"], c["n"]): c for c in cells}
    out = []
    for f in FAMILIES:
        c = by.get((f, 100)) or next((by[(f, n)] for n in reversed(NS) if (f, n) in by), None)
        facts = ""
        if c:
            vals = [fmt(c["edges"]), fmt(c["mean_hops"], 2), fmt(c["diameter"]), fmt(c["techo_fibra"]), fmt(c["techo_cuello"])]
            facts = "".join("<span>%s: %s</span>" % (html.escape(k), v) for k, v in zip(T[lang]["facts"], vals))
            facts = "<div class=\"facts\"><span>N=%d</span>%s</div>" % (c["n"], facts)
        svg = svg_inline(os.path.join(figs_dir, "topo_%s.svg" % f))
        out.append("<div class=\"topo-cell\"><h3><span class=\"sw\" style=\"background:%s\"></span>%s</h3>%s%s</div>"
                   % (COLORS[f], html.escape(LABEL[lang][f]), svg, facts))
    return "<div class=\"topo-grid\">%s</div>" % "".join(out)


def families_section(lang, cells, figs_dir, nar):
    t = T[lang]
    blocks = []
    for f in FAMILIES:
        reading = nar.get("fam_" + f, t["pending"])
        figs = fig_cards(figs_dir, lang, ["fill_%s" % f, "timeline_%s_n50" % f, "timeline_%s_n100" % f], cls="chart-row-3")
        blocks.append("<div class=\"fam-block\" id=\"fam-%s\"><h3><span class=\"sw\" style=\"background:%s\"></span>%s</h3>"
                      "<p class=\"reading\">%s</p>%s%s</div>"
                      % (f, COLORS[f], html.escape(LABEL[lang][f]), reading, figs, family_table(lang, cells, f)))
    return "<section id=\"sec_families\"><h2>%s</h2><p class=\"reading\">%s</p>%s</section>" % (
        html.escape(t["sec_families"]), html.escape(t["families_intro"]), "".join(blocks))


QKD_R0, QKD_ALPHA = 2000.0, 0.2


def qkd_section(lang, cells, figs_dir, nar):
    t = T[lang]
    # tabla estática: capacidad frente a distancia con los parámetros de diseño
    who = {5: ("star, ring, bridge, mesh, random", "estrella, anillo, puente, malla, aleatoria"),
           2: ("RGG, shortest edge", "RGG, arista más corta"), 22: ("RGG, median edge", "RGG, arista mediana"),
           47: ("RGG, longest edge", "RGG, arista más larga")}
    rows = []
    for d in (0, 1, 2, 5, 10, 15, 20, 22, 25, 30, 40, 47, 50):
        cap = QKD_R0 * 10 ** (-QKD_ALPHA * d / 10.0)
        w = who.get(d, ("", ""))[0 if lang == "en" else 1]
        rows.append("<tr><td class=\"mono\">%d</td><td class=\"mono\">%s</td><td class=\"mono\">%s %%</td><td>%s</td></tr>"
                    % (d, fmt(cap, 1), fmt(100 * cap / QKD_R0, 1), html.escape(w)))
    static = ("<div class=\"card\"><div class=\"reg-h\">%s</div><div class=\"twrap\"><table><tr>%s</tr>%s</table></div></div>"
              % (html.escape(t["qkd_static_title"]), "".join("<th>%s</th>" % html.escape(c) for c in t["qkd_static_cols"]), "".join(rows)))
    # tabla por N de la RGG: distancias y capacidades reales de sus aristas
    by = {c["n"]: c for c in cells if c["family"] == "rgg"}
    rrows = []
    for n in NS:
        c = by.get(n)
        if not c or not c.get("dist_km"):
            continue
        d, k = c["dist_km"], c["link_cap"]
        rrows.append("<tr>" + "".join("<td class=\"mono\">%s</td>" % html.escape(v) for v in [
            str(n), fmt(c["edges"]),
            "%s / %s / %s" % (fmt(d["min"], 1), fmt(d["median"], 1), fmt(d["max"], 1)),
            "%s / %s / %s" % (fmt(k["min"]), fmt(k["median"]), fmt(k["max"])),
            fmt(k["sum"]), fmt(c.get("techo_cuello"))]) + "</tr>")
    rgg = ("<div class=\"card\"><div class=\"reg-h\">%s</div><div class=\"twrap\"><table><tr>%s</tr>%s</table></div></div>"
           % (html.escape(t["qkd_rgg_title"]), "".join("<th>%s</th>" % html.escape(c) for c in t["qkd_cols"]), "".join(rrows)))
    return ("<section id=\"sec_qkd\"><h2>%s</h2><p class=\"reading\">%s</p>%s%s%s</section>"
            % (html.escape(t["sec_qkd"]), nar.get("sec_qkd", t["pending"]), fig_cards(figs_dir, lang, ["qkd_model"]), static, rgg))


def glossary_section(lang):
    t = T[lang]
    items = "".join("<div><b>%s</b><span>%s</span></div>" % (html.escape(k), html.escape(v)) for k, v in t["glossary"])
    return "<section id=\"sec_glossary\"><h2>%s</h2><p class=\"reading\">%s</p><div class=\"gloss\">%s</div></section>" % (
        html.escape(t["sec_glossary"]), html.escape(t["glossary_intro"]), items)


def data_section(lang):
    t = T[lang]
    files = "".join("<li><a href=\"/results/%s/%s\" download>%s</a> — %s</li>" % (DATA_DIR, fn, html.escape(fn), html.escape(desc))
                    for fn, desc in t["data_files"])
    return ("<section id=\"sec_data\"><h2>%s</h2><p class=\"reading\">%s</p><ul class=\"dl\">%s</ul><p class=\"reading\">%s</p></section>"
            % (html.escape(t["sec_data"]), html.escape(t["data_reading"]), files, t["data_repro"]))


def toc(lang):
    t = T[lang]
    order = ["sec_glossary", "sec_topos", "sec_qkd", "sec_families"] + [s for s, _, _ in FIG_SECTIONS] + ["sec_data", "sec_method"]
    items = "".join("<li><a href=\"#%s\">%s</a></li>" % (s, html.escape(t[s])) for s in order)
    return "<nav class=\"toc\"><b>%s</b><ol>%s</ol></nav>" % (html.escape(t["toc"]), items)


def build(lang, cells, figs_dir, narrative, style, date):
    t = T[lang]
    nar = narrative.get(lang, {}) if narrative else {}
    parts = []
    parts.append("<div style=\"max-width:1060px;margin:0 auto;padding:14px 28px 0;\"><a href=\"%s#results\" style='font-family:JetBrains Mono,monospace;font-size:12.5px;color:#93a1bb;text-decoration:none'>%s</a></div>"
                 % ("/" if lang == "en" else "/es/", html.escape(t["back"])))
    parts.append("<div class=\"wrap\">")
    parts.append("<header><div class=\"kicker\"><span>%s</span><span>%s</span></div><h1>%s</h1><p class=\"stand\">%s</p></header>"
                 % (html.escape(t["kicker"]), html.escape(date), html.escape(nar.get("title", t["title"])), html.escape(nar.get("stand", t["stand"]))))
    if nar.get("summary"):
        parts.append("<div class=\"note\">%s</div>" % nar["summary"])
    parts.append(toc(lang))
    parts.append(glossary_section(lang))
    parts.append("<section id=\"sec_topos\"><h2>%s</h2><p class=\"reading\">%s</p>%s</section>"
                 % (html.escape(t["sec_topos"]), t["topos_reading"], topo_cards(lang, cells, figs_dir)))
    parts.append(qkd_section(lang, cells, figs_dir, nar))
    parts.append(families_section(lang, cells, figs_dir, nar))
    for sec, figs, tables in FIG_SECTIONS:
        reading = nar.get(sec, t["pending"])
        parts.append("<section id=\"%s\"><h2>%s</h2><p class=\"reading\">%s</p>%s%s</section>"
                     % (sec, html.escape(t[sec]), reading, fig_cards(figs_dir, lang, figs),
                        "".join(table(lang, cells, k) for k in tables if not all_zero(cells, k))))
    parts.append(data_section(lang))
    parts.append("<section id=\"sec_method\"><h2>%s</h2><p class=\"reading\">%s</p></section>" % (html.escape(t["sec_method"]), t["method"]))
    parts.append("</div>")
    return ("<!doctype html>\n<html lang=\"%s\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n"
            "<meta name=\"theme-color\" content=\"#070b14\">\n<title>%s</title>\n%s\n%s\n</head>\n<body>\n%s\n</body>\n</html>\n"
            % (lang, html.escape(nar.get("title", t["title"])), style, EXTRA_STYLE, "\n".join(parts)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--analysis", required=True)
    ap.add_argument("--web", default=os.path.expanduser("~/Documentos/web_dkms"))
    ap.add_argument("--narrative", default=None)
    ap.add_argument("--date", default="2026-09")
    a = ap.parse_args()
    cells = json.load(open(os.path.join(a.analysis, "metrics.json")))
    figs_dir = os.path.join(a.analysis, "figs")
    narrative = json.load(open(a.narrative)) if a.narrative and os.path.exists(a.narrative) else {}
    ref = os.path.join(a.web, "public", "results", "scale-laws.html")
    style = re.search(r"<style>.*?</style>", open(ref, encoding="utf-8").read(), re.S).group(0) if os.path.exists(ref) else "<style></style>"
    outs = {"en": os.path.join(a.web, "public", "results", "campaign-2026-09.html"),
            "es": os.path.join(a.web, "public", "resultados", "campana-2026-09.html")}
    for lang, path in outs.items():
        os.makedirs(os.path.dirname(path), exist_ok=True)
        open(path, "w", encoding="utf-8").write(build(lang, cells, figs_dir, narrative, style, a.date))
        print("escrita", path, os.path.getsize(path), "bytes")
    # datos descargables (los mismos ficheros para los dos idiomas)
    ddir = os.path.join(a.web, "public", "results", DATA_DIR)
    os.makedirs(ddir, exist_ok=True)
    for fn in ("metrics.json", "TABLES.md", "ANALYSIS.md", "replay_fix.json"):
        src = os.path.join(a.analysis, fn)
        if os.path.exists(src):
            shutil.copyfile(src, os.path.join(ddir, fn))
    print("datos en", ddir)


if __name__ == "__main__":
    main()
