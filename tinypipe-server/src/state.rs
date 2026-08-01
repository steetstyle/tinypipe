use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_storage::SqliteStorage;
use tinypipe_tools::SubgraphToolRegistry;

use crate::meta::RouteConfig;

/// Dinamik plan önbelleği kapasitesi (`POST /api/run`).
pub const DYNAMIC_PLAN_CAPACITY: usize = 64;

/// (son erişim, kaynak kod, derlenmiş plan).
pub type DynamicPlanEntry = (Instant, String, Arc<CompiledPlan>);

pub struct AppState {
    pub storage: Arc<SqliteStorage>,
    pub registry: Arc<SubgraphToolRegistry<Arc<SqliteStorage>>>,
    /// graph id → derlenmiş plan (mevcut versiyon).
    pub plans: RwLock<HashMap<String, Arc<CompiledPlan>>>,
    /// yayın yolu → route yapılandırması.
    pub routes: RwLock<HashMap<String, RouteConfig>>,
    /// GET yanıt önbelleği: hash → (son kullanma, gövde).
    pub resp_cache: RwLock<HashMap<u64, (Instant, Vec<u8>)>>,
    /// dinamik kod hash'i → (son erişim, kod, plan). Kod saklanır ki
    /// hash çakışması yanlış plan döndürmesin (hash yalnızca adrestir).
    pub dynamic_plans: RwLock<HashMap<u64, DynamicPlanEntry>>,
    pub token: Option<String>,
    pub audit: bool,
}

impl AppState {
    pub fn new(
        storage: Arc<SqliteStorage>,
        registry: Arc<SubgraphToolRegistry<Arc<SqliteStorage>>>,
        token: Option<String>,
        audit: bool,
    ) -> Self {
        AppState {
            storage,
            registry,
            plans: RwLock::new(HashMap::new()),
            routes: RwLock::new(HashMap::new()),
            resp_cache: RwLock::new(HashMap::new()),
            dynamic_plans: RwLock::new(HashMap::new()),
            token,
            audit,
        }
    }

    /// Dinamik plan önbelleğine ekler; kapasite aşılırsa en eskiyi atar.
    /// (spawn_blocking iş parçacıklarından çağrılır — senkron kilit.)
    pub fn cache_dynamic_plan(&self, key: u64, code: String, plan: Arc<CompiledPlan>) {
        let mut plans = self.dynamic_plans.blocking_write();
        if plans.len() >= DYNAMIC_PLAN_CAPACITY {
            let oldest = plans
                .iter()
                .min_by_key(|(_, (ts, _, _))| *ts)
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                plans.remove(&k);
            }
        }
        plans.insert(key, (Instant::now(), code, plan));
    }

    /// FNV-1a 64-bit (bağımlılıksız, cache adresi için yeterli).
    pub fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}
