//! Clasificación de buffers DKMS en clases QoS según `fill_ratio`.
//!
//! Spec del Python `code_dkms/src/DKMS/control/buffer_priority.py`:
//!
//! | fill_ratio | clase | peso MCF |
//! |------------|-------|----------|
//! | < 0.10     | Priority    | 100 |
//! | < 0.30     | Important   |  30 |
//! | < 0.50     | Quickly     |  10 |
//! | < 0.70     | Relax       |   3 |
//! | < 0.95     | BestEffort  |   1 |
//! | ≥ 0.95     | Saturated   |   0 (excluido del MCF) |
//!
//! **Histéresis** en cada transición para evitar flapping:
//! - 5 % de margen entre clases consecutivas (entry vs exit threshold).
//! - 10 % específico en la frontera SATURATED (entra al 95 %, sale al 85 %).
//!
//! Como las prioridades son **strict** en el solver SDN
//! (`_solve_strict_priority_within_class`), un flow en BestEffort recibe
//! ~rate 0 en cuanto otro flow del mismo enlace está en Priority — los
//! pesos sirven para orden de clases y fairness intra-clase.

use std::fmt;

/// Mismo enum que `sdn::priority::TrafficPriority`. Se duplica aquí para
/// no acoplar el crate `dkms` con `sdn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferQos {
    Priority,
    Important,
    Quickly,
    Relax,
    BestEffort,
    Saturated,
}

impl BufferQos {
    pub fn as_str(self) -> &'static str {
        match self {
            BufferQos::Priority   => "priority",
            BufferQos::Important  => "important",
            BufferQos::Quickly    => "quickly",
            BufferQos::Relax      => "relax",
            BufferQos::BestEffort => "best_effort",
            BufferQos::Saturated  => "saturated",
        }
    }

    /// Orden ascendente del 1 (Priority = más alto) al 6 (Saturated).
    /// Útil para comparar transiciones (entrar a más estricto vs salir).
    pub fn rank(self) -> u8 {
        match self {
            BufferQos::Priority   => 1,
            BufferQos::Important  => 2,
            BufferQos::Quickly    => 3,
            BufferQos::Relax      => 4,
            BufferQos::BestEffort => 5,
            BufferQos::Saturated  => 6,
        }
    }
}

impl fmt::Display for BufferQos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Margen de histéresis (5 %) entre clases consecutivas.
const HYST_MARGIN: f64 = 0.05;

/// Umbral SATURATED: entrar al 95 %, salir al 85 % (margen 10 %).
const SAT_ENTER: f64 = 0.95;
const SAT_EXIT:  f64 = 0.85;

/// Umbrales de entrada (fill_ratio creciendo, va a una clase MENOR).
/// Tupla `(entry_threshold, clase_resultante)`. Ordenados de menor a mayor.
const ENTRY_THRESHOLDS: &[(f64, BufferQos)] = &[
    (0.10, BufferQos::Important),
    (0.30, BufferQos::Quickly),
    (0.50, BufferQos::Relax),
    (0.70, BufferQos::BestEffort),
];

/// Clasifica un `fill_ratio` en una clase QoS aplicando histéresis si se
/// pasa `prev`. Devuelve la nueva clase.
///
/// Reglas:
/// 1. Si `fill_ratio ≥ 0.95` → SATURATED (entry).
/// 2. Si `prev == SATURATED` y `fill_ratio > 0.85` → permanece SATURATED.
/// 3. Para los demás niveles: usa `ENTRY_THRESHOLDS` para calcular la
///    clase ideal yendo de menor a mayor fill. Si `prev` es una clase
///    "más restrictiva" (rank menor = fill esperado menor) que la
///    ideal, se aplica histéresis: el caller solo puede degradarse
///    cuando `fill_ratio` baja `HYST_MARGIN` por debajo del entry de
///    `prev`.
pub fn classify(fill_ratio: f64, prev: Option<BufferQos>) -> BufferQos {
    // SATURATED con histéresis 10 %.
    if let Some(BufferQos::Saturated) = prev {
        if fill_ratio > SAT_EXIT {
            return BufferQos::Saturated;
        }
        // cae por debajo del 85 % → reclassifica como si fuese fresh.
    } else if fill_ratio >= SAT_ENTER {
        return BufferQos::Saturated;
    }
    // Clase "ideal" por umbrales de entrada (camino de subida).
    let mut ideal = BufferQos::Priority;
    for (entry, cls) in ENTRY_THRESHOLDS {
        if fill_ratio >= *entry {
            ideal = *cls;
        }
    }
    // Si no hay `prev`, devolvemos `ideal` directamente.
    let Some(prev) = prev else { return ideal };
    // Sin cambio: mantén.
    if prev == ideal {
        return ideal;
    }
    // Subiendo (filling): prev tiene menos rank que ideal (clase más alta).
    // No aplicamos histéresis al subir — el ideal manda.
    if prev.rank() < ideal.rank() {
        return ideal;
    }
    // Bajando (emptying): prev tiene más rank que ideal (clase más baja).
    // Aplica histéresis: permanece en `prev` hasta que `fill_ratio` baje
    // `HYST_MARGIN` por debajo del entry de `prev`.
    let exit_of_prev = match prev {
        BufferQos::Important  => 0.10 - HYST_MARGIN,
        BufferQos::Quickly    => 0.30 - HYST_MARGIN,
        BufferQos::Relax      => 0.50 - HYST_MARGIN,
        BufferQos::BestEffort => 0.70 - HYST_MARGIN,
        // Priority no tiene exit (siempre se puede entrar/permanecer).
        BufferQos::Priority   => 0.0,
        // Saturated ya gestionado arriba.
        BufferQos::Saturated  => SAT_EXIT,
    };
    if fill_ratio > exit_of_prev {
        // Aún no cruzamos el exit → mantén la clase anterior.
        prev
    } else {
        ideal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_prev_uses_entry_thresholds() {
        assert_eq!(classify(0.00, None), BufferQos::Priority);
        assert_eq!(classify(0.09, None), BufferQos::Priority);
        assert_eq!(classify(0.10, None), BufferQos::Important);
        assert_eq!(classify(0.29, None), BufferQos::Important);
        assert_eq!(classify(0.30, None), BufferQos::Quickly);
        assert_eq!(classify(0.49, None), BufferQos::Quickly);
        assert_eq!(classify(0.50, None), BufferQos::Relax);
        assert_eq!(classify(0.69, None), BufferQos::Relax);
        assert_eq!(classify(0.70, None), BufferQos::BestEffort);
        assert_eq!(classify(0.94, None), BufferQos::BestEffort);
        assert_eq!(classify(0.95, None), BufferQos::Saturated);
        assert_eq!(classify(0.99, None), BufferQos::Saturated);
    }

    #[test]
    fn saturated_hysteresis() {
        // Ya en SATURATED, no salimos hasta caer al 85 %.
        assert_eq!(classify(0.90, Some(BufferQos::Saturated)), BufferQos::Saturated);
        assert_eq!(classify(0.86, Some(BufferQos::Saturated)), BufferQos::Saturated);
        // Cae al 85 % o menos → reclasifica.
        assert_eq!(classify(0.85, Some(BufferQos::Saturated)), BufferQos::BestEffort);
        assert_eq!(classify(0.69, Some(BufferQos::Saturated)), BufferQos::Relax);
    }

    #[test]
    fn hysteresis_between_classes() {
        // Subir a clase menos prioritaria: no aplica histéresis.
        assert_eq!(classify(0.31, Some(BufferQos::Important)), BufferQos::Quickly);
        // Bajar a más prioritaria: requiere cruzar exit del prev.
        // Important entry = 0.10, exit = 0.05.
        // En 0.08 (entre 0.05 y 0.10) permanecemos en Important.
        assert_eq!(classify(0.08, Some(BufferQos::Important)), BufferQos::Important);
        // En 0.04 (< exit) caemos a Priority.
        assert_eq!(classify(0.04, Some(BufferQos::Important)), BufferQos::Priority);
    }

    #[test]
    fn rank_orders_correctly() {
        assert!(BufferQos::Priority.rank() < BufferQos::Important.rank());
        assert!(BufferQos::Important.rank() < BufferQos::Quickly.rank());
        assert!(BufferQos::BestEffort.rank() < BufferQos::Saturated.rank());
    }
}
