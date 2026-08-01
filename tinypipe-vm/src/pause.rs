//! `tinypipe-vm` — pause/resume checkpointing.
//!
//! `Checkpoint`, yürütmenin herhangi bir noktasında alınan tam anlık görüntüdür
//! `PausePolicy`, hangi koşullarda checkpoint
//! alınacağını belirler; `StepObserver` ise node bazlı izleme sağlar.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use tinypipe_api::types::{Context, Value};

use crate::error::ExecutionResult;

/// Hangi noktada duraklanacağını belirten politika.
///
/// Her iki koşul da aynı anda kullanılabilir; ikisinden biri eşleşirse pause olur.
#[derive(Debug, Clone, Default)]
pub struct PausePolicy {
    /// Toplam çalıştırılan node sayısı bu değere ulaşınca duraklat.
    pub max_nodes: Option<u32>,
    /// Bu node id'lerinden biri çalıştıktan hemen sonra duraklat.
    pub pause_at_node_ids: Option<Vec<String>>,
}

impl PausePolicy {
    pub(crate) fn should_pause(&self, node_count: u32, node_id: &str) -> bool {
        if let Some(max) = self.max_nodes {
            if node_count >= max {
                return true;
            }
        }
        if let Some(ids) = &self.pause_at_node_ids {
            if ids.iter().any(|id| id == node_id) {
                return true;
            }
        }
        false
    }
}

/// LOOP gövdesi ortasında alınan checkpoint'te loop'un iç state'i.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoopState {
    /// LOOP node'unun index'i.
    pub loop_index: u32,
    /// Devam edilecek iterasyon.
    pub iteration: u32,
    /// Devam edilecek body node pozisyonu (body_vec içinde).
    pub body_position: usize,
}

/// Yürütme state'inin tam anlık görüntüsü. serde_json ile serialize edilir
/// (Value `#[serde(untagged)]` olduğu için JSON kullanılır).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Çalıştırılan node sayısı (budget hesabı dahil).
    pub node_count: u32,
    /// Topo order içinde devam edilecek pozisyon.
    pub position: usize,
    /// Bu ana kadar geçen süre (mikrosaniye).
    pub elapsed_us: u64,
    /// LOOP gövdesi ortasında duraklatıldıysa loop state'i.
    pub loop_state: Option<LoopState>,

    pub context: Context,
    pub node_outputs: HashMap<u32, Value>,
    pub enabled: HashSet<u32>,
    pub control_satisfied: Vec<u32>,
    pub loop_skipped: HashSet<u32>,
    pub execution_order: Vec<String>,
    pub output: Option<Value>,
}

/// Yürütme sonucu: ya tamamlandı ya da duraklatıldı.
#[derive(Debug)]
pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    Paused(Checkpoint),
}

/// Node bazlı yürütme gözlemcisi (step kayıtları için).
pub trait StepObserver {
    /// Node çalışmaya başlamadan önce çağrılır.
    fn on_node_start(&mut self, _node_id: &str) {}
    /// Node çalışmayı bitirdikten sonra çağrılır (ok veya skip).
    fn on_node_end(&mut self, _node_id: &str) {}
}

/// Hiçbir şey yapmayan gözlemci (varsayılan).
pub struct NoopObserver;

impl StepObserver for NoopObserver {}
