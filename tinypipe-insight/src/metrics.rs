//! Portföy metrik toplama — storage'dan ham veri alır, raporlanabilir yapılara çevirir.
//!
//! Tek geçişte her graph için:
//! - execution istatistikleri (adet, hata, ortalama, p95)
//! - plan yapısı (node/edge sayısı, tool histogramı, subgraph bağımlılıkları,
//!   env bağımlılıkları, dış HTTP endpoint'ler)
//! - yaşam döngüsü (last_event, versiyon sayısı / churn)
//!
//! `env` verilirse `scan_plan_env_deps` ile eksik ortam değişkenleri de
//! tespit edilir (devops/ceo raporları için). Verilmezse yalnızca bağımlılık
//! envanteri listelenir.

use std::collections::HashMap;

use tinypipe_api::storage::{GraphDefinition, GraphStorage};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_ir::env_deps::{scan_plan_env_deps, subgraph_targets};
use tinypipe_ir::plan::Opcode;

/// Tek graph için toplanan metrikler.
#[derive(Debug, Clone, Default)]
pub struct GraphStats {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub status: String,
    pub last_event: Option<String>,
    /// Versiyon sayısı (churn göstergesi).
    pub version_count: u64,
    /// Toplam execution sayısı (tüm versiyonlar).
    pub executions: u64,
    /// Başarısız execution sayısı.
    pub failed: u64,
    pub avg_duration_us: Option<u64>,
    pub p95_duration_us: Option<u64>,
    /// Compiled plan yapısı (plan yoksa None).
    pub node_count: Option<u32>,
    pub edge_count: Option<u32>,
    /// `(tool_target, call_count)` — azalan sırada.
    pub tool_calls: Vec<(String, u64)>,
    /// `subgraph:<name>` bağımlılıkları.
    pub subgraph_deps: Vec<String>,
    /// `(key, optional)` env bağımlılıkları.
    pub env_deps: Vec<(String, bool)>,
    /// Ortamda bulunmayan zorunlu anahtarlar (env sağlandıysa).
    pub missing_env: Vec<String>,
    /// Dış HTTP endpoint host'ları (http_request çağrıları).
    pub http_endpoints: Vec<String>,
}

/// Tüm portföyün özet metrikleri.
#[derive(Debug, Clone, Default)]
pub struct PortfolioMetrics {
    pub graphs: Vec<GraphStats>,
    pub total_executions: u64,
    pub total_failed: u64,
    /// Son olayı rollback olan graph sayısı.
    pub rollback_count: u64,
    /// Deploy edilmiş (active) graph sayısı.
    pub deployed_count: usize,
    /// En az bir zorunlu env değişkeni eksik olan graph sayısı (env sağlandıysa).
    pub env_risk_graphs: usize,
}

/// Storage'dan tüm metrikleri toplar.
/// `env`: mevcut ortam (eksik taraması için); None = sadece envanter.
pub fn collect<S: GraphStorage>(
    storage: &S,
    env: Option<&tinypipe_env::Env>,
) -> Result<PortfolioMetrics, String> {
    let graphs = storage.list_all_graphs(None, None).map_err(|e| e.to_string())?;
    let mut portfolio = PortfolioMetrics::default();

    for g in &graphs {
        let stats = collect_graph(storage, g, env)?;
        portfolio.total_executions += stats.executions;
        portfolio.total_failed += stats.failed;
        if stats.last_event.as_deref().map(|e| e.starts_with("rollback:")).unwrap_or(false) {
            portfolio.rollback_count += 1;
        }
        if stats.status == "deployed" || stats.version > 1 && g.active {
            portfolio.deployed_count += 1;
        }
        if !stats.missing_env.is_empty() {
            portfolio.env_risk_graphs += 1;
        }
        portfolio.graphs.push(stats);
    }
    Ok(portfolio)
}

fn collect_graph<S: GraphStorage>(
    storage: &S,
    graph: &GraphDefinition,
    env: Option<&tinypipe_env::Env>,
) -> Result<GraphStats, String> {
    let mut stats = GraphStats {
        id: graph.id.0.clone(),
        name: graph.name.clone(),
        version: graph.version.0,
        status: graph.status.clone(),
        last_event: graph.last_event.clone(),
        ..Default::default()
    };

    // Execution istatistikleri
    let executions = storage
        .list_executions(&graph.id, Some(500))
        .map_err(|e| e.to_string())?;
    stats.executions = executions.len() as u64;
    stats.failed = executions
        .iter()
        .filter(|e| e.status == tinypipe_api::types::ExecutionStatus::Failed)
        .count() as u64;

    let mut durations: Vec<u64> = executions
        .iter()
        .filter_map(|e| e.duration_us)
        .collect();
    durations.sort_unstable();
    if !durations.is_empty() {
        stats.avg_duration_us = Some(durations.iter().sum::<u64>() / durations.len() as u64);
        let p95_idx = (durations.len() as f64 * 0.95).ceil() as usize - 1;
        stats.p95_duration_us = Some(durations[p95_idx.min(durations.len() - 1)]);
    }

    // Versiyon sayısı (churn)
    stats.version_count = storage
        .list_versions(&graph.id)
        .map(|v| v.len() as u64)
        .unwrap_or(0);

    // Plan analizi
    if let Ok(plan_bytes) = storage.load_plan(&graph.id) {
        if let Ok(plan) = CompiledPlan::from_fb_bytes(&plan_bytes) {
            analyze_plan(&plan, &mut stats);
        }
    }

    // Eksik env taraması (env sağlandıysa)
    if let Some(env_map) = env {
        for (key, optional) in &stats.env_deps {
            if !optional && env_map.get(key).is_none() {
                stats.missing_env.push(key.clone());
            }
        }
    }

    Ok(stats)
}

/// Compiled plan'dan yapısal metrikleri çıkarır.
fn analyze_plan(plan: &CompiledPlan, stats: &mut GraphStats) {
    stats.node_count = Some(plan.nodes.len() as u32);
    stats.edge_count = Some(plan.edges.len() as u32);

    let mut tools: HashMap<String, u64> = HashMap::new();
    let mut endpoints: Vec<String> = Vec::new();
    for node in &plan.nodes {
        if node.op != Opcode::Call {
            continue;
        }
        let target = node
            .args
            .iter()
            .find(|a| a.key == "target")
            .and_then(|a| json_string_literal(&a.value))
            .unwrap_or_default();
        if target.is_empty() {
            continue;
        }
        *tools.entry(target.clone()).or_insert(0) += 1;
        if target == "http_request" {
            let url = node
                .args
                .iter()
                .find(|a| a.key == "url")
                .and_then(|a| json_string_literal(&a.value))
                .unwrap_or_default();
            if let Some(host) = tinypipe_ir::plan_view::url_host(&url) {
                if !endpoints.contains(&host) {
                    endpoints.push(host);
                }
            }
        }
    }
    stats.tool_calls = {
        let mut v: Vec<(String, u64)> = tools.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    };
    stats.http_endpoints = endpoints;

    stats.subgraph_deps = subgraph_targets(plan);
    stats.env_deps = scan_plan_env_deps(plan)
        .into_iter()
        .map(|d| (d.key, d.optional))
        .collect();
}

/// `"..."` JSON literal'ından string değer çıkarır; literal değilse `None`.
fn json_string_literal(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_storage::SqliteStorage;

    fn env_with(overrides: &[(&str, &str)]) -> tinypipe_env::Env {
        let map: std::collections::HashMap<String, String> = overrides
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        tinypipe_env::Env::new(vec![std::sync::Arc::new(
            tinypipe_env::static_provider::StaticEnvProvider::new(map),
        )])
    }

    #[test]
    fn test_collect_empty_portfolio() {
        let store = SqliteStorage::in_memory().unwrap();
        let m = collect(&store, None).unwrap();
        assert_eq!(m.graphs.len(), 0);
        assert_eq!(m.total_executions, 0);
    }

    #[test]
    fn test_collect_rollback_and_deployed_counts() {
        let store = SqliteStorage::in_memory().unwrap();
        let id = store.create_graph("a", "def graph(): pass").unwrap();
        store.deploy(&id, tinypipe_api::types::Version(1)).unwrap();
        store.update_graph(&id, "def graph(): return 1").unwrap();
        store.rollback(&id, tinypipe_api::types::Version(1)).unwrap();

        let m = collect(&store, None).unwrap();
        assert_eq!(m.graphs.len(), 1);
        assert_eq!(m.deployed_count, 1, "rollback keeps deployed status");
        assert_eq!(m.rollback_count, 1);
        let g = &m.graphs[0];
        assert_eq!(g.version_count, 3, "v1 + update v2 + rollback v3");
    }

    #[test]
    fn test_collect_missing_env_detected() {
        use tinypipe_api::types::Version;
        use tinypipe_ir::compiled::{CompiledArg, CompiledNode, CompiledPlan};
        use tinypipe_ir::plan::Opcode;

        let store = SqliteStorage::in_memory().unwrap();
        let id = store.create_graph("envg", "def graph(): return 1").unwrap();
        let node = CompiledNode {
            index: 0,
            id: "n0".into(),
            op: Opcode::Call,
            args: vec![
                CompiledArg { key: "target".into(), value: "\"env.get\"".into() },
                CompiledArg { key: "key".into(), value: "\"DB_URL\"".into() },
            ],
            inferred_type: None,
            branch_id: None,
            group_id: None,
        };
        let plan = CompiledPlan {
            version: 4,
            nodes: vec![node],
            edges: Vec::new(),
            metadata: Default::default(),
            id_map: None,
            groups: Vec::new(),
        };
        store
            .save_plan(&id, Version(1), &plan.to_fb_bytes().unwrap())
            .unwrap();

        // Boş ortam → DB_URL eksik
        let empty = env_with(&[]);
        let m = collect(&store, Some(&empty)).unwrap();
        assert_eq!(m.graphs[0].env_deps, vec![("DB_URL".into(), false)]);
        assert_eq!(
            m.graphs[0].missing_env,
            vec!["DB_URL".to_string()]
        );
        assert_eq!(m.env_risk_graphs, 1);

        // Dolu ortam → eksik yok
        let full = env_with(&[("DB_URL", "postgres://localhost/db")]);
        let m2 = collect(&store, Some(&full)).unwrap();
        assert!(m2.graphs[0].missing_env.is_empty());
        assert_eq!(m2.env_risk_graphs, 0);
    }
}
