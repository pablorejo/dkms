//! Ayudas para tests que dependen del entorno.
//!
//! Un test que no puede correr donde está (el `openssl` del sistema no sabe
//! ML-DSA, el backend de LP no es el que el test exige) tiene que decirlo en
//! voz alta, no terminar en verde sin haber comprobado nada. Vive fuera de
//! `#[cfg(test)]` porque los tests de otros crates también lo usan.

/// Registra un salto o, con `DKMS_NO_TEST_SKIPS=1` (lo pone la CI), lo
/// convierte en fallo: así un verde significa que todo corrió de verdad.
///
/// El llamador hace `return` justo después; esta función no lo hace por él
/// para que el salto quede visible en el propio test.
pub fn skip_or_fail(reason: &str) {
    if std::env::var_os("DKMS_NO_TEST_SKIPS").is_some() {
        panic!("test saltado con DKMS_NO_TEST_SKIPS activo: {reason}");
    }
    eprintln!("SKIPPED: {reason}");
}
