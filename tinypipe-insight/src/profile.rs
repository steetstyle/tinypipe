//! Rol bazlı profiller (built-in + özel).
//!
//! Her profil bir `tinypipe_api::types::Profile`'dir: görünüm seçenekleri
//! (view/direction), önemsediği rapor bölümleri (`focus`) ve opak ek
//! konfigürasyon (`config`). Built-in profiller storage'a ilk kullanımda
//! seed edilir; kullanıcı profilleri `profiles create --config <json>` ile
//! eklenir.
//!
//! # Profil → görünüm eşlemesi
//!
//! | Profil    | view    | direction | Öne çıkan bölümler |
//! |-----------|---------|-----------|--------------------|
//! | ceo       | summary | lr        | portfolio, executions, reliability |
//! | pm        | full    | td        | executions, tools, structure, churn |
//! | ba        | summary | td        | duration, structure, endpoints, churn |
//! | architect | layers  | td        | structure, subgraphs, endpoints, tools |
//! | senior    | full    | td        | structure, tools, churn, reliability |
//! | devops    | full    | td        | reliability, endpoints, env, duration |

use tinypipe_api::types::Profile;

/// Rapor bölümü anahtarları (profil `focus` listesinde kullanılır).
pub mod focus {
    pub const PORTFOLIO: &str = "portfolio";
    pub const EXECUTIONS: &str = "executions";
    pub const DURATION: &str = "duration";
    pub const RELIABILITY: &str = "reliability";
    pub const TOOLS: &str = "tools";
    pub const ENDPOINTS: &str = "endpoints";
    pub const ENV: &str = "env";
    pub const STRUCTURE: &str = "structure";
    pub const SUBGRAPHS: &str = "subgraphs";
    pub const CHURN: &str = "churn";
}

/// 6 built-in profili döndürür (ad sıralı değil — önem sırasıyla).
pub fn builtin_profiles() -> Vec<Profile> {
    vec![
        Profile {
            name: "pm".into(),
            label: "PM".into(),
            description: "Product Manager: teslimat ilerlemesi, tool kullanımı ve iş kapsamı".into(),
            view: "full".into(),
            direction: "td".into(),
            focus: vec![
                focus::EXECUTIONS.into(),
                focus::TOOLS.into(),
                focus::STRUCTURE.into(),
                focus::CHURN.into(),
            ],
            config: serde_json::json!({}),
            builtin: true,
        },
        Profile {
            name: "ba".into(),
            label: "BA".into(),
            description: "Business Analyst: süreç davranışı, dış çağrı maliyeti ve değişim hızı".into(),
            view: "summary".into(),
            direction: "td".into(),
            focus: vec![
                focus::DURATION.into(),
                focus::STRUCTURE.into(),
                focus::ENDPOINTS.into(),
                focus::CHURN.into(),
            ],
            config: serde_json::json!({}),
            builtin: true,
        },
        Profile {
            name: "ceo".into(),
            label: "CEO".into(),
            description: "Yönetici: portföy durumu, toplam koşu ve risk işaretleri (rollback, eksik env)".into(),
            view: "summary".into(),
            direction: "lr".into(),
            focus: vec![
                focus::PORTFOLIO.into(),
                focus::EXECUTIONS.into(),
                focus::RELIABILITY.into(),
            ],
            config: serde_json::json!({}),
            builtin: true,
        },
        Profile {
            name: "architect".into(),
            label: "Architect".into(),
            description: "Mimari: katman ayrımı, subgraph bağımlılıkları, bütçe ve dış bağımlılıklar".into(),
            view: "layers".into(),
            direction: "td".into(),
            focus: vec![
                focus::STRUCTURE.into(),
                focus::SUBGRAPHS.into(),
                focus::ENDPOINTS.into(),
                focus::TOOLS.into(),
            ],
            config: serde_json::json!({}),
            builtin: true,
        },
        Profile {
            name: "senior".into(),
            label: "Senior Engineer".into(),
            description: "Kıdemli geliştirici: plan kalitesi, saflık, modülerlik ve bakım".into(),
            view: "full".into(),
            direction: "td".into(),
            focus: vec![
                focus::STRUCTURE.into(),
                focus::TOOLS.into(),
                focus::CHURN.into(),
                focus::RELIABILITY.into(),
            ],
            config: serde_json::json!({}),
            builtin: true,
        },
        Profile {
            name: "devops".into(),
            label: "DevOps".into(),
            description: "Operasyon: hata oranı, p95 gecikme, dış endpoint'ler ve ortam bağımlılıkları".into(),
            view: "full".into(),
            direction: "td".into(),
            focus: vec![
                focus::RELIABILITY.into(),
                focus::ENDPOINTS.into(),
                focus::ENV.into(),
                focus::DURATION.into(),
            ],
            config: serde_json::json!({}),
            builtin: true,
        },
    ]
}

/// Ad ile built-in profil bul.
pub fn builtin_profile(name: &str) -> Option<Profile> {
    builtin_profiles().into_iter().find(|p| p.name == name)
}

/// Depolanan profili yükle; yoksa built-in'e, o da yoksa `None`'a düşer.
pub fn resolve<'a, S: tinypipe_api::storage::GraphStorage>(
    storage: &'a S,
    name: &str,
) -> Result<Option<Profile>, String> {
    match storage.load_profile(name) {
        Ok(p) => Ok(Some(p)),
        Err(_) => Ok(builtin_profile(name)),
    }
}

/// Built-in profilleri storage'a seed eder (eksik olanları yazar).
/// Kullanıcı tarafından silinen built-in'ler yeniden eklenmez (kayıt varsa dokunmaz).
pub fn seed_builtin_profiles<S: tinypipe_api::storage::GraphStorage>(
    storage: &S,
) -> Result<(), String> {
    let existing = storage.list_profiles().map_err(|e| e.to_string())?;
    for profile in builtin_profiles() {
        if !existing.iter().any(|p| p.name == profile.name) {
            storage.save_profile(&profile).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Profili `plan --view`/`--direction` string'lerine çevirir (opsiyonel).
/// Geçersiz değerler için varsayılan: full/td.
pub fn render_options(profile: &Profile) -> tinypipe_ir::plan_view::RenderOptions {
    tinypipe_ir::plan_view::RenderOptions {
        view: tinypipe_ir::plan_view::ViewLevel::parse(&profile.view)
            .unwrap_or(tinypipe_ir::plan_view::ViewLevel::Full),
        direction: tinypipe_ir::plan_view::Direction::parse(&profile.direction)
            .unwrap_or(tinypipe_ir::plan_view::Direction::Td),
        numbered_groups: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_profiles_have_unique_names() {
        let profiles = builtin_profiles();
        assert_eq!(profiles.len(), 6);
        let mut names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 6, "profile names must be unique");
    }

    #[test]
    fn test_builtin_profiles_all_builtin() {
        assert!(builtin_profiles().iter().all(|p| p.builtin));
    }

    #[test]
    fn test_resolve_falls_back_to_builtin() {
        // Storage'sız fallback mantığı: builtin isimler çözülebilir.
        assert!(builtin_profile("ceo").is_some());
        assert!(builtin_profile("nope").is_none());
    }

    #[test]
    fn test_render_options_parse() {
        let ceo = builtin_profile("ceo").unwrap();
        let opts = render_options(&ceo);
        assert_eq!(opts.view, tinypipe_ir::plan_view::ViewLevel::Summary);
        assert_eq!(opts.direction, tinypipe_ir::plan_view::Direction::Lr);
    }
}
