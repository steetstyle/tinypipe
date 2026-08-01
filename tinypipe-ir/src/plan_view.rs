//! Semantik plan renderer'ları: profile/görünüm odaklı mermaid + dot.
//!
//! Ham `CompiledPlan` doğrudan çizilmez — önce bir `ViewGraph`'a dönüştürülür:
//!
//! ```text
//! CompiledPlan ── label ─▶ simplify ─▶ collapse ─▶ emit(mermaid|dot)
//! ```
//!
//! - **label**: her node'a semantik, insan-okur etiket + şekil (Loop/Decide →
//!   baklava, Return → stadium, Call → yuvarlatılmış).
//! - **simplify**: yalnızca sabit/identifier `expr`'li ve `output`'u olmayan
//!   CALC node'ları görünümden gizlenir (plan değişmez, kenarlar yeniden bağlanır).
//! - **collapse** (Summary görünümü): her GROUP tek node'a iner; grup arası
//!   kenarlar erişilebilirlikle hesaplanır.
//!
//! Grup başlıkları `CompiledPlan.groups`'tan gelir (DSL'deki `with GROUP(...)`
//! bloklarından derlenir) — hiçbir başlık tahmin edilmez.

use crate::compiled::{CompiledNode, CompiledPlan};
use crate::plan::{EdgeKind, Opcode};

// ─── Görünüm seçenekleri ───────────────────────────────────────────

/// Detay seviyesi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLevel {
    /// Tüm node'lar + GROUP subgraph'ları + şekiller + control/data ayrımı.
    Full,
    /// Her GROUP tek node'a collapse edilir (üst düzey özet).
    Summary,
    /// Full + GROUP bazlı classDef renkleri (sorumluluk alanı vurgusu).
    Layers,
}

impl ViewLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(ViewLevel::Full),
            "summary" => Some(ViewLevel::Summary),
            "layers" => Some(ViewLevel::Layers),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ViewLevel::Full => "full",
            ViewLevel::Summary => "summary",
            ViewLevel::Layers => "layers",
        }
    }
}

/// Akış yönü.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Top-down (süreç odaklı).
    Td,
    /// Left-right (özet / genel bakış).
    Lr,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "td" => Some(Direction::Td),
            "lr" => Some(Direction::Lr),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Td => "td",
            Direction::Lr => "lr",
        }
    }
}

/// Profil/görünüm seçimleri — CLI `plan --profile/--view/--direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub view: ViewLevel,
    pub direction: Direction,
    /// GROUP başlıklarına otomatik `N. ` önek ekle (başlık rakamla başlamıyorsa).
    pub numbered_groups: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            view: ViewLevel::Full,
            direction: Direction::Td,
            numbered_groups: true,
        }
    }
}

// ─── Semantik model ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Default,
    Rounded,
    Diamond,
    Stadium,
}

#[derive(Clone)]
struct ViewNode {
    index: u32,
    label: String,
    shape: Shape,
    group_id: Option<u32>,
}

#[derive(Clone)]
struct ViewEdge {
    from: u32,
    to: u32,
    label: Option<String>,
    control: bool,
}

struct ViewGraph {
    nodes: Vec<ViewNode>,
    edges: Vec<ViewEdge>,
    groups: Vec<String>,
}

// ─── Public API ────────────────────────────────────────────────────

pub fn render_mermaid(plan: &CompiledPlan, options: RenderOptions) -> String {
    let view = build_view(plan, options.view);
    emit_mermaid(&view, options)
}

pub fn render_dot(plan: &CompiledPlan, options: RenderOptions) -> String {
    let view = build_view(plan, options.view);
    emit_dot(&view, options)
}

// ─── ViewGraph kurulumu ────────────────────────────────────────────

fn build_view(plan: &CompiledPlan, level: ViewLevel) -> ViewGraph {
    let hidden = hidden_nodes(plan);
    let view = project(plan, &hidden);

    match level {
        ViewLevel::Summary => collapse(&view),
        _ => view,
    }
}

/// Görünümden gizlenecek node'lar: sabit/identifier `expr`'li ve `output`
/// arg'ı olmayan CALC node'ları (veri taşımayan, sadece feed amaçlı).
fn hidden_nodes(plan: &CompiledPlan) -> std::collections::HashSet<u32> {
    let mut hidden = std::collections::HashSet::new();
    for n in &plan.nodes {
        if n.op != Opcode::Calc {
            continue;
        }
        let has_output = n.args.iter().any(|a| a.key == "output");
        if has_output {
            continue;
        }
        let raw = n
            .args
            .iter()
            .find(|a| a.key == "expr")
            .map(|a| a.value.as_str())
            .unwrap_or("");
        let expr = trim_quotes(raw);
        if is_simple_value(raw, &expr) {
            hidden.insert(n.index);
        }
    }
    hidden
}

/// Sabit literal veya basit identifier mı? (passthrough feed node'ları)
/// `raw` tırnaklı ham değer, `trimmed` sadeleştirilmiş hali.
fn is_simple_value(raw: &str, trimmed: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    // Tırnaklı literal — içeriği ne olursa olsun sabit.
    if raw.starts_with('"') || raw.starts_with('\'') {
        return true;
    }
    let t = trimmed.trim();
    if t.parse::<i64>().is_ok() || t.parse::<f64>().is_ok() {
        return true;
    }
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        }
        _ => false,
    }
}

/// Gizli node'ları atlayıp kenarları yeniden bağlar.
fn project(plan: &CompiledPlan, hidden: &std::collections::HashSet<u32>) -> ViewGraph {
    let mut nodes: Vec<ViewNode> = Vec::new();
    for n in &plan.nodes {
        if hidden.contains(&n.index) {
            continue;
        }
        let (label, shape) = semantic_label(n);
        nodes.push(ViewNode {
            index: n.index,
            label,
            shape,
            group_id: n.group_id,
        });
    }

    // Görünür kök: gizli node'ların in-edge'lerinden ilk görünür kaynak.
    fn visible_roots(
        plan: &CompiledPlan,
        hidden: &std::collections::HashSet<u32>,
        idx: u32,
    ) -> Vec<u32> {
        if !hidden.contains(&idx) {
            return vec![idx];
        }
        let mut roots = Vec::new();
        for e in &plan.edges {
            if e.to_index == idx {
                for r in visible_roots(plan, hidden, e.from_index) {
                    if !roots.contains(&r) {
                        roots.push(r);
                    }
                }
            }
        }
        roots
    }

    // Görünür uç: gizli node'ların out-edge'lerinden ilk görünür hedef.
    fn visible_leaves(
        plan: &CompiledPlan,
        hidden: &std::collections::HashSet<u32>,
        idx: u32,
    ) -> Vec<u32> {
        if !hidden.contains(&idx) {
            return vec![idx];
        }
        let mut leaves = Vec::new();
        for e in &plan.edges {
            if e.from_index == idx {
                for l in visible_leaves(plan, hidden, e.to_index) {
                    if !leaves.contains(&l) {
                        leaves.push(l);
                    }
                }
            }
        }
        leaves
    }

    let mut edges: Vec<ViewEdge> = Vec::new();
    // A→B data edge'i varsa control edge'i atla (kontrol sıralaması data
    // bağımlılığıyla zaten garanti edilir — tekrar çizim gürültüsü).
    let mut seen_any: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    for e in &plan.edges {
        let control = e.kind == EdgeKind::Control;
        for from in visible_roots(plan, hidden, e.from_index) {
            for to in visible_leaves(plan, hidden, e.to_index) {
                if from == to {
                    continue;
                }
                if control && seen_any.contains(&(from, to)) {
                    continue;
                }
                if !seen_any.insert((from, to)) {
                    continue;
                }
                let label = e
                    .condition
                    .clone()
                    .or_else(|| e.label.clone())
                    .map(|s| trim_quotes(&s))
                    .filter(|s| !s.is_empty());
                edges.push(ViewEdge {
                    from,
                    to,
                    label,
                    control,
                });
            }
        }
    }

    ViewGraph {
        nodes,
        edges,
        groups: plan.groups.clone(),
    }
}

/// Summary görünümü: her GROUP tek node'a collapse edilir; kenarlar
/// grup erişilebilirliğiyle yeniden hesaplanır.
fn collapse(view: &ViewGraph) -> ViewGraph {
    if view.groups.is_empty() {
        return ViewGraph {
            nodes: view.nodes.clone(),
            edges: view.edges.clone(),
            groups: Vec::new(),
        };
    }

    // Grup üyeleri (node index'ler)
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); view.groups.len()];
    for n in &view.nodes {
        if let Some(g) = n.group_id {
            if (g as usize) < members.len() {
                members[g as usize].push(n.index);
            }
        }
    }

    // Grup node'u için özet etiket: `2. Comments Loop (4 steps)`.
    let mut collapsed: Vec<ViewNode> = Vec::new();
    let mut group_of: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for (g, title) in view.groups.iter().enumerate() {
        let mut label = numbered_title(title, g + 1, true);
        let count = members[g].len();
        if count > 0 {
            let noun = if count == 1 { "step" } else { "steps" };
            label = format!("{} ({} {})", label, count, noun);
        }
        collapsed.push(ViewNode {
            index: u32::MAX - g as u32,
            label,
            shape: Shape::Rounded,
            group_id: None,
        });
        for m in &members[g] {
            group_of.insert(*m, g as u32);
        }
    }
    // Grupsuz node'lar kendi şekilleriyle kalır.
    for n in &view.nodes {
        if n.group_id.is_none() {
            collapsed.push(ViewNode {
                index: n.index,
                label: n.label.clone(),
                shape: n.shape,
                group_id: None,
            });
        }
    }

    // Grup erişilebilirlik kenarları: A grubunda herhangi bir node → B grubuna
    // (veya grupsuz node'a) kenar varsa A → B.
    let node_group = |idx: u32| -> Option<u32> {
        let n = view.nodes.iter().find(|n| n.index == idx)?;
        n.group_id
    };
    let mut edges: Vec<ViewEdge> = Vec::new();
    let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    for e in &view.edges {
        let from_key = match node_group(e.from) {
            Some(g) => u32::MAX - g,
            None => e.from,
        };
        let to_key = match node_group(e.to) {
            Some(g) => u32::MAX - g,
            None => e.to,
        };
        if from_key == to_key {
            continue;
        }
        if seen.insert((from_key, to_key)) {
            edges.push(ViewEdge {
                from: from_key,
                to: to_key,
                label: e.label.clone(),
                control: e.control,
            });
        }
    }

    ViewGraph {
        nodes: collapsed,
        edges,
        groups: Vec::new(),
    }
}

// ─── Semantik etiketleme ───────────────────────────────────────────

fn trim_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '\\')
        .to_string()
}

fn arg<'a>(n: &'a CompiledNode, key: &str) -> Option<String> {
    n.args
        .iter()
        .find(|a| a.key == key)
        .map(|a| trim_quotes(&a.value))
}

fn snake_to_title(s: &str) -> String {
    let mut out = String::new();
    for (i, part) in s.split('_').enumerate() {
        if part.is_empty() {
            continue;
        }
        if i > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_uppercase().next().unwrap_or(first));
            out.push_str(chars.as_str());
        }
    }
    out
}

/// `Seed Posts (user_id)` tarzı semantik etiket + şekil.
fn semantic_label(n: &CompiledNode) -> (String, Shape) {
    let shape = match n.op {
        Opcode::Loop | Opcode::Decide | Opcode::Switch => Shape::Diamond,
        Opcode::Act => {
            let t = arg(n, "type").unwrap_or_default();
            if t == "return" {
                Shape::Stadium
            } else {
                Shape::Default
            }
        }
        Opcode::Call => Shape::Rounded,
        Opcode::Parallel | Opcode::Merge | Opcode::Wait => Shape::Diamond,
        _ => Shape::Default,
    };

    let label = match n.op {
        Opcode::Input => format!("Input: {}", arg(n, "name").unwrap_or_default()),
        Opcode::Call => call_label(n),
        Opcode::Loop => {
            let target = arg(n, "target").unwrap_or_default();
            let max = arg(n, "max_iterations").unwrap_or_default();
            if target.is_empty() {
                format!("Loop (max {})", max)
            } else {
                format!("Loop: {} (max {})", target, max)
            }
        }
        Opcode::Decide => {
            let cond = arg(n, "condition")
                .or_else(|| arg(n, "expr"))
                .unwrap_or_default();
            if !cond.is_empty() {
                format!("{} ?", cond)
            } else {
                let src = arg(n, "source").unwrap_or_default();
                let op = arg(n, "op").unwrap_or_default();
                let val = arg(n, "value").unwrap_or_default();
                if !src.is_empty() {
                    format!("{} {} {} ?", src, op, val)
                } else {
                    "Decide".into()
                }
            }
        }
        Opcode::Switch => format!("Switch on {}", arg(n, "source").unwrap_or_default()),
        Opcode::Act => {
            let t = arg(n, "type").unwrap_or_default();
            if t == "return" {
                "Return".into()
            } else {
                format!("Act {}", t)
            }
        }
        Opcode::Parallel => "Parallel".into(),
        Opcode::Merge => "Merge".into(),
        Opcode::Wait => format!("Wait {}", arg(n, "duration").unwrap_or_default()),
        Opcode::Error => "Error".into(),
        Opcode::Calc => {
            let out = arg(n, "output");
            let expr = arg(n, "expr").unwrap_or_default();
            match (out, expr) {
                (Some(o), e) if !e.is_empty() => format!("{} = {}", o, shorten_expr(&e)),
                (None, e) if !e.is_empty() => shorten_expr(&e),
                (Some(o), _) => o,
                _ => "Calc".into(),
            }
        }
    };
    (label, shape)
}

/// Uzun ifadeleri kısalt: dict literali → `Summary (N fields)` özeti.
fn shorten_expr(expr: &str) -> String {
    let t = expr.trim();
    if t.starts_with('{') && t.ends_with('}') {
        let inner = &t[1..t.len() - 1];
        let fields: Vec<&str> = inner
            .split(',')
            .map(|f| f.trim())
            .filter(|f| !f.is_empty())
            .collect();
        if fields.len() > 1 {
            let names: Vec<&str> = fields
                .iter()
                .map(|f| {
                    f.split(':')
                        .next()
                        .unwrap_or(f)
                        .trim()
                        .trim_matches('\'')
                        .trim_matches('"')
                })
                .collect();
            return format!("Summary ({})", names.join(", "));
        }
    }
    if t.len() > 48 {
        return format!("{}…", &t[..48]);
    }
    t.to_string()
}

/// Call node etiketi: `subgraph:seed_posts` → `Seed Posts (user_id)`.
fn call_label(n: &CompiledNode) -> String {
    let target = arg(n, "target").unwrap_or_default();
    let kw_args: Vec<String> = n
        .args
        .iter()
        .filter(|a| a.key != "target" && a.key != "type" && a.key != "output")
        .map(|a| a.key.clone())
        .collect();

    let base = if let Some(sub) = target.strip_prefix("subgraph:") {
        humanize_subgraph(sub)
    } else if target == "http_request" {
        let url = arg(n, "url").unwrap_or_default();
        if let Some(host) = url_host(&url) {
            format!("HTTP: {}", host)
        } else {
            "HTTP Request".into()
        }
    } else if target.is_empty() {
        "Call".into()
    } else {
        snake_to_title(&target)
    };

    if kw_args.is_empty() {
        base
    } else {
        format!("{} ({})", base, kw_args.join(", "))
    }
}

/// `seed_posts` → `Seed Posts`; `fetch_users` → `Fetch Users`.
fn humanize_subgraph(sub: &str) -> String {
    snake_to_title(sub)
}

/// URL'den `host[:port]` çıkarır (external endpoint envanteri için ortak kural).
pub fn url_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.split('@').last()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

// ─── Emisyon ───────────────────────────────────────────────────────

fn mermaid_shape(node: &ViewNode) -> &'static str {
    match node.shape {
        Shape::Rounded => "(",
        Shape::Diamond => "{",
        Shape::Stadium => "([",
        _ => "[",
    }
}

fn mermaid_shape_close(node: &ViewNode) -> &'static str {
    match node.shape {
        Shape::Rounded => ")",
        Shape::Diamond => "}",
        Shape::Stadium => "])",
        _ => "]",
    }
}

fn mermaid_escape(s: &str) -> String {
    s.replace('#', "&#35;").replace('"', "'")
}

fn emit_mermaid(view: &ViewGraph, options: RenderOptions) -> String {
    let mut out = String::new();
    out.push_str("```mermaid\n");
    out.push_str(match options.direction {
        Direction::Td => "flowchart TD\n",
        Direction::Lr => "flowchart LR\n",
    });

    // Layers görünümünde grup bazlı classDef renkleri.
    if options.view == ViewLevel::Layers && !view.groups.is_empty() {
        let palette = ["#e1f5fe", "#f3e5f5", "#fff3e0", "#e8f5e9", "#fce4ec", "#e0f7fa"];
        let strokes = ["#0288d1", "#7b1fa2", "#f57c00", "#388e3c", "#c62828", "#00838f"];
        for i in 0..view.groups.len() {
            let c = palette[i % palette.len()];
            let s = strokes[i % strokes.len()];
            out.push_str(&format!(
                "    classDef group{} fill:{},stroke:{},stroke-width:2px;\n",
                i, c, s
            ));
        }
    }

    // GROUP subgraph blokları (Full/Layers görünümlerinde).
    if options.view != ViewLevel::Summary && !view.groups.is_empty() {
        let mut members: Vec<Vec<u32>> = vec![Vec::new(); view.groups.len()];
        for n in &view.nodes {
            if let Some(g) = n.group_id {
                if (g as usize) < members.len() {
                    members[g as usize].push(n.index);
                }
            }
        }
        for (g, title) in view.groups.iter().enumerate() {
            let numbered = numbered_title(title, g + 1, options.numbered_groups);
            out.push_str(&format!(
                "    subgraph G{}[\"{}\"]\n",
                g,
                mermaid_escape(&numbered)
            ));
            if options.direction == Direction::Td {
                out.push_str("        direction TB\n");
            }
            for m in &members[g] {
                let node = view.nodes.iter().find(|n| n.index == *m).unwrap();
                let li = nodes_index(view, node.index);
                out.push_str(&format!(
                    "        N{}{}\"{}\"{}\n",
                    node.index,
                    mermaid_shape(node),
                    labels(&view.nodes, li),
                    mermaid_shape_close(node)
                ));
            }
            out.push_str("    end\n");
        }
        // Grupsuz node'lar ana akışta.
        for n in &view.nodes {
            if n.group_id.is_some() {
                continue;
            }
            let li = nodes_index(view, n.index);
            out.push_str(&format!(
                "    N{}{}\"{}\"{}\n",
                n.index,
                mermaid_shape(n),
                labels(&view.nodes, li),
                mermaid_shape_close(n)
            ));
        }
        if options.view == ViewLevel::Layers {
            for n in &view.nodes {
                if let Some(g) = n.group_id {
                    out.push_str(&format!("    class N{} group{};\n", n.index, g));
                }
            }
        }
    } else {
        // Summary: düz node'lar.
        for n in &view.nodes {
            let li = nodes_index(view, n.index);
            out.push_str(&format!(
                "    N{}{}\"{}\"{}\n",
                n.index,
                mermaid_shape(n),
                labels(&view.nodes, li),
                mermaid_shape_close(n)
            ));
        }
    }

    for e in &view.edges {
        let arrow = if e.control { "-.->" } else { "-->" };
        match &e.label {
            Some(l) if !l.is_empty() => out.push_str(&format!(
                "    N{} {}|{}| N{}\n",
                e.from,
                arrow,
                mermaid_escape(l),
                e.to
            )),
            _ => out.push_str(&format!("    N{} {} N{}\n", e.from, arrow, e.to)),
        }
    }

    out.push_str("```\n");
    out
}

fn labels(nodes: &[ViewNode], index: usize) -> String {
    nodes.get(index).map(|n| mermaid_escape(&n.label)).unwrap_or_default()
}

fn nodes_index(view: &ViewGraph, index: u32) -> usize {
    view.nodes.iter().position(|n| n.index == index).unwrap_or(0)
}

fn numbered_title(title: &str, ordinal: usize, numbered: bool) -> String {
    if !numbered {
        return title.to_string();
    }
    let trimmed = title.trim();
    if trimmed.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        title.to_string()
    } else {
        format!("{}. {}", ordinal, title)
    }
}

fn emit_dot(view: &ViewGraph, options: RenderOptions) -> String {
    let _ = options;
    let mut out = String::new();
    out.push_str("digraph plan {\n");
    for n in &view.nodes {
        let label = n.label.replace('"', "'");
        out.push_str(&format!("    N{} [label=\"{}\"];\n", n.index, label));
    }
    for e in &view.edges {
        let style = if e.control { "style=dashed" } else { "style=solid" };
        match &e.label {
            Some(l) if !l.is_empty() => out.push_str(&format!(
                "    N{} -> N{} [label=\"{}\", {}];\n",
                e.from,
                e.to,
                l.replace('"', "'"),
                style
            )),
            _ => out.push_str(&format!(
                "    N{} -> N{} [{}];\n",
                e.from, e.to, style
            )),
        }
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::{CompiledArg, CompiledEdge, CompiledMetadata};

    fn arg(k: &str, v: &str) -> CompiledArg {
        CompiledArg {
            key: k.into(),
            value: v.into(),
        }
    }

    fn node(index: u32, op: Opcode, group: Option<u32>) -> CompiledNode {
        CompiledNode {
            index,
            id: format!("n{}", index),
            op,
            args: Vec::new(),
            inferred_type: None,
            branch_id: None,
            group_id: group,
        }
    }

    fn edge(from: u32, to: u32) -> CompiledEdge {
        CompiledEdge {
            from_index: from,
            to_index: to,
            condition: None,
            mapping: None,
            priority: None,
            label: None,
            kind: EdgeKind::Data,
        }
    }

    /// dashboard_seeds'e benzeyen minik bir plan: input → 2 subgraph call →
    /// loop → summary → return, GROUP'lu. Node 2 gizli passthrough feed.
    fn sample_plan() -> CompiledPlan {
        let mut n1 = node(1, Opcode::Call, Some(0));
        n1.args = vec![arg("target", "\"subgraph:seed_users\""), arg("output", "\"users\"")];
        let mut n2 = node(2, Opcode::Calc, Some(0));
        n2.args = vec![arg("expr", "\"users\"")];
        let mut n3 = node(3, Opcode::Call, Some(0));
        n3.args = vec![
            arg("target", "\"subgraph:seed_posts\""),
            arg("user_id", "\"user_id\""),
            arg("output", "\"posts\""),
        ];
        let mut n4 = node(4, Opcode::Loop, Some(1));
        n4.args = vec![arg("target", "\"i\""), arg("max_iterations", "10")];
        let mut n5 = node(5, Opcode::Call, Some(1));
        n5.args = vec![
            arg("target", "\"list.get\""),
            arg("array", "\"posts.items\""),
            arg("index", "\"i\""),
            arg("output", "\"p\""),
        ];
        let mut n6 = node(6, Opcode::Calc, Some(1));
        n6.args = vec![
            arg("expr", "\"total_comments + 1\""),
            arg("output", "\"total_comments\""),
        ];
        let mut n7 = node(7, Opcode::Calc, None);
        n7.args = vec![arg("expr", "{\"users\": users, \"posts\": posts}")];
        let mut n8 = node(8, Opcode::Act, None);
        n8.args = vec![arg("type", "\"return\"")];

        CompiledPlan {
            version: 4,
            nodes: vec![
                node(0, Opcode::Input, None),
                n1,
                n2,
                n3,
                n4,
                n5,
                n6,
                n7,
                n8,
            ],
            edges: vec![
                // input → (gizli feed) → seed_users
                edge(0, 2),
                edge(2, 1),
                edge(1, 3),
                edge(3, 4),
                edge(4, 5),
                edge(5, 6),
                edge(6, 4), // loop back
                edge(1, 7),
                edge(3, 7),
                edge(6, 7),
                edge(7, 8),
            ],
            metadata: CompiledMetadata::default(),
            id_map: None,
            groups: vec!["Seeding".into(), "Comments Loop".into()],
        }
    }

    #[test]
    fn test_semantic_labels() {
        let plan = sample_plan();
        let (label, _) = semantic_label(&plan.nodes[3]);
        assert_eq!(label, "Seed Posts (user_id)");
        let (label, _) = semantic_label(&plan.nodes[4]);
        assert_eq!(label, "Loop: i (max 10)");
        let (label, shape) = semantic_label(&plan.nodes[8]);
        assert_eq!(label, "Return");
        assert!(matches!(shape, Shape::Stadium));
        let (label, _) = semantic_label(&plan.nodes[7]);
        assert_eq!(label, "Summary (users, posts)");
    }

    #[test]
    fn test_hidden_nodes_skips_const_calcs() {
        let plan = sample_plan();
        let hidden = hidden_nodes(&plan);
        assert!(hidden.contains(&2), "const feed calc must be hidden");
        assert!(!hidden.contains(&6), "output calc must stay");
    }

    #[test]
    fn test_mermaid_full_has_subgraphs() {
        let out = render_mermaid(&sample_plan(), RenderOptions::default());
        assert!(out.contains("flowchart TD"));
        assert!(out.contains("subgraph G0[\"1. Seeding\"]"));
        assert!(out.contains("subgraph G1[\"2. Comments Loop\"]"));
        assert!(out.contains("Seed Posts (user_id)"));
        assert!(out.contains("Loop: i (max 10)"));
        // Gizli feed node yok
        assert!(!out.contains("N2["));
    }

    #[test]
    fn test_mermaid_summary_collapses() {
        let out = render_mermaid(
            &sample_plan(),
            RenderOptions {
                view: ViewLevel::Summary,
                direction: Direction::Lr,
                numbered_groups: true,
            },
        );
        assert!(out.contains("flowchart LR"));
        assert!(!out.contains("subgraph"), "summary must not have subgraphs");
        assert!(out.contains("1. Seeding (2 steps)"));
        assert!(out.contains("2. Comments Loop (3 steps)"));
        assert!(!out.contains("total_comments = total_comments + 1"));
        assert!(out.contains("Summary (users, posts)"));
        assert!(out.contains("Return"));
    }

    #[test]
    fn test_mermaid_layers_has_classdef() {
        let out = render_mermaid(
            &sample_plan(),
            RenderOptions {
                view: ViewLevel::Layers,
                ..Default::default()
            },
        );
        assert!(out.contains("classDef group0"));
        assert!(out.contains("class N1 group0;"));
    }

    #[test]
    fn test_dot_renders() {
        let out = render_dot(&sample_plan(), RenderOptions::default());
        assert!(out.contains("digraph plan {"));
        assert!(out.contains("N4 [label=\"Loop: i (max 10)\"];"));
        assert!(!out.contains("N2 ["));
    }

    #[test]
    fn test_url_host() {
        assert_eq!(
            url_host("https://api.example.com/v1/users"),
            Some("api.example.com".into())
        );
        assert_eq!(
            url_host("http://localhost:8080/ping"),
            Some("localhost:8080".into())
        );
        assert_eq!(url_host("not-a-url"), None);
    }

    #[test]
    fn test_view_level_parse() {
        assert_eq!(ViewLevel::parse("summary"), Some(ViewLevel::Summary));
        assert_eq!(ViewLevel::parse("layers"), Some(ViewLevel::Layers));
        assert_eq!(ViewLevel::parse("xml"), None);
        assert_eq!(Direction::parse("lr"), Some(Direction::Lr));
        assert_eq!(Direction::parse("td"), Some(Direction::Td));
        assert_eq!(Direction::parse("x"), None);
    }
}
