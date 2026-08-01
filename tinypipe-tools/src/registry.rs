//! `SubgraphToolRegistry` — `subgraph:` çağrılarını gerçek grafiklere bağlayan
//! registry (CLI execute/resume ve scheduler tarafından kullanılır).
//!
//! Gömülü tool'ları bir `MockToolRegistry`'den alır; `subgraph:<name>` target'ları
//! için çocuk grafiği storage'dan yükler ve `CompiledExecutor` ile çalıştırır.
//! Çocuk grafiğin kendi `subgraph:` çağrıları aynı registry üzerinden çözülür
//! (recursion derinliği VM'in `max_recursion_depth`'i tarafından sınırlanır).
//!
//! Performans: çağrı başına iki hızlı yol vardır — (1) name→id çözümlemesi
//! indeksli `find_graph_by_name` ile yapılır (tam tablo taraması yerine),
//! (2) decode edilmiş çocuk planlar `plan_cache`'te (graph_id, version) ile
//! saklanır; grafik versiyonu değişmedikçe plan yeniden decode edilmez.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::env_deps::EnvDepReport;
use crate::MockToolRegistry;
use tinypipe_api::storage::{GraphDefinition, GraphStorage};
use tinypipe_api::tool_registry::{SubgraphResult, ToolRegistry};
use tinypipe_api::types::{
    CallTarget, Context, DispatchError, GraphId, RegistryError, StorageError, ToolSpec, Value,
    Version,
};
use tinypipe_env::Env;
use tinypipe_ir::compiled::CompiledPlan;

pub struct SubgraphToolRegistry<S: GraphStorage> {
    inner: MockToolRegistry,
    storage: S,
    /// Self-reference: çocuk grafiklerin kendi subgraph çağrılarını da bu
    /// registry üzerinden çözebilmesi için (construct sonrası set edilir).
    self_ref: Mutex<Option<Arc<SubgraphToolRegistry<S>>>>,
    /// Decode edilmiş çocuk plan cache'i: graph_id → (version, plan).
    /// Version eşleşmiyorsa plan yeniden yüklenir (grafik güncellemeleri doğru kalır).
    plan_cache: Mutex<HashMap<GraphId, (Version, Arc<CompiledPlan>)>>,
}

impl<S: GraphStorage> SubgraphToolRegistry<S> {
    pub fn with_tools(storage: S, tools: MockToolRegistry) -> Self {
        SubgraphToolRegistry {
            inner: tools,
            storage,
            self_ref: Mutex::new(None),
            plan_cache: Mutex::new(HashMap::new()),
        }
    }

    /// `self_ref`'i set eder ve `Arc`'ı döndürür — execute/resume öncesi çağrılmalı.
    pub fn init(self: Arc<Self>) -> Arc<Self> {
        *self.self_ref.lock().unwrap() = Some(self.clone());
        self
    }

    /// Storage'a erişim (execute sonrası kayıt güncellemeleri için).
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Kayıtlı tool adları (sıralı) — daemon/server listeleme için.
    pub fn tool_names(&self) -> Vec<String> {
        self.inner.tool_names()
    }

    fn self_registry(&self) -> Arc<SubgraphToolRegistry<S>> {
        self.self_ref
            .lock()
            .unwrap()
            .clone()
            .expect("SubgraphToolRegistry::init must be called before use")
    }

    /// Execution'dan ÖNCE env bağımlılıklarını doğrular: kök grafik + tüm
    /// transitive subgraph'ları BFS ile gezilir, zorunlu ama ortamda eksik
    /// olan anahtarlar `EnvDepReport` olarak döner. Dinamik (runtime) key'ler
    /// ve opsiyonel (`default` / `:-`) anahtarlar raporlanmaz.
    pub fn validate_env(&self, root: &str, env: &tinypipe_env::Env) -> Result<Vec<EnvDepReport>, DispatchError> {
        let mut visited = std::collections::HashSet::new();
        let mut reports = Vec::new();
        self.validate_graph_env(root, env, &mut visited, &mut reports, String::new())?;
        Ok(reports)
    }

    fn validate_graph_env(
        &self,
        name: &str,
        env: &tinypipe_env::Env,
        visited: &mut std::collections::HashSet<GraphId>,
        reports: &mut Vec<EnvDepReport>,
        path_prefix: String,
    ) -> Result<(), DispatchError> {
        let graph = self.resolve_graph(name)?;
        if !visited.insert(graph.id.clone()) {
            return Ok(()); // döngü / tekrar ziyaret koruması
        }
        let plan = self.load_cached_plan(&graph)?;
        let missing: Vec<String> = crate::env_deps::scan_plan_env_deps(&plan)
            .into_iter()
            .filter(|d| !d.optional && env.get(&d.key).is_none())
            .map(|d| d.key)
            .collect();
        if !missing.is_empty() {
            reports.push(EnvDepReport {
                graph_path: format!("{path_prefix}{}", graph.name),
                missing,
            });
        }
        for sub in crate::env_deps::subgraph_targets(&plan) {
            self.validate_graph_env(
                &sub,
                env,
                visited,
                reports,
                format!("{path_prefix}{} → ", graph.name),
            )?;
        }
        Ok(())
    }

    /// `subgraph:<name>` hedefini graph definition'a çözer (version dahil).
    /// İsim öncelikli (indeksli sorgu); isim bulunamazsa id olarak dener.
    fn resolve_graph(&self, input: &str) -> Result<GraphDefinition, DispatchError> {
        match self.storage.find_graph_by_name(input) {
            Ok(graph) => return Ok(graph),
            Err(StorageError::GraphNotFound(_)) => {}
            Err(e) => return Err(DispatchError::ExecutionFailed(e.to_string())),
        }
        // İsim değilse UUID id olarak dene
        self.storage
            .load_graph(&GraphId::new(input))
            .map_err(|e| match e {
                StorageError::GraphNotFound(_) => {
                    DispatchError::NotFound(format!("graph '{input}'"))
                }
                other => DispatchError::ExecutionFailed(other.to_string()),
            })
    }

    /// Çocuk planı cache'ten (veya storage'dan) yükler.
    /// Cache'teki version güncel değilse yeniden decode edilir.
    fn load_cached_plan(&self, graph: &GraphDefinition) -> Result<Arc<CompiledPlan>, DispatchError> {
        if let Some((version, plan)) = self.plan_cache.lock().unwrap().get(&graph.id) {
            if *version == graph.version {
                return Ok(plan.clone());
            }
        }

        let bytes = match self.storage.load_plan_version(&graph.id, graph.version) {
            Ok(bytes) => bytes,
            Err(StorageError::PlanMissing(_)) | Err(StorageError::PlanVersionMissing(_, _)) => {
                // Self-heal: plan yoksa koddan derle ve kaydet
                let output = tinypipe_compiler::compile(&graph.code)
                    .map_err(|e| DispatchError::ExecutionFailed(format!("recompile: {e}")))?;
                self.storage
                    .save_plan(&graph.id, graph.version, &output.fb_binary)
                    .map_err(|e| DispatchError::ExecutionFailed(e.to_string()))?;
                output.fb_binary
            }
            Err(e) => return Err(DispatchError::ExecutionFailed(e.to_string())),
        };
        let plan = Arc::new(
            CompiledPlan::from_fb_bytes(&bytes)
                .map_err(|e| DispatchError::ExecutionFailed(format!("plan decode: {e}")))?,
        );
        self.plan_cache
            .lock()
            .unwrap()
            .insert(graph.id.clone(), (graph.version, plan.clone()));
        Ok(plan)
    }

    fn run_subgraph(
        &self,
        name: &str,
        input: Context,
        env: &Env,
    ) -> Result<SubgraphResult, DispatchError> {
        let graph = self.resolve_graph(name)?;
        let plan = self.load_cached_plan(&graph)?;
        let registry = self.self_registry();
        // Çocuk, modül adıyla scope'lu ortam görünümü alır (örn. `SEED_USERS_URL`)
        let child_env = std::sync::Arc::new(env.scoped(name));
        let executor = tinypipe_vm::CompiledExecutor::with_env(
            &plan,
            registry.as_ref() as &dyn tinypipe_api::tool_registry::ToolRegistry,
            child_env,
        );
        let result = executor
            .execute(input)
            .map_err(|e| DispatchError::ExecutionFailed(e.to_string()))?;
        Ok(SubgraphResult {
            context: result.context,
            output: result.output.unwrap_or(Value::Null),
        })
    }
}

impl<S: GraphStorage> ToolRegistry for SubgraphToolRegistry<S> {
    fn resolve(&self, name: &str, version: &str) -> Result<ToolSpec, RegistryError> {
        self.inner.resolve(name, version)
    }

    fn dispatch(
        &self,
        call: &CallTarget,
        context: &Context,
        env: &Env,
    ) -> Result<Value, DispatchError> {
        self.inner.dispatch(call, context, env)
    }

    fn execute_subgraph(
        &self,
        name: &str,
        input: Context,
        env: &Env,
    ) -> Result<SubgraphResult, DispatchError> {
        self.run_subgraph(name, input, env)
    }

    fn latest_schema_hash(&self, name: &str) -> Result<String, RegistryError> {
        self.inner.latest_schema_hash(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::types::Value;
    use tinypipe_storage::SqliteStorage;

    #[test]
    fn test_subgraph_call_executes_child_and_returns_output() {
        let store = SqliteStorage::in_memory().unwrap();
        let child = tinypipe_compiler::compile("def graph(x: int):\n    return x + 1").unwrap();
        let id = store
            .create_graph("child1", "def graph(x: int):\n    return x + 1")
            .unwrap();
        store.save_plan(&id, Version(1), &child.fb_binary).unwrap();
        let registry = Arc::new(SubgraphToolRegistry::with_tools(
            store,
            crate::default_tools(),
        ))
        .init();

        let mut ctx = Context::new();
        ctx.set("x".into(), Value::Int(41));
        let result = registry.execute_subgraph("child1", ctx, &tinypipe_env::Env::empty()).unwrap();
        assert_eq!(result.output, Value::Int(42));
    }

    #[test]
    fn test_validate_env_reports_missing_keys_transitively() {
        let store = SqliteStorage::in_memory().unwrap();

        // Çocuk grafik: zorunlu DB_URL + opsiyonel LOG_LEVEL
        let child_code = "def graph():\n    u = call(\"env.get\", key=\"DB_URL\")\n    l = call(\"env.get\", key=\"LOG_LEVEL\", default=\"info\")\n    return u";
        let child = tinypipe_compiler::compile(child_code).unwrap();
        let child_id = store.create_graph("child1", child_code).unwrap();
        store.save_plan(&child_id, Version(1), &child.fb_binary).unwrap();

        // Kök grafik: subgraph:child1 + kendi env.template bağımlılığı
        let root_code =
            "def graph():\n    c = call(\"subgraph:child1\")\n    t = call(\"env.template\", value=\"http://${API_HOST}:${PORT:-8080}/${TLS_CERT}\")\n    return t";
        let root = tinypipe_compiler::compile(root_code).unwrap();
        let root_id = store.create_graph("root", root_code).unwrap();
        store.save_plan(&root_id, Version(1), &root.fb_binary).unwrap();

        let registry = Arc::new(SubgraphToolRegistry::with_tools(
            store,
            crate::default_tools(),
        ))
        .init();

        // PORT opsiyonel olduğundan sağlanmasa da eksik sayılmaz.
        let mut vars = std::collections::HashMap::new();
        vars.insert("PORT".into(), "8080".into());
        let env = tinypipe_env::Env::new(vec![Arc::new(
            tinypipe_env::static_provider::StaticEnvProvider::new(vars.clone()),
        )]);

        let reports = registry.validate_env("root", &env).unwrap();
        assert_eq!(reports.len(), 2);
        let child_report = reports
            .iter()
            .find(|r| r.graph_path.contains("child1"))
            .unwrap();
        assert_eq!(child_report.missing, vec!["DB_URL".to_string()]);
        let root_report = reports.iter().find(|r| r.graph_path == "root").unwrap();
        assert_eq!(root_report.missing, vec!["API_HOST".to_string(), "TLS_CERT".to_string()]);

        // Eksikler tamamlanınca rapor boş olmalı
        vars.insert("DB_URL".into(), "postgres://x".into());
        vars.insert("API_HOST".into(), "localhost".into());
        vars.insert("TLS_CERT".into(), "/certs/x.pem".into());
        let full_env = tinypipe_env::Env::new(vec![Arc::new(
            tinypipe_env::static_provider::StaticEnvProvider::new(vars),
        )]);
        assert!(registry.validate_env("root", &full_env).unwrap().is_empty());
    }

    #[test]
    fn test_plan_cache_invalidates_on_graph_update() {
        let store = SqliteStorage::in_memory().unwrap();
        let v1 = tinypipe_compiler::compile("def graph():\n    return 1").unwrap();
        let id = store
            .create_graph("child2", "def graph():\n    return 1")
            .unwrap();
        store.save_plan(&id, Version(1), &v1.fb_binary).unwrap();
        let registry = Arc::new(SubgraphToolRegistry::with_tools(
            store,
            crate::default_tools(),
        ))
        .init();

        let first = registry
            .execute_subgraph("child2", Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(first.output, Value::Int(1));

        // Grafik güncellenir (v2) — plan yeni versiyon için yok, self-heal derler.
        let updated = registry
            .storage()
            .update_graph(&id, "def graph():\n    return 2")
            .unwrap();
        assert_eq!(updated.0, 2);

        let second = registry
            .execute_subgraph("child2", Context::new(), &tinypipe_env::Env::empty())
            .unwrap();
        assert_eq!(second.output, Value::Int(2));

        // Cache'teki eski version artık kullanılmamalı
        assert_eq!(registry.plan_cache.lock().unwrap().get(&id).unwrap().0, Version(2));
    }

    #[test]
    fn test_subgraph_resolve_missing_graph() {
        let store = SqliteStorage::in_memory().unwrap();
        let registry = Arc::new(SubgraphToolRegistry::with_tools(
            store,
            crate::default_tools(),
        ))
        .init();
        let err = registry
            .execute_subgraph("nope", Context::new(), &tinypipe_env::Env::empty())
            .unwrap_err();
        assert!(matches!(err, DispatchError::NotFound(_)));
    }
}
