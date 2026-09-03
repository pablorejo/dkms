//! Endurecimiento del proceso: material de clave fuera de swap y de core dumps.
//!
//! Los cinco binarios manejan claves en RAM (buffers ENC/DEC del QKC, secretos
//! de época, `master_secret` del ORR, claves de transporte del DKMS). Dos fugas
//! del sistema operativo las sacan del proceso sin que nadie las lea del socket:
//!
//! * **swap**: una página con clave paginada a disco sobrevive al proceso y
//!   queda en la partición de swap. `mlockall(MCL_CURRENT|MCL_FUTURE)` fija
//!   todas las páginas —presentes y futuras— en RAM. `MCL_FUTURE` es
//!   imprescindible: casi todo el material de clave se asigna DESPUÉS del
//!   arranque (los buffers se llenan en caliente), así que bloquear solo las
//!   páginas actuales no protegería nada.
//! * **core dump**: si el proceso peta —y el perfil release lleva
//!   `panic = "abort"`—, un core dump vuelca toda la RAM, claves incluidas, a
//!   un fichero. `prctl(PR_SET_DUMPABLE, 0)` lo impide y de paso quita al
//!   proceso del `ptrace` de procesos no-root.
//!
//! Ambas son **best-effort**: `mlockall` necesita `CAP_IPC_LOCK` o un
//! `RLIMIT_MEMLOCK` holgado, que un contenedor sin privilegios puede no tener.
//! Un fallo se avisa ALTO con la remediación y **no aborta el arranque**:
//! preferimos un módulo corriendo con una defensa de menos a uno que no
//! arranca. Llamar pronto en `main`, tras `logging::init()` (usa `tracing`) y
//! antes de tocar claves. Es idempotente.

#![allow(unsafe_code)] // las llamadas libc de este módulo, y solo ellas

/// Fija la memoria del proceso en RAM y desactiva los core dumps. Ver el
/// módulo. No-op con aviso en plataformas que no son Linux.
#[cfg(target_os = "linux")]
pub fn harden_process() {
    // mlockall: todas las páginas presentes y futuras, dentro de RAM.
    // SAFETY: llamada libc sin punteros; devuelve 0 (ok) o -1 (errno).
    let locked = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if locked != 0 {
        let e = std::io::Error::last_os_error();
        tracing::warn!(
            error = %e,
            "hardening: mlockall falló; el material de clave PUEDE ir a swap. \
             Concede CAP_IPC_LOCK o sube RLIMIT_MEMLOCK (docker: --ulimit memlock=-1; \
             systemd: LimitMEMLOCK=infinity; k8s: securityContext.capabilities.add=[IPC_LOCK])"
        );
    }
    // PR_SET_DUMPABLE = 0: sin core dumps (claves a disco) ni ptrace ajeno.
    // SAFETY: prctl variádica con escalares; devuelve 0 (ok) o -1 (errno).
    let nodump = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) };
    if nodump != 0 {
        let e = std::io::Error::last_os_error();
        tracing::warn!(
            error = %e,
            "hardening: PR_SET_DUMPABLE=0 falló; un core dump podría contener claves"
        );
    }
    if locked == 0 && nodump == 0 {
        tracing::info!("hardening: mlockall + no-coredump activos");
    }
}

/// No-op fuera de Linux (mlockall/prctl son específicos de Linux). Los
/// despliegues reales corren en Linux; esto solo mantiene la compilación
/// portable para desarrollo local.
#[cfg(not(target_os = "linux"))]
pub fn harden_process() {
    tracing::debug!("hardening: no-op (plataforma no Linux)");
}
