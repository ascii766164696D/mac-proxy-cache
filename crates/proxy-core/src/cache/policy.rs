use http::{HeaderMap, Method, Uri};
use http_cache_semantics::CachePolicy;
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Serializable wrapper around CachePolicy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPolicy {
    inner: CachePolicy,
}

impl CachedPolicy {
    /// Create a new CachePolicy from an HTTP request and response.
    pub fn from_request_response(
        req: &impl http_cache_semantics::RequestLike,
        res: &impl http_cache_semantics::ResponseLike,
    ) -> Self {
        Self {
            inner: CachePolicy::new(req, res),
        }
    }

    /// Check if the cached response is still fresh.
    pub fn is_fresh(&self) -> bool {
        use http_cache_semantics::BeforeRequest;
        match self
            .inner
            .before_request(&MinimalRequest, std::time::SystemTime::now())
        {
            BeforeRequest::Fresh(_) => true,
            BeforeRequest::Stale { .. } => false,
        }
    }

    /// Check if the response is storable (cacheable).
    pub fn is_storable(&self) -> bool {
        self.inner.is_storable()
    }

    /// Get the time-to-live remaining.
    pub fn time_to_live(&self) -> std::time::Duration {
        self.inner.time_to_live(std::time::SystemTime::now())
    }

    /// Serialize the policy to bytes for SQLite storage.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(&self.inner)
            .map_err(|e| Error::Proxy(format!("Failed to serialize cache policy: {}", e)))
    }

    /// Deserialize the policy from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let inner: CachePolicy = serde_json::from_slice(bytes)
            .map_err(|e| Error::Proxy(format!("Failed to deserialize cache policy: {}", e)))?;
        Ok(Self { inner })
    }

    /// Get the inner CachePolicy reference.
    pub fn inner(&self) -> &CachePolicy {
        &self.inner
    }
}

/// Minimal request implementation for freshness checks.
struct MinimalRequest;

impl http_cache_semantics::RequestLike for MinimalRequest {
    fn method(&self) -> &Method {
        &Method::GET
    }

    fn uri(&self) -> Uri {
        "/".parse().unwrap()
    }

    fn headers(&self) -> &HeaderMap {
        static EMPTY: std::sync::LazyLock<HeaderMap> =
            std::sync::LazyLock::new(HeaderMap::new);
        &EMPTY
    }

    fn is_same_uri(&self, _other: &Uri) -> bool {
        true
    }
}
