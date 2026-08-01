//! Plan dump renderer'ları: text / mermaid / dot formatları.
//!
//! CLI'dan bağımsızdır — `CompiledPlan` üzerinde çalışır, sonucu `String`
//! olarak döndürür (CLI sadece yazdırır).
//!
//! - **Text**: ham node/edge dökümü (debug/audit).
//! - **Mermaid / Dot**: `plan_view`'daki semantik renderer (label + simplify +
//!   GROUP subgraph'ları + collapse). Mermaid çıktısı mermaid.live'da,
//!   dot çıktısı graphviz ile render edilebilir.

use crate::compiled::CompiledPlan;
use crate::plan_view::{render_dot, render_mermaid, RenderOptions};

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

    /// `--format` dışındaki görünüm seçimlerinin (`--view/--direction`)
    /// geçerli olup olmadığı.
    pub fn supports_options(self) -> bool {
        !matches!(self, PlanFormat::Text)
    }
}

/// Text dump header bilgisi (kaynak graph ve encoded boyut, CLI'den gelir).
pub struct PlanDumpHeader<'a> {
    pub graph_name: &'a str,
    pub graph_version: u64,
    pub encoded_len: usize,
}

impl PlanFormat {
    pub fn render(
        self,
        plan: &CompiledPlan,
        header: &PlanDumpHeader,
        options: RenderOptions,
    ) -> String {
        match self {
            PlanFormat::Text => dump_text(plan, header),
            PlanFormat::Mermaid => render_mermaid(plan, options),
            PlanFormat::Dot => render_dot(plan, options),
        }
    }
}

/// Arg değerini ekran için sadeleştir: tırnakları ve eski formatın
/// JSON-escape'li tırnaklarını (`\"`) kaldır.
fn arg_value(raw: &str) -> String {
    raw.trim_matches(|c| c == '"' || c == '\\').to_string()
}

fn dump_text(plan: &CompiledPlan, header: &PlanDumpHeader) -> String {
    use crate::plan::EdgeKind;

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
            out.push_str(&format!("      {} = {}\n", a.key, arg_value(&a.value)));
        }
        if let Some(bid) = n.branch_id {
            out.push_str(&format!("      (branch_id: {})\n", bid));
        }
        if let Some(gid) = n.group_id {
            let title = plan
                .groups
                .get(gid as usize)
                .map(|s| s.as_str())
                .unwrap_or("?");
            out.push_str(&format!("      (group: {} -> {})\n", gid, title));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::{CompiledArg, CompiledMetadata, CompiledNode};
    use crate::plan::{EdgeKind, Opcode};
    use crate::plan_view::{Direction, ViewLevel};

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
                    group_id: None,
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
                    group_id: None,
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
                    group_id: None,
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
                    from_index: 0,
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
            groups: Vec::new(),
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
        let out = PlanFormat::Text.render(&sample_plan(), &header(), RenderOptions::default());
        assert!(out.contains("Graph: g1 (v3)"));
        assert!(out.contains("FlatBuffers (42 bytes)"));
        assert!(out.contains("Nodes (3):"));
        assert!(out.contains("[1] \"n1\" op=Decide"));
        assert!(out.contains("source = x"));
        assert!(out.contains("op = lt"));
        assert!(out.contains("value = 5"));
        assert!(out.contains("Edges (3):"));
        assert!(out.contains("1 -> 2 [data cond=true]"));
        assert!(out.contains("0 -> 2 [control]"));
        assert!(out.contains("Metadata: version=1"));
    }

    #[test]
    fn test_dump_text_shows_group() {
        let mut plan = sample_plan();
        plan.groups = vec!["Seeding".into()];
        plan.nodes[1].group_id = Some(0);
        let out = PlanFormat::Text.render(&plan, &header(), RenderOptions::default());
        assert!(out.contains("(group: 0 -> Seeding)"));
    }

    #[test]
    fn test_dump_mermaid_semantic_labels() {
        let out = PlanFormat::Mermaid.render(&sample_plan(), &header(), RenderOptions::default());
        assert!(out.contains("```mermaid"));
        assert!(out.contains("flowchart TD"));
        assert!(out.contains("N0[\"Input: x\"]"));
        assert!(out.contains("N1{\"x lt 5 ?\"}"), "Decide → diamond");
        assert!(out.contains("N2([\"Return\"])"), "Return → stadium");
        assert!(out.contains("N0 --> N1"));
        assert!(out.contains("N1 -->|true| N2"));
        // Control edge → dashed (data edge'i olmayan çiftlerde)
        assert!(out.contains("N0 -.-> N2"));
    }

    #[test]
    fn test_dump_mermaid_honors_options() {
        let options = RenderOptions {
            view: ViewLevel::Summary,
            direction: Direction::Lr,
            numbered_groups: true,
        };
        let out = PlanFormat::Mermaid.render(&sample_plan(), &header(), options);
        assert!(out.contains("flowchart LR"));
        assert!(!out.contains("subgraph"));
    }

    #[test]
    fn test_dump_dot_renders_graph() {
        let out = PlanFormat::Dot.render(&sample_plan(), &header(), RenderOptions::default());
        assert!(out.contains("digraph plan {"));
        assert!(out.contains("N0 [label=\"Input: x\"];"));
        assert!(out.contains("N1 -> N2 [label=\"true\", style=solid];"));
        assert!(out.contains("N0 -> N2 [style=dashed];"));
    }

    #[test]
    fn test_supports_options() {
        assert!(!PlanFormat::Text.supports_options());
        assert!(PlanFormat::Mermaid.supports_options());
        assert!(PlanFormat::Dot.supports_options());
    }
}
