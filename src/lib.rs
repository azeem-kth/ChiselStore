//! ChiselStore is an embeddable, distributed [SQLite][1] for Rust, powered
//! by [Little Raft][2].
//!
//! ## Getting Started
//!
//! ChiselStore is a distributed SQLite that you can embed in your
//! application. With ChiselStore, clients (external applications) connect to
//! one of the cluster's nodes to execute SQL statements, such as `CREATE TABLE`,
//! `INSERT` or `SELECT` statements.
//!
//! Under the hood, ChiselStore uses the Raft consensus protocol to replicate
//! the SQL statements to all nodes in the cluster, which apply the statements
//! to their local in-memory SQLite instance. Raft guarantees that all of the
//! SQLite instances in the cluster have identical contents, which allows the
//! cluster to keep operating even if some of the nodes become unavailable.
//!
//! As ChiselStore uses the Raft consensus algorithm, it provides strong
//! consistency (linearizability) by default. SQL statements on a cluster of
//! ChiselStore appear to execute as if there is only one copy of the data
//! because SQL statements execute on the Raft cluster leader node. As strong
//! consistency limits performance, ChiselStore provides an optional
//! consistency [`Consistency::RelaxedReads`] mode, allowing clients to
//! perform read operations on the local node. The relaxed read mode can,
//! however, result in reading stale data so use it with caution.
//!
//! ChiselStore is currently not suitable for production use because it lacks
//! support for Raft snapshots and joint consensus. That is, the replicated
//! log of SQL statements is never truncated, and it is not possible for
//! nodes to join and leave a cluster dynamically. There is, however, a plan
//! to implement support for the missing features to make ChiselStore suitable
//! for production use cases.
//!
//! ChiselStore comes with batteries included and embedding it to your
//! application as simple as:

// #![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

pub mod errors;
pub mod rpc;
pub mod server;

pub use errors::StoreError;
// pub use server::Consistency;
pub use server::StoreCommand;
pub use server::StoreServer;
pub use server::StoreTransport;
