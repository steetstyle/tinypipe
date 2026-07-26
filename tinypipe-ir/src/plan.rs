//! Execution plan types — v1 JSON-based, ileride FlatBuffers binary.
//!
//! `ExecutionPlan` bir DAG graph'ını temsil eder:
//! - `nodes`: opcode'lu node listesi
//! - `edges`: node'lar arası yönlü bağlantılar
//! - `metadata`: compiler versiyonu, budget limitleri, optimizasyonlar

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Type system — JSON Schema types + composite types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Type {
    String,
    Int,
    Float,
    Bool,
    Null,
    Any,
}

/// Argüman değeri — bir node parametresinin değeri.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Array(Vec<ArgValue>),
    Object(HashMap<String, ArgValue>),
}

impl From<&str> for ArgValue {
    fn from(s: &str) -> Self { ArgValue::String(s.to_owned()) }
}

impl From<i64> for ArgValue {
    fn from(v: i64) -> Self { ArgValue::Int(v) }
}

impl From<f64> for ArgValue {
    fn from(v: f64) -> Self { ArgValue::Float(v) }
}

/// Bir node parametresi (key-value).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Arg {
    pub key: String,
    pub value: ArgValue,
}

impl Arg {
    pub fn new(key: &str, value: ArgValue) -> Self {
        Arg { key: key.to_owned(), value }
    }
}

/// Node tipleri — FlatBuffers Opcode enum'ının Rust karşılığı.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Opcode {
    Input = 0,
    Call = 1,
    Calc = 2,
    Decide = 3,
    Switch = 4,
    Act = 5,
    Parallel = 6,
    Loop = 7,
    Wait = 8,
    Merge = 9,
    Error = 10,
}

impl Opcode {
    /// Tüm opcode'ları döndür (enum iteration).
    pub fn all() -> &'static [Opcode] {
        &[
            Opcode::Input, Opcode::Call, Opcode::Calc, Opcode::Decide,
            Opcode::Switch, Opcode::Act, Opcode::Parallel, Opcode::Loop,
            Opcode::Wait, Opcode::Merge, Opcode::Error,
        ]
    }

    /// Opcode'un saf (pure) olup olmadığı — yan etkisiz hesaplama.
    pub fn is_pure(&self) -> bool {
        matches!(self, Opcode::Input | Opcode::Calc | Opcode::Decide
            | Opcode::Switch | Opcode::Wait | Opcode::Merge)
    }

    /// Opcode'un dallanma yapıp yapmadığı (birden çok çıkış edge'i).
    pub fn is_branch(&self) -> bool {
        matches!(self, Opcode::Decide | Opcode::Switch)
    }

    /// İnsan okunabilir isim.
    pub fn display_name(&self) -> &'static str {
        match self {
            Opcode::Input => "INPUT",
            Opcode::Call => "CALL",
            Opcode::Calc => "CALC",
            Opcode::Decide => "DECIDE",
            Opcode::Switch => "SWITCH",
            Opcode::Act => "ACT",
            Opcode::Parallel => "PARALLEL",
            Opcode::Loop => "LOOP",
            Opcode::Wait => "WAIT",
            Opcode::Merge => "MERGE",
            Opcode::Error => "ERROR",
        }
    }
}

/// Tool dependency — FlatBuffers IR ToolDep table karşılığı.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDep {
    pub name: String,
    pub version: String,
    pub pure: bool,
    pub schema_hash: String,
}

/// Execution plan node'u.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub op: Opcode,
    pub args: Vec<Arg>,
    pub inferred_type: Option<Type>,
    /// PARALLEL branch ID: hangi parallel branch'e ait olduğu.
    /// `None` = global scope (branch dışı veya PARALLEL yok).
    /// `Some(id)` = bu node, branch `id`'ye ait.
    pub branch_id: Option<u32>,
}

impl Node {
    pub fn new(id: &str, op: Opcode) -> Self {
        Node { id: id.to_owned(), op, args: Vec::new(), inferred_type: None, branch_id: None }
    }

    pub fn with_arg(mut self, key: &str, value: ArgValue) -> Self {
        self.args.push(Arg::new(key, value));
        self
    }

    /// Bu node'u belirtilen PARALLEL branch'ine ata.
    pub fn with_branch(mut self, branch_id: u32) -> Self {
        self.branch_id = Some(branch_id);
        self
    }
}

/// Edge kind — veri akışı vs kontrol akışı (sequential).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Data dependency: the target node needs the source node's output value.
    Data,
    /// Control-flow edge: the target node must execute AFTER the source node completes.
    Control,
}

impl Default for EdgeKind {
    fn default() -> Self {
        EdgeKind::Data
    }
}

/// Edge (yönlü bağlantı).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub condition: Option<String>,
    pub mapping: Option<HashMap<String, String>>,
    pub priority: Option<i32>,
    pub label: Option<String>,
    pub kind: EdgeKind,
}

impl Edge {
    pub fn new(from: &str, to: &str) -> Self {
        Edge {
            from: from.to_owned(),
            to: to.to_owned(),
            condition: None,
            mapping: None,
            priority: None,
            label: None,
            kind: EdgeKind::Data,
        }
    }

    /// Create a control-flow (sequential) edge.
    /// The target node executes only after the source node completes.
    pub fn control(from: &str, to: &str) -> Self {
        Edge {
            from: from.to_owned(),
            to: to.to_owned(),
            condition: None,
            mapping: None,
            priority: None,
            label: None,
            kind: EdgeKind::Control,
        }
    }

    pub fn with_condition(from: &str, to: &str, condition: &str) -> Self {
        Edge {
            from: from.to_owned(),
            to: to.to_owned(),
            condition: Some(condition.to_owned()),
            mapping: None,
            priority: None,
            label: None,
            kind: EdgeKind::Data,
        }
    }

    pub fn with_mapping(from: &str, to: &str, mapping: HashMap<String, String>) -> Self {
        Edge {
            from: from.to_owned(),
            to: to.to_owned(),
            condition: None,
            mapping: Some(mapping),
            priority: None,
            label: None,
            kind: EdgeKind::Data,
        }
    }
}

/// FlatBuffers IR metadata'sı.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub compiler_version: String,
    pub compiled_at: String,
    pub node_count: u32,
    pub edge_count: u32,
    pub max_node_execution_count: u32,
    pub max_context_memory_bytes: u32,
    pub max_recursion_depth: u32,
    pub max_execution_time_ms: u32,
    pub optimizations: Vec<String>,
    /// Tool dependencies (semver pin + schema hash).
    pub tool_deps: Vec<ToolDep>,
    /// Subgraph dependencies (targets starting with "subgraph:").
    pub subgraph_dependencies: Vec<String>,
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata {
            compiler_version: "0.1.0".into(),
            compiled_at: String::new(),
            node_count: 0,
            edge_count: 0,
            max_node_execution_count: 10_000,
            max_context_memory_bytes: 10 * 1024 * 1024, // 10 MB
            max_recursion_depth: 5,
            max_execution_time_ms: 30_000,
            optimizations: Vec::new(),
            tool_deps: Vec::new(),
            subgraph_dependencies: Vec::new(),
        }
    }
}

/// Full execution plan — compiler çıktısı, VM girdisi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub version: u16,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub metadata: Metadata,
}

impl ExecutionPlan {
    pub fn new(nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        let node_count = nodes.len() as u32;
        let edge_count = edges.len() as u32;
        ExecutionPlan {
            version: 3,
            metadata: Metadata { node_count, edge_count, ..Default::default() },
            nodes,
            edges,
        }
    }

    /// Node'u ID'sine göre bul. v1: O(n) linear scan, v2: HashMap index.
    pub fn get_node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Bir node'dan çıkan edge'leri bul.
    pub fn edges_from(&self, node_id: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.from == node_id).collect()
    }

    /// Bir node'a gelen edge'leri bul.
    pub fn edges_to(&self, node_id: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.to == node_id).collect()
    }

    /// Topolojik sırayı döndür (Kahn's algorithm).
    pub fn topological_order(&self) -> Result<Vec<&Node>, String> {
        let mut in_degree: std::collections::HashMap<&str, usize> = self.nodes.iter()
            .map(|n| (n.id.as_str(), 0)).collect();

        for edge in &self.edges {
            if let Some(deg) = in_degree.get_mut(edge.to.as_str()) {
                *deg += 1;
            }
        }

        let mut queue: Vec<&Node> = self.nodes.iter()
            .filter(|n| in_degree.get(n.id.as_str()) == Some(&0))
            .collect();

        let mut result = Vec::new();

        while let Some(node) = queue.pop() {
            result.push(node);
            for edge in self.edges_from(&node.id) {
                if let Some(deg) = in_degree.get_mut(edge.to.as_str()) {
                    *deg -= 1;
                    if *deg == 0 {
                        if let Some(next) = self.get_node(&edge.to) {
                            queue.push(next);
                        }
                    }
                }
            }
        }

        if result.len() != self.nodes.len() {
            return Err("Graph contains a cycle".into());
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> ExecutionPlan {
        ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc).with_arg("expr", "x + 1".into()),
                Node::new("output1", Opcode::Act).with_arg("type", "notify".into()),
            ],
            vec![
                Edge::new("input1", "calc1"),
                Edge::new("calc1", "output1"),
            ],
        )
    }

    #[test]
    fn test_plan_new() {
        let plan = sample_plan();
        assert_eq!(plan.version, 3);
        assert_eq!(plan.metadata.node_count, 3);
        assert_eq!(plan.metadata.edge_count, 2);
    }

    #[test]
    fn test_get_node() {
        let plan = sample_plan();
        assert!(plan.get_node("calc1").is_some());
        assert!(plan.get_node("nonexistent").is_none());
    }

    #[test]
    fn test_edges_from() {
        let plan = sample_plan();
        let edges = plan.edges_from("input1");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to, "calc1");
    }

    #[test]
    fn test_edges_to() {
        let plan = sample_plan();
        let edges = plan.edges_to("calc1");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, "input1");
    }

    #[test]
    fn test_topological_order() {
        let plan = sample_plan();
        let order = plan.topological_order().unwrap();
        assert_eq!(order.len(), 3);
        // input1 her zaman ilk
        assert_eq!(order[0].id, "input1");
        // output1 her zaman son
        assert_eq!(order[2].id, "output1");
    }

    #[test]
    fn test_cycle_detection() {
        let plan = ExecutionPlan::new(
            vec![
                Node::new("a", Opcode::Input),
                Node::new("b", Opcode::Calc),
            ],
            vec![
                Edge::new("a", "b"),
                Edge::new("b", "a"), // cycle!
            ],
        );
        assert!(plan.topological_order().is_err());
    }

    #[test]
    fn test_opcode_pure() {
        assert!(Opcode::Calc.is_pure());
        assert!(Opcode::Decide.is_pure());
        assert!(!Opcode::Act.is_pure());
        assert!(!Opcode::Call.is_pure());
    }

    #[test]
    fn test_opcode_branch() {
        assert!(Opcode::Decide.is_branch());
        assert!(Opcode::Switch.is_branch());
        assert!(!Opcode::Calc.is_branch());
    }

    #[test]
    fn test_opcode_all_count() {
        assert_eq!(Opcode::all().len(), 11);
    }

    #[test]
    fn test_node_with_args() {
        let n = Node::new("test", Opcode::Call)
            .with_arg("target", "math.add".into())
            .with_arg("timeout", 5000i64.into());
        assert_eq!(n.args.len(), 2);
        assert_eq!(n.args[0].key, "target");
    }

    #[test]
    fn test_edge_with_condition() {
        let e = Edge::with_condition("decide1", "calc1", "$x > 0");
        assert_eq!(e.condition, Some("$x > 0".into()));
    }

    #[test]
    fn test_metadata_default() {
        let m: Metadata = Default::default();
        assert_eq!(m.max_node_execution_count, 10_000);
        assert_eq!(m.max_context_memory_bytes, 10 * 1024 * 1024);
        assert_eq!(m.max_execution_time_ms, 30_000);
    }

    #[test]
    fn test_serde_roundtrip() {
        let plan = sample_plan();
        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: ExecutionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, deserialized);
    }
}
