// Copyright 2016 PingCAP, Inc.
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

pub mod bootstrap;
pub mod cmd_resp;
pub mod config;
pub mod engine;
pub mod fsm;
pub mod keys;
pub mod msg;
pub mod transport;
pub mod util;

mod local_metrics;
mod metrics;
mod peer;
mod peer_storage;
mod region_snapshot;
mod snap;
mod worker;

pub use self::bootstrap::{
    bootstrap_store, clear_prepare_bootstrap, clear_prepare_bootstrap_state, prepare_bootstrap,
    write_prepare_bootstrap,
};
pub use self::config::Config;
pub use self::engine::{Iterable, Mutable, Peekable};
pub use self::fsm::{
    create_event_loop, new_compaction_listener, DestroyPeerJob, Store, StoreChannel, StoreInfo,
    StoreStat,
};
pub use self::msg::{
    Callback, Msg, ReadCallback, ReadResponse, SeekRegionCallback, SeekRegionFilter,
    SeekRegionResult, SignificantMsg, Tick, WriteCallback, WriteResponse,
};
pub use self::peer::{
    Peer, PeerStat, ProposalContext, ReadExecutor, RequestInspector, RequestPolicy,
};
pub use self::peer_storage::{
    clear_meta, do_snapshot, init_apply_state, init_raft_state, write_initial_apply_state,
    write_initial_raft_state, write_peer_state, CacheQueryStats, PeerStorage, SnapState,
    RAFT_INIT_LOG_INDEX, RAFT_INIT_LOG_TERM,
};
pub use self::region_snapshot::{RegionIterator, RegionSnapshot};
pub use self::snap::{
    check_abort, copy_snapshot, ApplyOptions, Error as SnapError, SnapEntry, SnapKey, SnapManager,
    SnapManagerBuilder, Snapshot, SnapshotDeleter, SnapshotStatistics,
};
pub use self::transport::Transport;
pub use self::util::Engines;
pub use self::worker::{KeyEntry, ReadTask};

// Only used in tests
#[cfg(test)]
pub use self::worker::{SplitCheckRunner, SplitCheckTask};
