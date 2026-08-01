//! Env bağımlılığı taraması — saf IR fonksiyonları.
//!
//! Bir grafiğin `env.get` / `env.template` çağrılarından ihtiyaç duyduğu
//! ortam değişkenlerini çıkarır. `SubgraphToolRegistry::validate_env` ile
//! kök grafik + tüm transitive subgraph'ları BFS ile gezilir ve eksik
//! değişkenler **execution'dan önce** raporlanır (mid-execution hatası yerine).
//!
//! Kurallar:
//! - `env.get(key="X")` — literal key → bağımlılık; `default` kwarg'ı varsa opsiyonel.
//! - `env.get(key=degisken)` — dinamik key → statik bilinemez, atlanır (runtime hata verir).
//! - `env.template(value="...${X}...")` — `${X}` ve `{{X}}` literal key → bağımlılık;
//!   `${X:-default}` opsiyonel, düz `${X}` zorunlu sayılır (boş config = hata olurdu).
//!
//! Bu modül tinypipe-tools'a bağımlı değildir — insight/raporlama katmanları
//! ağır tool bağımlılıkları çekmeden aynı taramayı kullanabilir.

use crate::compiled::{CompiledNode, CompiledPlan};
use crate::plan::Opcode;

/// Tek bir ortam değişkeni bağımlılığı.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvDep {
    pub key: String,
    /// `env.get`'te `default` kwarg'ı veya template'te `:-default` varsa `true`.
    pub optional: bool,
}

/// Eksik değişken raporu — hangi grafik zincirinde eksik.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvDepReport {
    /// Örn: `dashboard_seeds → seed_posts`
    pub graph_path: String,
    /// Zorunlu ama ortamda bulunmayan anahtarlar.
    pub missing: Vec<String>,
}

/// Compiled plan'daki tüm env bağımlılıklarını çıkarır.
pub fn scan_plan_env_deps(plan: &CompiledPlan) -> Vec<EnvDep> {
    let mut deps = Vec::new();
    for node in &plan.nodes {
        if node.op != Opcode::Call {
            continue;
        }
        let Some(target) = node
            .args
            .iter()
            .find(|a| a.key == "target")
            .and_then(|a| json_string_literal(&a.value))
        else {
            continue;
        };
        match target.as_str() {
            "env.get" => collect_env_get_deps(node, &mut deps),
            "env.template" => collect_env_template_deps(node, &mut deps),
            _ => {}
        }
    }
    deps
}

/// Compiled plan'daki subgraph çağrı hedefleri (`subgraph:<name>`).
pub fn subgraph_targets(plan: &CompiledPlan) -> Vec<String> {
    let mut targets = Vec::new();
    for node in &plan.nodes {
        if node.op != Opcode::Call {
            continue;
        }
        let Some(target) = node
            .args
            .iter()
            .find(|a| a.key == "target")
            .and_then(|a| json_string_literal(&a.value))
        else {
            continue;
        };
        if let Some(name) = target.strip_prefix("subgraph:") {
            targets.push(name.to_string());
        }
    }
    targets
}

fn collect_env_get_deps(node: &CompiledNode, deps: &mut Vec<EnvDep>) {
    let Some(key_arg) = node.args.iter().find(|a| a.key == "key") else {
        return;
    };
    // Yalnızca literal string key'ler statik bilinir; dinamikler atlanır.
    let Some(key) = json_string_literal(&key_arg.value) else {
        return;
    };
    let optional = node.args.iter().any(|a| a.key == "default");
    deps.push(EnvDep { key, optional });
}

fn collect_env_template_deps(node: &CompiledNode, deps: &mut Vec<EnvDep>) {
    let Some(value) = node
        .args
        .iter()
        .find(|a| a.key == "value")
        .and_then(|a| json_string_literal(&a.value))
    else {
        return;
    };
    deps.extend(extract_template_keys(&value));
}

/// `"..."` JSON literal'ından string değer çıkarır; literal değilse `None`.
fn json_string_literal(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

/// `${KEY}`, `${KEY:-default}`, `${KEY:?}`, `{{KEY}}` placeholder'larını çıkarır.
pub fn extract_template_keys(s: &str) -> Vec<EnvDep> {
    let mut deps = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let double = chars[i] == '{' && chars.get(i + 1) == Some(&'{');
        let dollar = chars[i] == '$' && chars.get(i + 1) == Some(&'{');
        let start = if double { i + 2 } else if dollar { i + 2 } else { i + 1 };
        if !(double || dollar) {
            i += 1;
            continue;
        }
        // Kapanış `}` (çift için `}}`) bul
        let close = if double {
            find_seq(&chars, start, '}', '}')
        } else {
            find_seq(&chars, start, '}', '\0')
        };
        let Some(end) = close else {
            i += 1;
            continue;
        };
        let spec: String = chars[start..end].iter().collect();
        if let Some(key) = spec.strip_suffix(":?") {
            deps.push(EnvDep { key: key.to_string(), optional: false });
        } else if let Some((key, _)) = spec.split_once(":-") {
            deps.push(EnvDep { key: key.to_string(), optional: true });
        } else {
            deps.push(EnvDep { key: spec, optional: false });
        }
        i = if double { end + 2 } else { end + 1 };
    }
    deps
}

fn find_seq(chars: &[char], start: usize, c: char, next: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == c && (next == '\0' || chars.get(i + 1) == Some(&next)) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::{CompiledArg, CompiledNode, CompiledPlan};

    fn arg(k: &str, v: &str) -> CompiledArg {
        CompiledArg {
            key: k.into(),
            value: v.into(),
        }
    }

    fn call_node(index: u32, target: &str, extra: &[(&str, &str)]) -> CompiledNode {
        let mut args = vec![arg("target", target)];
        for (k, v) in extra {
            args.push(arg(k, v));
        }
        CompiledNode {
            index,
            id: format!("n{}", index),
            op: Opcode::Call,
            args,
            inferred_type: None,
            branch_id: None,
            group_id: None,
        }
    }

    fn plan_with(nodes: Vec<CompiledNode>) -> CompiledPlan {
        CompiledPlan {
            version: 4,
            nodes,
            edges: Vec::new(),
            metadata: crate::compiled::CompiledMetadata::default(),
            id_map: None,
            groups: Vec::new(),
        }
    }

    #[test]
    fn test_env_get_literal_and_default() {
        let plan = plan_with(vec![
            call_node(0, "\"env.get\"", &[("key", "\"DB_URL\"")]),
            call_node(
                1,
                "\"env.get\"",
                &[("key", "\"API_TOKEN\""), ("default", "\"none\"")],
            ),
        ]);
        let deps = scan_plan_env_deps(&plan);
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&EnvDep { key: "DB_URL".into(), optional: false }));
        assert!(deps.contains(&EnvDep { key: "API_TOKEN".into(), optional: true }));
    }

    #[test]
    fn test_env_get_dynamic_key_skipped() {
        let plan = plan_with(vec![call_node(0, "\"env.get\"", &[("key", "k")])]);
        assert!(scan_plan_env_deps(&plan).is_empty());
    }

    #[test]
    fn test_env_template_deps() {
        let plan = plan_with(vec![call_node(
            0,
            "\"env.template\"",
            &[("value", "\"postgres://${HOST}:${PORT:-5432}/db\"")],
        )]);
        let deps = scan_plan_env_deps(&plan);
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&EnvDep { key: "HOST".into(), optional: false }));
        assert!(deps.contains(&EnvDep { key: "PORT".into(), optional: true }));
    }

    #[test]
    fn test_subgraph_targets_collected() {
        let plan = plan_with(vec![
            call_node(0, "\"subgraph:child1\"", &[]),
            call_node(1, "\"subgraph:child2\"", &[]),
            call_node(2, "\"math.add\"", &[]),
        ]);
        let mut targets = subgraph_targets(&plan);
        targets.sort();
        assert_eq!(targets, vec!["child1".to_string(), "child2".to_string()]);
    }

    #[test]
    fn test_extract_template_keys_unit() {
        let deps = extract_template_keys("a${X}b{{Y:-d}}c${Z:?}d");
        assert_eq!(deps.len(), 3);
        assert!(deps.contains(&EnvDep { key: "X".into(), optional: false }));
        assert!(deps.contains(&EnvDep { key: "Y".into(), optional: true }));
        assert!(deps.contains(&EnvDep { key: "Z".into(), optional: false }));
    }
}
