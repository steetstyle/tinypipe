//! Plan dump renderer'ları: text / mermaid / dot formatları.
//!
//! CLI'dan bağımsızdır — `CompiledPlan` üzerinde çalışır, sonucu `String`
//! olarak döndürür (CLI sadece yazdırır). Mermaid çıktısı mermaid.live,
//! dot çıktısı graphviz ile render edilebilir.

use crate::compiled::{CompiledEdge, CompiledNode, CompiledPlan};
use crate::plan::{EdgeKind, Opcode};

/// Plan dump formatları (CLI `plan --format` için).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlanFormat {
    /// İnsan-okur tablo dökümü (varsayılan).
    Text,
    /// Mermaid flowchart (mermaid.live'da render edilebilir).
    Mermaid,
    /// Graphviz DOT.
    Dot,
}

impl PlanFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(PlanFormat::Text),
            "mermaid" => Some(PlanFormat::Mermaid),
            "dot" => Some(PlanFormat::Dot),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PlanFormat::Text => "text",
            PlanFormat::Mermaid => "mermaid",
            PlanFormat::Dot => "dot",
        }
    }
}

/// Text dump header bilgisi (kaynak graph ve encoded boyut, CLI'den gelir).
pub struct PlanDumpHeader<'a> {
    pub graph_name: &'a str,
    pub graph_version: u64,
    pub encoded_len: usize,
}

impl PlanFormat {
    pub fn render(self, plan: &CompiledPlan, header: &PlanDumpHeader) -> String {
        match self {
            PlanFormat::Text => dump_text(plan, header),
            PlanFormat::Mermaid => dump_mermaid(plan),
            PlanFormat::Dot => dump_dot(plan),
        }
    }
}

/// Arg değerini ekran için sadeleştir: JSON string tırnaklarını kaldır.
fn arg_value(raw: &str) -> String {
    raw.trim_matches('"').to_string()
}

/// Node etiketi: `op: kısa özet` (mermaid/dot için).
fn node_label(n: &CompiledNode) -> String {
    let arg = |key: &str| {
        n.args
            .iter()
            .find(|a| a.key == key)
            .map(|a| arg_value(&a.value))
    };
    let op = format!("{:?}", n.op);
    match n.op {
        Opcode::Input => arg("name").map(|v| format!("Input {}", v)).unwrap_or(op),
        Opcode::Calc => match (arg("output"), arg("expr")) {
            (Some(out), Some(expr)) => format!("{} = {}", out, expr),
            (None, Some(expr)) => expr,
            _ => op,
        },
        Opcode::Act => arg("type").map(|v| format!("Act {}", v)).unwrap_or(op),
        Opcode::Decide => {
            let src = arg("source").unwrap_or_default();
            let op = arg("op").unwrap_or_default();
            let val = arg("value").unwrap_or_default();
            match (src.is_empty(), op.is_empty(), val.is_empty()) {
                (false, false, false) => format!("Decide {} {} {}", src, op, val),
                _ => arg("condition")
                    .map(|c| format!("Decide {}", c))
                    .unwrap_or(op),
            }
        }
        Opcode::Loop => {
            let target = arg("target").unwrap_or_default();
            let max = arg("max_iterations").unwrap_or_default();
            if target.is_empty() {
                op
            } else {
                format!("Loop {} max={}", target, max)
            }
        }
        _ => op,
    }
}

/// Edge etiketi: koşullu edge'ler koşulunu, control edge'leri "control" gösterir.
fn edge_label(e: &CompiledEdge) -> Option<String> {
    match e.condition.as_deref() {
        Some(c) => Some(c.to_string()),
        None => match e.kind {
            EdgeKind::Control => Some("control".into()),
            EdgeKind::Data => None,
        },
    }
}

fn dump_text(plan: &CompiledPlan, header: &PlanDumpHeader) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Graph: {} (v{})\n",
        header.graph_name, header.graph_version
    ));
    out.push_str(&format!(
        "Format: FlatBuffers ({} bytes)\n\n",
        header.encoded_len
    ));

    out.push_str(&format!("Nodes ({}):\n", plan.nodes.len()));
    for n in &plan.nodes {
        out.push_str(&format!("  [{}] {:?} op={:?}\n", n.index, n.id, n.op));
        for a in &n.args {
            out.push_str(&format!("      {} = {}\n", a.key, a.value));
        }
        if let Some(bid) = n.branch_id {
            out.push_str(&format!("      (branch_id: {})\n", bid));
        }
    }

    out.push_str(&format!("\nEdges ({}):\n", plan.edges.len()));
    for e in &plan.edges {
        let kind = match e.kind {
            EdgeKind::Data => "data",
            EdgeKind::Control => "control",
        };
        let cond = e
            .condition
            .as_deref()
            .map(|c| format!(" cond={}", c))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {} -> {} [{}{}]\n",
            e.from_index, e.to_index, kind, cond
        ));
    }

    out.push_str(&format!(
        "\nMetadata: version={} max_nodes={} max_time_ms={} max_mem_bytes={}\n",
        plan.version,
        plan.metadata.max_node_execution_count,
        plan.metadata.max_execution_time_ms,
        plan.metadata.max_context_memory_bytes
    ));
    out
}

fn dump_mermaid(plan: &CompiledPlan) -> String {
    let mut out = String::new();
    out.push_str("```mermaid\nflowchart LR\n");
    for n in &plan.nodes {
        let label = node_label(n).replace('"', "'");
        out.push_str(&format!("    N{}[\"{}\"]\n", n.index, label));
    }
    for e in &plan.edges {
        match edge_label(e) {
            Some(l) => out.push_str(&format!(
                "    N{} -->|{}| N{}\n",
                e.from_index, l, e.to_index
            )),
            None => out.push_str(&format!("    N{} --> N{}\n", e.from_index, e.to_index)),
        }
    }
    out.push_str("```\n");
    out
}

fn dump_dot(plan: &CompiledPlan) -> String {
    let mut out = String::new();
    out.push_str("digraph plan {\n");
    for n in &plan.nodes {
        let label = node_label(n).replace('"', "'");
        out.push_str(&format!("    N{} [label=\"{}\"];\n", n.index, label));
    }
    for e in &plan.edges {
        match edge_label(e) {
            Some(l) => out.push_str(&format!(
                "    N{} -> N{} [label=\"{}\"];\n",
                e.from_index, e.to_index, l
            )),
            None => out.push_str(&format!("    N{} -> N{};\n", e.from_index, e.to_index)),
        }
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::{CompiledArg, CompiledMetadata};

    fn sample_plan() -> CompiledPlan {
        CompiledPlan {
            version: 1,
            nodes: vec![
                CompiledNode {
                    index: 0,
                    id: "n0".into(),
                    op: Opcode::Input,
                    args: vec![CompiledArg {
                        key: "name".into(),
                        value: "\"x\"".into(),
                    }],
                    inferred_type: None,
                    branch_id: None,
                },
                CompiledNode {
                    index: 1,
                    id: "n1".into(),
                    op: Opcode::Decide,
                    args: vec![
                        CompiledArg {
                            key: "source".into(),
                            value: "\"x\"".into(),
                        },
                        CompiledArg {
                            key: "op".into(),
                            value: "\"lt\"".into(),
                        },
                        CompiledArg {
                            key: "value".into(),
                            value: "5".into(),
                        },
                    ],
                    inferred_type: None,
                    branch_id: None,
                },
                CompiledNode {
                    index: 2,
                    id: "n2".into(),
                    op: Opcode::Act,
                    args: vec![CompiledArg {
                        key: "type".into(),
                        value: "\"return\"".into(),
                    }],
                    inferred_type: None,
                    branch_id: None,
                },
            ],
            edges: vec![
                crate::compiled::CompiledEdge {
                    from_index: 0,
                    to_index: 1,
                    condition: None,
                    mapping: None,
                    priority: None,
                    label: None,
                    kind: EdgeKind::Data,
                },
                crate::compiled::CompiledEdge {
                    from_index: 1,
                    to_index: 2,
                    condition: Some("true".into()),
                    mapping: None,
                    priority: None,
                    label: None,
                    kind: EdgeKind::Data,
                },
                crate::compiled::CompiledEdge {
                    from_index: 1,
                    to_index: 2,
                    condition: None,
                    mapping: None,
                    priority: None,
                    label: None,
                    kind: EdgeKind::Control,
                },
            ],
            metadata: CompiledMetadata::default(),
            id_map: None,
        }
    }

    fn header() -> PlanDumpHeader<'static> {
        PlanDumpHeader {
            graph_name: "g1",
            graph_version: 3,
            encoded_len: 42,
        }
    }

    #[test]
    fn test_plan_format_parse() {
        assert_eq!(PlanFormat::parse("text"), Some(PlanFormat::Text));
        assert_eq!(PlanFormat::parse("mermaid"), Some(PlanFormat::Mermaid));
        assert_eq!(PlanFormat::parse("dot"), Some(PlanFormat::Dot));
        assert_eq!(PlanFormat::parse("xml"), None);
    }

    #[test]
    fn test_dump_text_contains_all_sections() {
        let out = PlanFormat::Text.render(&sample_plan(), &header());
        assert!(out.contains("Graph: g1 (v3)"));
        assert!(out.contains("FlatBuffers (42 bytes)"));
        assert!(out.contains("Nodes (3):"));
        assert!(out.contains("[1] \"n1\" op=Decide"));
        assert!(out.contains("source = \"x\""));
        assert!(out.contains("Edges (3):"));
        assert!(out.contains("1 -> 2 [data cond=true]"));
        assert!(out.contains("1 -> 2 [control]"));
        assert!(out.contains("Metadata: version=1"));
    }

    #[test]
    fn test_dump_mermaid_renders_graph() {
        let out = PlanFormat::Mermaid.render(&sample_plan(), &header());
        assert!(out.contains("```mermaid"));
        assert!(out.contains("flowchart LR"));
        assert!(out.contains("N0[\"Input x\"]"));
        assert!(out.contains("N1[\"Decide x lt 5\"]"));
        assert!(out.contains("N2[\"Act return\"]"));
        assert!(out.contains("N0 --> N1"));
        assert!(out.contains("N1 -->|true| N2"));
        assert!(out.contains("N1 -->|control| N2"));
    }

    #[test]
    fn test_dump_dot_renders_graph() {
        let out = PlanFormat::Dot.render(&sample_plan(), &header());
        assert!(out.contains("digraph plan {"));
        assert!(out.contains("N0 [label=\"Input x\"];"));
        assert!(out.contains("N1 -> N2 [label=\"true\"];"));
        assert!(out.contains("N1 -> N2 [label=\"control\"];"));
        assert!(out.contains("N0 -> N1;"));
    }
}
