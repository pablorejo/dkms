//! Caché SAE → DKMS con *single-flight*.
//!
//! Cuando un SAE pide claves para `[SAE_Y, SAE_Z1, SAE_Z2]`, el DKMS tiene
//! que saber en qué DKMS reside cada SAE para agrupar la distribución.
//! Eso se llama el *SAE binding*. La fuente de verdad es la SDN, pero
//! consultarla por cada petición es prohibitivo bajo carga, así que
//! cacheamos con TTL e *invalidación push* cuando la SDN nos notifica un
//! cambio.
//!
//! El **single-flight** se obtiene gratis con `moka::future::Cache::try_get_with`:
//! varias tareas que pidan el mismo `SaeId` simultáneamente compartirán la
//! única llamada al *resolver* subyacente.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use moka::future::Cache;
use tracing::trace;

use common::ids::{NodeId, SaeId};

use crate::error::{DkmsError, Result};

/// Implementación concreta de "dónde vive este SAE". Lo cumplen tanto el
/// cliente gRPC contra la SDN como una tabla estática local.
#[async_trait]
pub trait SaeResolver: Send + Sync + 'static {
    async fn resolve(&self, sae: &SaeId) -> Result<NodeId>;
}

/// Resolver estático para tests/local-dev: un mapa `SaeId → NodeId`.
pub struct StaticSaeResolver {
    map: dashmap::DashMap<SaeId, NodeId>,
}

impl StaticSaeResolver {
    pub fn new() -> Self {
        Self {
            map: dashmap::DashMap::new(),
        }
    }

    pub fn with_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (SaeId, NodeId)>,
    {
        let me = Self::new();
        for (k, v) in entries {
            me.map.insert(k, v);
        }
        me
    }

    pub fn insert(&self, sae: SaeId, node: NodeId) {
        self.map.insert(sae, node);
    }
}

impl Default for StaticSaeResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SaeResolver for StaticSaeResolver {
    async fn resolve(&self, sae: &SaeId) -> Result<NodeId> {
        self.map
            .get(sae)
            .map(|n| n.clone())
            .ok_or_else(|| DkmsError::SaeBindingLookupFailed(sae.clone()))
    }
}

/// Resolver que pregunta a la SDN vía gRPC `GetSaeBinding`. Pensado
/// para envolverse en [`SaeBindingCache`] (TTL + single-flight) para
/// que sólo la **primera** consulta de un SAE pegue a la red —
/// las siguientes se sirven desde la cache hasta que expire la TTL.
pub struct SdnSaeResolver {
    sdn: Arc<crate::southbound::SdnClient>,
}

impl SdnSaeResolver {
    pub fn new(sdn: Arc<crate::southbound::SdnClient>) -> Self {
        Self { sdn }
    }
}

#[async_trait]
impl SaeResolver for SdnSaeResolver {
    async fn resolve(&self, sae: &SaeId) -> Result<NodeId> {
        let binding = self.sdn.get_sae_binding(sae.as_str()).await.map_err(|e| {
            // Cualquier error de red lo convertimos a "lookup failed"
            // para que el caller no se cuelgue y la moka cache no
            // memoize el error indefinidamente — la TTL la borra a
            // tiempo, igual que con un SAE realmente desconocido.
            tracing::debug!(error = %e, sae = %sae, "sdn get_sae_binding failed");
            DkmsError::SaeBindingLookupFailed(sae.clone())
        })?;
        if binding.dkms_id.is_empty() {
            return Err(DkmsError::SaeBindingLookupFailed(sae.clone()));
        }
        Ok(NodeId::new(binding.dkms_id))
    }
}

/// Caché con TTL y *single-flight* delante de cualquier [`SaeResolver`].
pub struct SaeBindingCache {
    cache: Cache<SaeId, NodeId>,
    upstream: Arc<dyn SaeResolver>,
}

impl SaeBindingCache {
    pub fn new(upstream: Arc<dyn SaeResolver>, ttl_secs: u64, max_entries: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_entries)
            .time_to_live(Duration::from_secs(ttl_secs.max(1)))
            .build();
        Self { cache, upstream }
    }

    pub async fn resolve(&self, sae: &SaeId) -> Result<NodeId> {
        let upstream = self.upstream.clone();
        let sae_clone = sae.clone();
        let res = self
            .cache
            .try_get_with(sae.clone(), async move {
                trace!(%sae_clone, "sae binding cache miss, querying upstream");
                upstream.resolve(&sae_clone).await
            })
            .await;
        // moka envuelve nuestro error en Arc<DkmsError>. Lo desenvolvemos
        // exponiendo solo el `SaeBindingLookupFailed` que es el caso útil.
        res.map_err(|e| match e.as_ref() {
            DkmsError::SaeBindingLookupFailed(s) => DkmsError::SaeBindingLookupFailed(s.clone()),
            _ => DkmsError::SaeBindingLookupFailed(sae.clone()),
        })
    }

    /// Invalidación explícita — la SDN debería empujarla cuando un SAE
    /// cambia de DKMS.
    pub async fn invalidate(&self, sae: &SaeId) {
        self.cache.invalidate(sae).await;
    }

    pub async fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_returns_cached_value() {
        let r = Arc::new(StaticSaeResolver::with_entries([(
            SaeId::new("alice"),
            NodeId::new("dkms-a"),
        )]));
        let cache = SaeBindingCache::new(r, 60, 100);
        let n = cache.resolve(&SaeId::new("alice")).await.unwrap();
        assert_eq!(n.as_str(), "dkms-a");
        // Segunda llamada debe seguir devolviendo el mismo valor.
        let n2 = cache.resolve(&SaeId::new("alice")).await.unwrap();
        assert_eq!(n2.as_str(), "dkms-a");
    }

    #[tokio::test]
    async fn unknown_sae_surfaces_lookup_failed() {
        let r = Arc::new(StaticSaeResolver::new());
        let cache = SaeBindingCache::new(r, 60, 100);
        let err = cache.resolve(&SaeId::new("ghost")).await.unwrap_err();
        assert!(matches!(err, DkmsError::SaeBindingLookupFailed(_)));
    }
}
