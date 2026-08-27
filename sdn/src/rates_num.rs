//! Asignadores de rates de producción sobre routing fraccional FIJO.
//!
//! El LP MCMCF-λ reparte la escasez minimizando `Σσ` (utilitarista): el
//! símplex devuelve vértices que dejan commodities enteras a cero mientras
//! otras reciben el 100 % — medido en CESGA 2026-08-25 (jobs 9265640 y
//! 9266817): 95-96 % de las muestras de `/rate` a cero tenían demanda real,
//! con pares fijos a cero los 300 s de carga. Este módulo lo sustituye como
//! camino de producción con dos asignadores que NO pueden matar de hambre:
//!
//! * [`NumState`] — descomposición dual (NUM, Kelly): precios `μ_e` por
//!   arista, rate `x_k = (w_k / Σ f·μ)^{1/α}` por commodity. Con α=1
//!   (log-utility, default) la utilidad marginal en 0 es infinita: nadie
//!   queda a cero por matemática, no por vigilancia. Es el algoritmo
//!   distribuido del punto 7 del roadmap computado donde hoy ya está la
//!   información (la SDN); moverlo físicamente a los nodos es transporte,
//!   no algoritmo.
//! * [`maxmin_allocate`] — waterfilling progresivo exacto: la propiedad
//!   max-min lexicográfica que promete el paper, en aritmética pura (sin
//!   LP, sin símplex que falsear).
//!
//! Ambos usan el MISMO routing que los QKCs ejecutan: las tablas WCMP de
//! `wcmp_from_topology` (topología + capacidad), convertidas por commodity
//! en fracciones por arista con [`commodity_edge_fractions`]. Los precios y
//! el waterfill deciden CUÁNTO; nunca POR DÓNDE — el desacoplo
//! routing/rates de 2026-08-24 se mantiene intacto.
//!
//! El LP sigue disponible (`SDN_RATE_ALLOCATOR=lp`) como oráculo de
//! referencia y para el estudio de multipath libre.

use std::collections::{BTreeMap, HashMap, VecDeque};

use common::security::KeyGrade;
use tracing::{info, warn};

use crate::demand::CommodityDemand;
use crate::mcf::{flow_id, WcmpNextHop};
use crate::mcmcf::{McmcfInputs, McmcfSolution, FLOW_EPSILON, T_REPLAN_SECONDS};
use crate::topology::edge_key;

/// Tabla WCMP tal y como la produce `wcmp_from_topology`:
/// `qkc_de_tránsito → qkc_destino → next hops con peso`.
pub type Wcmp = HashMap<String, HashMap<String, Vec<WcmpNextHop>>>;

/// Qué mecanismo produce las rates que la SDN publica en `/rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allocator {
    /// Precios α-fair (default). Ver [`NumState`].
    Num,
    /// Waterfilling max-min exacto. Ver [`maxmin_allocate`].
    Maxmin,
    /// El LP MCMCF-λ de siempre (oráculo / estudio).
    Lp,
}

impl Allocator {
    /// `SDN_RATE_ALLOCATOR` (env) pisa al valor del config, igual que
    /// `SDN_SOLVER` gobierna el backend del LP. Un valor desconocido cae a
    /// `num` con aviso — nunca a un modo silenciosamente distinto.
    pub fn resolve(cfg_value: &str) -> Self {
        let v = std::env::var("SDN_RATE_ALLOCATOR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cfg_value.to_string());
        match v.to_ascii_lowercase().as_str() {
            "" | "num" => Self::Num,
            "maxmin" => Self::Maxmin,
            "lp" => Self::Lp,
            other => {
                warn!(
                    value = other,
                    "rate_allocator desconocido (num|maxmin|lp); usando num"
                );
                Self::Num
            }
        }
    }
}

/// Parámetros del asignador por precios. Vienen del `SdnConfig` (campos
/// planos — ojo con el caveat de config-rs y las secciones anidadas).
#[derive(Debug, Clone, Copy)]
pub struct NumParams {
    /// Curvatura de la utilidad α-fair. 1.0 = proportional fairness
    /// (log-utility): la utilidad marginal diverge en 0, así que ninguna
    /// commodity con demanda puede quedar a cero. α→∞ tiende al max-min
    /// (para el max-min exacto está el modo `maxmin`).
    pub alpha: f64,
    /// Paso del gradiente de precios. Con la sobrecarga normalizada por
    /// capacidad y recortada, 0.2 converge en decenas de ticks sin oscilar.
    pub gamma: f64,
    /// Peso del llenado proactivo frente al drenaje en `w_k = δ_k +
    /// fill_weight·R_k/T`. Sustituye a la semántica λ·R global del LP: el
    /// llenado es demanda de menor prioridad, no un multiplicador de red.
    pub fill_weight: f64,
}

impl Default for NumParams {
    fn default() -> Self {
        Self {
            alpha: 1.0,
            gamma: 0.2,
            fill_weight: 0.1,
        }
    }
}

/// Por debajo de este precio la commodity está "sin restricción visible" y
/// su rate se decide por el cap útil, no por la división (que explotaría).
const PRICE_EPSILON: f64 = 1e-9;
/// Recorte de la sobrecarga normalizada `(load−c)/c` en el update de μ: sin
/// él, el primer tick tras un arranque en frío (μ=0 ⇒ todo el mundo a su
/// cap) puede escalar el precio ×40 y pasarse de frenada.
const OVERLOAD_CLAMP: f64 = 5.0;

/// Una commodity preparada: su demanda y las fracciones de su flujo por
/// arista según el WCMP de su grade.
struct Flow<'a> {
    c: &'a CommodityDemand,
    fractions: HashMap<(String, String), f64>,
    routable: bool,
}

/// Fracción del flujo de una commodity que atraviesa cada arista (clave
/// canónica no dirigida), propagando masa 1.0 de `src_qkc` a `dst_qkc` por
/// el DAG WCMP y normalizando los pesos enteros en cada salto. Devuelve
/// además la masa que llega al destino: < 1.0 significa entradas sin ruta
/// (no debería ocurrir con tablas de `wcmp_from_topology`, que son
/// completas por construcción).
pub fn commodity_edge_fractions(
    wcmp: &Wcmp,
    src_qkc: &str,
    dst_qkc: &str,
) -> (HashMap<(String, String), f64>, f64) {
    let mut fractions: HashMap<(String, String), f64> = HashMap::new();
    let mut arrived = 0.0;
    let mut pending: HashMap<String, f64> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    pending.insert(src_qkc.to_string(), 1.0);
    queue.push_back(src_qkc.to_string());
    // El WCMP reparte solo hacia vecinos estrictamente más cercanos al
    // destino, así que esto es un DAG y termina. La cota es un blindaje por
    // si una tabla corrupta introdujera un ciclo: mejor cortar y avisar que
    // colgar el tick de rates.
    let mut steps = 0usize;
    while let Some(v) = queue.pop_front() {
        steps += 1;
        if steps > 100_000 {
            warn!(
                src = src_qkc,
                dst = dst_qkc,
                "wcmp con ciclo aparente; fracciones truncadas"
            );
            break;
        }
        let Some(m) = pending.remove(&v) else {
            continue;
        };
        if m <= 0.0 {
            continue;
        }
        if v == dst_qkc {
            arrived += m;
            continue;
        }
        let Some(hops) = wcmp
            .get(&v)
            .and_then(|d| d.get(dst_qkc))
            .filter(|h| !h.is_empty())
        else {
            // Masa perdida: este tránsito no conoce el destino.
            continue;
        };
        let total: f64 = hops.iter().map(|h| h.weight as f64).sum();
        if total <= 0.0 {
            continue;
        }
        for h in hops {
            let next = h.qkc_id.to_string();
            let frac = m * h.weight as f64 / total;
            *fractions.entry(edge_key(&v, &next)).or_insert(0.0) += frac;
            *pending.entry(next.clone()).or_insert(0.0) += frac;
            queue.push_back(next);
        }
    }
    (fractions, arrived)
}

/// Prepara las commodities activas (mismo filtro que el LP: ambos extremos
/// con QKC resoluble) con sus fracciones por arista, cada una sobre la
/// tabla de su grade — QKD sobre el subgrafo QKD, PQC sobre el completo.
fn prepare_flows<'a>(inputs: &'a McmcfInputs, wcmp_full: &Wcmp, wcmp_qkd: &Wcmp) -> Vec<Flow<'a>> {
    inputs
        .commodities
        .iter()
        .filter(|c| {
            inputs.dkms_to_qkc.contains_key(&c.src_dkms)
                && inputs.dkms_to_qkc.contains_key(&c.dst_dkms)
        })
        .map(|c| {
            let src = inputs.dkms_to_qkc[&c.src_dkms].as_str();
            let dst = inputs.dkms_to_qkc[&c.dst_dkms].as_str();
            let table = if c.grade == KeyGrade::Qkd {
                wcmp_qkd
            } else {
                wcmp_full
            };
            let (fractions, arrived) = commodity_edge_fractions(table, src, dst);
            Flow {
                c,
                fractions,
                routable: arrived > 0.99,
            }
        })
        .collect()
}

/// Empaqueta rates por commodity en la forma que `into_mcf_snapshot` ya
/// entiende — `lambda` y `edge_flows` son artefactos del LP y van vacíos.
fn assemble(flows: &[Flow], xs: &[f64]) -> McmcfSolution {
    let mut rates: HashMap<String, f64> = HashMap::with_capacity(flows.len());
    let mut rates_grade: HashMap<(String, KeyGrade), f64> = HashMap::with_capacity(flows.len());
    for (f, &x) in flows.iter().zip(xs) {
        let x = if x < FLOW_EPSILON { 0.0 } else { x };
        let fid = flow_id(&f.c.src_dkms, &f.c.dst_dkms);
        *rates.entry(fid.clone()).or_insert(0.0) += x;
        rates_grade.insert((fid, f.c.grade), x);
    }
    McmcfSolution {
        lambda: 0.0,
        edge_flows: Vec::new(),
        rates,
        rates_grade,
    }
}

/// Estado persistente del asignador por precios: `μ_e` por arista. Vive en
/// el `SdnService` y sobrevive entre ticks; las aristas que desaparecen de
/// la topología pierden su precio y las nuevas arrancan en 0.
#[derive(Debug, Default)]
pub struct NumState {
    prices: HashMap<(String, String), f64>,
    ticks: u64,
}

impl NumState {
    /// Un paso de la descomposición dual. Por diseño en dos tiempos:
    ///
    /// 1. **Rates desde los precios**: `x_k = (w_k / Σ f·μ)^{1/α}`, capada a
    ///    lo útil (`δ_k + R_k/T`: servir el drenaje y poder llenar el buffer
    ///    en un horizonte de replan — sin el cap, μ≈0 daría rates infinitas).
    /// 2. **Precios desde la carga**: `μ_e ← [μ_e + γ·clamp((load−c)/c)]⁺`,
    ///    con la carga SIN escalar para no perder señal de gradiente.
    ///
    /// Lo PUBLICADO, en cambio, se proyecta a factible: cada commodity se
    /// reduce por la peor sobrecarga de sus aristas, así que las rates
    /// entregadas nunca prometen por encima de un corte aunque los precios
    /// aún estén convergiendo (arranque en frío, churn).
    pub fn step(
        &mut self,
        inputs: &McmcfInputs,
        wcmp_full: &Wcmp,
        wcmp_qkd: &Wcmp,
        p: &NumParams,
    ) -> McmcfSolution {
        let flows = prepare_flows(inputs, wcmp_full, wcmp_qkd);
        let alpha = p.alpha.max(0.1);

        // 1. Rates desde los precios.
        let mut xs = vec![0.0f64; flows.len()];
        for (i, f) in flows.iter().enumerate() {
            if !f.routable {
                continue;
            }
            let w = f.c.drain_rate + p.fill_weight * f.c.remaining() / T_REPLAN_SECONDS;
            if w <= 0.0 {
                continue;
            }
            let x_cap = f.c.drain_rate + f.c.remaining() / T_REPLAN_SECONDS;
            let price: f64 = f
                .fractions
                .iter()
                .map(|(e, fr)| fr * self.prices.get(e).copied().unwrap_or(0.0))
                .sum();
            xs[i] = if price <= PRICE_EPSILON {
                x_cap
            } else {
                (w / price).powf(1.0 / alpha).min(x_cap)
            };
        }

        // Carga cruda por arista (la que ve el gradiente).
        let mut load: HashMap<&(String, String), f64> = HashMap::new();
        for (i, f) in flows.iter().enumerate() {
            if xs[i] <= 0.0 {
                continue;
            }
            for (e, fr) in &f.fractions {
                *load.entry(e).or_insert(0.0) += fr * xs[i];
            }
        }

        // Proyección a factible de lo publicado.
        let mut overload: HashMap<&(String, String), f64> = HashMap::new();
        for (e, cap) in &inputs.edge_capacity {
            let l = load.get(e).copied().unwrap_or(0.0);
            if *cap > 0.0 && l > *cap {
                overload.insert(e, l / cap);
            }
        }
        let mut out = xs.clone();
        if !overload.is_empty() {
            for (i, f) in flows.iter().enumerate() {
                let worst = f
                    .fractions
                    .keys()
                    .filter_map(|e| overload.get(e))
                    .fold(1.0f64, |a, b| a.max(*b));
                out[i] = xs[i] / worst;
            }
        }

        // 2. Precios desde la carga cruda.
        for (e, cap) in &inputs.edge_capacity {
            if *cap <= 0.0 {
                continue;
            }
            let l = load.get(e).copied().unwrap_or(0.0);
            let grad = ((l - cap) / cap).clamp(-1.0, OVERLOAD_CLAMP);
            let mu = self.prices.entry(e.clone()).or_insert(0.0);
            *mu = (*mu + p.gamma * grad).max(0.0);
        }
        self.prices
            .retain(|e, _| inputs.edge_capacity.contains_key(e));

        self.ticks += 1;
        if self.ticks.is_multiple_of(5) {
            let served: f64 = out.iter().sum();
            let with_demand = flows
                .iter()
                .zip(&out)
                .filter(|(f, _)| f.c.drain_rate >= 0.01)
                .count();
            let starved = flows
                .iter()
                .zip(&out)
                .filter(|(f, x)| f.c.drain_rate >= 0.01 && **x < FLOW_EPSILON)
                .count();
            let max_mu = self.prices.values().cloned().fold(0.0f64, f64::max);
            let priced = self.prices.values().filter(|m| **m > 0.0).count();
            info!(
                n_commodities = flows.len(),
                served_total = format!("{served:.1}"),
                with_demand,
                // El invariante que este asignador existe para garantizar:
                // con α=1 esto debe ser 0 SIEMPRE que haya ruta.
                starved,
                priced_edges = priced,
                max_price = format!("{max_mu:.4}"),
                "mcmcf.num: x = (w/precio)^(1/α), publicado proyectado a factible",
            );
        }

        assemble(&flows, &out)
    }
}

/// Waterfilling progresivo exacto (max-min lexicográfico) sobre el routing
/// fraccional fijo, en dos pasadas: primero el drenaje (δ_k), después el
/// llenado (R_k/T) sobre la capacidad residual — el mismo orden lexicográfico
/// drenaje-antes-que-llenado que el LP, sin mezclar unidades en una pasada.
pub fn maxmin_allocate(inputs: &McmcfInputs, wcmp_full: &Wcmp, wcmp_qkd: &Wcmp) -> McmcfSolution {
    let flows = prepare_flows(inputs, wcmp_full, wcmp_qkd);
    let mut residual: BTreeMap<(String, String), f64> = inputs
        .edge_capacity
        .iter()
        .map(|(e, c)| (e.clone(), *c))
        .collect();

    let drain: Vec<f64> = flows.iter().map(|f| f.c.drain_rate).collect();
    let fill: Vec<f64> = flows
        .iter()
        .map(|f| f.c.remaining() / T_REPLAN_SECONDS)
        .collect();
    let t_drain = waterfill(&flows, &drain, &mut residual);
    let t_fill = waterfill(&flows, &fill, &mut residual);

    let xs: Vec<f64> = (0..flows.len())
        .map(|i| t_drain[i] * drain[i] + t_fill[i] * fill[i])
        .collect();

    let min_drain_frac = t_drain
        .iter()
        .zip(&drain)
        .filter(|(_, d)| **d >= 0.01)
        .map(|(t, _)| *t)
        .fold(f64::INFINITY, f64::min);
    info!(
        n_commodities = flows.len(),
        served_total = format!("{:.1}", xs.iter().sum::<f64>() + 0.0),
        // La garantía max-min en una cifra: la peor fracción de drenaje
        // servida. El vértice del LP la dejaba en 0.0 con pares al 100 %
        // servidos al lado; aquí sube hasta donde el peor corte permita.
        min_drain_frac = format!(
            "{:.3}",
            if min_drain_frac.is_finite() {
                min_drain_frac
            } else {
                1.0
            }
        ),
        "mcmcf.maxmin: waterfill drenaje→llenado",
    );

    assemble(&flows, &xs)
}

/// Una pasada de progressive filling: sube la fracción común `t` de todas
/// las commodities activas hasta que una arista se agota o una commodity
/// llega a t=1; congela y repite. Consume `residual` (la segunda pasada ve
/// lo que dejó la primera). Devuelve la fracción servida por commodity.
///
/// Termina siempre: cada iteración o bien agota una arista o bien completa
/// una commodity (Δ es exactamente el mínimo de ambas familias de cotas), y
/// hay un número finito de ambas.
fn waterfill(
    flows: &[Flow],
    demand: &[f64],
    residual: &mut BTreeMap<(String, String), f64>,
) -> Vec<f64> {
    let n = flows.len();
    let mut t = vec![0.0f64; n];
    let mut active: Vec<usize> = (0..n)
        .filter(|&i| flows[i].routable && demand[i] > FLOW_EPSILON)
        .collect();

    while !active.is_empty() {
        // Consumo por arista de las activas, a fracción unitaria.
        let mut slope: BTreeMap<&(String, String), f64> = BTreeMap::new();
        for &i in &active {
            for (e, fr) in &flows[i].fractions {
                *slope.entry(e).or_insert(0.0) += fr * demand[i];
            }
        }
        // Δ = min(techo de las commodities, techo de las aristas).
        let mut delta = active
            .iter()
            .map(|&i| 1.0 - t[i])
            .fold(f64::INFINITY, f64::min);
        for (e, s) in &slope {
            if *s > FLOW_EPSILON {
                if let Some(r) = residual.get(*e) {
                    delta = delta.min((r.max(0.0)) / s);
                }
            }
        }
        if !delta.is_finite() {
            break;
        }
        if delta > 0.0 {
            for &i in &active {
                t[i] += delta;
            }
            for (e, s) in &slope {
                if let Some(r) = residual.get_mut(*e) {
                    *r -= delta * s;
                }
            }
        }
        // Congelar: primero las commodities completas, después las que tocan
        // una arista agotada. Si nada se congela con Δ=0, no hay progreso
        // posible (numérico) y se corta.
        let exhausted: Vec<&(String, String)> = slope
            .keys()
            .filter(|e| residual.get(**e).map(|r| *r <= 1e-9).unwrap_or(false))
            .copied()
            .collect();
        let before = active.len();
        active.retain(|&i| {
            t[i] < 1.0 - 1e-12 && !flows[i].fractions.keys().any(|e| exhausted.contains(&e))
        });
        if active.len() == before && delta <= 0.0 {
            break;
        }
    }
    t
}

/// Contexto de asignación que viaja con el servicio: qué asignador manda,
/// sus parámetros y el estado de precios (compartido — el debouncer y el
/// tick capturan clones y todos pisan el mismo `NumState`).
#[derive(Clone)]
pub struct RateAlloc {
    pub allocator: Allocator,
    pub params: NumParams,
    pub state: std::sync::Arc<parking_lot::Mutex<NumState>>,
}

impl RateAlloc {
    pub fn new(allocator: Allocator, params: NumParams) -> Self {
        Self {
            allocator,
            params,
            state: std::sync::Arc::new(parking_lot::Mutex::new(NumState::default())),
        }
    }
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    fn hop(id: u32, w: u32) -> WcmpNextHop {
        WcmpNextHop {
            qkc_id: id,
            weight: w,
        }
    }

    /// Tabla mínima: 1→2 directo.
    fn wcmp_line() -> Wcmp {
        let mut w = Wcmp::new();
        w.entry("1".into())
            .or_default()
            .insert("2".into(), vec![hop(2, 1)]);
        w.entry("2".into())
            .or_default()
            .insert("1".into(), vec![hop(1, 1)]);
        w
    }

    /// Rombo 1→4 vía 2 (peso 3) y vía 3 (peso 1).
    fn wcmp_diamond() -> Wcmp {
        let mut w = Wcmp::new();
        w.entry("1".into())
            .or_default()
            .insert("4".into(), vec![hop(2, 3), hop(3, 1)]);
        w.entry("2".into())
            .or_default()
            .insert("4".into(), vec![hop(4, 1)]);
        w.entry("3".into())
            .or_default()
            .insert("4".into(), vec![hop(4, 1)]);
        w
    }

    fn commodity(src: &str, dst: &str, drain: f64, level: f64) -> CommodityDemand {
        CommodityDemand {
            src_dkms: src.into(),
            dst_dkms: dst.into(),
            level,
            capacity: 4096.0,
            drain_rate: drain,
            timestamp_ms: 1,
            grade: KeyGrade::Pqc,
        }
    }

    fn inputs_line(c: Vec<CommodityDemand>, cap: f64) -> McmcfInputs {
        let mut edge_capacity = HashMap::new();
        edge_capacity.insert(("1".to_string(), "2".to_string()), cap);
        let mut dkms_to_qkc = HashMap::new();
        dkms_to_qkc.insert("dA".to_string(), "1".to_string());
        dkms_to_qkc.insert("dB".to_string(), "2".to_string());
        McmcfInputs {
            commodities: c,
            edge_capacity,
            dkms_to_qkc,
            pqc_edges: Default::default(),
        }
    }

    #[test]
    fn fractions_split_by_weight_and_reach_destination() {
        let (fr, arrived) = commodity_edge_fractions(&wcmp_diamond(), "1", "4");
        assert!((arrived - 1.0).abs() < 1e-9);
        assert!((fr[&edge_key("1", "2")] - 0.75).abs() < 1e-9);
        assert!((fr[&edge_key("1", "3")] - 0.25).abs() < 1e-9);
        assert!((fr[&edge_key("2", "4")] - 0.75).abs() < 1e-9);
        assert!((fr[&edge_key("3", "4")] - 0.25).abs() < 1e-9);
    }

    #[test]
    fn fractions_lose_mass_without_route() {
        let (fr, arrived) = commodity_edge_fractions(&Wcmp::new(), "1", "4");
        assert!(fr.is_empty());
        assert_eq!(arrived, 0.0);
    }

    /// Dos commodities compartiendo una arista de 100 con demandas 300/100:
    /// el equilibrio proporcional (α=1, sin fill) reparte 75/25 — pesos
    /// proporcionales, capacidad agotada. KKT: precio > 0 ⇔ arista llena.
    #[test]
    fn num_converges_to_proportional_share_and_kkt_holds() {
        let cs = vec![
            commodity("dA", "dB", 300.0, 4096.0), // buffer lleno: sin fill
            commodity("dB", "dA", 100.0, 4096.0),
        ];
        let mut inputs = inputs_line(cs, 100.0);
        inputs.dkms_to_qkc.insert("dB".to_string(), "2".to_string());
        let w = wcmp_line();
        let mut st = NumState::default();
        let p = NumParams {
            fill_weight: 0.0,
            ..Default::default()
        };
        let mut sol = st.step(&inputs, &w, &w, &p);
        for _ in 0..400 {
            sol = st.step(&inputs, &w, &w, &p);
        }
        let a = sol.rates[&flow_id("dA", "dB")];
        let b = sol.rates[&flow_id("dB", "dA")];
        // Factibilidad (proyección) y reparto proporcional con tolerancia.
        assert!(a + b <= 100.0 + 1.0, "capacidad violada: {a}+{b}");
        assert!((a - 75.0).abs() < 8.0, "a={a} lejos de 75");
        assert!((b - 25.0).abs() < 8.0, "b={b} lejos de 25");
        // Nadie a cero: la garantía del log-utility.
        assert!(a > 1.0 && b > 1.0);
    }

    /// Arranque en frío: precios a cero, todo el mundo pediría su cap — lo
    /// publicado en el primer tick ya viene proyectado a factible.
    #[test]
    fn num_first_tick_output_is_feasible() {
        let cs = vec![
            commodity("dA", "dB", 5000.0, 4096.0),
            commodity("dB", "dA", 5000.0, 4096.0),
        ];
        let inputs = inputs_line(cs, 100.0);
        let w = wcmp_line();
        let mut st = NumState::default();
        let sol = st.step(&inputs, &w, &w, &NumParams::default());
        let total: f64 = sol.rates.values().sum();
        assert!(total <= 100.0 + 1.0, "primer tick infactible: {total}");
    }

    /// El caso que motivó todo: bajo sobrecarga, NADIE con demanda queda a
    /// cero (el vértice del LP dejaba pares al 100 % de ceros).
    #[test]
    fn num_never_starves_under_overload() {
        let cs = vec![
            commodity("dA", "dB", 800.0, 4096.0),
            commodity("dB", "dA", 800.0, 4096.0),
        ];
        let inputs = inputs_line(cs, 100.0);
        let w = wcmp_line();
        let mut st = NumState::default();
        let p = NumParams::default();
        let mut sol = st.step(&inputs, &w, &w, &p);
        for _ in 0..200 {
            sol = st.step(&inputs, &w, &w, &p);
        }
        for (fid, r) in &sol.rates {
            assert!(*r > 1.0, "{fid} muerto de hambre con demanda: {r}");
        }
    }

    /// Waterfill a mano: arista de 100, demandas 300 y 100 → fracción común
    /// t = 100/400 = 0.25 ⇒ 75 y 25. La misma respuesta que el proporcional
    /// aquí (un solo cuello): las diferencias aparecen con cuellos anidados.
    #[test]
    fn waterfill_single_bottleneck_common_fraction() {
        let cs = vec![
            commodity("dA", "dB", 300.0, 4096.0),
            commodity("dB", "dA", 100.0, 4096.0),
        ];
        let inputs = inputs_line(cs, 100.0);
        let w = wcmp_line();
        let sol = maxmin_allocate(&inputs, &w, &w);
        let a = sol.rates[&flow_id("dA", "dB")];
        let b = sol.rates[&flow_id("dB", "dA")];
        assert!((a - 75.0).abs() < 1e-6, "a={a}");
        assert!((b - 25.0).abs() < 1e-6, "b={b}");
    }

    /// Lexicográfico de verdad: con capacidad de sobra la demanda se sirve
    /// entera (t=1) y el resto de la arista queda para el llenado.
    #[test]
    fn waterfill_serves_full_demand_then_fills() {
        // δ=10 con arista de 1000: drenaje entero + llenado con el residual.
        let cs = vec![commodity("dA", "dB", 10.0, 96.0)]; // R = 4000
        let inputs = inputs_line(cs, 1000.0);
        let w = wcmp_line();
        let sol = maxmin_allocate(&inputs, &w, &w);
        let r = sol.rates[&flow_id("dA", "dB")];
        // 10 de drenaje + min(4000/5, 990) = 10 + 800 = 810.
        assert!((r - 810.0).abs() < 1e-6, "r={r}");
    }

    /// Determinismo: mismas entradas, misma salida, dos construcciones.
    #[test]
    fn maxmin_is_deterministic() {
        let mk = || {
            let cs = vec![
                commodity("dA", "dB", 300.0, 100.0),
                commodity("dB", "dA", 100.0, 2000.0),
            ];
            let inputs = inputs_line(cs, 150.0);
            let w = wcmp_line();
            maxmin_allocate(&inputs, &w, &w).rates
        };
        assert_eq!(mk(), mk());
    }
}
