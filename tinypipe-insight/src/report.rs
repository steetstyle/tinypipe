//! Profil bazlı rapor üretimi.
//!
//! Her profil `focus` listesindeki bölüm anahtarlarına göre raporlanır
//! (bkz. `crate::profile::focus`). Bilinmeyen anahtarlar sessizce atlanır —
//! kullanıcı profilleri `--config` ile serbest bölüm kombinasyonu seçebilir.
//!
//! Çıktı düz metindir (CLI'e hazır), sabit genişlik hizalı.

use tinypipe_api::types::Profile;

use crate::metrics::{GraphStats, PortfolioMetrics};
use crate::profile::focus;

/// Süreyi insan dostu gösterir: 950 µs → `950µs`, 12.4 ms → `12.4ms`, 3.1 s → `3.1s`.
pub fn format_duration(us: u64) -> String {
    if us < 1_000 {
        format!("{}µs", us)
    } else if us < 1_000_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    }
}

/// `last_event` değerini kısa okur (kısaltma gerektirmez — zaten kısadır).
fn format_last_event(ev: &Option<String>) -> &str {
    ev.as_deref().unwrap_or("—")
}

/// Profil focus'una göre tam rapor metnini üretir.
pub fn render(profile: &Profile, portfolio: &PortfolioMetrics) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "tinypipe report — {}\n{}\n\n",
        profile.label, profile.description
    ));

    for key in &profile.focus {
        match key.as_str() {
            focus::PORTFOLIO => section_portfolio(&mut out, portfolio),
            focus::EXECUTIONS => section_executions(&mut out, portfolio),
            focus::RELIABILITY => section_reliability(&mut out, portfolio),
            focus::DURATION => section_duration(&mut out, portfolio),
            focus::TOOLS => section_tools(&mut out, portfolio),
            focus::ENDPOINTS => section_endpoints(&mut out, portfolio),
            focus::ENV => section_env(&mut out, portfolio),
            focus::STRUCTURE => section_structure(&mut out, portfolio),
            focus::SUBGRAPHS => section_subgraphs(&mut out, portfolio),
            focus::CHURN => section_churn(&mut out, portfolio),
            _ => {}
        }
    }
    out
}

fn section_portfolio(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Portfolio\n");
    if p.graphs.is_empty() {
        out.push_str("(boş portföy)\n\n");
        return;
    }
    out.push_str(&format!(
        "{:<22} {:<3} {:<10} {:<22} {:<6} {:<6} {:<10} {:<10}\n",
        "graph", "ver", "status", "last_event", "exec", "fail", "avg", "p95"
    ));
    for g in &p.graphs {
        out.push_str(&format!(
            "{:<22} {:<3} {:<10} {:<22} {:<6} {:<6} {:<10} {:<10}\n",
            truncate(&g.name, 22),
            format!("v{}", g.version),
            truncate(&g.status, 10),
            format_last_event(&g.last_event),
            g.executions,
            g.failed,
            g.avg_duration_us.map(format_duration).unwrap_or_else(|| "—".into()),
            g.p95_duration_us.map(format_duration).unwrap_or_else(|| "—".into()),
        ));
    }
    out.push('\n');
}

fn section_executions(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Executions\n");
    let failed_rate = if p.total_executions > 0 {
        (p.total_failed as f64 / p.total_executions as f64 * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };
    out.push_str(&format!(
        "Toplam: {} · Başarısız: {} (%{:.1}) · Rollback: {} · Deploy: {}\n\n",
        p.total_executions, p.total_failed, failed_rate, p.rollback_count, p.deployed_count
    ));
}

fn section_reliability(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Reliability\n");
    let mut rows: Vec<(&GraphStats, f64)> = p
        .graphs
        .iter()
        .filter(|g| g.executions > 0)
        .map(|g| {
            let rate = g.failed as f64 / g.executions as f64 * 100.0;
            (g, (rate * 10.0).round() / 10.0)
        })
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if rows.is_empty() {
        out.push_str("(execution kaydı yok)\n\n");
        return;
    }
    for (g, rate) in rows {
        out.push_str(&format!(
            "{:<22} {:>6.1}% başarısız ({}/{})\n",
            truncate(&g.name, 22),
            rate,
            g.failed,
            g.executions
        ));
    }
    out.push('\n');
}

fn section_duration(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Duration\n");
    let mut rows: Vec<&GraphStats> = p.graphs.iter().filter(|g| g.p95_duration_us.is_some()).collect();
    rows.sort_by_key(|g| g.p95_duration_us.unwrap_or(0));
    if rows.is_empty() {
        out.push_str("(süre kaydı yok)\n\n");
        return;
    }
    for g in rows {
        out.push_str(&format!(
            "{:<22} p95 {:>10} · ort {:>10}  ({}, {} örnek)\n",
            truncate(&g.name, 22),
            format_duration(g.p95_duration_us.unwrap_or(0)),
            format_duration(g.avg_duration_us.unwrap_or(0)),
            "tüm versiyonlar",
            g.executions
        ));
    }
    out.push('\n');
}

fn section_tools(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Tools\n");
    let mut any = false;
    for g in &p.graphs {
        if g.tool_calls.is_empty() {
            continue;
        }
        any = true;
        let calls: Vec<String> = g
            .tool_calls
            .iter()
            .map(|(t, n)| format!("{} x{}", t, n))
            .collect();
        out.push_str(&format!("{} — {}\n", truncate(&g.name, 22), calls.join(", ")));
    }
    if !any {
        out.push_str("(tool çağrısı yok)\n");
    }
    out.push('\n');
}

fn section_endpoints(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## External Endpoints\n");
    let mut any = false;
    for g in &p.graphs {
        if g.http_endpoints.is_empty() {
            continue;
        }
        any = true;
        out.push_str(&format!(
            "{} — {}\n",
            truncate(&g.name, 22),
            g.http_endpoints.join(", ")
        ));
    }
    if !any {
        out.push_str("(dış HTTP çağrısı yok)\n");
    }
    out.push('\n');
}

fn section_env(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Env\n");
    if p.graphs.is_empty() {
        out.push_str("(boş portföy)\n\n");
        return;
    }
    let mut any = false;
    for g in &p.graphs {
        if g.env_deps.is_empty() && g.missing_env.is_empty() {
            continue;
        }
        any = true;
        let deps: Vec<String> = g
            .env_deps
            .iter()
            .map(|(k, opt)| {
                if *opt {
                    format!("{} (opt)", k)
                } else {
                    k.clone()
                }
            })
            .collect();
        out.push_str(&format!("{} — {}\n", truncate(&g.name, 22), deps.join(", ")));
        if !g.missing_env.is_empty() {
            out.push_str(&format!(
                "    ⚠ EKSİK: {}\n",
                g.missing_env.join(", ")
            ));
        }
    }
    if !any {
        out.push_str("(env bağımlılığı yok)\n");
    }
    out.push('\n');
}

fn section_structure(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Structure\n");
    let mut any = false;
    for g in &p.graphs {
        let (n, e) = match (g.node_count, g.edge_count) {
            (Some(n), Some(e)) => (n, e),
            _ => continue,
        };
        any = true;
        let sub = if g.subgraph_deps.is_empty() {
            String::new()
        } else {
            format!("  (subgraph: {})", g.subgraph_deps.join(", "))
        };
        out.push_str(&format!(
            "{:<22} {} node · {} edge{}\n",
            truncate(&g.name, 22),
            n,
            e,
            sub
        ));
    }
    if !any {
        out.push_str("(plan kaydı yok — grafları compile edin)\n");
    }
    out.push('\n');
}

fn section_subgraphs(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Subgraph Deps\n");
    let mut any = false;
    for g in &p.graphs {
        if g.subgraph_deps.is_empty() {
            continue;
        }
        any = true;
        out.push_str(&format!(
            "{:<22} → {}\n",
            truncate(&g.name, 22),
            g.subgraph_deps.join(", ")
        ));
    }
    if !any {
        out.push_str("(subgraph bağımlılığı yok)\n");
    }
    out.push('\n');
}

fn section_churn(out: &mut String, p: &PortfolioMetrics) {
    out.push_str("## Change Churn\n");
    let mut rows: Vec<&GraphStats> = p.graphs.iter().collect();
    rows.sort_by_key(|g| std::cmp::Reverse(g.version_count));
    for g in rows {
        out.push_str(&format!(
            "{:<22} {} versiyon · son olay: {}\n",
            truncate(&g.name, 22),
            g.version_count,
            format_last_event(&g.last_event)
        ));
    }
    out.push('\n');
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::collect;
    use tinypipe_api::storage::GraphStorage;
    use tinypipe_storage::SqliteStorage;

    fn profile(name: &str) -> Profile {
        crate::profile::builtin_profile(name).unwrap()
    }

    #[test]
    fn test_render_empty_portfolio() {
        let store = SqliteStorage::in_memory().unwrap();
        let m = collect(&store, None).unwrap();
        let text = render(&profile("ceo"), &m);
        assert!(text.contains("tinypipe report — CEO"));
        assert!(text.contains("Portfolio"));
        assert!(text.contains("(boş portföy)"));
    }

    #[test]
    fn test_render_ceo_shows_sections() {
        let store = SqliteStorage::in_memory().unwrap();
        let id = store.create_graph("demo", "def graph(): return 1").unwrap();
        store.deploy(&id, tinypipe_api::types::Version(1)).unwrap();

        let m = collect(&store, None).unwrap();
        let text = render(&profile("ceo"), &m);
        // CEO focus: portfolio, executions, reliability
        assert!(text.contains("## Portfolio"));
        assert!(text.contains("## Executions"));
        assert!(text.contains("## Reliability"));
        // CEO focus dışı bölümler yok:
        assert!(!text.contains("## Duration"));
        assert!(!text.contains("## Tools"));
    }

    #[test]
    fn test_render_devops_shows_env() {
        let store = SqliteStorage::in_memory().unwrap();
        let m = collect(&store, None).unwrap();
        let text = render(&profile("devops"), &m);
        assert!(text.contains("## Reliability"));
        assert!(text.contains("## Env"));
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(950), "950µs");
        assert_eq!(format_duration(12_400), "12.4ms");
        assert_eq!(format_duration(3_100_000), "3.1s");
    }
}
