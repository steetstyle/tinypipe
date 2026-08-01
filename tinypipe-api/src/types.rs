//! Ortak tipler — CallTarget, Context, Value, ToolSpec, GraphId, Version.
//!
//! # Scope Isolation (v3.1+)
//!
//! PARALLEL branch'leri için scope izolasyonu:
//! - Her PARALLEL branch kendi `Scope`'unu alır (parent scope'un kopyası)
//! - Branch içindeki `get()` önce local scope'a, sonra global context'e bakar
//! - Branch içindeki `set()` local scope'a yazar (cross-branch contamination yok)
//! - MERGE node'u branch scope'larını `MergeStrategy` ile birleştirir

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Execution'daki değer türü. Serde JSON Value üzerine kurulu — Tool Registry
/// JSON Schema ile uyumlu, VM içinde zero-copy değil ama esnek.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Toplama işlemi (sum/avg için)
    pub fn checked_add(&self, other: &Value) -> Option<Value> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a.checked_add(*b).map(Value::Int),
            (Value::Float(a), Value::Float(b)) => Some(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Some(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Some(Value::Float(a + *b as f64)),
            _ => None,
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_owned())
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::Array(v.into_iter().map(Into::into).collect())
    }
}

// ─── Scope & MergeStrategy ───────────────────────────────────────

/// Parallel branch scope'larını birleştirme stratejisi.
///
/// Her anahtar için ayrı strateji belirtilebilir. Belirtilmeyen anahtarlar
/// `Last` stratejisini kullanır (backward compatible).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MergeStrategy {
    /// Son yazan kazanır (varsayılan, backward compatible)
    Last,
    /// İlk yazan kazanır
    First,
    /// Minimum değer (numeric tipler)
    Min,
    /// Maksimum değer (numeric tipler)
    Max,
    /// Birleştir (string/array)
    Concat,
    /// Topla (numeric tipler)
    Sum,
    /// Ortalama (numeric tipler)
    Avg,
}

impl Default for MergeStrategy {
    fn default() -> Self {
        MergeStrategy::Last
    }
}

/// Bir PARALLEL branch'inin izole edilmiş değişken scope'u.
///
/// Her branch kendi Scope'unu alır. Branch'ler sibling scope'lara
/// erişemez, sadece parent scope'u (global context) okuyabilir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub variables: HashMap<String, Value>,
    /// Anahtar başına merge stratejisi (opsiyonel)
    pub merge_strategies: HashMap<String, MergeStrategy>,
}

impl Scope {
    pub fn new() -> Self {
        Scope {
            variables: HashMap::new(),
            merge_strategies: HashMap::new(),
        }
    }

    /// Parent scope'dan kopya alarak child scope oluştur.
    /// Branch, parent değişkenlerini başlangıç değeri olarak alır.
    pub fn child(parent: &Scope) -> Self {
        Scope {
            variables: parent.variables.clone(),
            merge_strategies: parent.merge_strategies.clone(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.variables.get(key)
    }

    pub fn set(&mut self, key: String, value: Value) {
        self.variables.insert(key, value);
    }

    /// Set merge strategy for a specific field.
    pub fn set_merge_strategy(&mut self, key: String, strategy: MergeStrategy) {
        self.merge_strategies.insert(key, strategy);
    }

    /// Get the effective merge strategy for a key (defaults to Last).
    pub fn get_merge_strategy(&self, key: &str) -> MergeStrategy {
        self.merge_strategies
            .get(key)
            .copied()
            .unwrap_or(MergeStrategy::Last)
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Context (scope-aware, branch isolation) ─────────────────────

/// Execution context — working memory for graph execution.
///
/// # Scope Isolation (v3.1+)
///
/// PARALLEL branch'leri için izole edilmiş değişken scope'u:
/// - Her branch kendi `Scope`'unu alır (parent scope'un kopyası)
/// - `get(key)`: önce active branch scope'a, sonra parent, sonra global 'variables'a bakar
/// - `set(key, value)`: active branch varsa scope'a, yoksa global'e yazar
/// - `enter_parallel()` / `set_branch()` / `merge_branches()`: VM tarafından yönetilir
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// Global değişkenler (scope dışı)
    pub variables: HashMap<String, Value>,

    /// Branch scope'ları: branch_id → Scope
    /// PARALLEL çalışırken her branch'in kendi değişkenleri burada saklanır
    branch_scopes: HashMap<u32, Scope>,

    /// Parent scope snapshot (PARALLEL girişinde alınır)
    parent_scope: Option<Scope>,

    /// Şu anda aktif olan branch (None = global scope)
    active_branch: Option<u32>,
}

impl Context {
    pub fn new() -> Self {
        Context {
            variables: HashMap::new(),
            branch_scopes: HashMap::new(),
            parent_scope: None,
            active_branch: None,
        }
    }

    pub fn with_capacity(n: usize) -> Self {
        Context {
            variables: HashMap::with_capacity(n),
            branch_scopes: HashMap::new(),
            parent_scope: None,
            active_branch: None,
        }
    }

    // ── Scope-aware get/set ────────────────────────────────────

    /// Değişken okuma:
    /// 1. Active branch scope (varsa)
    /// 2. Parent scope (PARALLEL girişindeki snapshot, varsa)
    /// 3. Global variables
    pub fn get(&self, key: &str) -> Option<&Value> {
        // Active branch scope
        if let Some(bid) = self.active_branch {
            if let Some(scope) = self.branch_scopes.get(&bid) {
                if let Some(val) = scope.variables.get(key) {
                    return Some(val);
                }
            }
        }
        // Parent scope (PARALLEL girişi snapshot'ı)
        if let Some(ref parent) = self.parent_scope {
            if let Some(val) = parent.variables.get(key) {
                return Some(val);
            }
        }
        // Global
        self.variables.get(key)
    }

    /// Değişken yazma: active branch varsa scope'a, yoksa global'e yazar.
    pub fn set(&mut self, key: String, value: Value) {
        if let Some(bid) = self.active_branch {
            self.branch_scopes
                .entry(bid)
                .or_insert_with(Scope::new)
                .variables
                .insert(key, value);
        } else {
            self.variables.insert(key, value);
        }
    }

    // ── Parallel scope management ──────────────────────────────

    /// PARALLEL girişinde çağrılır.
    /// Global context'in snapshot'ını parent scope olarak kaydeder.
    pub fn enter_parallel(&mut self) {
        self.parent_scope = Some(Scope {
            variables: self.variables.clone(),
            merge_strategies: HashMap::new(),
        });
        self.branch_scopes.clear();
        self.active_branch = None;
    }

    /// PARALLEL'den çıkışta çağrılır.
    /// Tüm branch scope'larını merge eder ve state'i temizler.
    pub fn exit_parallel(&mut self) {
        self.active_branch = None;
    }

    /// Aktif branch'i değiştirir.
    /// Branch scope BOŞ başlar — `get()` parent scope'a fallthrough yapar.
    /// Sadece `set()` ile yazılan değişkenler branch scope'a eklenir,
    /// böylece MERGE sırasında sadece gerçekten değiştirilen değişkenler birleşir.
    pub fn set_branch(&mut self, branch_id: u32) {
        self.active_branch = Some(branch_id);
        // Empty scope — only variables explicitly set() by the branch appear here.
        // get() falls through to parent_scope → global.variables automatically.
        self.branch_scopes
            .entry(branch_id)
            .or_insert_with(Scope::new);
    }

    /// Branch'i devre dışı bırak (aktif branch yok = global scope).
    pub fn clear_branch(&mut self) {
        self.active_branch = None;
    }

    /// Bir branch scope'unu doğrudan context'e ekler.
    /// Paralel yürütmede branch thread'leri kendi scope'larını ana context'e
    /// bu yolla geri verir; MERGE node'u `merge_branches()` ile birleştirir.
    pub fn insert_branch_scope(&mut self, branch_id: u32, scope: Scope) {
        self.branch_scopes.insert(branch_id, scope);
    }

    /// Bir branch scope'unu context'ten çıkarır (paralel thread sonuç toplama).
    pub fn take_branch_scope(&mut self, branch_id: u32) -> Option<Scope> {
        self.branch_scopes.remove(&branch_id)
    }

    /// Aktif branch için bir anahtarın merge stratejisini belirle.
    /// MERGE sırasında bu strateji kullanılır.
    pub fn set_merge_strategy(&mut self, key: &str, strategy: MergeStrategy) {
        if let Some(bid) = self.active_branch {
            self.branch_scopes
                .entry(bid)
                .or_insert_with(Scope::new)
                .merge_strategies
                .insert(key.to_owned(), strategy);
        }
    }

    /// Tüm branch scope'larını merge_strategy'ye göre global'e birleştirir.
    /// First = en düşük branch_id, Last = en yüksek branch_id (deterministik).
    pub fn merge_branches(&mut self) {
        let mut scopes: Vec<(u32, Scope)> = self.branch_scopes.drain().collect();
        // branch_id'e göre sırala: First = low bid, Last = high bid
        scopes.sort_by_key(|(bid, _)| *bid);
        for (_bid, scope) in scopes {
            let keys: Vec<String> = scope.variables.keys().cloned().collect();
            for key in keys {
                let value = scope.variables.get(&key).cloned().unwrap_or(Value::Null);
                let strategy = scope.get_merge_strategy(&key);
                match strategy {
                    MergeStrategy::Last => {
                        self.variables.insert(key, value);
                    }
                    MergeStrategy::First => {
                        self.variables.entry(key).or_insert(value);
                    }
                    MergeStrategy::Min => {
                        self.variables
                            .entry(key)
                            .and_modify(|existing| {
                                if let (Some(a), Some(b)) = (value.as_f64(), existing.as_f64()) {
                                    if a < b {
                                        *existing = value.clone();
                                    }
                                }
                            })
                            .or_insert(value);
                    }
                    MergeStrategy::Max => {
                        self.variables
                            .entry(key)
                            .and_modify(|existing| {
                                if let (Some(a), Some(b)) = (value.as_f64(), existing.as_f64()) {
                                    if a > b {
                                        *existing = value.clone();
                                    }
                                }
                            })
                            .or_insert(value);
                    }
                    MergeStrategy::Concat => {
                        self.variables
                            .entry(key)
                            .and_modify(|existing| match (existing.clone(), value.clone()) {
                                (Value::String(a), Value::String(b)) => {
                                    *existing = Value::String(a + &b);
                                }
                                (Value::Array(a), Value::Array(b)) => {
                                    let mut merged = a;
                                    merged.extend(b);
                                    *existing = Value::Array(merged);
                                }
                                (_, _) => {
                                    *existing = value.clone();
                                }
                            })
                            .or_insert(value);
                    }
                    MergeStrategy::Sum => {
                        self.variables
                            .entry(key)
                            .and_modify(|existing| {
                                if let Some(sum) = existing.checked_add(&value) {
                                    *existing = sum;
                                }
                            })
                            .or_insert(value);
                    }
                    MergeStrategy::Avg => {
                        self.variables
                            .entry(key)
                            .and_modify(|existing| {
                                if let (Some(a), Some(b)) = (existing.as_f64(), value.as_f64()) {
                                    *existing = Value::Float((a + b) / 2.0);
                                }
                            })
                            .or_insert(value);
                    }
                }
            }
        }
        self.parent_scope = None;
        self.active_branch = None;
    }

    /// Scope'lar dahil tüm değişkenleri döndür (ACT return için).
    pub fn all_variables(&self) -> HashMap<String, Value> {
        let mut result = self.variables.clone();
        // Active branch scope varsa onu da ekle
        if let Some(bid) = self.active_branch {
            if let Some(scope) = self.branch_scopes.get(&bid) {
                for (k, v) in &scope.variables {
                    result.insert(k.clone(), v.clone());
                }
            }
        }
        result
    }

    /// Tahmini byte boyutu (context memory limit kontrolü için)
    pub fn estimated_bytes(&self) -> u64 {
        let global: u64 = self
            .variables
            .iter()
            .map(|(k, v)| (k.len() + serde_json::to_string(v).unwrap_or_default().len()) as u64)
            .sum();
        let scopes: u64 = self
            .branch_scopes
            .iter()
            .flat_map(|(_, s)| s.variables.iter())
            .map(|(k, v)| (k.len() + serde_json::to_string(v).unwrap_or_default().len()) as u64)
            .sum();
        global + scopes
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// CALL opcode'unun hedefini tanımlar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallTarget {
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: HashMap<String, Value>,
}

impl CallTarget {
    pub fn new(name: &str) -> Self {
        CallTarget {
            name: name.to_owned(),
            args: Vec::new(),
            kwargs: HashMap::new(),
        }
    }
}

/// Tool spesifikasyonu — Tool Registry'den dönen metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub pure: bool,
    pub version: String,
    pub schema_hash: String,
}

/// Graph ID (UUID string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphId(pub String);

impl GraphId {
    pub fn new(id: &str) -> Self {
        GraphId(id.to_owned())
    }
}

impl std::fmt::Display for GraphId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Versiyon numarası.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version(pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Execution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Running,
    Paused,
    Completed,
    Failed,
}

/// Execution kaydı.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub graph_id: GraphId,
    pub graph_version: Version,
    pub input: Context,
    pub output: Option<Value>,
    pub status: ExecutionStatus,
    pub error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_us: Option<u64>,
    pub context: Option<Context>,
}

/// Execution step kaydı (per-node snapshot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStep {
    pub id: String,
    pub execution_id: String,
    pub node_id: String,
    pub node_op: String,
    pub status: String,
    pub error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_us: Option<u64>,
    pub context_before: Option<Context>,
    pub context_after: Option<Context>,
    pub parent_step_id: Option<String>,
}

/// Registry hataları.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RegistryError {
    #[error("Tool '{0}' not found")]
    NotFound(String),
    #[error("Version '{0}' not found for tool '{1}'")]
    VersionNotFound(String, String),
    #[error("Schema hash mismatch for tool '{0}': expected {1}, got {2}")]
    SchemaMismatch(String, String, String),
}

/// Dispatch hataları.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DispatchError {
    #[error("Tool '{0}' not found")]
    NotFound(String),
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
    #[error("Timeout after {0}ms")]
    Timeout(u64),
    #[error("Dispatch internal error: {0}")]
    Internal(String),
}

/// Storage hataları.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StorageError {
    #[error("Graph '{0}' not found")]
    GraphNotFound(GraphId),
    #[error("Version {0} not found for graph '{1}'")]
    VersionNotFound(Version, GraphId),
    #[error("Execution '{0}' not found")]
    ExecutionNotFound(String),
    #[error("Storage internal error: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_from_i64() {
        let v: Value = 42i64.into();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn test_value_from_str() {
        let v: Value = "hello".into();
        assert_eq!(v, Value::String("hello".to_owned()));
    }

    #[test]
    fn test_context_estimated_bytes() {
        let mut ctx = Context::new();
        ctx.set("a".into(), Value::Int(1));
        ctx.set("b".into(), Value::String("hello".into()));
        assert!(ctx.estimated_bytes() > 0);
    }

    #[test]
    fn test_call_target_new() {
        let ct = CallTarget::new("math.add");
        assert_eq!(ct.name, "math.add");
        assert!(ct.args.is_empty());
        assert!(ct.kwargs.is_empty());
    }
}
