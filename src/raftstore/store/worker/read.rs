// Copyright 2018 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cell::RefCell;
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kvproto::errorpb;
use kvproto::metapb;
use kvproto::raft_cmdpb::{CmdType, RaftCmdRequest, RaftCmdResponse};
use mio;
use prometheus::local::LocalHistogram;
use rocksdb::DB;
use time::Timespec;

use raftstore::errors::RAFTSTORE_IS_BUSY;
use raftstore::store::msg::Callback;
use raftstore::store::util::{self, LeaseState, RemoteLease};
use raftstore::store::Store;
use raftstore::store::{
    cmd_resp, Msg as StoreMsg, Peer, ReadExecutor, ReadResponse, RequestInspector, RequestPolicy,
};
use raftstore::Result;
use util::collections::HashMap;
use util::time::duration_to_sec;
use util::timer::Timer;
use util::transport::{NotifyError, Sender};
use util::worker::{Runnable, RunnableWithTimer};

use super::metrics::*;

/// A read only delegate of `Peer`.
#[derive(Debug)]
pub struct ReadDelegate {
    region: metapb::Region,
    peer_id: u64,
    term: u64,
    applied_index_term: u64,
    leader_lease: Option<RemoteLease>,
    last_valid_ts: RefCell<Timespec>,

    tag: String,
}

impl ReadDelegate {
    fn from_peer(peer: &Peer) -> ReadDelegate {
        let region = peer.region().clone();
        let region_id = region.get_id();
        let peer_id = peer.peer.get_id();
        ReadDelegate {
            region,
            peer_id,
            term: peer.term(),
            applied_index_term: peer.get_store().applied_index_term(),
            leader_lease: None,
            last_valid_ts: RefCell::new(Timespec::new(0, 0)),
            tag: format!("[region {}] {}", region_id, peer_id),
        }
    }

    fn update(&mut self, progress: Progress) {
        match progress {
            Progress::Region(region) => {
                self.region = region;
            }
            Progress::Term(term) => {
                self.term = term;
            }
            Progress::AppliedIndexTerm(applied_index_term) => {
                self.applied_index_term = applied_index_term;
            }
            Progress::LeaderLease(leader_lease) => {
                self.leader_lease = Some(leader_lease);
            }
        }
    }

    // TODO: return ReadResponse once we remove batch snapshot.
    fn handle_read(
        &self,
        req: &RaftCmdRequest,
        executor: &mut ReadExecutor,
        metrics: &mut ReadMetrics,
    ) -> Option<ReadResponse> {
        if let Some(ref lease) = self.leader_lease {
            let term = lease.term();
            if term == self.term {
                let snapshot_time = executor.snapshot_time().unwrap();
                let mut last_valid_ts = self.last_valid_ts.borrow_mut();
                if *last_valid_ts == snapshot_time /* quick path for lease checking. */
                    || lease.inspect(Some(snapshot_time)) == LeaseState::Valid
                {
                    // Cache snapshot_time for remaining requests in the same batch.
                    *last_valid_ts = snapshot_time;
                    let mut resp = executor.execute(req, &self.region);
                    // Leader can read local if and only if it is in lease.
                    cmd_resp::bind_term(&mut resp.response, term);
                    return Some(resp);
                } else {
                    metrics.rejected_by_lease_expire += 1;
                    debug!("{} rejected by lease expire", self.tag);
                }
            } else {
                metrics.rejected_by_term_mismatch += 1;
                debug!("{} rejected by term mismatch", self.tag);
            }
        }

        None
    }
}

impl Display for ReadDelegate {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "ReadDelegate for region {}, \
             leader {} at term {}, applied_index_term {}, has lease {}",
            self.region.get_id(),
            self.peer_id,
            self.term,
            self.applied_index_term,
            self.leader_lease.is_some(),
        )
    }
}

#[derive(Debug)]
pub enum Progress {
    Region(metapb::Region),
    Term(u64),
    AppliedIndexTerm(u64),
    LeaderLease(RemoteLease),
}

impl Progress {
    pub fn region(region: metapb::Region) -> Progress {
        Progress::Region(region)
    }

    pub fn term(term: u64) -> Progress {
        Progress::Term(term)
    }

    pub fn applied_index_term(applied_index_term: u64) -> Progress {
        Progress::AppliedIndexTerm(applied_index_term)
    }

    pub fn leader_lease(lease: RemoteLease) -> Progress {
        Progress::LeaderLease(lease)
    }
}

pub enum Task {
    Register(ReadDelegate),
    Update((u64, Progress)),
    Read(StoreMsg),
    Destroy(u64),
}

impl Task {
    pub fn register(peer: &Peer) -> Task {
        let delegate = ReadDelegate::from_peer(peer);
        Task::Register(delegate)
    }

    pub fn update(region_id: u64, progress: Progress) -> Task {
        Task::Update((region_id, progress))
    }

    pub fn destroy(region_id: u64) -> Task {
        Task::Destroy(region_id)
    }

    pub fn read(msg: StoreMsg) -> Task {
        Task::Read(msg)
    }

    /// Task accepts `Mag`s that contain Get/Snap requests and BatchRaftSnapCmds.
    /// Returns `true`, it can be saftly sent to localreader,
    /// Returns `false`, it must not be sent to localreader.
    #[inline]
    pub fn acceptable(msg: &StoreMsg) -> bool {
        match *msg {
            StoreMsg::RaftCmd { ref request, .. } => {
                if request.has_admin_request() || request.has_status_request() {
                    false
                } else {
                    for r in request.get_requests() {
                        match r.get_cmd_type() {
                            CmdType::Get | CmdType::Snap => (),
                            CmdType::Delete
                            | CmdType::Put
                            | CmdType::DeleteRange
                            | CmdType::Prewrite
                            | CmdType::IngestSST
                            | CmdType::Invalid => return false,
                        }
                    }
                    true
                }
            }
            StoreMsg::BatchRaftSnapCmds { .. } => true,
            _ => false,
        }
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Task::Register(ref delegate) => write!(f, "localreader Task::Register {:?}", delegate),
            Task::Read(ref msg) => write!(f, "localreader Task::Msg {:?}", msg),
            Task::Update(ref progress) => write!(f, "localreader Task::Update {:?}", progress),
            Task::Destroy(region_id) => write!(f, "localreader Task::Destroy region {}", region_id),
        }
    }
}

fn handle_busy(cmd: StoreMsg) {
    let mut err = errorpb::Error::new();
    err.set_message(RAFTSTORE_IS_BUSY.to_owned());
    let mut server_is_busy = errorpb::ServerIsBusy::new();
    server_is_busy.set_reason(RAFTSTORE_IS_BUSY.to_owned());
    err.set_server_is_busy(server_is_busy);
    let mut resp = RaftCmdResponse::new();
    resp.mut_header().set_error(err);

    let read_resp = ReadResponse {
        response: resp,
        snapshot: None,
    };

    match cmd {
        StoreMsg::RaftCmd { callback, .. } => callback.invoke_read(read_resp),
        StoreMsg::BatchRaftSnapCmds {
            batch, on_finished, ..
        } => on_finished.invoke_batch_read(vec![Some(read_resp); batch.len()]),
        other => panic!("unexpected cmd {:?}", other),
    }
}

pub struct LocalReader<C: Sender<StoreMsg>> {
    store_id: u64,
    kv_engine: Arc<DB>,
    metrics: RefCell<ReadMetrics>,
    // region id -> ReadDelegate
    delegates: HashMap<u64, ReadDelegate>,
    // A channel to raftstore.
    ch: C,
    tag: String,
}

impl LocalReader<mio::Sender<StoreMsg>> {
    pub fn new<T, P>(store: &Store<T, P>) -> Self {
        let mut delegates =
            HashMap::with_capacity_and_hasher(store.get_peers().len(), Default::default());
        for (&region_id, p) in store.get_peers() {
            let delegate = ReadDelegate::from_peer(p);
            info!(
                "{} create ReadDelegate for peer {:?}",
                delegate.tag, delegate.peer_id
            );
            delegates.insert(region_id, delegate);
        }
        let store_id = store.store_id();
        LocalReader {
            delegates,
            store_id,
            kv_engine: store.kv_engine(),
            ch: store.get_sendch().into_inner(),
            metrics: Default::default(),
            tag: format!("[store {}]", store_id),
        }
    }

    pub fn new_timer() -> Timer<()> {
        let mut timer = Timer::new(1);
        timer.add_task(Duration::from_millis(METRICS_FLUSH_INTERVAL), ());
        timer
    }
}

impl<C: Sender<StoreMsg>> LocalReader<C> {
    fn redirect(&self, cmd: StoreMsg) {
        debug!("{} localreader redirect {:?}", self.tag, cmd);
        match self.ch.send(cmd) {
            Ok(()) => (),
            Err(NotifyError::Full(cmd)) => {
                self.metrics.borrow_mut().rejected_by_channel_full += 1;
                handle_busy(cmd)
            }
            Err(err) => {
                panic!("localreader redirect failed: {:?}", err);
            }
        }
    }

    fn pre_propose_raft_command<'a>(
        &'a self,
        req: &RaftCmdRequest,
    ) -> Result<Option<&'a ReadDelegate>> {
        // Check store id.
        if let Err(e) = util::check_store_id(req, self.store_id) {
            self.metrics.borrow_mut().rejected_by_store_id_mismatch += 1;
            debug!("rejected by store id not match {:?}", e);
            return Err(e);
        }

        // Check region id.
        let region_id = req.get_header().get_region_id();
        let delegate = match self.delegates.get(&region_id) {
            Some(delegate) => {
                fail_point!("localreader_on_find_delegate");
                delegate
            }
            None => {
                self.metrics.borrow_mut().rejected_by_no_region += 1;
                debug!("rejected by no region {}", region_id);
                return Ok(None);
            }
        };
        // Check peer id.
        if let Err(e) = util::check_peer_id(req, delegate.peer_id) {
            self.metrics.borrow_mut().rejected_by_peer_id_mismatch += 1;
            return Err(e);
        }

        // Check term.
        if let Err(e) = util::check_term(req, delegate.term) {
            debug!(
                "delegate.term {}, header.term {}",
                delegate.term,
                req.get_header().get_term()
            );
            self.metrics.borrow_mut().rejected_by_term_mismatch += 1;
            return Err(e);
        }

        // Check region epoch.
        if util::check_region_epoch(req, &delegate.region, false).is_err() {
            self.metrics.borrow_mut().rejected_by_epoch += 1;
            // Stale epoch, redirect it to raftstore to get the latest region.
            debug!("{} rejected by stale epoch", delegate.tag);
            return Ok(None);
        }

        let mut inspector = Inspector {
            delegate,
            metrics: &mut *self.metrics.borrow_mut(),
        };
        match inspector.inspect(req) {
            Ok(RequestPolicy::ReadLocal) => Ok(Some(delegate)),
            // It can not handle other policies.
            Ok(_) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // It can only handle read command.
    fn propose_raft_command(
        &mut self,
        request: RaftCmdRequest,
        callback: Callback,
        send_time: Instant,
        executor: &mut ReadExecutor,
    ) {
        let region_id = request.get_header().get_region_id();
        match self.pre_propose_raft_command(&request) {
            Ok(Some(delegate)) => {
                let mut metrics = self.metrics.borrow_mut();
                if let Some(resp) = delegate.handle_read(&request, executor, &mut *metrics) {
                    callback.invoke_read(resp);
                    return;
                }
            }
            // It can not handle the rquest, forwards to raftstore.
            Ok(None) => {}
            Err(e) => {
                let mut response = cmd_resp::new_error(e);
                if let Some(delegate) = self.delegates.get(&region_id) {
                    cmd_resp::bind_term(&mut response, delegate.term);
                }
                callback.invoke_read(ReadResponse {
                    response,
                    snapshot: None,
                });
                return;
            }
        }

        self.redirect(StoreMsg::RaftCmd {
            send_time,
            request,
            callback,
        });
    }

    fn propose_batch_raft_snapshot_command(
        &mut self,
        batch: Vec<RaftCmdRequest>,
        on_finished: Callback,
        executor: &mut ReadExecutor,
    ) {
        let size = batch.len();
        let mut ret = Vec::with_capacity(size);
        for req in batch {
            let region_id = req.get_header().get_region_id();
            match self.pre_propose_raft_command(&req) {
                Ok(Some(delegate)) => {
                    let mut metrics = self.metrics.borrow_mut();
                    let resp = delegate.handle_read(&req, executor, &mut *metrics);
                    ret.push(resp);
                }
                // It can not handle the rquest, instead of forwarding to raftstore,
                // it returns a `None` which means users need to retry the requsets
                // via `async_snapshot`.
                Ok(None) => {
                    ret.push(None);
                }
                Err(e) => {
                    let mut response = cmd_resp::new_error(e);
                    if let Some(delegate) = self.delegates.get(&region_id) {
                        cmd_resp::bind_term(&mut response, delegate.term);
                    }
                    ret.push(Some(ReadResponse {
                        response,
                        snapshot: None,
                    }));
                }
            }
        }

        on_finished.invoke_batch_read(ret);
    }
}

struct Inspector<'r, 'm> {
    delegate: &'r ReadDelegate,
    metrics: &'m mut ReadMetrics,
}

impl<'r, 'm> RequestInspector for Inspector<'r, 'm> {
    fn has_applied_to_current_term(&mut self) -> bool {
        if self.delegate.applied_index_term == self.delegate.term {
            true
        } else {
            debug!(
                "{} rejected by applied_index_term {} != term {} ",
                self.delegate.tag, self.delegate.applied_index_term, self.delegate.term
            );
            self.metrics.rejected_by_appiled_term += 1;
            false
        }
    }

    fn inspect_lease(&mut self) -> LeaseState {
        // TODO: disable localreader if we did not enable raft's check_quorum.
        if self.delegate.leader_lease.is_some() {
            // We skip lease check, because it is postponed until `handle_read`.
            LeaseState::Valid
        } else {
            debug!("{} rejected by leader lease", self.delegate.tag);
            self.metrics.rejected_by_no_lease += 1;
            LeaseState::Expired
        }
    }
}

impl<C: Sender<StoreMsg>> Runnable<Task> for LocalReader<C> {
    fn run(&mut self, _: Task) {
        unreachable!()
    }

    fn run_batch(&mut self, tasks: &mut Vec<Task>) {
        self.metrics
            .borrow()
            .batch_requests_size
            .observe(tasks.len() as _);

        let mut sent = None;
        let mut executor = ReadExecutor::new(
            self.kv_engine.clone(),
            false, /* dont check region epoch */
            true,  /* we need snapshot time */
        );

        for task in tasks.drain(..) {
            match task {
                Task::Register(delegate) => {
                    info!("{} register ReadDelegate", delegate.tag);
                    self.delegates.insert(delegate.region.get_id(), delegate);
                }
                Task::Read(StoreMsg::RaftCmd {
                    send_time,
                    request,
                    callback,
                }) => {
                    self.propose_raft_command(request, callback, send_time, &mut executor);
                    if sent.is_none() {
                        sent = Some(send_time);
                    }
                }
                Task::Read(StoreMsg::BatchRaftSnapCmds {
                    send_time,
                    batch,
                    on_finished,
                }) => {
                    self.propose_batch_raft_snapshot_command(batch, on_finished, &mut executor);
                    if sent.is_none() {
                        sent = Some(send_time);
                    }
                }
                Task::Read(other) => {
                    unimplemented!("unsupported Msg {:?}", other);
                }
                Task::Update((region_id, progress)) => {
                    if let Some(delegate) = self.delegates.get_mut(&region_id) {
                        delegate.update(progress);
                    } else {
                        warn!(
                            "update unregistered ReadDelegate, region_id: {}, {:?}",
                            region_id, progress
                        );
                    }
                }
                Task::Destroy(region_id) => {
                    if let Some(delegate) = self.delegates.remove(&region_id) {
                        info!("{} destroy ReadDelegate", delegate.tag);
                    }
                }
            }
        }

        if let Some(send_time) = sent {
            self.metrics
                .borrow_mut()
                .requests_wait_duration
                .observe(duration_to_sec(send_time.elapsed()));
        }
    }
}

const METRICS_FLUSH_INTERVAL: u64 = 15; // 15s

impl<C: Sender<StoreMsg>> RunnableWithTimer<Task, ()> for LocalReader<C> {
    fn on_timeout(&mut self, timer: &mut Timer<()>, _: ()) {
        self.metrics.borrow_mut().flush();
        timer.add_task(Duration::from_secs(METRICS_FLUSH_INTERVAL), ());
    }
}

struct ReadMetrics {
    requests_wait_duration: LocalHistogram,
    batch_requests_size: LocalHistogram,

    // TODO: record rejected_by_read_quorum.
    rejected_by_store_id_mismatch: i64,
    rejected_by_peer_id_mismatch: i64,
    rejected_by_term_mismatch: i64,
    rejected_by_lease_expire: i64,
    rejected_by_no_region: i64,
    rejected_by_no_lease: i64,
    rejected_by_epoch: i64,
    rejected_by_appiled_term: i64,
    rejected_by_channel_full: i64,
}

impl Default for ReadMetrics {
    fn default() -> ReadMetrics {
        ReadMetrics {
            requests_wait_duration: LOCAL_READ_WAIT_DURATION.local(),
            batch_requests_size: LOCAL_READ_BATCH_REQUESTS.local(),
            rejected_by_store_id_mismatch: 0,
            rejected_by_peer_id_mismatch: 0,
            rejected_by_term_mismatch: 0,
            rejected_by_lease_expire: 0,
            rejected_by_no_region: 0,
            rejected_by_no_lease: 0,
            rejected_by_epoch: 0,
            rejected_by_appiled_term: 0,
            rejected_by_channel_full: 0,
        }
    }
}

impl ReadMetrics {
    fn flush(&mut self) {
        self.requests_wait_duration.flush();
        self.batch_requests_size.flush();
        if self.rejected_by_store_id_mismatch > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["store_id_mismatch"])
                .inc_by(self.rejected_by_store_id_mismatch);
            self.rejected_by_store_id_mismatch = 0;
        }
        if self.rejected_by_peer_id_mismatch > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["peer_id_mismatch"])
                .inc_by(self.rejected_by_peer_id_mismatch);
            self.rejected_by_peer_id_mismatch = 0;
        }
        if self.rejected_by_term_mismatch > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["term_mismatch"])
                .inc_by(self.rejected_by_term_mismatch);
            self.rejected_by_term_mismatch = 0;
        }
        if self.rejected_by_lease_expire > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["lease_expire"])
                .inc_by(self.rejected_by_lease_expire);
            self.rejected_by_lease_expire = 0;
        }
        if self.rejected_by_no_region > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["no_region"])
                .inc_by(self.rejected_by_no_region);
            self.rejected_by_no_region = 0;
        }
        if self.rejected_by_no_lease > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["no_lease"])
                .inc_by(self.rejected_by_no_lease);
            self.rejected_by_no_lease = 0;
        }
        if self.rejected_by_epoch > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["epoch"])
                .inc_by(self.rejected_by_epoch);
            self.rejected_by_epoch = 0;
        }
        if self.rejected_by_appiled_term > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["appiled_term"])
                .inc_by(self.rejected_by_appiled_term);
            self.rejected_by_appiled_term = 0;
        }
        if self.rejected_by_channel_full > 0 {
            LOCAL_READ_REJECT
                .with_label_values(&["channel_full"])
                .inc_by(self.rejected_by_channel_full);
            self.rejected_by_channel_full = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::*;
    use std::thread;

    use kvproto::raft_cmdpb::*;
    use tempdir::TempDir;
    use time::Duration;

    use raftstore::store::util::Lease;
    use raftstore::store::Callback;
    use storage::ALL_CFS;
    use util::rocksdb;
    use util::time::monotonic_raw_now;

    use super::*;

    fn new_reader(
        path: &str,
        store_id: u64,
    ) -> (
        TempDir,
        LocalReader<SyncSender<StoreMsg>>,
        Receiver<StoreMsg>,
    ) {
        let path = TempDir::new(path).unwrap();
        let db = rocksdb::new_engine(path.path().to_str().unwrap(), ALL_CFS, None).unwrap();
        let (ch, rx) = sync_channel(1);
        let reader = LocalReader {
            store_id,
            ch,
            kv_engine: Arc::new(db),
            delegates: HashMap::default(),
            metrics: Default::default(),
            tag: "foo".to_owned(),
        };
        (path, reader, rx)
    }

    fn new_peers(store_id: u64, pr_ids: Vec<u64>) -> Vec<metapb::Peer> {
        pr_ids
            .into_iter()
            .map(|id| {
                let mut pr = metapb::Peer::new();
                pr.set_store_id(store_id);
                pr.set_id(id);
                pr
            })
            .collect()
    }

    fn must_extract_cmds(msg: StoreMsg) -> Vec<RaftCmdRequest> {
        match msg {
            StoreMsg::RaftCmd { request, .. } => vec![request],
            StoreMsg::BatchRaftSnapCmds { batch, .. } => batch,
            other => panic!("unexpected msg: {:?}", other),
        }
    }

    fn must_redirect(
        reader: &mut LocalReader<SyncSender<StoreMsg>>,
        rx: &Receiver<StoreMsg>,
        cmd: RaftCmdRequest,
    ) {
        let task = Task::read(StoreMsg::new_raft_cmd(
            cmd.clone(),
            Callback::Read(Box::new(|resp| {
                panic!("unexpected invoke, {:?}", resp);
            })),
        ));
        reader.run_batch(&mut vec![task]);
        assert_eq!(
            must_extract_cmds(
                rx.recv_timeout(Duration::seconds(5).to_std().unwrap())
                    .unwrap()
            ),
            vec![cmd]
        );
    }

    #[test]
    fn test_read() {
        let store_id = 2;
        let (_tmp, mut reader, rx) = new_reader("test-local-reader", store_id);

        // region: 1,
        // peers: 2, 3, 4,
        // leader:2,
        // from "" to "",
        // epoch 1, 1,
        // term 6.
        let mut region1 = metapb::Region::new();
        region1.set_id(1);
        let prs = new_peers(store_id, vec![2, 3, 4]);
        region1.set_peers(prs.clone().into());
        let epoch13 = {
            let mut ep = metapb::RegionEpoch::new();
            ep.set_conf_ver(1);
            ep.set_version(3);
            ep
        };
        let leader2 = prs[0].clone();
        region1.set_region_epoch(epoch13.clone());
        let term6 = 6;
        let mut lease = Lease::new(Duration::seconds(1)); // 1s is long enough.

        let mut cmd = RaftCmdRequest::new();
        let mut header = RaftRequestHeader::new();
        header.set_region_id(1);
        header.set_peer(leader2.clone());
        header.set_region_epoch(epoch13.clone());
        header.set_term(term6);
        cmd.set_header(header);
        let mut req = Request::new();
        req.set_cmd_type(CmdType::Snap);
        cmd.set_requests(vec![req].into());

        // The region is not register yet.
        must_redirect(&mut reader, &rx, cmd.clone());
        assert_eq!(reader.metrics.borrow().rejected_by_no_region, 1);

        // Register region 1
        lease.renew(monotonic_raw_now());
        let remote = lease.maybe_new_remote_lease(term6).unwrap();
        // But the applied_index_term is stale.
        let register_region1 = Task::Register(ReadDelegate {
            tag: String::new(),
            region: region1.clone(),
            peer_id: leader2.get_id(),
            term: term6,
            applied_index_term: term6 - 1,
            leader_lease: Some(remote),
            last_valid_ts: RefCell::new(Timespec::new(0, 0)),
        });
        reader.run_batch(&mut vec![register_region1]);
        assert!(reader.delegates.get(&1).is_some());
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);

        // The applied_index_term is stale
        must_redirect(&mut reader, &rx, cmd.clone());
        assert_eq!(reader.metrics.borrow().rejected_by_appiled_term, 1);

        // Make the applied_index_term matches current term.
        let pg = Progress::applied_index_term(term6);
        let update_region1 = Task::update(1, pg);
        reader.run_batch(&mut vec![update_region1]);
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);

        // Let's read.
        let region = region1.clone();
        let task = Task::read(StoreMsg::new_raft_cmd(
            cmd.clone(),
            Callback::Read(Box::new(move |resp: ReadResponse| {
                let snap = resp.snapshot.unwrap();
                assert_eq!(snap.get_region(), &region);
            })),
        ));
        reader.run_batch(&mut vec![task]);
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);

        // Wait for expiration.
        thread::sleep(Duration::seconds(1).to_std().unwrap());
        must_redirect(&mut reader, &rx, cmd.clone());

        // Renew lease.
        lease.renew(monotonic_raw_now());

        // Batch snapshot.
        let region = region1.clone();
        let batch_task = Task::read(StoreMsg::new_batch_raft_snapshot_cmd(
            vec![cmd.clone()],
            Box::new(move |mut resps: Vec<Option<ReadResponse>>| {
                assert_eq!(resps.len(), 1);
                let snap = resps.remove(0).unwrap().snapshot.unwrap();
                assert_eq!(snap.get_region(), &region);
            }),
        ));
        reader.run_batch(&mut vec![batch_task]);
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);

        // Store id mismatch.
        let mut cmd_store_id = cmd.clone();
        cmd_store_id
            .mut_header()
            .mut_peer()
            .set_store_id(store_id + 1);
        let task = Task::read(StoreMsg::new_raft_cmd(
            cmd_store_id,
            Callback::Read(Box::new(move |resp: ReadResponse| {
                let err = resp.response.get_header().get_error();
                assert!(err.has_store_not_match());
                assert!(resp.snapshot.is_none());
            })),
        ));
        reader.run_batch(&mut vec![task]);
        assert_eq!(reader.metrics.borrow().rejected_by_store_id_mismatch, 1);

        // metapb::Peer id mismatch.
        let mut cmd_peer_id = cmd.clone();
        cmd_peer_id
            .mut_header()
            .mut_peer()
            .set_id(leader2.get_id() + 1);
        let task = Task::read(StoreMsg::new_raft_cmd(
            cmd_peer_id,
            Callback::Read(Box::new(move |resp: ReadResponse| {
                assert!(
                    resp.response.get_header().has_error(),
                    "{:?}",
                    resp.response
                );
                assert!(resp.snapshot.is_none());
            })),
        ));
        reader.run_batch(&mut vec![task]);
        assert_eq!(reader.metrics.borrow().rejected_by_peer_id_mismatch, 1);

        // Read quorum.
        let mut cmd_read_quorum = cmd.clone();
        cmd_read_quorum.mut_header().set_read_quorum(true);
        must_redirect(&mut reader, &rx, cmd_read_quorum);

        // Term mismatch.
        let mut cmd_term = cmd.clone();
        cmd_term.mut_header().set_term(term6 - 2);
        let task = Task::read(StoreMsg::new_raft_cmd(
            cmd_term,
            Callback::Read(Box::new(move |resp: ReadResponse| {
                let err = resp.response.get_header().get_error();
                assert!(err.has_stale_command(), "{:?}", resp);
                assert!(resp.snapshot.is_none());
            })),
        ));
        reader.run_batch(&mut vec![task]);
        assert_eq!(reader.metrics.borrow().rejected_by_term_mismatch, 1);

        // Stale epoch.
        let mut epoch12 = epoch13.clone();
        epoch12.set_version(2);
        let mut cmd_epoch = cmd.clone();
        cmd_epoch.mut_header().set_region_epoch(epoch12);
        must_redirect(&mut reader, &rx, cmd_epoch);
        assert_eq!(reader.metrics.borrow().rejected_by_epoch, 1);

        // Expire lease manually, and it can not be renewed.
        let previous_lease_rejection = reader.metrics.borrow().rejected_by_lease_expire;
        lease.expire();
        lease.renew(monotonic_raw_now());
        must_redirect(&mut reader, &rx, cmd.clone());
        assert_eq!(
            reader.metrics.borrow().rejected_by_lease_expire,
            previous_lease_rejection + 1
        );

        // Channel full.
        let task1 = Task::read(StoreMsg::new_raft_cmd(cmd.clone(), Callback::None));
        let task_full = Task::read(StoreMsg::new_raft_cmd(
            cmd.clone(),
            Callback::Read(Box::new(move |resp: ReadResponse| {
                let err = resp.response.get_header().get_error();
                assert!(err.has_server_is_busy(), "{:?}", resp);
                assert!(resp.snapshot.is_none());
            })),
        ));
        reader.run_batch(&mut vec![task1]);
        reader.run_batch(&mut vec![task_full]);
        rx.try_recv().unwrap();
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(reader.metrics.borrow().rejected_by_channel_full, 1);

        // Reject by term mismatch in lease.
        let previous_term_rejection = reader.metrics.borrow().rejected_by_term_mismatch;
        let mut cmd9 = cmd.clone();
        cmd9.mut_header().set_term(term6 + 3);
        let msg = StoreMsg::new_raft_cmd(
            cmd9.clone(),
            Callback::Read(Box::new(|resp| {
                panic!("unexpected invoke, {:?}", resp);
            })),
        );
        let mut batch = vec![
            Task::update(1, Progress::term(term6 + 3)),
            Task::update(1, Progress::applied_index_term(term6 + 3)),
            Task::read(msg),
        ];
        reader.run_batch(&mut batch);
        assert_eq!(
            must_extract_cmds(
                rx.recv_timeout(Duration::seconds(5).to_std().unwrap())
                    .unwrap()
            ),
            vec![cmd9]
        );
        assert_eq!(
            reader.metrics.borrow().rejected_by_term_mismatch,
            previous_term_rejection + 1,
        );

        // Destroy region 1.
        let destroy_region1 = Task::destroy(1);
        reader.run_batch(&mut vec![destroy_region1]);
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
        assert!(reader.delegates.get(&1).is_none());
    }
}
