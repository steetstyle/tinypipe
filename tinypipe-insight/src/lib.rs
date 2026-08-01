//! tinypipe-insight — rol bazlı metrik toplama, profil yönetimi ve rapor üretimi.
//!
//! Katmanlı tasarım:
//! - [`profile`]: 6 built-in rol profili (PM, BA, CEO, Architect, Senior, DevOps)
//!   ve bunların storage'a seed edilmesi. Her profil görünüm seçenekleri
//!   (`view`/`direction`) + önemsediği rapor bölümleri (`focus`) tutar.
//! - [`metrics`]: storage'dan ham veri toplar → portföy metrikleri
//!   (execution istatistikleri, plan yapısı, env bağımlılıkları, churn...).
//! - [`report`]: profilin focus'una göre düz metin rapor üretir.

pub mod metrics;
pub mod profile;
pub mod report;
