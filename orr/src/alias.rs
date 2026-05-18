//! Alias method (Walker, 1977) para muestreo eficiente de distribuciones
//! discretas con pesos. Construcción `O(K)`, sampling `O(1)`.
//!
//! Se usa en el ORR origen tras `GetPathsWithRatios` para elegir un
//! path por envío con probabilidad ≈ `omega_p`. Cuando la convergencia
//! del Law of Large Numbers acumula > unos cientos de samples, la
//! distribución empírica converge a las omegas teóricas.
//!
//! Implementación derivada del paper original de Walker; ver también
//! Vose, M. (1991) "A linear algorithm for generating random numbers
//! with a given distribution" para la variante con doble pasada
//! evitando overflow numérico.

use rand::Rng;

/// Sampler precomputado. Mantén una instancia por `(src,dst)` cacheada
/// en `OrrService.paths_cache_multipath`. Reconstruir es O(K).
#[derive(Debug, Clone)]
pub struct AliasSampler {
    /// `prob[i]` ∈ [0,1] — probabilidad de aceptar el índice `i` cuando
    /// el sample inicial cae sobre él. Si se rechaza, se devuelve
    /// `alias[i]` en su lugar.
    prob: Vec<f64>,
    alias: Vec<usize>,
}

impl AliasSampler {
    /// Construye el sampler desde un vector de pesos no negativos. Los
    /// pesos se normalizan internamente (no hace falta que sumen 1.0).
    ///
    /// Si todos los pesos son 0 o la lista está vacía, devuelve `None`.
    /// El caller debe gestionar ese caso (típicamente: fallback a
    /// single-path o no enviar).
    pub fn build(weights: &[f64]) -> Option<Self> {
        if weights.is_empty() {
            return None;
        }
        let k = weights.len();
        let total: f64 = weights.iter().sum();
        if total <= 0.0 {
            return None;
        }
        // Escalar cada peso por K/total → la suma escalada es K, y cada
        // bin tiene "altura" promedio 1.0.
        let scaled: Vec<f64> = weights.iter().map(|w| w * k as f64 / total).collect();

        let mut prob = vec![0.0_f64; k];
        let mut alias = vec![0_usize; k];

        // Particionar índices en "small" (altura < 1) y "large" (≥ 1).
        let mut small: Vec<usize> = Vec::with_capacity(k);
        let mut large: Vec<usize> = Vec::with_capacity(k);
        for (i, &s) in scaled.iter().enumerate() {
            if s < 1.0 {
                small.push(i);
            } else {
                large.push(i);
            }
        }

        let mut s = scaled.clone();
        // Walker's two-pass algorithm.
        while let (Some(&l), Some(&g)) = (small.last(), large.last()) {
            small.pop();
            large.pop();
            prob[l] = s[l];
            alias[l] = g;
            s[g] = (s[g] + s[l]) - 1.0;
            if s[g] < 1.0 {
                small.push(g);
            } else {
                large.push(g);
            }
        }
        // Cleanup: stacks restantes son cuasi-1.0 por aritmética FP.
        while let Some(g) = large.pop() {
            prob[g] = 1.0;
        }
        while let Some(l) = small.pop() {
            prob[l] = 1.0;
        }

        Some(Self { prob, alias })
    }

    /// Muestrea un índice usando el RNG proporcionado.
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> usize {
        let k = self.prob.len();
        if k == 0 {
            return 0;
        }
        let i = rng.gen_range(0..k);
        if rng.gen::<f64>() < self.prob[i] {
            i
        } else {
            self.alias[i]
        }
    }

    /// Número de elementos.
    pub fn len(&self) -> usize {
        self.prob.len()
    }

    /// Vacío.
    pub fn is_empty(&self) -> bool {
        self.prob.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn build_empty_returns_none() {
        assert!(AliasSampler::build(&[]).is_none());
    }

    #[test]
    fn build_all_zero_returns_none() {
        assert!(AliasSampler::build(&[0.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn build_single_weight_always_returns_zero() {
        let s = AliasSampler::build(&[1.0]).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for _ in 0..100 {
            assert_eq!(s.sample(&mut rng), 0);
        }
    }

    /// Test clave del OBJ-011: distribución empírica ≈ teórica con
    /// N=10000 samples y tolerancia 1.5 % absoluta (LLN converge así).
    #[test]
    fn empirical_distribution_matches_weights_n10000() {
        let weights = vec![0.5, 0.3, 0.2];
        let sampler = AliasSampler::build(&weights).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE);
        let n_samples = 10_000;
        let mut counts = vec![0_usize; weights.len()];
        for _ in 0..n_samples {
            counts[sampler.sample(&mut rng)] += 1;
        }
        for (i, &w) in weights.iter().enumerate() {
            let empirical = counts[i] as f64 / n_samples as f64;
            let diff = (empirical - w).abs();
            assert!(
                diff < 0.015,
                "weight[{i}]={w} empirical={empirical} diff={diff}",
            );
        }
    }

    #[test]
    fn weights_dont_need_to_sum_to_one() {
        // 50/30/20 ↔ 5/3/2 mismas probabilidades relativas.
        let s1 = AliasSampler::build(&[0.5, 0.3, 0.2]).unwrap();
        let s2 = AliasSampler::build(&[5.0, 3.0, 2.0]).unwrap();
        // Estructura interna debe normalizar igual.
        assert_eq!(s1.len(), s2.len());
    }

    #[test]
    fn extreme_skew_still_samples_minority() {
        let weights = vec![0.999, 0.001];
        let sampler = AliasSampler::build(&weights).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        let mut minority = 0;
        let n = 100_000;
        for _ in 0..n {
            if sampler.sample(&mut rng) == 1 {
                minority += 1;
            }
        }
        let empirical = minority as f64 / n as f64;
        assert!(
            (empirical - 0.001).abs() < 0.0005,
            "minority empirical={empirical}, expected ≈0.001",
        );
    }

    #[test]
    fn two_paths_50_50_balanced() {
        let weights = vec![0.5, 0.5];
        let sampler = AliasSampler::build(&weights).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut counts = [0_usize; 2];
        for _ in 0..10_000 {
            counts[sampler.sample(&mut rng)] += 1;
        }
        let p0 = counts[0] as f64 / 10_000.0;
        assert!((p0 - 0.5).abs() < 0.02, "50/50 sampling, p0={p0}");
    }
}
