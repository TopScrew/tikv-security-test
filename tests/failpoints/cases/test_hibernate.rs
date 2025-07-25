// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    sync::{atomic::*, mpsc::channel, *},
    thread,
    time::Duration,
};

use kvproto::raft_serverpb::{ExtraMessage, ExtraMessageType, RaftMessage};
use raft::eraftpb::MessageType;
use raftstore::store::{PeerMsg, PeerTick};
use test_raftstore::*;
use tikv_util::{config::ReadableDuration, HandyRwLock};

#[test]
fn test_break_leadership_on_restart() {
    let mut cluster = new_node_cluster(0, 3);
    let base_tick_ms = 50;
    cluster.cfg.raft_store.raft_base_tick_interval = ReadableDuration::millis(base_tick_ms);
    cluster.cfg.raft_store.raft_heartbeat_ticks = 2;
    cluster.cfg.raft_store.raft_election_timeout_ticks = 10;
    // So the random election timeout will always be 10, which makes the case more
    // stable.
    cluster.cfg.raft_store.raft_min_election_timeout_ticks = 10;
    cluster.cfg.raft_store.raft_max_election_timeout_ticks = 11;
    configure_for_hibernate(&mut cluster.cfg);
    cluster.pd_client.disable_default_operator();
    let r = cluster.run_conf_change();
    cluster.pd_client.must_add_peer(r, new_peer(2, 2));
    cluster.pd_client.must_add_peer(r, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Wait until all peers of region 1 hibernate and then stop peer 2.
    thread::sleep(Duration::from_millis(base_tick_ms * 30));
    cluster.stop_node(2);

    // Peer 3 will:
    // 1. steps a heartbeat message from its leader and then ticks 1 time.
    // 2. ticks a peer_stale_state_check, which will change state from Idle to
    //    PreChaos.
    // 3. continues to tick until it hibernates totally.
    let (tx, rx) = mpsc::sync_channel(128);
    fail::cfg_callback("on_raft_base_tick_idle", move || tx.send(0).unwrap()).unwrap();
    let mut raft_msg = RaftMessage::default();
    raft_msg.region_id = 1;
    raft_msg.set_from_peer(new_peer(1, 1));
    raft_msg.set_to_peer(new_peer(3, 3));
    raft_msg.mut_region_epoch().version = 1;
    raft_msg.mut_region_epoch().conf_ver = 3;
    raft_msg.mut_message().msg_type = MessageType::MsgHeartbeat;
    raft_msg.mut_message().from = 1;
    raft_msg.mut_message().to = 3;
    raft_msg.mut_message().term = 6;
    let router = cluster.sim.rl().get_router(3).unwrap();
    router.send_raft_message(raft_msg).unwrap();

    rx.recv_timeout(Duration::from_millis(200)).unwrap();
    fail::remove("on_raft_base_tick_idle");
    router
        .send(1, PeerMsg::Tick(PeerTick::CheckPeerStaleState))
        .unwrap();

    // Wait until the peer 3 hibernates again.
    // Until here, peer 3 will be like `election_elapsed=3 && missing_ticks=6`.
    thread::sleep(Duration::from_millis(base_tick_ms * 10));

    // Restart the peer 2 and it will broadcast `MsgRequestPreVote` later, which
    // will wake up peer 1 and 3.
    let (tx, rx) = mpsc::sync_channel(128);
    let filter = RegionPacketFilter::new(1, 3)
        .direction(Direction::Send)
        .msg_type(MessageType::MsgRequestVote)
        .when(Arc::new(AtomicBool::new(false)))
        .set_msg_callback(Arc::new(move |m| drop(tx.send(m.clone()))));
    cluster.add_send_filter(CloneFilterFactory(filter));
    cluster.run_node(2).unwrap();

    // Peer 3 shouldn't start a new election, otherwise the leader may step down
    // incorrectly.
    rx.recv_timeout(Duration::from_secs(2)).unwrap_err();
}

#[test]
fn test_restart_peer_busy_on_apply() {
    let mut cluster = new_node_cluster(0, 3);
    let base_tick_ms = 50;
    cluster.cfg.raft_store.raft_base_tick_interval = ReadableDuration::millis(base_tick_ms);
    cluster.cfg.raft_store.raft_heartbeat_ticks = 2;
    cluster.cfg.raft_store.raft_election_timeout_ticks = 10;
    // So the random election timeout will always be 10, which makes the case more
    // stable.
    cluster.cfg.raft_store.raft_min_election_timeout_ticks = 10;
    cluster.cfg.raft_store.raft_max_election_timeout_ticks = 11;
    // Set a fairy small leader transfer log gap
    cluster.cfg.raft_store.leader_transfer_max_log_lag = 10;
    cluster.cfg.raft_store.min_pending_apply_region_count = 1;
    configure_for_hibernate(&mut cluster.cfg);
    cluster.pd_client.disable_default_operator();
    let r = cluster.run_conf_change();
    cluster.pd_client.must_add_peer(r, new_peer(2, 1002));
    cluster.pd_client.must_add_peer(r, new_peer(3, 1003));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Make a log gap on Peer 1003.
    cluster.stop_node(3);
    for _ in 0..=11 {
        cluster.must_put(b"k2", b"v2");
    }

    // Pause the applying processing in Peer 3 to make a log gap.
    fail::cfg("on_handle_apply_1003", "pause").unwrap();
    // Restart the node 3 and check the Peer 1003 is under applying stage.
    cluster.run_node(3).unwrap();

    // Wait for a while to make Peer 1003 enter `hibernate` state and
    // check the node is still busy on applying.
    thread::sleep(Duration::from_millis(base_tick_ms * 30));
    // Check hibernated.
    let (tx, rx) = mpsc::sync_channel(128);
    fail::cfg_callback("on_raft_base_tick_idle", move || tx.send(0).unwrap()).unwrap();
    let mut raft_msg = RaftMessage::default();
    raft_msg.region_id = 1;
    raft_msg.set_from_peer(new_peer(1, 1));
    raft_msg.set_to_peer(new_peer(3, 1003));
    raft_msg.mut_region_epoch().version = 1;
    raft_msg.mut_region_epoch().conf_ver = 3;
    raft_msg.mut_message().msg_type = MessageType::MsgHeartbeat;
    raft_msg.mut_message().from = 1;
    raft_msg.mut_message().to = 1003;
    raft_msg.mut_message().term = 6;
    let router = cluster.sim.rl().get_router(3).unwrap();
    router.send_raft_message(raft_msg).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
    fail::remove("on_raft_base_tick_idle");
    cluster.must_send_store_heartbeat(3);
    thread::sleep(Duration::from_millis(base_tick_ms));
    let stats = cluster.pd_client.get_store_stats(3).unwrap();
    assert!(stats.is_busy);

    // Recover the applying processing on Peer 1003 and wait for a while, then
    // the region will be hibernated.
    fail::remove("on_handle_apply_1003");
    thread::sleep(Duration::from_millis(base_tick_ms * 10));
    cluster.must_send_store_heartbeat(3);
    thread::sleep(Duration::from_millis(base_tick_ms));
    let stats = cluster.pd_client.get_store_stats(3).unwrap();
    assert!(!stats.is_busy);
}

// This case creates a cluster with 3 TiKV instances, and then wait all peers
// hibernate.
//
// After that, propose a command and stop the leader node immediately.
// With failpoint `receive_raft_message_from_outside`, we can make the proposal
// reach 2 followers *after* `StoreUnreachable` is broadcasted.
//
// 2 followers may become GroupState::Chaos after `StoreUnreachable` is
// received, and become `GroupState::Ordered` after the proposal is received.
// But they should keep wakeful for a while.
#[test]
fn test_store_disconnect_with_hibernate() {
    let mut cluster = new_server_cluster(0, 3);
    let base_tick_ms = 50;
    cluster.cfg.raft_store.raft_base_tick_interval = ReadableDuration::millis(base_tick_ms);
    cluster.cfg.raft_store.raft_heartbeat_ticks = 2;
    cluster.cfg.raft_store.raft_election_timeout_ticks = 10;
    cluster.cfg.raft_store.unreachable_backoff = ReadableDuration::millis(500);
    cluster.cfg.server.raft_client_max_backoff = ReadableDuration::millis(200);
    // Use a small range but still random election timeouts, which makes the case
    // more stable.
    cluster.cfg.raft_store.raft_min_election_timeout_ticks = 10;
    cluster.cfg.raft_store.raft_max_election_timeout_ticks = 13;
    configure_for_hibernate(&mut cluster.cfg);
    cluster.pd_client.disable_default_operator();
    let r = cluster.run_conf_change();
    cluster.pd_client.must_add_peer(r, new_peer(2, 2));
    cluster.pd_client.must_add_peer(r, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Wait until all peers of region 1 hibernate.
    thread::sleep(Duration::from_millis(base_tick_ms * 40));

    // Stop the region leader.
    fail::cfg("receive_raft_message_from_outside", "pause").unwrap();
    let _ = cluster.async_put(b"k2", b"v2").unwrap();
    cluster.stop_node(1);

    // Wait for a while so that the failpoint can be triggered on followers.
    thread::sleep(Duration::from_millis(100));
    fail::remove("receive_raft_message_from_outside");

    // Wait for a while. Peers of region 1 shouldn't hibernate.
    thread::sleep(Duration::from_millis(base_tick_ms * 40));
    must_get_equal(&cluster.get_engine(2), b"k2", b"v2");
    must_get_equal(&cluster.get_engine(3), b"k2", b"v2");
}

#[test]
fn test_check_long_uncommitted_proposals_while_hibernate() {
    let mut cluster = new_node_cluster(0, 3);
    let base_tick_ms = 50;
    cluster.cfg.raft_store.raft_base_tick_interval = ReadableDuration::millis(base_tick_ms);
    cluster.cfg.raft_store.raft_heartbeat_ticks = 2;
    cluster.cfg.raft_store.raft_election_timeout_ticks = 10;
    // So the random election timeout will always be 10, which makes the case more
    // stable.
    cluster.cfg.raft_store.raft_min_election_timeout_ticks = 10;
    cluster.cfg.raft_store.raft_max_election_timeout_ticks = 11;
    configure_for_hibernate(&mut cluster.cfg);
    cluster.cfg.raft_store.check_long_uncommitted_interval = ReadableDuration::millis(200);
    cluster.cfg.raft_store.long_uncommitted_base_threshold = ReadableDuration::millis(500);
    cluster.cfg.raft_store.check_leader_lease_interval = ReadableDuration::hours(1);

    cluster.pd_client.disable_default_operator();
    let r = cluster.run_conf_change();
    cluster.pd_client.must_add_peer(r, new_peer(2, 2));
    cluster.pd_client.must_add_peer(r, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Wait until all peers of region 1 hibernate.
    fail::cfg("on_check_long_uncommitted_tick_1", "return").unwrap();
    thread::sleep(Duration::from_millis(base_tick_ms * 30));

    // Must not tick CheckLongUncommitted after hibernate.
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    fail::cfg_callback("on_check_long_uncommitted_proposals_1", move || {
        let _ = tx.lock().unwrap().send(());
    })
    .unwrap();
    rx.recv_timeout(2 * cluster.cfg.raft_store.long_uncommitted_base_threshold.0)
        .unwrap_err();

    // Must keep ticking CheckLongUncommitted if leader weak up.
    fail::remove("on_check_long_uncommitted_tick_1");
    cluster.must_put(b"k1", b"v1");
    rx.recv_timeout(2 * cluster.cfg.raft_store.long_uncommitted_base_threshold.0)
        .unwrap();
}

#[test]
fn test_forcely_awaken_hibenrate_regions() {
    let mut cluster = new_node_cluster(0, 3);
    let base_tick_ms = 50;
    cluster.cfg.raft_store.raft_base_tick_interval = ReadableDuration::millis(base_tick_ms);
    cluster.cfg.raft_store.raft_heartbeat_ticks = 2;
    cluster.cfg.raft_store.raft_election_timeout_ticks = 10;
    // So the random election timeout will always be 10, which makes the case more
    // stable.
    cluster.cfg.raft_store.raft_min_election_timeout_ticks = 10;
    cluster.cfg.raft_store.raft_max_election_timeout_ticks = 11;
    configure_for_hibernate(&mut cluster.cfg);
    cluster.pd_client.disable_default_operator();
    let r = cluster.run_conf_change();
    cluster.pd_client.must_add_peer(r, new_peer(2, 2));
    cluster.pd_client.must_add_peer(r, new_peer(3, 3));

    cluster.must_put(b"k1", b"v1");
    must_get_equal(&cluster.get_engine(2), b"k1", b"v1");
    must_get_equal(&cluster.get_engine(3), b"k1", b"v1");

    // Wait until all peers of region 1 hibernate.
    thread::sleep(Duration::from_millis(base_tick_ms * 30));

    // Firstly, send `CheckPeerStaleState` message to trigger the check.
    let router = cluster.sim.rl().get_router(3).unwrap();
    router
        .send(1, PeerMsg::Tick(PeerTick::CheckPeerStaleState))
        .unwrap();

    // Secondly, forcely send `MsgRegionWakeUp` message for awakening hibernated
    // regions.
    let (tx, rx) = mpsc::sync_channel(128);
    fail::cfg_callback("on_raft_base_tick_chaos", move || {
        tx.send(base_tick_ms).unwrap()
    })
    .unwrap();
    let mut message = RaftMessage::default();
    message.region_id = 1;
    message.set_from_peer(new_peer(3, 3));
    message.set_to_peer(new_peer(3, 3));
    message.mut_region_epoch().version = 1;
    message.mut_region_epoch().conf_ver = 3;
    let mut msg = ExtraMessage::default();
    msg.set_type(ExtraMessageType::MsgRegionWakeUp);
    msg.forcely_awaken = true;
    message.set_extra_msg(msg);
    router.send_raft_message(message).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        base_tick_ms
    );
    fail::remove("on_raft_base_tick_chaos");
}
