//! `GraphStorage` trait'i — Graph tanımları ve execution kayıtları için soyut arayüz.
//!
//! TinyOS tarafından implemente edilir (SQLite veya başka bir backend),
//! tinypipe-vm tarafından tüketilir.
//! Test'lerde mock implementasyon kullanılır.

use crate::types::{
    Execution, ExecutionStep, GraphId, Profile, StorageError, Version,
};

/// Graph definition (nodes + edges) — storage'dan dönen tam graph.
#[derive(Debug, Clone)]
pub struct GraphDefinition {
    pub id: GraphId,
    pub name: String,
    pub version: Version,
    pub status: String,
    pub code: String,
    pub execution_plan: Option<Vec<u8>>,
    pub active: bool,
    /// Hangi versiyonun deploy edildiği (None = draft).
    pub active_version: Option<Version>,
    pub parent_id: Option<GraphId>,
    /// Branch tree: hangi node'dan fork edildi (string ID).
    pub fork_node: Option<String>,
    /// Branch tree: fork sebebi / etiketi ("yaş<25").
    pub fork_label: Option<String>,
    /// Son yaşam döngüsü olayı: `deploy: v3`, `rollback: v2`, `fork: <parent>`.
    pub last_event: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Graph CRUD ve execution persistence için ana trait.
pub trait GraphStorage: Send + Sync {
    /// Yeni graph oluştur. Restricted Python kodunu alır, ilk versiyonu (v1) oluşturur.
    fn create_graph(&self, name: &str, code: &str) -> Result<GraphId, StorageError>;

    /// Varolan graph'ı güncelle. Yeni versiyon oluşturur (immutable model).
    fn update_graph(&self, id: &GraphId, code: &str) -> Result<Version, StorageError>;

    /// Graph'ı deployment'a al. `active` flag'ini yeni versiyona çevir.
    fn deploy(&self, id: &GraphId, version: Version) -> Result<(), StorageError>;

    /// Graph'ı belirtilen versiyona döndür (rollback).
    /// `graphs` tablosundaki code + version'ı eski versiyona çeker,
    /// `active_version`'ı da günceller (eğer deploy edilmişse).
    fn rollback(&self, id: &GraphId, version: Version) -> Result<(), StorageError>;

    /// Bir graph'ın tüm versiyonlarını listele.
    fn list_versions(&self, id: &GraphId) -> Result<Vec<(u64, String, String)>, StorageError>;

    /// Graph'ın execution_plan blob'unu (FlatBuffers IR) yükle.
    fn load_plan(&self, id: &GraphId) -> Result<Vec<u8>, StorageError>;

    /// Tüm graph'ları listele (name→id çözümleme ve subgraph lookup için).
    fn list_all_graphs(&self, limit: Option<u64>, offset: Option<u64>)
        -> Result<Vec<GraphDefinition>, StorageError>;

    /// İsme göre tekil graph bul (subgraph dispatch'in hızlı yolu —
    /// tüm tablo taraması yerine indeksli sorgu).
    fn find_graph_by_name(&self, name: &str) -> Result<GraphDefinition, StorageError>;

    /// Graph'ın belirli bir versiyonunun execution_plan blob'unu yükle
    /// (pause/resume ve scheduler için immutable versiyon okuma).
    fn load_plan_version(&self, id: &GraphId, version: Version) -> Result<Vec<u8>, StorageError>;

    /// Compiled plan blob'unu hem `graphs` (current) hem `graph_versions` (immutable) satırlarına yazar.
    fn save_plan(&self, id: &GraphId, version: Version, plan: &[u8]) -> Result<(), StorageError>;

    /// Graph definition'ı yükle (metadata + code).
    fn load_graph(&self, id: &GraphId) -> Result<GraphDefinition, StorageError>;

    /// Execution kaydı oluştur/güncelle.
    fn save_execution(&self, exec: &Execution) -> Result<(), StorageError>;

    /// Execution step kaydet.
    fn save_step(&self, step: &ExecutionStep) -> Result<(), StorageError>;

    /// Execution'ı ID ile yükle.
    fn load_execution(&self, id: &str) -> Result<Execution, StorageError>;

    /// Bir graph'ın execution kayıtlarını listele (en yeni önce).
    fn list_executions(
        &self,
        graph_id: &GraphId,
        limit: Option<u64>,
    ) -> Result<Vec<Execution>, StorageError>;

    /// Bir execution'ın step kayıtlarını listele (sıralı).
    fn list_steps(&self, execution_id: &str) -> Result<Vec<ExecutionStep>, StorageError>;

    /// paused durumundaki execution'ları döndür (scheduler için).
    fn list_paused_executions(&self) -> Result<Vec<Execution>, StorageError>;

    /// Execution'ın checkpoint blob'unu (serde_json-serialized `Checkpoint`) kaydet.
    /// Blob formatı storage'ı ilgilendirmez — opak veridir.
    fn save_checkpoint(&self, execution_id: &str, blob: &[u8]) -> Result<(), StorageError>;

    /// Execution'ın checkpoint blob'unu yükle (resume için).
    fn load_checkpoint(&self, execution_id: &str) -> Result<Vec<u8>, StorageError>;

    // ==================== Branch Explore (v2.1) ====================

    /// Varolan graph'ı fork eder. Yeni bir child graph oluşturur.
    /// - `id`: kaynak graph
    /// - `fork_node`: hangi node'dan fork edildi (string ID)
    /// - `code`: forked yeni kod
    /// - `label`: fork sebebi / etiketi (optional)
    fn fork_graph(
        &self,
        id: &GraphId,
        fork_node: &str,
        code: &str,
        label: Option<&str>,
    ) -> Result<GraphId, StorageError>;

    /// Bir graph'ın tüm child (fork) graph'larını döndürür.
    fn list_children(&self, id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError>;

    /// Bir graph'ın tüm lineage'ını (kendisi + parent + grandparent + ...) döndürür.
    /// En eski ancestor ilk sırada, graph'ın kendisi son sırada.
    fn graph_lineage(&self, id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError>;

    /// Fork tree'yi döndürür: graph + tüm child'ları recursive olarak.
    /// Her graph'ın altında `children: Vec<GraphDefinition>` alanı doldurulur.
    fn graph_tree(&self, id: &GraphId) -> Result<GraphTreeNode, StorageError>;

    // ==================== Profiles (v2.5) ====================

    /// Tüm profilleri listele (built-in'ler dahil, ad sıralı).
    fn list_profiles(&self) -> Result<Vec<Profile>, StorageError>;

    /// Profili ad ile yükle.
    fn load_profile(&self, name: &str) -> Result<Profile, StorageError>;

    /// Profil kaydet (INSERT OR REPLACE). Aynı adla built-in profil varsa
    /// built-in olmayan kayıt üzerine yazar; built-in kayıt silinemez/ezilemez.
    fn save_profile(&self, profile: &Profile) -> Result<(), StorageError>;

    /// Profili sil (yalnızca built-in olmayanlar).
    fn delete_profile(&self, name: &str) -> Result<(), StorageError>;
}

/// Branch tree node — recursive children.
#[derive(Debug, Clone)]
pub struct GraphTreeNode {
    pub graph: GraphDefinition,
    pub children: Vec<GraphTreeNode>,
}

/// `Arc<T>` üzerinden paylaşılan storage erişimi (subgraph registry, scheduler).
impl<T: GraphStorage> GraphStorage for std::sync::Arc<T> {
    fn create_graph(&self, name: &str, code: &str) -> Result<GraphId, StorageError> {
        self.as_ref().create_graph(name, code)
    }
    fn update_graph(&self, id: &GraphId, code: &str) -> Result<Version, StorageError> {
        self.as_ref().update_graph(id, code)
    }
    fn deploy(&self, id: &GraphId, version: Version) -> Result<(), StorageError> {
        self.as_ref().deploy(id, version)
    }
    fn rollback(&self, id: &GraphId, version: Version) -> Result<(), StorageError> {
        self.as_ref().rollback(id, version)
    }
    fn list_versions(&self, id: &GraphId) -> Result<Vec<(u64, String, String)>, StorageError> {
        self.as_ref().list_versions(id)
    }
    fn load_plan(&self, id: &GraphId) -> Result<Vec<u8>, StorageError> {
        self.as_ref().load_plan(id)
    }
    fn list_all_graphs(
        &self,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Result<Vec<GraphDefinition>, StorageError> {
        self.as_ref().list_all_graphs(limit, offset)
    }
    fn find_graph_by_name(&self, name: &str) -> Result<GraphDefinition, StorageError> {
        self.as_ref().find_graph_by_name(name)
    }
    fn load_plan_version(&self, id: &GraphId, version: Version) -> Result<Vec<u8>, StorageError> {
        self.as_ref().load_plan_version(id, version)
    }
    fn save_plan(&self, id: &GraphId, version: Version, plan: &[u8]) -> Result<(), StorageError> {
        self.as_ref().save_plan(id, version, plan)
    }
    fn load_graph(&self, id: &GraphId) -> Result<GraphDefinition, StorageError> {
        self.as_ref().load_graph(id)
    }
    fn save_execution(&self, exec: &Execution) -> Result<(), StorageError> {
        self.as_ref().save_execution(exec)
    }
    fn save_step(&self, step: &ExecutionStep) -> Result<(), StorageError> {
        self.as_ref().save_step(step)
    }
    fn load_execution(&self, id: &str) -> Result<Execution, StorageError> {
        self.as_ref().load_execution(id)
    }
    fn list_executions(
        &self,
        graph_id: &GraphId,
        limit: Option<u64>,
    ) -> Result<Vec<Execution>, StorageError> {
        self.as_ref().list_executions(graph_id, limit)
    }
    fn list_steps(&self, execution_id: &str) -> Result<Vec<ExecutionStep>, StorageError> {
        self.as_ref().list_steps(execution_id)
    }
    fn list_paused_executions(&self) -> Result<Vec<Execution>, StorageError> {
        self.as_ref().list_paused_executions()
    }
    fn save_checkpoint(&self, execution_id: &str, blob: &[u8]) -> Result<(), StorageError> {
        self.as_ref().save_checkpoint(execution_id, blob)
    }
    fn load_checkpoint(&self, execution_id: &str) -> Result<Vec<u8>, StorageError> {
        self.as_ref().load_checkpoint(execution_id)
    }
    fn fork_graph(
        &self,
        id: &GraphId,
        fork_node: &str,
        code: &str,
        label: Option<&str>,
    ) -> Result<GraphId, StorageError> {
        self.as_ref().fork_graph(id, fork_node, code, label)
    }
    fn list_children(&self, id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError> {
        self.as_ref().list_children(id)
    }
    fn graph_lineage(&self, id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError> {
        self.as_ref().graph_lineage(id)
    }
    fn graph_tree(&self, id: &GraphId) -> Result<GraphTreeNode, StorageError> {
        self.as_ref().graph_tree(id)
    }
    fn list_profiles(&self) -> Result<Vec<Profile>, StorageError> {
        self.as_ref().list_profiles()
    }
    fn load_profile(&self, name: &str) -> Result<Profile, StorageError> {
        self.as_ref().load_profile(name)
    }
    fn save_profile(&self, profile: &Profile) -> Result<(), StorageError> {
        self.as_ref().save_profile(profile)
    }
    fn delete_profile(&self, name: &str) -> Result<(), StorageError> {
        self.as_ref().delete_profile(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockStorage;

    impl GraphStorage for MockStorage {
        fn create_graph(&self, name: &str, _code: &str) -> Result<GraphId, StorageError> {
            Ok(GraphId::new(&format!("graph_{}", name)))
        }

        fn update_graph(&self, _id: &GraphId, _code: &str) -> Result<Version, StorageError> {
            Ok(Version(2))
        }

        fn deploy(&self, _id: &GraphId, _version: Version) -> Result<(), StorageError> {
            Ok(())
        }

        fn rollback(&self, _id: &GraphId, _version: Version) -> Result<(), StorageError> {
            Ok(())
        }

        fn list_versions(&self, _id: &GraphId) -> Result<Vec<(u64, String, String)>, StorageError> {
            Ok(vec![(1, "def graph(): pass".into(), "0".into())])
        }

        fn load_plan(&self, _id: &GraphId) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::Internal("not implemented".into()))
        }

        fn list_all_graphs(
            &self,
            _limit: Option<u64>,
            _offset: Option<u64>,
        ) -> Result<Vec<GraphDefinition>, StorageError> {
            Ok(Vec::new())
        }

        fn find_graph_by_name(&self, _name: &str) -> Result<GraphDefinition, StorageError> {
            Err(StorageError::Internal("not implemented".into()))
        }

        fn load_plan_version(
            &self,
            _id: &GraphId,
            _version: Version,
        ) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::Internal("not implemented".into()))
        }

        fn save_plan(
            &self,
            _id: &GraphId,
            _version: Version,
            _plan: &[u8],
        ) -> Result<(), StorageError> {
            Ok(())
        }

        fn load_graph(&self, id: &GraphId) -> Result<GraphDefinition, StorageError> {
            Err(StorageError::GraphNotFound(id.clone()))
        }

        fn save_execution(&self, _exec: &Execution) -> Result<(), StorageError> {
            Ok(())
        }

        fn save_step(&self, _step: &ExecutionStep) -> Result<(), StorageError> {
            Ok(())
        }

        fn load_execution(&self, id: &str) -> Result<Execution, StorageError> {
            Err(StorageError::ExecutionNotFound(id.into()))
        }

        fn list_executions(
            &self,
            _graph_id: &GraphId,
            _limit: Option<u64>,
        ) -> Result<Vec<Execution>, StorageError> {
            Ok(vec![])
        }

        fn list_steps(&self, _execution_id: &str) -> Result<Vec<ExecutionStep>, StorageError> {
            Ok(vec![])
        }

        fn list_paused_executions(&self) -> Result<Vec<Execution>, StorageError> {
            Ok(vec![])
        }

        fn save_checkpoint(&self, _execution_id: &str, _blob: &[u8]) -> Result<(), StorageError> {
            Ok(())
        }

        fn load_checkpoint(&self, execution_id: &str) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::ExecutionNotFound(execution_id.into()))
        }

        fn fork_graph(
            &self,
            _id: &GraphId,
            _fork_node: &str,
            code: &str,
            _label: Option<&str>,
        ) -> Result<GraphId, StorageError> {
            Ok(GraphId::new(&format!("fork_{}", code.len())))
        }

        fn list_children(&self, _id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError> {
            Ok(vec![])
        }

        fn graph_lineage(&self, _id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError> {
            Err(StorageError::Internal("not implemented".into()))
        }

        fn graph_tree(&self, _id: &GraphId) -> Result<GraphTreeNode, StorageError> {
            Err(StorageError::Internal("not implemented".into()))
        }

        fn list_profiles(&self) -> Result<Vec<Profile>, StorageError> {
            Ok(vec![])
        }

        fn load_profile(&self, name: &str) -> Result<Profile, StorageError> {
            Err(StorageError::Internal(format!(
                "profile '{}' not available in mock",
                name
            )))
        }

        fn save_profile(&self, _profile: &Profile) -> Result<(), StorageError> {
            Ok(())
        }

        fn delete_profile(&self, _name: &str) -> Result<(), StorageError> {
            Ok(())
        }
    }

    #[test]
    fn test_mock_storage_create() {
        let store = MockStorage;
        let id = store.create_graph("test", "def graph(): pass").unwrap();
        assert_eq!(id.0, "graph_test");
    }

    #[test]
    fn test_mock_storage_deploy() {
        let store = MockStorage;
        assert!(store.deploy(&GraphId::new("g1"), Version(2)).is_ok());
    }
}
