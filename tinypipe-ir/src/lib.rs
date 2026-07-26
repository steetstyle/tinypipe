//! `tinypipe-ir` — Execution plan types, opcode enum, DAG yapıları.
//!
//! v1: `ExecutionPlan` (JSON-based, string ID'ler).
//! v2: `CompiledPlan` (binary, uint32 index'ler, O(1) random access).
//! v3: FlatBuffers binary format (zero-copy, canonical .fbs schema).

pub mod plan;
pub mod compiled;

// Include FlatBuffers generated bindings
#[allow(clippy::all)]
#[allow(unused)]
mod fb {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/execution_plan_generated.rs"));
}

pub use plan::{Arg, ArgValue, Edge, ExecutionPlan, Metadata, Node, Opcode, ToolDep, Type};
pub use compiled::{CompiledArg, CompiledEdge, CompiledMetadata, CompiledNode, CompiledPlan};
pub use fb::root_as_execution_plan;
