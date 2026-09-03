#!/usr/bin/env python3
"""Genera la página de resultados de la campaña 2026-09 para la web
(`~/Documentos/web_dkms`, Astro): una página estática por idioma, con las
figuras SVG incrustadas y las tablas, en el mismo estilo que
`public/results/scale-laws.html` (se le copia el bloque <style>).

    campaign_web.py --analysis DIR --web DIR [--narrative narrative.json]

  DIR/metrics.json y DIR/figs/*.svg vienen de campaign_analyze.py. El
  narrative.json lleva los textos de lectura (EN/ES) por sección; sin él se
  emiten marcadores «(pendiente)» para que la página siempre sea válida.

Salida: <web>/public/results/campaign-2026-09.html (EN) y
        <web>/public/resultados/campana-2026-09.html (ES).
"""
import argparse
import html
import json
import os
import re

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

T = {
    "en": {
        "title": "Six topologies, ten sizes, three loads",
        "kicker": "dkms-rust · CESGA FT3 · N=10–100 · 6 topologies × 3 loads · QKD quditto per link · factory defaults",
        "back": "← D-KMS — back to the site",
        "stand": "Every cell is a full deployment on one 64-core node: SDN + N×(QKC, ORR, DKMS) + one simulated QKD link (quditto) per edge, brought up from empty buffers and driven through rest, paced load and closed-loop saturation. Same factory defaults and the same harness across the 60 cells; the three affected by the replay finding were re-measured with the fixed binary.",
        "sec_topos": "The six topologies",
        "topos_reading": "Drawn at N=20. Every link is QKD: capacity R₀·10^(−αd/10) with R₀=2000 keys/s, α=0.2 dB/km and d=5 km (1588.7 keys/s) — except in the RGG, where each edge carries its own geometric distance (2–47 km → 233–1811 keys/s). Facts at N=100.",
        "sec_theory": "What the fibre allows",
        "sec_l0": "L0 — rest: filling from empty",
        "sec_l1": "L1 — paced load at half the fibre ceiling",
        "sec_l2": "L2 — closed-loop saturation",
        "sec_rec": "Recovery after saturation",
        "sec_health": "Integrity and health",
        "sec_res": "Resources on the node",
        "sec_method": "Method",
        "method": "One SLURM job per cell (full 64-core node, 48–200 GB). <b>L0</b>: from empty buffers until every ordered pair reaches 4096 transport keys (+60 s) or 600 s. <b>L1</b>: one flow per ordered pair, rate-capped so the aggregate offered load is 50 % of the fibre ceiling Σcap/ħ (capped at 30 000 keys/s), 300 s. <b>L2</b>: one closed-loop flow per ordered pair with a 100 ms pause after each 429/503, 300 s; the sustained figure is corrected by the buffer stock drained during the window ((served − ΔΣenc)/window over the last 2/3 of L2). <b>REC</b>: up to 180 s for the generator to refill. Integrity: an ETSI-014 enc/dec round with byte comparison on 10 nodes after L1 and at the end; under L2, recv_corrupt and 503 counts. Every module logs its state every 5 s; a sampler aggregates them.",
        "facts": ["edges", "mean hops", "diameter", "fibre ceiling (keys/s)", "bottleneck link (keys/s)"],
        "pending": "(reading pending: campaign still running)",
        "cols": {"N": "N"},
    },
    "es": {
        "title": "Seis topologías, diez tamaños, tres cargas",
        "kicker": "dkms-rust · CESGA FT3 · N=10–100 · 6 topologías × 3 cargas · quditto QKD por enlace · defaults de fábrica",
        "back": "← D-KMS — volver a la web",
        "stand": "Cada celda es un despliegue completo en un nodo de 64 cores: SDN + N×(QKC, ORR, DKMS) + un enlace QKD simulado (quditto) por arista, levantado desde buffers vacíos y llevado por reposo, carga pautada y saturación en bucle cerrado. Los mismos defaults de fábrica y el mismo arnés en las 60 celdas; las tres afectadas por el hallazgo del replay se volvieron a medir con el binario corregido.",
        "sec_topos": "Las seis topologías",
        "topos_reading": "Dibujadas a N=20. Todos los enlaces son QKD: capacidad R₀·10^(−αd/10) con R₀=2000 claves/s, α=0.2 dB/km y d=5 km (1588,7 claves/s) — salvo en la RGG, donde cada arista lleva su distancia geométrica (2–47 km → 233–1811 claves/s). Datos a N=100.",
        "sec_theory": "Lo que permite la fibra",
        "sec_l0": "L0 — reposo: llenado desde vacío",
        "sec_l1": "L1 — carga pautada a la mitad del techo de fibra",
        "sec_l2": "L2 — saturación en bucle cerrado",
        "sec_rec": "Recuperación tras la saturación",
        "sec_health": "Integridad y salud",
        "sec_res": "Recursos del nodo",
        "sec_method": "Método",
        "method": "Un job SLURM por celda (nodo entero de 64 cores, 48–200 GB). <b>L0</b>: desde buffers vacíos hasta que todos los pares ordenados llegan a 4096 claves de transporte (+60 s) o 600 s. <b>L1</b>: un flujo por par ordenado, pautado para que la oferta agregada sea el 50 % del techo de fibra Σcap/ħ (acotado a 30 000 claves/s), 300 s. <b>L2</b>: un flujo en bucle cerrado por par ordenado con 100 ms de pausa tras cada 429/503, 300 s; la cifra sostenida se corrige con el stock de buffers drenado en la ventana ((servido − ΔΣenc)/ventana sobre los últimos 2/3 de L2). <b>REC</b>: hasta 180 s para que el generador rellene. Integridad: una ronda ETSI-014 enc/dec con comparación de bytes en 10 nodos tras L1 y al final; bajo L2, recv_corrupt y los 503. Cada módulo escribe su estado cada 5 s; un muestreador los agrega.",
        "facts": ["aristas", "saltos medios", "diámetro", "techo de fibra (claves/s)", "enlace cuello (claves/s)"],
        "pending": "(lectura pendiente: campaña en curso)",
        "cols": {"N": "N"},
    },
}

FIG_SECTIONS = [
    ("sec_theory", ["ceilings", "hops"], ["techo_fibra", "mean_hops"]),
    ("sec_l0", ["l0_fill_time", "l0_fill_slope"], ["t_full_s", "l0_fill_slope"]),
    ("sec_l1", ["l1_served_vs_offered", "l1_latency"], ["l1_served", "l1_p99"]),
    ("sec_l2", ["l2_sustained", "l2_fibre_utilisation", "l2_effective_hops", "l2_jain", "l2_min_pair_share", "l2_reject", "l2_latency_p99"],
     ["l2_sustained", "l2_ratio", "l2_util", "l2_hops", "l2_jain", "l2_minshare", "l2_reject", "l2_p99"]),
    ("sec_rec", ["rec_time"], ["t_recover_s"]),
    ("sec_health", [], ["health", "expired", "intake", "keys_client_err"]),
    ("sec_res", ["l2_cpu_modules", "rss_peak"], ["cpu", "rss"]),
]

TABLE_TITLES = {
    "en": {"techo_fibra": "Fibre ceiling Σcap/ħ (keys/s)", "mean_hops": "Mean hops (ordered pairs)",
           "t_full_s": "Time until every pair is full (s; — = not within 600 s)",
           "l0_fill_slope": "Fill slope, all pairs (keys/s)", "l1_served": "L1 served (keys/s) / offered",
           "l1_p99": "L1 latency p99 (ms)", "l2_sustained": "L2 sustained, stock-corrected (keys/s; * = window relaxed to the last two sampler records, the sampler starved by the load client at N ≥ 80)",
           "l2_ratio": "L2 sustained / uniform-demand ceiling", "l2_util": "L2 fibre utilisation (Σtaken/Σcap)",
           "l2_hops": "L2 effective hops per delivered key (vs uniform mean)", "l2_jain": "L2 Jain fairness index (keys per pair)",
           "l2_minshare": "L2 worst pair / uniform share", "l2_reject": "L2 rejected fraction (429/503)",
           "l2_p99": "L2 latency p99 (ms)", "t_recover_s": "Recovery to full after L2 (s; — = not within 180 s)",
           "health": "recv_corrupt / peel_failed / frame-auth rejects / dead+panics / exchanges with different bytes (429/503 in the rounds are backpressure, not counted here)",
           "keys_client_err": "Integrity rounds the CLIENT could not run (its curl cannot load an ML-DSA certificate) — not a system result",
           "expired": "Keys expired at the sender (emitted, no ACK within 30 s — material discarded, never corrupt)",
           "intake": "Frames dropped by the QKC's bounded intake queue (≥; only nodes 1–2 keep logs; node 1 is the star's hub)",
           "cpu": "Modules CPU under L2 (% of 6400)", "rss": "Peak RSS under L2 (MB)"},
    "es": {"techo_fibra": "Techo de fibra Σcap/ħ (claves/s)", "mean_hops": "Saltos medios (pares ordenados)",
           "t_full_s": "Tiempo hasta todos los pares a tope (s; — = no en 600 s)",
           "l0_fill_slope": "Pendiente de llenado, todos los pares (claves/s)", "l1_served": "L1 servido (claves/s) / ofrecido",
           "l1_p99": "L1 latencia p99 (ms)", "l2_sustained": "L2 sostenido corregido por stock (claves/s; * = ventana relajada a los dos últimos registros del muestreador, que el cliente de carga deja sin CPU a N ≥ 80)",
           "l2_ratio": "L2 sostenido / techo de demanda uniforme", "l2_util": "L2 utilización de la fibra (Σtaken/Σcap)",
           "l2_hops": "L2 saltos efectivos por clave entregada (vs media uniforme)", "l2_jain": "L2 índice de Jain (claves por par)",
           "l2_minshare": "L2 peor par / reparto uniforme", "l2_reject": "L2 fracción rechazada (429/503)",
           "l2_p99": "L2 latencia p99 (ms)", "t_recover_s": "Recuperación a tope tras L2 (s; — = no en 180 s)",
           "health": "recv_corrupt / peel_failed / rechazos del sello / muertos+panics / intercambios con bytes distintos (los 429/503 de las rondas son contrapresión y no cuentan aquí)",
                      "keys_client_err": "Rondas de integridad que el CLIENTE no pudo ejecutar (su curl no carga un certificado ML-DSA): no es un resultado del sistema",
           "expired": "Claves expiradas en el emisor (emitidas sin ACK en 30 s: material descartado, nunca corrupto)",
           "intake": "Frames descartados por la cola de entrada acotada del QKC (≥; solo los nodos 1–2 conservan log; el nodo 1 es el hub de la estrella)",
"cpu": "CPU de los módulos bajo L2 (% de 6400)", "rss": "RSS pico bajo L2 (MB)"},
}


def fmt(v, d=0):
    if v is None:
        return "—"
    if isinstance(v, float):
        return ("{:,.%df}" % d).format(v).replace(",", " ")
    if isinstance(v, int):
        return "{:,}".format(v).replace(",", " ")
    return str(v)


def cell_value(c, key):
    L1, L2 = c.get("L1") or {}, c.get("L2") or {}
    h = c.get("health") or {}
    if key == "techo_fibra":
        return fmt(c["techo_fibra"])
    if key == "mean_hops":
        return fmt(c["mean_hops"], 2)
    if key == "t_full_s":
        return fmt(c.get("t_full_s"))
    if key == "l0_fill_slope":
        return fmt(c.get("l0_fill_slope_keys_per_s"))
    if key == "l1_served":
        off = c.get("l1_offered_total")
        return "%s / %s" % (fmt(L1.get("served_keys_per_s")), fmt(off)) if L1 else "—"
    if key == "l1_p99":
        return fmt(L1.get("lat_p99_ms"), 1) if L1 else "—"
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
    if key == "l2_jain":
        return fmt(L2.get("jain_index"), 3) if L2 else "—"
    if key == "l2_minshare":
        return fmt(L2.get("min_pair_share"), 2) if L2 else "—"
    if key == "l2_reject":
        return fmt(L2.get("reject_frac"), 3) if L2 else "—"
    if key == "l2_p99":
        return fmt(L2.get("lat_p99_ms"), 1) if L2 else "—"
    if key == "t_recover_s":
        return fmt(c.get("t_recover_s"))
    if key == "keys_client_err":
        v = (c.get("keys_L1") or {}).get("client_error", 0) + (c.get("keys_final") or {}).get("client_error", 0)
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
    if key == "cpu":
        cpu = L2.get("cpu_mean_pct") if L2 else None
        return fmt(sum(v for k, v in cpu.items() if k != "sae_load")) if cpu else "—"
    if key == "rss":
        return fmt(L2.get("rss_total_peak_mb")) if L2 else "—"
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
    return not any(cell_value(c, key) not in ("0", "\u2014") for c in cells)


def fig_path(figs_dir, lang, name):
    """Las gráficas van rotuladas por idioma (figs/en/ para el inglés); las
    figuras neutras (topologías) viven solo en figs/."""
    cand = os.path.join(figs_dir, lang, name + ".svg")
    if lang != "es" and os.path.exists(cand):
        return cand
    return os.path.join(figs_dir, name + ".svg")


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
    parts.append("<section><h2>%s</h2><p class=\"reading\">%s</p>%s</section>"
                 % (html.escape(t["sec_topos"]), t["topos_reading"], topo_cards(lang, cells, figs_dir)))
    for sec, figs, tables in FIG_SECTIONS:
        reading = nar.get(sec, t["pending"])
        fig_html = "".join("<div class=\"card\">%s</div>" % svg_inline(fig_path(figs_dir, lang, f)) for f in figs)
        parts.append("<section><h2>%s</h2><p class=\"reading\">%s</p>%s%s</section>"
                     % (html.escape(t[sec]), reading,
                        ("<div class=\"chart-row\">%s</div>" % fig_html) if fig_html else "",
                        "".join(table(lang, cells, k) for k in tables if not all_zero(cells, k))))
    parts.append("<section><h2>%s</h2><p class=\"reading\">%s</p></section>" % (html.escape(t["sec_method"]), t["method"]))
    parts.append("</div>")
    return ("<!doctype html>\n<html lang=\"%s\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n"
            "<meta name=\"theme-color\" content=\"#070b14\">\n<title>%s</title>\n%s\n</head>\n<body>\n%s\n</body>\n</html>\n"
            % (lang, html.escape(nar.get("title", t["title"])), style, "\n".join(parts)))


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


if __name__ == "__main__":
    main()
