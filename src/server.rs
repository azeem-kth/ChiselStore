//! ChiselStore server module.

use crate::errors::StoreError;
use async_notify::Notify;
use async_trait::async_trait;
// use crossbeam_channel as channel;
// use crossbeam_channel::{Receiver, Sender};
use derivative::Derivative;
use sqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
// use std::thread::sleep;
use std::time::Duration;

use omnipaxos_core::messages::Message;
use omnipaxos_core::ballot_leader_election::{Ballot, BallotLeaderElection, BLEConfig};
use omnipaxos_core::ballot_leader_election::messages::BLEMessage;
use omnipaxos_core::sequence_paxos::{SequencePaxos, SequencePaxosConfig};
use omnipaxos_core::storage::{Snapshot, StopSignEntry, Storage};

/// ChiselStore transport layer.
///
/// Your application should implement this trait to provide network access
/// to the ChiselStore server.
#[async_trait]
pub trait StoreTransport {
    /// Send a store command message `msg` to `to_id` node.
    fn send_seqpaxos(&self, to_id: u64, msg: Message<StoreCommand, ()>);
/*
    async fn delegate(
        &self,
        to_id: usize,
        sql: String,
        consistency: Consistency,
    ) -> Result<QueryResults, StoreError>;*/

    // defining the ble message here for RPC communication
    fn send_ble(&self, to_id: u64, msg: BLEMessage);
}
/*
/// Consistency mode.
#[derive(Debug)]
pub enum Consistency {
    /// Strong consistency. Both reads and writes go through the Raft leader,
    /// which makes them linearizable.
    Strong,
    /// Relaxed reads. Reads are performed on the local node, which relaxes
    /// read consistency and allows stale reads.
    RelaxedReads,
}*/


/// Store command.
///
/// A store command is a SQL statement that is replicated in the Raft cluster.
/// A log entry = "{id : 1, sql: insert into * }"
#[derive(Clone, Debug)]
pub struct StoreCommand {
    /// Unique ID of this command.
    pub id: usize,
    /// The SQL statement of this command.
    pub sql: String,
}
#[derive(Debug)]
pub struct qr_res_handler {
    q_notifiers: HashMap<u64, Arc<Notify>>,
    q_results: HashMap<u64, Result<QueryResults, StoreError>>,
}
#[derive(Clone, Debug)]
pub struct KVSnapshot {
    pub snapshotted: HashMap<usize, String>,
}
impl qr_res_handler {
    fn default() -> Self {
        Self { q_notifiers: HashMap::new(), q_results: HashMap::new()}
    }
    pub fn add_notifier(&mut self, id: u64, notifier: Arc<Notify>) {
        self.q_notifiers.insert(id, notifier);
    }
    pub fn remove_result(&mut self, id: &u64) -> Option<Result<QueryResults, StoreError>> {
        self.q_results.remove(id)
    }
    pub fn add_result(&mut self, id: u64, result: Result<QueryResults, StoreError>) {
        if let Some(completion) = self.q_notifiers.remove(&(id)) {
            self.q_results.insert(id, result);
            completion.notify();
        }
    }
}
impl Snapshot<StoreCommand> for KVSnapshot {
    fn create(entries: &[StoreCommand]) -> Self {
        let mut snapshotted = HashMap::new();
        for e in entries {
            let StoreCommand { id, sql } = e;
            snapshotted.insert(id.clone(), sql.clone());
        }
        Self { snapshotted }
    }
    fn merge(&mut self, delta: Self) {
        for (k, v) in delta.snapshotted {
            self.snapshotted.insert(k, v);
        }
    }
    fn use_snapshots() -> bool {
        true
    }
}
/// Store configuration.
#[derive(Debug)]
struct StoreConfig {
    /// Connection pool size.
    conn_pool_size: usize,
    query_results: Arc<Mutex<qr_res_handler>>,
}

#[derive(Derivative)]
#[derivative(Debug)]
struct Store<S> where S: Snapshot<StoreCommand> {
    /// ID of the node this Cluster objects is on.
    id: u64,
    log: Vec<StoreCommand>,
    n_prom: Ballot,
    acc_round: Ballot,
    ld: u64,
    trimmed_idx: u64,
    snapshot: Option<S>,
    stopsign: Option<StopSignEntry>,
    #[derivative(Debug = "ignore")]
    conn_pool: Vec<Arc<Mutex<Connection>>>,
    conn_idx: usize,
    pending_transitions: Vec<StoreCommand>,
    query_results: Arc<Mutex<qr_res_handler>>,
}

impl<S> Store<S> where S: Snapshot<StoreCommand> {
    pub fn new(id: u64, config: StoreConfig) -> Self {
        let mut conn_pool = vec![];
        let conn_pool_size = config.conn_pool_size;
        for _ in 0..conn_pool_size {
            // FIXME: Let's use the 'memdb' VFS of SQLite, which allows concurrent threads
            // accessing the same in-memory database.
            let flags = OpenFlags::new()
                .set_read_write()
                .set_create()
                .set_no_mutex();
            let mut conn =
                Connection::open_with_flags(format!("node{}.db", id), flags).unwrap();
            conn.set_busy_timeout(5000).unwrap();
            conn_pool.push(Arc::new(Mutex::new(conn)));
        }
        let conn_idx = 0;
        Store {
            id,
            log: vec![],
            n_prom: Ballot::default(),
            acc_round: Ballot::default(),
            ld: 0,
            trimmed_idx: 0,
            snapshot: None,
            stopsign: None,
            conn_pool,
            conn_idx,
            pending_transitions: Vec::new(),
            query_results: config.query_results
        }
    }
    /*pub fn is_leader(&self) -> bool {
        match self.leader {
            Some(id) => id == self.this_id,
            _ => false,
        }
    }*/

    pub fn get_connection(&mut self) -> Arc<Mutex<Connection>> {
        let idx = self.conn_idx % self.conn_pool.len();
        let conn = &self.conn_pool[idx];
        self.conn_idx += 1;
        conn.clone()
    }
}

impl<S> Storage<StoreCommand, S> for Store<S> where S: Snapshot<StoreCommand>
{
    fn get_promise(&self) -> Ballot {
        self.n_prom
    }
    fn set_promise(&mut self, n_prom: Ballot) {
        self.n_prom = n_prom;
    }
    fn append_on_prefix(&mut self, from_idx: u64, entries: Vec<StoreCommand>) -> u64 {
        self.log.truncate(from_idx as usize);
        self.append_entries(entries)
    }
    fn append_entry(&mut self, entry: StoreCommand) -> u64 {
        self.log.push(entry);
        self.get_log_len()
    }
    fn get_decided_idx(&self) -> u64 {
        self.ld
    }
    fn append_entries(&mut self, entries: Vec<StoreCommand>) -> u64 {
        let mut e = entries;
        self.log.append(&mut e);
        self.get_log_len()
    }
    fn set_decided_idx(&mut self, ld: u64) {
        let runqueries = self.log[(self.ld as usize)..(ld as usize)].to_vec();
        for q in runqueries.iter() {
            let conn = self.get_connection();
            let results = query(conn, q.sql.clone());
            let mut query_results = self.query_results.lock().unwrap();
            query_results.add_result(q.id as u64, results);
        }
        self.ld = ld;
    }
    fn get_accepted_round(&self) -> Ballot {
        self.acc_round
    }
    fn set_accepted_round(&mut self, na: Ballot) {
        self.acc_round = na;
    }
    fn get_entries(&self, from: u64, to: u64) -> &[StoreCommand] {
        self.log.get(from as usize..to as usize).unwrap_or(&[])
    }
    fn get_log_len(&self) -> u64 {
        self.log.len() as u64
    }
    fn get_suffix(&self, from: u64) -> &[StoreCommand] {
        match self.log.get(from as usize..) {
            Some(s) => s,
            None => &[],
        }
    }
    fn get_stopsign(&self) -> Option<StopSignEntry> {
        self.stopsign.clone()
    }
    fn set_stopsign(&mut self, s: StopSignEntry) {
        self.stopsign = Some(s);
    }
    fn get_snapshot(&self) -> Option<S> {
        self.snapshot.clone()
    }
    fn set_snapshot(&mut self, snapshot: S) {
        self.snapshot = Some(snapshot);
    }
    fn get_compacted_idx(&self) -> u64 {
        self.trimmed_idx
    }
    fn set_compacted_idx(&mut self, trimmed_idx: u64) {
        self.trimmed_idx = trimmed_idx;
    }
    fn trim(&mut self, trimmed_idx: u64) {
        self.log.drain(0..trimmed_idx as usize);
    }
}
fn query(conn: Arc<Mutex<Connection>>, sql: String) -> Result<QueryResults, StoreError> {
    let conn = conn.lock().unwrap();
    let mut rows = vec![];
    conn.iterate(sql, |pairs| {
        let mut row = QueryRow::new();
        for &(_, value) in pairs.iter() {
            row.values.push(value.unwrap().to_string());
        }
        rows.push(row);
        true
    })?;
    Ok(QueryResults { rows })
}

/// ChiselStore server.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct StoreServer<T: StoreTransport + Send + Sync> {
    id: u64,
    command_id: AtomicU64,
    query_results: Arc<Mutex<qr_res_handler>>,
    #[derivative(Debug = "ignore")]
    sp: Arc<Mutex<SequencePaxos<StoreCommand, (), Store<()>>>>,
    #[derivative(Debug = "ignore")]
    ble: Arc<Mutex<BallotLeaderElection>>,
    transport: T,
}

/// Query row.
#[derive(Debug)]
pub struct QueryRow {
    /// Column values of the row.
    pub values: Vec<String>,
}

impl QueryRow {
    fn new() -> Self {
        QueryRow { values: Vec::new() }
    }
}

/// Query results.
#[derive(Debug)]
pub struct QueryResults {
    /// Query result rows.
    pub rows: Vec<QueryRow>,
}

/*
const NOP_TRANSITION_ID: usize = 0;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(500);
const MIN_ELECTION_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_ELECTION_TIMEOUT: Duration = Duration::from_millis(950);*/

impl<T: StoreTransport + Send + Sync> StoreServer<T> {
    /// Start a new server as part of a ChiselStore cluster.
    pub fn start(id: u64, peers: Vec<u64>, transport: T) -> Result<Self, StoreError> {
        let command_id = AtomicU64::new(0);
        let configuration_id = 0;
        let mut sp_config = SequencePaxosConfig::default();
        sp_config.set_configuration_id(configuration_id);
        sp_config.set_pid(id);
        sp_config.set_peers(peers.to_vec());
        let query_results = Arc::new(Mutex::new(qr_res_handler::default()));
        let config = StoreConfig{ 
            conn_pool_size: 20, 
            query_results: query_results.clone() 
        };
        let store = Store::new(id, config);
        let sp = Arc::new(Mutex::new(SequencePaxos::with(sp_config, store)));
        let mut ble_config = BLEConfig::default();
        ble_config.set_pid(id);
        ble_config.set_peers(peers);
        ble_config.set_hb_delay(20); // from omni paxos documentation, configure it yourself
        let ble = Arc::new(Mutex::new(BallotLeaderElection::with(ble_config)));
        Ok(StoreServer {
            id,
            command_id,
            query_results,
            sp,
            ble,
            transport,
        })
    }

    /// Run the blocking event loop.
    pub async fn run(&self) {
        loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let mut sp = self.sp.lock().unwrap();
            let mut ble = self.ble.lock().unwrap();
            for msg in sp.get_outgoing_msgs() {
                let receiver = msg.to;
                self.transport.send_seqpaxos(receiver, msg);
            }
            for msg in ble.get_outgoing_msgs() {
                let receiver = msg.to;
                self.transport.send_ble(receiver,msg);
            }
        }
    }
    pub async fn run_ble(&self) {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut sp = self.sp.lock().unwrap();
            let mut ble = self.ble.lock().unwrap();
            if let Some(leader) = ble.tick() {
                sp.handle_leader(leader);
            }
        }
    }
    pub fn is_leader(&self) -> bool {
        let seq_paxos = self.sp.lock().unwrap();
        seq_paxos.get_current_leader() == (self.id as u64)
    }
    pub async fn query<S: AsRef<str>>(&self, sql_statement: S) -> Result<QueryResults, StoreError> {
        let results = {
            let (notify, id) = {
                let id = self.command_id.fetch_add(1, Ordering::SeqCst);
                let command = StoreCommand {
                    id: id as usize,
                    sql: sql_statement.as_ref().to_string()
                };
                let notify = Arc::new(Notify::new());
                self.query_results.lock().unwrap().add_notifier(id, notify.clone());
                self.sp.lock().unwrap().append(command).expect("Failed to append");
                (notify, id)
            };
            notify.notified().await;
            let results = self.query_results.lock().unwrap().remove_result(&id).unwrap();
            results?
        };
        Ok(results)
    }
    pub fn handle_ble_msg(&self, msg: BLEMessage) {
        self.ble.lock().unwrap().handle(msg);
    }
    pub fn handle_sp_msg(&self, msg: Message<StoreCommand, ()>) {
        self.sp.lock().unwrap().handle(msg);
    }
}
