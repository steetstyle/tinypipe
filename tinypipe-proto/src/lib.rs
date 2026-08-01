//! tinypipe gRPC stubs — `proto/tinypipe.proto`'dan üretilir.
//!
//! `tinypipe-daemon` (server tarafı), `tinypipe-tools` (CLI köprüsü) ve
//! diğer dillerdeki worker SDK'ları (örn. Go) aynı proto dosyasından üretilir —
//! tek kaynak, diller arası uyumluluk garantisi.

pub mod tinypipe {
    pub mod v1 {
        tonic::include_proto!("tinypipe.v1");
    }
}

/// Üretilmiş tüm tipleri kısa isimle kullanmak için yeniden ihraç (örn. `TaskRequest`).
pub use tinypipe::v1::*;
