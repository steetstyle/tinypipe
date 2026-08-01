//! `CompiledPlan` — Codegen çıktısı: uint32 node index'li kompakt binary format.
//!
//! v1'de `ExecutionPlan` JSON string ID'ler kullanır (O(n) linear scan).
//! v2'de `CompiledPlan` uint32 index'ler kullanır (O(1) random access) ve
//! `FlatBuffers` ile binary serialize edilir — JSON'un ~%20'si boyutunda.
//!
//! # Format
//!
//! | Format | Yöntem | Kullanım |
//! |--------|--------|----------|
//! | FlatBuffers | `to_fb_bytes()` / `from_fb_bytes()` | **Canonical**, cross-language, schema-verified |
//!
//! # Dönüşüm
//!
//! Codegen aşamasında `ExecutionPlan` (string ID'ler) → `CompiledPlan` (uint32 index'ler):
//!
//! ```ignore
//! let plan: ExecutionPlan = ...;
//! let compiled: CompiledPlan = CompiledPlan::from_execution_plan(&plan)?;
//! let bytes: Vec<u8> = compiled.to_fb_bytes()?;  // FlatBuffers (canonical)
//! ```
//!
//! VM'de:
//! ```ignore
//! let compiled: CompiledPlan = CompiledPlan::from_fb_bytes(&bytes)?;
//! let node = compiled.nodes.get(42);  // O(1), HashMap yok
//! let next = compiled.nodes[edge.to_index as usize];  // O(1)
//! ```

use serde::{Deserialize, Serialize};

use crate::plan::{ArgValue, EdgeKind, ExecutionPlan, Opcode, ToolDep, Type};

// Re-export FlatBuffers root accessor so VM and other crates can use it
pub use crate::fb::root_as_execution_plan;

/// Compiled node — `ExecutionPlan::Node`'un index bazlı versiyonu.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledNode {
    /// Node index (codegen tarafından atanır, topolojik sırada).
    pub index: u32,
    /// İnsan okunabilir ID (debug/audit için, VM kullanmaz).
    pub id: String,
    /// Opcode (1 byte).
    pub op: Opcode,
    /// Argüman listesi.
    pub args: Vec<CompiledArg>,
    /// Inferred type (v2.5+).
    pub inferred_type: Option<Type>,
    /// PARALLEL branch ID: bu node hangi branch'e ait.
    /// VM bu alanı scope isolation için kullanır.
    pub branch_id: Option<u32>,
    /// GROUP index'i: `plan.groups[group_id]` başlığına işaret eder.
    /// Yalnızca görüntüleme metadata'sı — VM tarafından kullanılmaz.
    pub group_id: Option<u32>,
}

/// Compiled arg — key-value çifti.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledArg {
    pub key: String,
    pub value: String, // JSON-encoded değer
}

/// Compiled edge — uint32 index'lerle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledEdge {
    pub from_index: u32,
    pub to_index: u32,
    pub condition: Option<String>,
    pub mapping: Option<Vec<(String, String)>>,
    pub priority: Option<i32>,
    pub label: Option<String>,
    pub kind: EdgeKind,
}

/// Compiled metadata — ExecutionPlan::Metadata ile aynı.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledMetadata {
    pub compiler_version: String,
    pub compiled_at: String,
    pub node_count: u32,
    pub edge_count: u32,
    pub max_node_execution_count: u32,
    pub max_context_memory_bytes: u32,
    pub max_recursion_depth: u32,
    pub max_execution_time_ms: u32,
    pub optimizations: Vec<String>,
    /// Tool bağımlılıkları (semver pin + schema hash).
    pub tool_deps: Vec<ToolDep>,
    /// Subgraph dependencies (targets starting with "subgraph:").
    pub subgraph_dependencies: Vec<String>,
    /// META(...) opaque JSON (title, description, owner, tags, ...).
    pub meta_json: String,
}

impl Default for CompiledMetadata {
    fn default() -> Self {
        CompiledMetadata {
            compiler_version: "0.2.0".into(),
            compiled_at: String::new(),
            node_count: 0,
            edge_count: 0,
            max_node_execution_count: 10_000,
            max_context_memory_bytes: 10 * 1024 * 1024,
            max_recursion_depth: 5,
            max_execution_time_ms: 30_000,
            optimizations: Vec::new(),
            tool_deps: Vec::new(),
            subgraph_dependencies: Vec::new(),
            meta_json: String::new(),
        }
    }
}

/// Compiled execution plan — binary-serializable, O(1) random access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledPlan {
    /// Plan format versiyonu.
    pub version: u16,
    /// Node'lar (index sıralı, 0..N-1).
    pub nodes: Vec<CompiledNode>,
    /// Edge'ler (uint32 index'lerle).
    pub edges: Vec<CompiledEdge>,
    /// Metadata.
    pub metadata: CompiledMetadata,
    /// String ID → uint32 index mapping (VM'in string→index çözümlemesi için).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_map: Option<Vec<(String, u32)>>,
    /// GROUP başlıkları: `node.group_id` bu vektöre index'ler.
    pub groups: Vec<String>,
}

impl CompiledPlan {
    /// ExecutionPlan'den CompiledPlan oluştur.
    /// String ID'leri uint32 index'lere dönüştürür.
    pub fn from_execution_plan(plan: &ExecutionPlan, optimizations: Vec<String>) -> Self {
        let node_count = plan.nodes.len();

        // String ID → index mapping (topolojik sırada veya insertion order)
        let mut id_to_index: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::with_capacity(node_count);
        let mut compiled_nodes: Vec<CompiledNode> = Vec::with_capacity(node_count);

        // GROUP başlıkları → index tablosu (display-only metadata)
        let mut group_names: Vec<String> = Vec::new();
        let mut group_index: std::collections::HashMap<&str, u32> =
            std::collections::HashMap::new();

        for (idx, node) in plan.nodes.iter().enumerate() {
            let index = idx as u32;
            id_to_index.insert(node.id.as_str(), index);

            let args: Vec<CompiledArg> = node
                .args
                .iter()
                .map(|a| CompiledArg {
                    key: a.key.clone(),
                    // String'ler transform'dan zaten tırnaklı gelir — JSON encode
                    // eklemek çift-encode olurdu. Diğer varyantlar JSON string'e çevrilir.
                    value: match &a.value {
                        ArgValue::String(s) => s.clone(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    },
                })
                .collect();

            let group_id = node.group.as_deref().map(|g| {
                *group_index.entry(g).or_insert_with(|| {
                    group_names.push(g.to_owned());
                    (group_names.len() - 1) as u32
                })
            });

            compiled_nodes.push(CompiledNode {
                index,
                id: node.id.clone(),
                op: node.op,
                args,
                inferred_type: node.inferred_type.clone(),
                branch_id: node.branch_id,
                group_id,
            });
        }

        // Edge dönüşümü
        let compiled_edges: Vec<CompiledEdge> = plan
            .edges
            .iter()
            .map(|e| {
                let from_index = id_to_index
                    .get(e.from.as_str())
                    .copied()
                    .unwrap_or(u32::MAX);
                let to_index = id_to_index.get(e.to.as_str()).copied().unwrap_or(u32::MAX);
                let mapping = e
                    .mapping
                    .as_ref()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
                CompiledEdge {
                    from_index,
                    to_index,
                    condition: e.condition.clone(),
                    mapping,
                    priority: e.priority,
                    label: e.label.clone(),
                    kind: e.kind.clone(),
                }
            })
            .collect();

        // ID map (opsiyonel, reverse lookup için)
        let id_map: Vec<(String, u32)> = id_to_index
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect();

        CompiledPlan {
            version: 4,
            nodes: compiled_nodes,
            edges: compiled_edges,
            metadata: CompiledMetadata {
                compiler_version: plan.metadata.compiler_version.clone(),
                compiled_at: plan.metadata.compiled_at.clone(),
                node_count: plan.metadata.node_count,
                edge_count: plan.metadata.edge_count,
                max_node_execution_count: plan.metadata.max_node_execution_count,
                max_context_memory_bytes: plan.metadata.max_context_memory_bytes,
                max_recursion_depth: plan.metadata.max_recursion_depth,
                max_execution_time_ms: plan.metadata.max_execution_time_ms,
                optimizations,
                tool_deps: plan.metadata.tool_deps.clone(),
                subgraph_dependencies: plan.metadata.subgraph_dependencies.clone(),
                meta_json: plan.metadata.meta_json.clone(),
            },
            id_map: Some(id_map),
            groups: group_names,
        }
    }

    /// Index'e göre node'u O(1) döndür.
    pub fn get_node(&self, index: u32) -> Option<&CompiledNode> {
        self.nodes.get(index as usize)
    }

    /// Bir node'dan çıkan edge'leri bul (O(m) — edge sayısı kadar).
    /// Not: v2'de edge listesi node bazlı indekslenebilir.
    pub fn edges_from(&self, node_index: u32) -> Vec<&CompiledEdge> {
        self.edges
            .iter()
            .filter(|e| e.from_index == node_index)
            .collect()
    }

    // ─── FlatBuffers serialization ───────────────────────────────

    /// Serialize to FlatBuffers binary format (zero-copy compatible).
    pub fn to_fb_bytes(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        use flatbuffers::FlatBufferBuilder;

        let mut fbb = FlatBufferBuilder::with_capacity(4096);

        // Build id_map vector
        let id_map_vec = if let Some(ref id_map) = self.id_map {
            let entries: Vec<_> = id_map
                .iter()
                .map(|(id, idx)| {
                    let id_off = fbb.create_string(id);
                    crate::fb::IdEntry::create(
                        &mut fbb,
                        &crate::fb::IdEntryArgs {
                            id: Some(id_off),
                            index: *idx,
                        },
                    )
                })
                .collect();
            Some(fbb.create_vector(&entries))
        } else {
            None
        };

        // Build tool_deps vector
        let tool_deps: Vec<_> = self
            .metadata
            .tool_deps
            .iter()
            .map(|dep| {
                let name = fbb.create_string(&dep.name);
                let version = fbb.create_string(&dep.version);
                let schema_hash = fbb.create_string(&dep.schema_hash);
                crate::fb::ToolDep::create(
                    &mut fbb,
                    &crate::fb::ToolDepArgs {
                        name: Some(name),
                        version: Some(version),
                        pure_: dep.pure,
                        schema_hash: Some(schema_hash),
                    },
                )
            })
            .collect();
        let tool_deps_vec = fbb.create_vector(&tool_deps);

        // Build optimizations vector
        let optimizations: Vec<_> = self
            .metadata
            .optimizations
            .iter()
            .map(|s| fbb.create_string(s))
            .collect();
        let optimizations_vec = fbb.create_vector(&optimizations);

        // Build subgraph_dependencies vector
        let subgraph_deps: Vec<_> = self
            .metadata
            .subgraph_dependencies
            .iter()
            .map(|s| fbb.create_string(s))
            .collect();
        let subgraph_deps_vec = fbb.create_vector(&subgraph_deps);

        // Build meta_json (META(...) opaque JSON, empty = absent)
        let meta_json = fbb.create_string(&self.metadata.meta_json);

        // Build metadata
        let compiler_version = fbb.create_string(&self.metadata.compiler_version);
        let compiled_at = fbb.create_string(&self.metadata.compiled_at);
        let metadata = crate::fb::CompiledMetadata::create(
            &mut fbb,
            &crate::fb::CompiledMetadataArgs {
                compiler_version: Some(compiler_version),
                compiled_at: Some(compiled_at),
                node_count: self.metadata.node_count,
                edge_count: self.metadata.edge_count,
                max_node_execution_count: self.metadata.max_node_execution_count,
                max_context_memory_bytes: self.metadata.max_context_memory_bytes,
                max_recursion_depth: self.metadata.max_recursion_depth,
                max_execution_time_ms: self.metadata.max_execution_time_ms,
                optimizations: Some(optimizations_vec),
                tool_deps: Some(tool_deps_vec),
                subgraph_dependencies: Some(subgraph_deps_vec),
                meta_json: Some(meta_json),
            },
        );

        // Build edges
        let edges: Vec<_> = self
            .edges
            .iter()
            .map(|edge| {
                let condition = if let Some(ref cond) = edge.condition {
                    Some(fbb.create_string(cond))
                } else {
                    None
                };
                let label = if let Some(ref l) = edge.label {
                    Some(fbb.create_string(l))
                } else {
                    None
                };
                let mapping = edge.mapping.as_ref().map(|m| {
                    let entries: Vec<_> = m
                        .iter()
                        .map(|(from_k, to_k)| {
                            let from = fbb.create_string(from_k);
                            let to = fbb.create_string(to_k);
                            crate::fb::MappingEntry::create(
                                &mut fbb,
                                &crate::fb::MappingEntryArgs {
                                    from: Some(from),
                                    to: Some(to),
                                },
                            )
                        })
                        .collect();
                    fbb.create_vector(&entries)
                });
                let priority = edge.priority.unwrap_or(0);
                let kind = match edge.kind {
                    EdgeKind::Data => crate::fb::EdgeKind::EK_Data,
                    EdgeKind::Control => crate::fb::EdgeKind::EK_Control,
                };
                crate::fb::CompiledEdge::create(
                    &mut fbb,
                    &crate::fb::CompiledEdgeArgs {
                        from_index: edge.from_index,
                        to_index: edge.to_index,
                        condition,
                        mapping,
                        priority,
                        label,
                        kind,
                    },
                )
            })
            .collect();
        let edges_vec = fbb.create_vector(&edges);

        // Build nodes
        let nodes: Vec<_> = self
            .nodes
            .iter()
            .map(|node| {
                let id = fbb.create_string(&node.id);
                let args: Vec<_> = node
                    .args
                    .iter()
                    .map(|arg| {
                        let key = fbb.create_string(&arg.key);
                        let value = fbb.create_string(&arg.value);
                        crate::fb::CompiledArg::create(
                            &mut fbb,
                            &crate::fb::CompiledArgArgs {
                                key: Some(key),
                                value: Some(value),
                            },
                        )
                    })
                    .collect();
                let args_vec = fbb.create_vector(&args);
                let inferred_type = match node.inferred_type {
                    None => crate::fb::Type::Unspecified,
                    Some(Type::String) => crate::fb::Type::String,
                    Some(Type::Int) => crate::fb::Type::Int,
                    Some(Type::Float) => crate::fb::Type::Float,
                    Some(Type::Bool) => crate::fb::Type::Bool,
                    Some(Type::Null) => crate::fb::Type::Null,
                    Some(Type::Any) => crate::fb::Type::Any,
                };
                let branch_id = node.branch_id.unwrap_or(u32::MAX);
                let group_id = node.group_id.unwrap_or(u32::MAX);
                crate::fb::CompiledNode::create(
                    &mut fbb,
                    &crate::fb::CompiledNodeArgs {
                        index: node.index,
                        id: Some(id),
                        op: crate::fb::Opcode(node.op as u8),
                        args: Some(args_vec),
                        inferred_type,
                        branch_id,
                        group_id,
                    },
                )
            })
            .collect();
        let nodes_vec = fbb.create_vector(&nodes);

        // Build groups vector (display-only GROUP titles)
        let groups: Vec<_> = self.groups.iter().map(|g| fbb.create_string(g)).collect();
        let groups_vec = fbb.create_vector(&groups);

        // Build root ExecutionPlan
        let root = crate::fb::ExecutionPlan::create(
            &mut fbb,
            &crate::fb::ExecutionPlanArgs {
                version: self.version,
                nodes: Some(nodes_vec),
                edges: Some(edges_vec),
                metadata: Some(metadata),
                id_map: id_map_vec,
                groups: Some(groups_vec),
            },
        );

        crate::fb::finish_execution_plan_buffer(&mut fbb, root);
        Ok(fbb.finished_data().to_vec())
    }

    /// Deserialize from FlatBuffers binary format.
    pub fn from_fb_bytes(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        let fb_plan = crate::fb::root_as_execution_plan(bytes)
            .map_err(|e| format!("FlatBuffers verification failed: {:?}", e))?;

        // Convert metadata
        let fb_meta = fb_plan
            .metadata()
            .ok_or_else(|| "FlatBuffers plan missing metadata".to_string())?;
        let tool_deps: Vec<ToolDep> = fb_meta
            .tool_deps()
            .map(|deps| {
                deps.iter()
                    .map(|dep| ToolDep {
                        name: dep.name().unwrap_or_default().to_string(),
                        version: dep.version().unwrap_or_default().to_string(),
                        pure: dep.pure_(),
                        schema_hash: dep.schema_hash().unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let optimizations: Vec<String> = fb_meta
            .optimizations()
            .map(|opts| opts.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let subgraph_deps: Vec<String> = fb_meta
            .subgraph_dependencies()
            .map(|deps| deps.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();

        let metadata = CompiledMetadata {
            compiler_version: fb_meta.compiler_version().unwrap_or_default().to_string(),
            compiled_at: fb_meta.compiled_at().unwrap_or_default().to_string(),
            node_count: fb_meta.node_count(),
            edge_count: fb_meta.edge_count(),
            max_node_execution_count: fb_meta.max_node_execution_count(),
            max_context_memory_bytes: fb_meta.max_context_memory_bytes(),
            max_recursion_depth: fb_meta.max_recursion_depth(),
            max_execution_time_ms: fb_meta.max_execution_time_ms(),
            optimizations,
            tool_deps,
            subgraph_dependencies: subgraph_deps,
            meta_json: fb_meta.meta_json().unwrap_or_default().to_string(),
        };

        // Convert edges
        let edges: Vec<CompiledEdge> = fb_plan
            .edges()
            .map(|fb_edges| {
                fb_edges
                    .iter()
                    .map(|e| {
                        let kind = match e.kind() {
                            crate::fb::EdgeKind::EK_Data => EdgeKind::Data,
                            crate::fb::EdgeKind::EK_Control => EdgeKind::Control,
                            _ => EdgeKind::Data,
                        };
                        let condition = e.condition().and_then(|c| {
                            if c.is_empty() {
                                None
                            } else {
                                Some(c.to_string())
                            }
                        });
                        let label = e.label().and_then(|l| {
                            if l.is_empty() {
                                None
                            } else {
                                Some(l.to_string())
                            }
                        });
                        let mapping = e.mapping().map(|m| {
                            m.iter()
                                .map(|entry| {
                                    (
                                        entry.from().unwrap_or_default().to_string(),
                                        entry.to().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect()
                        });
                        CompiledEdge {
                            from_index: e.from_index(),
                            to_index: e.to_index(),
                            condition,
                            mapping,
                            priority: if e.priority() == 0 {
                                None
                            } else {
                                Some(e.priority())
                            },
                            label,
                            kind,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Convert nodes
        let nodes: Vec<CompiledNode> = fb_plan
            .nodes()
            .map(|fb_nodes| {
                fb_nodes
                    .iter()
                    .map(|n| {
                        let args: Vec<CompiledArg> = n
                            .args()
                            .map(|fb_args| {
                                fb_args
                                    .iter()
                                    .map(|a| CompiledArg {
                                        key: a.key().unwrap_or_default().to_string(),
                                        value: a.value().unwrap_or_default().to_string(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let inferred_type = {
                            let t = n.inferred_type().0;
                            match t {
                                0 => None, // Unspecified
                                1 => Some(Type::Int),
                                2 => Some(Type::Float),
                                3 => Some(Type::String),
                                4 => Some(Type::Bool),
                                5 => Some(Type::Null),
                                6 => Some(Type::Any),
                                // List=7, Map=8 → fallback to Any
                                7 | 8 => Some(Type::Any),
                                _ => None,
                            }
                        };
                        let op = match n.op().0 {
                            0 => Opcode::Input,
                            1 => Opcode::Call,
                            2 => Opcode::Calc,
                            3 => Opcode::Decide,
                            4 => Opcode::Switch,
                            5 => Opcode::Act,
                            6 => Opcode::Parallel,
                            7 => Opcode::Loop,
                            8 => Opcode::Wait,
                            9 => Opcode::Merge,
                            10 => Opcode::Error,
                            _ => Opcode::Input,
                        };
                        let fb_branch_id = n.branch_id();
                        let branch_id = if fb_branch_id == u32::MAX {
                            None
                        } else {
                            Some(fb_branch_id)
                        };
                        let fb_group_id = n.group_id();
                        let group_id = if fb_group_id == u32::MAX {
                            None
                        } else {
                            Some(fb_group_id)
                        };
                        CompiledNode {
                            index: n.index(),
                            id: n.id().unwrap_or_default().to_string(),
                            op,
                            args,
                            inferred_type,
                            branch_id,
                            group_id,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Convert id_map
        let id_map: Option<Vec<(String, u32)>> = fb_plan.id_map().map(|map| {
            map.iter()
                .map(|entry| (entry.id().unwrap_or_default().to_string(), entry.index()))
                .collect()
        });

        // Convert groups (display-only GROUP titles)
        let groups: Vec<String> = fb_plan
            .groups()
            .map(|gs| gs.iter().map(|g| g.to_string()).collect())
            .unwrap_or_default();

        Ok(CompiledPlan {
            version: fb_plan.version(),
            nodes,
            edges,
            metadata,
            id_map,
            groups,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Edge, Node};

    fn sample_plan() -> ExecutionPlan {
        ExecutionPlan::new(
            vec![
                Node::new("input1", Opcode::Input).with_arg("name", "x".into()),
                Node::new("calc1", Opcode::Calc).with_arg("expr", "x + 1".into()),
                Node::new("act1", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![Edge::new("input1", "calc1"), Edge::new("calc1", "act1")],
        )
    }

    #[test]
    fn test_compile_plan() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        assert_eq!(compiled.nodes.len(), 3);
        assert_eq!(compiled.edges.len(), 2);
        assert_eq!(compiled.nodes[0].index, 0);
        assert_eq!(compiled.nodes[0].id, "input1");
        assert_eq!(compiled.nodes[1].op, Opcode::Calc);
    }

    #[test]
    fn test_edge_indices() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let edge = &compiled.edges[0];
        assert_eq!(edge.from_index, 0); // input1
        assert_eq!(edge.to_index, 1); // calc1
        let edge = &compiled.edges[1];
        assert_eq!(edge.from_index, 1); // calc1
        assert_eq!(edge.to_index, 2); // act1
    }

    #[test]
    fn test_get_node() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let node = compiled.get_node(1).unwrap();
        assert_eq!(node.id, "calc1");
        assert!(compiled.get_node(99).is_none());
    }

    #[test]
    fn test_edges_from() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let edges = compiled.edges_from(0); // input1 → calc1
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to_index, 1);

        let edges = compiled.edges_from(2); // act1 → nothing
        assert_eq!(edges.len(), 0);
    }

    #[test]
    fn test_fb_roundtrip() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let bytes = compiled.to_fb_bytes().expect("fb serialize");
        let deserialized = CompiledPlan::from_fb_bytes(&bytes).expect("fb deserialize");
        assert_eq!(compiled, deserialized);
    }

    #[test]
    fn test_fb_roundtrip_with_metadata() {
        let mut plan = sample_plan();
        plan.metadata.compiler_version = "tinypipe@0.1.0".into();
        plan.metadata.compiled_at = "2026-07-25T12:00:00Z".into();
        plan.metadata.node_count = 3;
        plan.metadata.edge_count = 2;
        plan.metadata.max_node_execution_count = 100;
        plan.metadata.max_context_memory_bytes = 4096;
        plan.metadata.max_recursion_depth = 5;
        plan.metadata.max_execution_time_ms = 5000;
        plan.metadata.optimizations = vec!["calc_fusion".into(), "dead_code".into()];
        plan.metadata.tool_deps.push(ToolDep {
            name: "math".into(),
            version: "^1.0".into(),
            pure: true,
            schema_hash: "abc".into(),
        });
        plan.metadata.subgraph_dependencies.push("sub/v1".into());

        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let bytes = compiled.to_fb_bytes().expect("fb serialize");
        let deserialized = CompiledPlan::from_fb_bytes(&bytes).expect("fb deserialize");
        assert_eq!(compiled, deserialized);
    }

    #[test]
    fn test_fb_with_inferred_type() {
        let mut plan = sample_plan();
        plan.nodes[1].inferred_type = Some(crate::plan::Type::Float);
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let bytes = compiled.to_fb_bytes().expect("fb serialize");
        let deserialized = CompiledPlan::from_fb_bytes(&bytes).expect("fb deserialize");
        assert_eq!(compiled, deserialized);
    }

    #[test]
    fn test_fb_roundtrip_with_condition_and_mapping() {
        use crate::plan::{Edge, Node};
        use std::collections::HashMap;
        let mut plan = ExecutionPlan::new(
            vec![
                Node::new("decide", Opcode::Decide).with_arg("expr", "x > 5".into()),
                Node::new("act_yes", Opcode::Act).with_arg("type", "return".into()),
                Node::new("act_no", Opcode::Act).with_arg("type", "return".into()),
            ],
            vec![
                Edge::with_condition("decide", "act_yes", "true"),
                Edge::with_condition("decide", "act_no", "false"),
            ],
        );
        // Add mappings to second edge
        let mut map = HashMap::new();
        map.insert("x".into(), "result".into());
        plan.edges[1].mapping = Some(map);

        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let bytes = compiled.to_fb_bytes().expect("fb serialize");
        let deserialized = CompiledPlan::from_fb_bytes(&bytes).expect("fb deserialize");
        assert_eq!(compiled, deserialized);
    }

    #[test]
    fn test_fb_binary_is_smaller_than_json() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let json = serde_json::to_string(&plan).unwrap();
        let bytes = compiled.to_fb_bytes().expect("fb serialize");
        assert!(
            bytes.len() < json.len(),
            "FlatBuffers ({} bytes) should be smaller than JSON ({} bytes)",
            bytes.len(),
            json.len()
        );
    }

    #[test]
    fn test_fb_is_smaller_than_json() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let json = serde_json::to_string(&plan).unwrap();
        let bytes = compiled.to_fb_bytes().expect("serialize");
        assert!(
            bytes.len() < json.len(),
            "binary ({} bytes) should be smaller than JSON ({} bytes)",
            bytes.len(),
            json.len()
        );
    }

    #[test]
    fn test_id_map() {
        let plan = sample_plan();
        let compiled = CompiledPlan::from_execution_plan(&plan, vec![]);
        let id_map = compiled.id_map.as_ref().unwrap();
        // Find index for "calc1"
        let idx = id_map.iter().find(|(id, _)| id == "calc1").map(|(_, i)| *i);
        assert_eq!(idx, Some(1));
    }

    #[test]
    fn test_tool_dep() {
        let dep = ToolDep {
            name: "arac_sorgu".into(),
            version: "^1.0.0".into(),
            pure: true,
            schema_hash: "abc123".into(),
        };
        assert_eq!(dep.name, "arac_sorgu");
        assert_eq!(dep.version, "^1.0.0");
        assert_eq!(dep.schema_hash, "abc123");
    }
}
