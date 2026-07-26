//! `GraphStorage` trait'i — Graph tanımları ve execution kayıtları için soyut arayüz.
//!
//! TinyOS tarafından implemente edilir (SQLite veya başka bir backend),
//! tinypipe-vm tarafından tüketilir.
//! Test'lerde mock implementasyon kullanılır.

use crate::types::{
    Execution, ExecutionStep, GraphId, StorageError, Version,
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
    pub parent_id: Option<GraphId>,
    /// Branch tree: hangi node'dan fork edildi (string ID).
    pub fork_node: Option<String>,
    /// Branch tree: fork sebebi / etiketi ("yaş<25").
    pub fork_label: Option<String>,
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

    /// Graph'ın execution_plan blob'unu (FlatBuffers IR) yükle.
    fn load_plan(&self, id: &GraphId) -> Result<Vec<u8>, StorageError>;

    /// Graph definition'ı yükle (metadata + code).
    fn load_graph(&self, id: &GraphId) -> Result<GraphDefinition, StorageError>;

    /// Execution kaydı oluştur/güncelle.
    fn save_execution(&self, exec: &Execution) -> Result<(), StorageError>;

    /// Execution step kaydet.
    fn save_step(&self, step: &ExecutionStep) -> Result<(), StorageError>;

    /// Execution'ı ID ile yükle.
    fn load_execution(&self, id: &str) -> Result<Execution, StorageError>;

    /// paused durumundaki execution'ları döndür (scheduler için).
    fn list_paused_executions(&self) -> Result<Vec<Execution>, StorageError>;

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
}

/// Branch tree node — recursive children.
#[derive(Debug, Clone)]
pub struct GraphTreeNode {
    pub graph: GraphDefinition,
    pub children: Vec<GraphTreeNode>,
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

        fn load_plan(&self, _id: &GraphId) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::Internal("not implemented".into()))
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

        fn list_paused_executions(&self) -> Result<Vec<Execution>, StorageError> {
            Ok(vec![])
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
