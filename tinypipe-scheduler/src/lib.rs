//! `tinypipe-scheduler` — paused execution'ları checkpoint'ten sürdüren scheduler.
//!
//! Execution'lar `execute --pause-after N` ile pause'a alınır (checkpoint BLOB
//! olarak storage'da saklanır). Scheduler `list_paused_executions`'ı okuyup her
//! execution'ı `CompiledExecutor::resume` ile ilerletir:
//!
//! - `max_nodes` verildiyse her turda o kadar node daha çalıştırır (tekrar pause).
//! - `max_nodes = None` ise execution'ı tamamlanana kadar sürdürür.
//!
//! Tek tur: `Scheduler::run_once` — tüm paused execution'lar birer segment
//! ilerletilir. Sürekli mod: `Scheduler::run_loop` — paused listesi boşalana
//! kadar (veya `max_rounds` sınırına kadar) tur atar.

use tinypipe_api::storage::GraphStorage;
use tinypipe_api::types::{Execution, ExecutionStatus, ExecutionStep};
use tinypipe_ir::compiled::CompiledPlan;
use tinypipe_vm::{Checkpoint, CompiledExecutor, ExecutionOutcome, ExecutionResult, PausePolicy};

/// Scheduler hataları (fatal — turu durdurur).
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("checkpoint decode failed: {0}")]
    CheckpointDecode(String),
    #[error("plan decode failed: {0}")]
    PlanDecode(String),
    #[error("execution failed: {0}")]
    Execution(String),
}

/// Tek turun özeti.
#[derive(Debug, Clone, Default)]
pub struct SchedulerSummary {
    /// İşlenen toplam paused execution.
    pub processed: usize,
    /// Tamamlanan execution sayısı.
    pub completed: usize,
    /// Tekrar pause'a alınan (daha node kalan) execution sayısı.
    pub still_paused: usize,
    /// Tek tek execution'larda başarısız olanlar (tur durmaz).
    pub failed: usize,
}

/// Checkpoint tabanlı scheduler.
pub struct Scheduler<S: GraphStorage> {
    storage: std::sync::Arc<S>,
    env: std::sync::Arc<tinypipe_env::Env>,
}

impl<S: GraphStorage> Scheduler<S> {
    pub fn new(storage: S) -> Self {
        Self::with_env(storage, std::sync::Arc::new(tinypipe_env::Env::empty()))
    }

    /// Ortam görünümüyle scheduler kurar (resume edilen execution'ların
    /// `env.*` tool'ları bu ortamı görür).
    pub fn with_env(storage: S, env: std::sync::Arc<tinypipe_env::Env>) -> Self {
        Self {
            storage: std::sync::Arc::new(storage),
            env,
        }
    }

    /// Storage'a erişim (testler ve CLI raporlama için).
    pub fn storage(&self) -> &S {
        self.storage.as_ref()
    }

    /// Tüm paused execution'ları birer segment ilerletir.
    /// `max_nodes`: bu turda her execution için çalıştırılacak max node sayısı
    /// (None = tamamlanana kadar sürdür).
    pub fn run_once(&self, max_nodes: Option<u32>) -> Result<SchedulerSummary, SchedulerError> {
        let paused = self
            .storage
            .list_paused_executions()
            .map_err(|e| SchedulerError::Storage(e.to_string()))?;

        let policy = PausePolicy {
            max_nodes,
            ..Default::default()
        };

        let mut summary = SchedulerSummary {
            processed: paused.len(),
            ..Default::default()
        };

        for exec in paused {
            match self.resume_one(&exec, &policy) {
                Ok(outcome) => match outcome {
                    ResumeOutcome::Completed => summary.completed += 1,
                    ResumeOutcome::StillPaused => summary.still_paused += 1,
                },
                Err(e) => {
                    eprintln!("DBG resume failed for execution {}: {}", exec.id, e);
                    summary.failed += 1;
                }
            }
        }
        Ok(summary)
    }

    /// Paused listesi boşalana kadar (veya `max_rounds` sınırına kadar) tur atar.
    /// Her turda `max_nodes` kadar node ilerletilir; `max_nodes = None` ise
    /// zaten tek turda biter.
    pub fn run_loop(
        &self,
        max_nodes: Option<u32>,
        max_rounds: usize,
    ) -> Result<SchedulerSummary, SchedulerError> {
        let mut total = SchedulerSummary::default();
        for _ in 0..max_rounds {
            let round = self.run_once(max_nodes)?;
            total.processed += round.processed;
            total.completed += round.completed;
            total.still_paused += round.still_paused;
            total.failed += round.failed;
            if round.completed == 0 && round.still_paused == 0 {
                break; // liste boş
            }
            if round.still_paused == 0 {
                break;
            }
        }
        Ok(total)
    }

    fn resume_one(
        &self,
        exec: &Execution,
        policy: &PausePolicy,
    ) -> Result<ResumeOutcome, SchedulerError> {
        // 1. Checkpoint blob'unu yükle ve decode et
        let blob = self
            .storage
            .load_checkpoint(&exec.id)
            .map_err(|e| SchedulerError::Storage(e.to_string()))?;
        if blob.is_empty() {
            return Err(SchedulerError::CheckpointDecode(
                "no checkpoint stored for paused execution".into(),
            ));
        }
        let checkpoint: Checkpoint = serde_json::from_slice(&blob)
            .map_err(|e| SchedulerError::CheckpointDecode(e.to_string()))?;

        // 2. Plan'ı (immutable versiyondan) yükle — resume ile aynı versiyon
        let plan_bytes = self
            .storage
            .load_plan_version(&exec.graph_id, exec.graph_version)
            .map_err(|e| SchedulerError::Storage(e.to_string()))?;
        let plan = CompiledPlan::from_fb_bytes(&plan_bytes)
            .map_err(|e| SchedulerError::PlanDecode(e.to_string()))?;

        // 3. Env doğrulaması — resume'dan ÖNCE tüm (transitive) bağımlılıklar
        //    ortamda yoksa bu execution atlanır ve hata raporlanır.
        let tools = tinypipe_tools::default_tools();
        // Daemon varsa remote tool'lar da registry'ye eklenir (çakışan isimlerde
        // built-in kazanır); daemon kapalıysa sessizce built-in'lerle devam edilir.
        if std::env::var("TINYPIPE_NO_DAEMON").is_err() {
            let _ = tinypipe_tools::register_daemon_tools(
                &tools,
                &tinypipe_tools::daemon_addr_from_env(),
            );
        }
        let registry = std::sync::Arc::new(tinypipe_tools::SubgraphToolRegistry::with_tools(
            self.storage.clone(),
            tools,
        ))
        .init();
        match registry.validate_env(&exec.graph_id.0, &self.env) {
            Ok(reports) if !reports.is_empty() => {
                let detail: Vec<String> = reports
                    .iter()
                    .flat_map(|r| {
                        r.missing
                            .iter()
                            .map(|k| format!("{}.{}", r.graph_path, k))
                    })
                    .collect();
                return Err(SchedulerError::Execution(format!(
                    "env check failed, missing: {}",
                    detail.join(", ")
                )));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(SchedulerError::Execution(format!("env check failed: {e}")));
            }
        }

        // 4. Resume et — subgraph çağrıları için gerçek registry (storage paylaşımlı)
        let executor = CompiledExecutor::with_env(
            &plan,
            registry.as_ref() as &dyn tinypipe_api::tool_registry::ToolRegistry,
            self.env.clone(),
        );
        let result = executor
            .resume(&checkpoint, policy, None)
            .map_err(|e| SchedulerError::Execution(e.to_string()))?;

        match result {
            ExecutionOutcome::Completed(res) => {
                self.record_steps(&exec.id, &plan, &res)?;
                let mut updated = exec.clone();
                updated.status = ExecutionStatus::Completed;
                updated.output = res.output;
                updated.context = Some(res.context);
                updated.duration_us = Some(res.duration_us);
                updated.completed_at = Some(crate::now_micros());
                self.storage
                    .save_execution(&updated)
                    .map_err(|e| SchedulerError::Storage(e.to_string()))?;
                Ok(ResumeOutcome::Completed)
            }
            ExecutionOutcome::Paused(new_cp) => {
                let new_blob = serde_json::to_vec(&new_cp)
                    .map_err(|e| SchedulerError::CheckpointDecode(e.to_string()))?;
                self.storage
                    .save_checkpoint(&exec.id, &new_blob)
                    .map_err(|e| SchedulerError::Storage(e.to_string()))?;
                Ok(ResumeOutcome::StillPaused)
            }
        }
    }

    /// Bu segmentte gerçekten çalışan node'ları step olarak kaydeder.
    /// `node_durations` checkpoint'ten taşınan eski node'ları içermez — resume
    /// segmentleri çift kayıt yaratmaz.
    fn record_steps(
        &self,
        execution_id: &str,
        plan: &CompiledPlan,
        result: &ExecutionResult,
    ) -> Result<(), SchedulerError> {
        let node_by_id: std::collections::HashMap<&str, &tinypipe_ir::compiled::CompiledNode> =
            plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut cursor: u64 = crate::now_micros().parse().unwrap_or(0);
        for (node_id, duration_us) in &result.node_durations {
            let op = node_by_id
                .get(node_id.as_str())
                .map(|n| format!("{:?}", n.op))
                .unwrap_or_else(|| "unknown".into());
            let started_at = cursor;
            let completed_at = started_at + duration_us;
            cursor = completed_at;
            let step = ExecutionStep {
                id: uuid::Uuid::new_v4().to_string(),
                execution_id: execution_id.to_string(),
                node_id: node_id.clone(),
                node_op: op,
                status: "completed".into(),
                error: None,
                started_at: started_at.to_string(),
                completed_at: Some(completed_at.to_string()),
                duration_us: Some(*duration_us),
                context_before: None,
                context_after: None,
                parent_step_id: None,
            };
            self.storage
                .save_step(&step)
                .map_err(|e| SchedulerError::Storage(e.to_string()))?;
        }
        Ok(())
    }
}

enum ResumeOutcome {
    Completed,
    StillPaused,
}

fn now_micros() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinypipe_api::types::{Context, Value, Version};
    use tinypipe_ir::plan::{Edge, ExecutionPlan, Node, Opcode};
    use tinypipe_storage::SqliteStorage;

    /// max_iter=5'lik bir loop graph'ı: x=0 → x=5 çıktısı.
    fn loop_plan() -> ExecutionPlan {
        ExecutionPlan::new(
            vec![
                Node::new("input_x", Opcode::Input).with_arg("name", "x".into()),
                Node::new("loop1", Opcode::Loop).with_arg("max_iterations", 5i64.into()),
                Node::new("body_calc", Opcode::Calc)
                    .with_arg("expr", "x + 1".into())
                    .with_arg("output", "x".into()),
                Node::new("body_decide", Opcode::Decide)
                    .with_arg("source", "x".into())
                    .with_arg("op", "lt".into())
                    .with_arg("value", 5i64.into()),
                Node::new("output", Opcode::Act)
                    .with_arg("type", "return".into())
                    .with_arg("value", "x".into()),
            ],
            vec![
                Edge::new("input_x", "loop1"),
                Edge::new("loop1", "body_calc"),
                Edge::new("body_calc", "body_decide"),
                Edge::control("loop1", "output"),
            ],
        )
    }

    /// Storage'a: graph + plan + paused execution + checkpoint hazırlar.
    fn seed_paused_execution(
        store: &SqliteStorage,
        exec_id: &str,
        max_nodes: u32,
    ) -> (tinypipe_api::types::GraphId, Version) {
        let graph_id = store
            .create_graph("loop_graph", "def graph(): pass")
            .unwrap();

        // Plan'ı FlatBuffers'a çevirip kaydet
        let plan = loop_plan();
        let compiled = tinypipe_ir::compiled::CompiledPlan::from_execution_plan(&plan, vec![]);
        let fb = compiled.to_fb_bytes().unwrap();
        store.save_plan(&graph_id, Version(1), &fb).unwrap();

        let mut input = Context::new();
        input.set("x".into(), Value::Int(0));
        let exec = Execution {
            id: exec_id.into(),
            graph_id: graph_id.clone(),
            graph_version: Version(1),
            input: input.clone(),
            output: None,
            status: ExecutionStatus::Paused,
            error: None,
            started_at: "0".into(),
            completed_at: None,
            duration_us: None,
            context: Some(input.clone()),
        };
        store.save_execution(&exec).unwrap();

        // İlk segmenti çalıştırıp pause checkpoint'ini kaydet
        let reg = tinypipe_tools::mock_tools();
        let executor = CompiledExecutor::new(&compiled, &reg);
        let policy = PausePolicy {
            max_nodes: Some(max_nodes),
            ..Default::default()
        };
        let outcome = executor.execute_with(input, &policy, None).unwrap();
        let cp = match outcome {
            ExecutionOutcome::Paused(cp) => cp,
            ExecutionOutcome::Completed(_) => panic!("expected pause"),
        };
        let blob = serde_json::to_vec(&cp).unwrap();
        store.save_checkpoint(exec_id, &blob).unwrap();

        (graph_id, Version(1))
    }

    #[test]
    fn test_run_once_completes_single_paused_execution() {
        let store = SqliteStorage::in_memory().unwrap();
        seed_paused_execution(&store, "exec-1", 3);

        let scheduler = Scheduler::new(store);
        let store = scheduler.storage();
        let summary = scheduler.run_once(None).unwrap();
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.still_paused, 0);
        assert_eq!(summary.failed, 0);

        let exec = store.load_execution("exec-1").unwrap();
        assert!(matches!(exec.status, ExecutionStatus::Completed));
        assert_eq!(exec.output, Some(Value::Int(5)));
    }

    #[test]
    fn test_run_loop_with_budget_resumes_in_steps() {
        let store = SqliteStorage::in_memory().unwrap();
        seed_paused_execution(&store, "exec-2", 2);

        let scheduler = Scheduler::new(store);
        let store = scheduler.storage();
        let summary = scheduler.run_loop(Some(2), 100).unwrap();
        // Budetli turlar: loop 10 segmentte ilerler, son turda tamamlanır
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 0);
        assert!(
            summary.still_paused >= 10,
            "loop multi-segment ilerlemeli, got {}",
            summary.still_paused
        );
        // still_paused kümülatiftir; önemli olan son durumun Completed olması
        let exec = store.load_execution("exec-2").unwrap();
        assert!(matches!(exec.status, ExecutionStatus::Completed));
        assert_eq!(exec.output, Some(Value::Int(5)));
    }

    #[test]
    fn test_run_once_skips_empty_list() {
        let store = SqliteStorage::in_memory().unwrap();
        let scheduler = Scheduler::new(store);
        let summary = scheduler.run_once(None).unwrap();
        assert_eq!(summary.processed, 0);
        assert_eq!(summary.completed, 0);
    }

    #[test]
    fn test_run_once_failure_does_not_abort_round() {
        let store = SqliteStorage::in_memory().unwrap();
        seed_paused_execution(&store, "good-1", 3);

        // Checkpoint'i olmayan (boş blob) paused execution da ekle
        let graph_id = store.create_graph("other", "def graph(): pass").unwrap();
        let exec = Execution {
            id: "broken-1".into(),
            graph_id: graph_id.clone(),
            graph_version: Version(1),
            input: Context::new(),
            output: None,
            status: ExecutionStatus::Paused,
            error: None,
            started_at: "0".into(),
            completed_at: None,
            duration_us: None,
            context: None,
        };
        store.save_execution(&exec).unwrap();

        let scheduler = Scheduler::new(store);
        let store = scheduler.storage();
        let summary = scheduler.run_once(None).unwrap();
        assert_eq!(summary.processed, 2);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);

        // Sağlam execution yine de tamamlandı
        let good = store.load_execution("good-1").unwrap();
        assert!(matches!(good.status, ExecutionStatus::Completed));
    }

    #[test]
    fn test_plan_version_used_for_resume() {
        // Versiyonlu plan okuma yolu (load_plan_version) kullanılmalı:
        // graph v2'ye güncellense bile paused execution eski versiyondan sürdürülür.
        let store = SqliteStorage::in_memory().unwrap();
        let (graph_id, version) = seed_paused_execution(&store, "exec-v1", 3);

        // Graph'ı v2'ye güncelle (farklı plan)
        store
            .update_graph(&graph_id, "def graph(): return 999")
            .unwrap();

        let scheduler = Scheduler::new(store);
        let store = scheduler.storage();
        let summary = scheduler.run_once(None).unwrap();
        assert_eq!(summary.completed, 1);

        let exec = store.load_execution("exec-v1").unwrap();
        assert_eq!(exec.graph_version, version);
        assert_eq!(exec.output, Some(Value::Int(5)));
    }
}
