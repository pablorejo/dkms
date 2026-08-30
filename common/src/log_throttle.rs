//! Throttle para logs por evento.
//!
//! Un `warn!` por frame en un modo de fallo sostenido —enlace seco, PSK
//! asimétrica, KME caído— no es diagnóstico, es un disco lleno (752 MB en
//! diez minutos, medido en el generador antes de agregar por tick). El patrón
//! de la casa es contar siempre y hablar en las potencias de dos: el primer
//! caso sale entero, y después la frecuencia decae sola sin perder la escala.

/// `true` en las ocurrencias 1.ª, 2.ª, 3.ª, 5.ª, 9.ª, 17.ª… `n` es el valor
/// del contador **antes** de este evento, que es justo lo que devuelve
/// `fetch_add(1, _)`.
pub fn nth_is_loud(n: u64) -> bool {
    n == 0 || n.is_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::nth_is_loud;

    #[test]
    fn first_events_are_loud_then_only_powers_of_two() {
        let loud: Vec<u64> = (0..40).filter(|&n| nth_is_loud(n)).collect();
        assert_eq!(loud, vec![0, 1, 2, 4, 8, 16, 32]);
    }
}
