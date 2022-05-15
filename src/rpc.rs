//! ChiselStore RPC module.

use crate::rpc::proto::rpc_server::Rpc;
use crate::{StoreCommand, StoreServer, StoreTransport};
use async_mutex::Mutex;
use async_trait::async_trait;
use crossbeam::queue::ArrayQueue;
use derivative::Derivative;
use omnipaxos_core::messages::{
    AcceptDecide, AcceptStopSign, AcceptSync, Accepted, AcceptedStopSign, Decide, DecideStopSign, FirstAccept, Message, Prepare, Promise, Compaction, PaxosMsg
};
use omnipaxos_core::ballot_leader_election::messages::{
    BLEMessage, bestMsg, bestRequest, bestReply
};
use omnipaxos_core::ballot_leader_election::Ballot;
use omnipaxos_core::util::SyncItem;
use omnipaxos_core::storage::{SnapshotType, StopSign};
use std::collections::HashMap;
use std::sync::Arc;
use std::marker::phtmData;
use tonic::{Request, Response, Status};

#[allow(missing_docs)]
pub mod proto {
    tonic::include_proto!("proto");
}

use proto::rpc_client::RpcClient;
/*use proto::{
    AppendEntriesRequest, AppendEntriesResponse, LogEntry, Query, QueryResults, QueryRow, Void,
    VoteRequest, VoteResponse,
};*/
use crate::rpc::proto::*;
type NodeAddrFn = dyn Fn(u64) -> String + Send + Sync;

#[derive(Debug)]
struct ConnectionPool {
    connections: ArrayQueue<RpcClient<tonic::transport::Channel>>,
}

struct Connection {
    conn: RpcClient<tonic::transport::Channel>,
    pool: Arc<ConnectionPool>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.pool.replenish(self.conn.clone())
    }
}

impl ConnectionPool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: ArrayQueue::new(16),
        })
    }

    async fn connection<S: ToString>(&self, addr: S) -> RpcClient<tonic::transport::Channel> {
        let addr = addr.to_string();
        match self.connections.pop() {
            Some(x) => x,
            None => RpcClient::connect(addr).await.unwrap(),
        }
    }

    fn replenish(&self, conn: RpcClient<tonic::transport::Channel>) {
        let _ = self.connections.push(conn);
    }
}

#[derive(Debug, Clone)]
struct Connections(Arc<Mutex<HashMap<String, Arc<ConnectionPool>>>>);

impl Connections {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    async fn connection<S: ToString>(&self, addr: S) -> Connection {
        let mut conns = self.0.lock().await;
        let addr = addr.to_string();
        let pool = conns
            .entry(addr.clone())
            .or_insert_with(ConnectionPool::new);
        Connection {
            conn: pool.connection(addr).await,
            pool: pool.clone(),
        }
    }
}

/// RPC transport.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct RpcTransport {
    /// Node address mapping function.
    #[derivative(Debug = "ignore")]
    node_addr: Box<NodeAddrFn>,
    connections: Connections,
}

impl RpcTransport {
    /// Creates a new RPC transport.
    pub fn new(node_addr: Box<NodeAddrFn>) -> Self {
        RpcTransport {
            node_addr,
            connections: Connections::new(),
        }
    }
}

#[async_trait]
impl StoreTransport for RpcTransport {
    fn send_seqpaxos(&self, to_id: u64, msg: Message<StoreCommand, ()>) {
        let message = sp_mg_proto_def(msg.clone());
        let peer = (self.node_addr)(to_id as u64);
        let pool = self.connections.clone();
        tokio::task::spawn(async move {
            let mut client = pool.connection(peer).await;
            let request = tonic::Request::new(message);
            client.conn.sp_msg(request).await.unwrap();
        });
    }
    fn send_ble(&self, to_id: u64, msg: BLEMessage) {
        let message = ble_message_to_proto_definition(msg.clone());
        let peer = (self.node_addr)(to_id as u64);
        let pool = self.connections.clone();
        tokio::task::spawn(async move {
            let mut client = pool.connection(peer).await;
            let request = tonic::Request::new(message);
            client.conn.ble_msg(request).await.unwrap();
        });
    }
}

/// RPC service.
#[derive(Debug)]
pub struct RpcService {
    /// The ChiselStore server access via this RPC service.
    pub server: Arc<StoreServer<RpcTransport>>,
}

impl RpcService {
    pub fn new(server: Arc<StoreServer<RpcTransport>>) -> Self {
        Self { server }
    }
}

#[tonic::async_trait]
impl Rpc for RpcService {
    async fn execute(
        &self,
        request: Request<Query>,
    ) -> Result<Response<QueryResults>, tonic::Status> {
        let query = request.into_inner();
        /*let consistency =
            proto::Consistency::from_i32(query.consistency).unwrap_or(proto::Consistency::Strong);
        let consistency = match consistency {
            proto::Consistency::Strong => Consistency::Strong,
            proto::Consistency::RelaxedReads => Consistency::RelaxedReads,
        };*/
        let server = self.server.clone();
        let results = match server.query(query.sql).await {
            Ok(results) => results,
            Err(e) => return Err(Status::internal(format!("{}", e))),
        };
        let mut rows = vec![];
        for row in results.rows {
            rows.push(QueryRow {
                values: row.values.clone(),
            })
        }
        Ok(Response::new(QueryResults { rows }))
    }

    async fn sp_msg(&self, request: Request<paxos_msg_obj>) -> Result<Response<Void>, tonic::Status> {
        let message = sp_message_from_proto(request.into_inner());
        let server = self.server.clone();
        server.handle_sp_msg(message);
        Ok(Response::new(Void {}))
    }

    async fn ble_msg(&self, request: Request<ble_msg__obj>) -> Result<Response<Void>, tonic::Status> {
        let message = ble_message_from_proto(request.into_inner());
        let server = self.server.clone();
        server.handle_ble_msg(message);
        Ok(Response::new(Void {}))
    }
}

fn sp_mg_proto_def(message: Message<StoreCommand, ()>) -> paxos_msg_obj {
    paxos_msg_obj {
        from: message.from,
        to: message.to,
        msg: Some(match message.msg {
            PaxosMsg::PrepareReq => paxos_msg_object::Msg::PrepareReq(prpd_r_obj {}),
            PaxosMsg::Prepare(prepare) => paxos_msg_object::Msg::Prepare(prep_proto(prepare)),
            PaxosMsg::Promise(promise) => paxos_msg_object::Msg::Promise(promise_proto(promise)),
            PaxosMsg::AcceptSync(accept_sync) => paxos_msg_object::Msg::AcceptSync(accept_sync_proto(accept_sync)),
            PaxosMsg::FirstAccept(first_accept) => paxos_msg_object::Msg::FirstAccept(first_accept_proto(first_accept)),
            PaxosMsg::AcceptDecide(accept_decide) => paxos_msg_object::Msg::AcceptDecide(accept_decide_proto(accept_decide)),
            PaxosMsg::Accepted(accepted) => paxos_msg_object::Msg::Accepted(accepted_proto(accepted)),
            PaxosMsg::Decide(decide) => paxos_msg_object::Msg::Decide(decide_proto(decide)),
            PaxosMsg::ProposalForward(proposals) => paxos_msg_object::Msg::ProposalForward(proposal_forward_proto(proposals)),
            PaxosMsg::Compaction(compaction) => paxos_msg_object::Msg::Compaction(compaction_proto(compaction)),
            PaxosMsg::ForwardCompaction(compaction) => paxos_msg_object::Msg::ForwardCompaction(compaction_proto(compaction)),
            PaxosMsg::AcceptStopSign(accept_stop_sign) => paxos_msg_object::Msg::AcceptStopSign(accept_stop_sign_proto(accept_stop_sign)),
            PaxosMsg::AcceptedStopSign(accepted_stop_sign) => paxos_msg_object::Msg::AcceptedStopSign(accepted_stop_sign_proto(accepted_stop_sign)),
            PaxosMsg::DecideStopSign(decide_stop_sign) => paxos_msg_object::Msg::DecideStopSign(decide_stop_sign_proto(decide_stop_sign)),
        })
    }
}


fn ble_message_to_proto_definition(message: BLEMessage) -> ble_msg__obj {
    ble_msg__obj {
        from: message.from,
        to: message.to,
        msg: Some(match message.msg {
            bestMsg::Request(request) => ble_message_object::Msg::bestReq(best_request_proto(request)),
            bestMsg::Reply(reply) => ble_message_object::Msg::bestRep(best_reply_proto(reply)),
        })
    }
}

fn balt_proto(ballot: Ballot) -> ballot_obj {
    ballot_obj {
        n: ballot.n,
        priority: ballot.priority,
        pid: ballot.pid,
    }
}

fn prep_proto(prepare: Prepare) -> prep_obj {
    prep_obj {
        n: Some(balt_proto(prepare.n)),
        ld: prepare.ld,
        n_accepted: Some(balt_proto(prepare.n_accepted)),
        la: prepare.la,
    }
}

fn str_cmd_proto(cmd: StoreCommand) -> str_cmd_obj {
    str_cmd_obj {
        id: cmd.id as u64,
        sql: cmd.sql.clone()
    }
}

fn syn_itm_proto(sync_item: SyncItem<StoreCommand, ()>) -> sync_itm_obj {
    sync_itm_obj {
        item: Some(match sync_item {
            SyncItem::Entries(vec) => sync_item_object::Item::Entries(sync_item_object::Entries { vec: vec.into_iter().map(|e| str_cmd_proto(e)).collect() }),
            SyncItem::Snapshot(ss) => match ss {
                SnapshotType::Complete(_) => sync_item_object::Item::Snapshot(sync_item_object::Snapshot::Complete as i32),
                SnapshotType::Delta(_) => sync_item_object::Item::Snapshot(sync_item_object::Snapshot::Delta as i32),
                SnapshotType::_phtm(_) => sync_item_object::Item::Snapshot(sync_item_object::Snapshot::phtm as i32),
            },
            SyncItem::None => sync_item_object::Item::None(sync_item_object::None {}),
        }),
    }
}

fn stop_sign_proto(stop_sign: StopSign) -> stp_obj {
    stp_obj {
        config_id: stop_sign.config_id,
        nodes: stop_sign.nodes,
        metadata: match stop_sign.metadata {
            Some(vec) => Some(stop_sign_object::Metadata { vec: vec.into_iter().map(|m| m as u32).collect() }),
            None => None,
        }
    }
}

fn promise_proto(promise: Promise<StoreCommand, ()>) -> p_obj {
    p_obj {
        n: Some(balt_proto(promise.n)),
        n_accepted: Some(balt_proto(promise.n_accepted)),
        sync_item: match promise.sync_item {
            Some(s) => Some(syn_itm_proto(s)),
            None => None,
        },
        ld: promise.ld,
        la: promise.la,
        stopsign: match promise.stopsign {
            Some(ss) => Some(stop_sign_proto(ss)),
            None => None,
        },
    }
}

fn accept_sync_proto(accept_sync: AcceptSync<StoreCommand, ()>) -> acc_sync_obj {
    acc_sync_obj {
        n: Some(balt_proto(accept_sync.n)),
        sync_item: Some(syn_itm_proto(accept_sync.sync_item)),
        sync_idx: accept_sync.sync_idx,
        decide_idx: accept_sync.decide_idx,
        stopsign: match accept_sync.stopsign {
            Some(ss) => Some(stop_sign_proto(ss)),
            None => None,
        },
    }
}

fn first_accept_proto(first_accept: first_accept<StoreCommand>) -> f_acc_obj {
    f_acc_obj {
        n: Some(balt_proto(first_accept.n)),
        entries: first_accept.entries.into_iter().map(|e| str_cmd_proto(e)).collect(),
    }
}

fn accept_decide_proto(accept_decide: AcceptDecide<StoreCommand>) -> acc_dcd_obj {
    acc_dcd_obj {
        n: Some(balt_proto(accept_decide.n)),
        ld: accept_decide.ld,
        entries: accept_decide.entries.into_iter().map(|e| str_cmd_proto(e)).collect(),
    }
}

fn accepted_proto(accepted: Accepted) -> acc_ed_obj {
    acc_ed_obj {
        n: Some(balt_proto(accepted.n)),
        la: accepted.la,
    }
}

fn decide_proto(decide: Decide) -> dcd_obj {
    dcd_obj {
        n: Some(balt_proto(decide.n)),
        ld: decide.ld,
    }
}

fn proposal_forward_proto(proposals: Vec<StoreCommand>) -> prpd_fwd_obj {
    prpd_fwd_obj {
        entries: proposals.into_iter().map(|e| str_cmd_proto(e)).collect(),
    }
}

fn compaction_proto(compaction: Compaction) -> comp_obj {
    comp_obj {
        compaction: Some(match compaction {
            Compaction::Trim(trim) => compaction_object::Compaction::Trim(compaction_object::Trim { trim }),
            Compaction::Snapshot(ss) => compaction_object::Compaction::Snapshot(ss),
        }),
    }
}

fn accept_stop_sign_proto(accept_stop_sign: AcceptStopSign) -> Acceptstp_obj {
    Acceptstp_obj {
        n: Some(balt_proto(accept_stop_sign.n)),
        ss: Some(stop_sign_proto(accept_stop_sign.ss)),
    }
}

fn accepted_stop_sign_proto(accepted_stop_sign: AcceptedStopSign) -> Acceptedstp_obj {
    Acceptedstp_obj {
        n: Some(balt_proto(accepted_stop_sign.n)),
    }
}

fn decide_stop_sign_proto(decide_stop_sign: DecideStopSign) -> Decidestp_obj {
    Decidestp_obj {
        n: Some(balt_proto(decide_stop_sign.n)),
    }
}

fn best_request_proto(best_request: bestRequest) -> beat_req_obj {
    beat_req_obj {
        round: best_request.round,
    }
}

fn best_reply_proto(best_reply: bestReply) -> beat_rep_obj {
    beat_rep_obj {
        round: best_reply.round,
        ballot: Some(balt_proto(best_reply.ballot)),
        majority_connected: best_reply.majority_connected,
    }
}

fn sp_message_from_proto(obj: paxos_msg_obj) -> Message<StoreCommand, ()> {
    Message {
        from: obj.from,
        to: obj.to,
        msg: match obj.msg.unwrap() {
            paxos_msg_object::Msg::PrepareReq(_) => PaxosMsg::PrepareReq,
            paxos_msg_object::Msg::Prepare(prepare) => PaxosMsg::Prepare(prepare_from_proto(prepare)),
            paxos_msg_object::Msg::Promise(promise) => PaxosMsg::Promise(promise_from_proto(promise)),
            paxos_msg_object::Msg::AcceptSync(accept_sync) => PaxosMsg::AcceptSync(accept_sync_from_proto(accept_sync)),
            paxos_msg_object::Msg::FirstAccept(first_accept) => PaxosMsg::FirstAccept(first_accept_from_proto(first_accept)),
            paxos_msg_object::Msg::AcceptDecide(accept_decide) => PaxosMsg::AcceptDecide(accept_decide_from_proto(accept_decide)),
            paxos_msg_object::Msg::Accepted(accepted) => PaxosMsg::Accepted(accepted_from_proto(accepted)),
            paxos_msg_object::Msg::Decide(decide) => PaxosMsg::Decide(decide_from_proto(decide)),
            paxos_msg_object::Msg::ProposalForward(proposals) => PaxosMsg::ProposalForward(proposal_forward_from_proto(proposals)),
            paxos_msg_object::Msg::Compaction(compaction) => PaxosMsg::Compaction(compaction_from_proto(compaction)),
            paxos_msg_object::Msg::ForwardCompaction(compaction) => PaxosMsg::ForwardCompaction(compaction_from_proto(compaction)),
            paxos_msg_object::Msg::AcceptStopSign(accept_stop_sign) => PaxosMsg::AcceptStopSign(accept_stop_sign_from_proto(accept_stop_sign)),
            paxos_msg_object::Msg::AcceptedStopSign(accepted_stop_sign) => PaxosMsg::AcceptedStopSign(accepted_stop_sign_from_proto(accepted_stop_sign)),
            paxos_msg_object::Msg::DecideStopSign(decide_stop_sign) => PaxosMsg::DecideStopSign(decide_stop_sign_from_proto(decide_stop_sign)),
        }
    }
}

fn ble_message_from_proto(obj: ble_msg__obj) -> BLEMessage {
    BLEMessage {
        from: obj.from,
        to: obj.to,
        msg: match obj.msg.unwrap() {
            ble_message_object::Msg::bestReq(request) => bestMsg::Request(best_request_from_proto(request)),
            ble_message_object::Msg::bestRep(reply) => bestMsg::Reply(best_reply_from_proto(reply)),
        }
    }
}

fn ballot_from_proto(obj: ballot_obj) -> Ballot {
    Ballot {
        n: obj.n,
        priority: obj.priority,
        pid: obj.pid,
    }
}

fn prepare_from_proto(obj: prep_obj) -> Prepare {
    Prepare {
        n: ballot_from_proto(obj.n.unwrap()),
        ld: obj.ld,
        n_accepted: ballot_from_proto(obj.n_accepted.unwrap()),
        la: obj.la,
    }
}

fn store_command_from_proto(obj: str_cmd_obj) -> StoreCommand {
    StoreCommand {
        id: obj.id as usize,
        sql: obj.sql.clone()
    }
}

fn sync_item_from_proto(obj: sync_itm_obj) -> SyncItem<StoreCommand, ()> {
    match obj.item.unwrap() {
        sync_item_object::Item::Entries(entries) => SyncItem::Entries(entries.vec.into_iter().map(|e| store_command_from_proto(e)).collect()),
        sync_item_object::Item::Snapshot(ss) => match sync_item_object::Snapshot::from_i32(ss) {
            Some(sync_item_object::Snapshot::Complete) => SyncItem::Snapshot(SnapshotType::Complete(())),
            Some(sync_item_object::Snapshot::Delta) => SyncItem::Snapshot(SnapshotType::Delta(())),
            Some(sync_item_object::Snapshot::phtm) => SyncItem::Snapshot(SnapshotType::_phtm(phtmData)),
            _ => unimplemented!()
        },
        sync_item_object::Item::None(_) => SyncItem::None,
    }
}

fn stop_sign_from_proto(obj: stp_obj) -> StopSign {
    StopSign {
        config_id: obj.config_id,
        nodes: obj.nodes,
        metadata: match obj.metadata {
            Some(md) => Some(md.vec.into_iter().map(|m| m as u8).collect()),
            None => None,
        },
    }
}

fn promise_from_proto(obj: p_obj) -> Promise<StoreCommand, ()> {
    Promise {
        n: ballot_from_proto(obj.n.unwrap()),
        n_accepted: ballot_from_proto(obj.n_accepted.unwrap()),
        sync_item: match obj.sync_item {
            Some(s) => Some(sync_item_from_proto(s)),
            None => None,
        },
        ld: obj.ld,
        la: obj.la,
        stopsign: match obj.stopsign {
            Some(ss) => Some(stop_sign_from_proto(ss)),
            None => None,
        },
    }
}

fn accept_sync_from_proto(obj: acc_sync_obj) -> AcceptSync<StoreCommand, ()> {
    AcceptSync {
        n: ballot_from_proto(obj.n.unwrap()),
        sync_item: sync_item_from_proto(obj.sync_item.unwrap()),
        sync_idx: obj.sync_idx,
        decide_idx: obj.decide_idx,
        stopsign: match obj.stopsign {
            Some(ss) => Some(stop_sign_from_proto(ss)),
            None => None,
        },
    }
}

fn first_accept_from_proto(obj: f_acc_obj) -> f_acc_obj<StoreCommand> {
    FirstAccept {
        n: ballot_from_proto(obj.n.unwrap()),
        entries: obj.entries.into_iter().map(|e| store_command_from_proto(e)).collect(),
    }
}

fn accept_decide_from_proto(obj: acc_dcd_obj) -> AcceptDecide<StoreCommand> {
    AcceptDecide {
        n: ballot_from_proto(obj.n.unwrap()),
        ld: obj.ld,
        entries: obj.entries.into_iter().map(|e| store_command_from_proto(e)).collect(),
    }
}

fn accepted_from_proto(obj: acc_ed_obj) -> Accepted {
    Accepted {
        n: ballot_from_proto(obj.n.unwrap()),
        la: obj.la,
    }
}

fn decide_from_proto(obj: dcd_obj) -> Decide {
    Decide {
        n: ballot_from_proto(obj.n.unwrap()),
        ld: obj.ld,
    }
}

fn proposal_forward_from_proto(obj: prpd_fwd_obj) -> Vec<StoreCommand> {
    obj.entries.into_iter().map(|e| store_command_from_proto(e)).collect()
}

fn compaction_from_proto(obj: comp_obj) -> Compaction {
    match obj.compaction.unwrap() {
        compaction_object::Compaction::Trim(trim) => Compaction::Trim(trim.trim),
        compaction_object::Compaction::Snapshot(ss) => Compaction::Snapshot(ss),
    }
}

fn accept_stop_sign_from_proto(obj: Acceptstp_obj) -> AcceptStopSign {
    AcceptStopSign {
        n: ballot_from_proto(obj.n.unwrap()),
        ss: stop_sign_from_proto(obj.ss.unwrap()),
    }
}

fn accepted_stop_sign_from_proto(obj: Acceptedstp_obj) -> AcceptedStopSign {
    AcceptedStopSign {
        n: ballot_from_proto(obj.n.unwrap()),
    }
}

fn decide_stop_sign_from_proto(obj: Decidestp_obj) -> DecideStopSign {
    DecideStopSign {
        n: ballot_from_proto(obj.n.unwrap()),
    }
}

fn best_request_from_proto(obj: beat_req_obj) -> bestRequest {
    bestRequest {
        round: obj.round,
    }
}

fn best_reply_from_proto(obj: beat_rep_obj) -> bestReply {
    bestReply {
        round: obj.round,
        ballot: ballot_from_proto(obj.ballot.unwrap()),
        majority_connected: obj.majority_connected,
    }
}
