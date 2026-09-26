use super::*;
use crate::web::AppleWebHooks;
use easytier::{common::config::ConfigFileControl, web_client::WebClientHooks};

// Pause the real RPC after insertion but before our post-run hook claims
// the reservation. An overwrite RPC deletes the instance before pre-run.
struct PausedPostRun {
    hooks: Arc<InstanceCoordinator>,
    first: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl WebClientHooks for PausedPostRun {
    async fn pre_run_network_instance(&self, config: &TomlConfigLoader) -> Result<(), String> {
        AppleWebHooks::new(self.hooks.clone())
            .pre_run_network_instance(config)
            .await
    }

    async fn post_run_network_instance(&self, id: &Uuid) -> Result<(), String> {
        if self.first.swap(false, Ordering::Relaxed) {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        AppleWebHooks::new(self.hooks.clone())
            .post_run_network_instance(id)
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rpc_overwrite_before_post_run_preserves_the_replacement() {
    use easytier::{
        proto::api::manage::{NetworkConfig, RunNetworkInstanceRequest, WebClientService},
        rpc_service::instance_manage::InstanceManageRpcService,
    };

    let hooks = hooks();
    let monitor = tokio::spawn(hooks.clone().monitor());
    let config = config();
    let id = config.get_id();
    let paused = Arc::new(PausedPostRun {
        hooks: hooks.clone(),
        first: std::sync::atomic::AtomicBool::new(true),
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let rpc = InstanceManageRpcService::new(hooks.manager.clone(), paused.clone());
    let request = RunNetworkInstanceRequest {
        config: Some(NetworkConfig::new_from_config(config).unwrap()),
        overwrite: true,
        ..Default::default()
    };
    let first_rpc = rpc.clone();
    let first_request = request.clone();
    let first = tokio::spawn(async move {
        first_rpc
            .run_network_instance(Default::default(), first_request)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), paused.entered.notified())
        .await
        .unwrap();
    let old_generation = hooks.status().unwrap().generation;
    assert!(hooks.manager.list_network_instance_ids().contains(&id));

    let mut replacement_request = request;
    replacement_request.config.as_mut().unwrap().virtual_ipv4 = Some("10.42.0.2".into());
    let replacement = tokio::spawn(async move {
        rpc.run_network_instance(Default::default(), replacement_request)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while hooks.status().unwrap().generation == old_generation {
            assert!(
                !replacement.is_finished(),
                "overwrite was rejected after deleting the original instance"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!replacement.is_finished());
    assert!(hooks.manager.list_network_instance_ids().is_empty());
    assert!(hooks.state.lock().unwrap().pending.is_none());
    paused.resume.notify_one();

    tokio::time::timeout(Duration::from_secs(8), async {
        while !awaiting_ack(&hooks) {
            assert!(
                !replacement.is_finished(),
                "replacement did not reach setup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let generation = hooks.status().unwrap().generation;
    assert!(hooks.complete_setup(old_generation, Ok(())).is_err());
    hooks.complete_setup(generation, Ok(())).unwrap();
    first.await.unwrap().unwrap();
    replacement.await.unwrap().unwrap();
    let status = hooks.status().unwrap();
    assert_eq!(status.status, "running");
    assert!(status.error.is_none());
    assert_eq!(
        status.options.unwrap().ipv4.as_deref(),
        Some("10.42.0.2/24")
    );
    assert_eq!(hooks.manager.list_network_instance_ids(), vec![id]);
    hooks.stop();
    monitor.abort();
    hooks.manager.delete_network_instance(vec![id]).unwrap();
}

fn hooks() -> Arc<InstanceCoordinator> {
    Arc::new(InstanceCoordinator::new(
        Arc::new(NetworkInstanceManager::new()),
        None,
    ))
}

fn awaiting_ack(hooks: &InstanceCoordinator) -> bool {
    hooks
        .state
        .lock()
        .unwrap()
        .active
        .as_ref()
        .is_some_and(|v| matches!(v.phase, InstancePhase::AwaitingAck(_)))
}

fn config() -> TomlConfigLoader {
    TomlConfigLoader::new_from_str(
        "ipv4 = '10.42.0.1/24'\nlisteners = []\n[flags]\nno_tun = true\nenable_ipv6 = false\n",
    )
    .unwrap()
}

#[tokio::test]
async fn late_delete_hook_preserves_a_new_same_id_reservation() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config.clone(), false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    hooks.claim_setup(id).unwrap();

    // The old delete RPC is preempted between deletion and its ID-only hook.
    hooks.manager.delete_network_instance(vec![id]).unwrap();
    hooks.reserve(&config).await.unwrap();
    let generation = hooks.status().unwrap().generation;
    hooks.reconcile_removed(&[id]).await.unwrap();

    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    let claim = hooks.claim_setup(id);
    hooks.reconcile_removed(&[id]).await.unwrap();
    let remaining = hooks.manager.list_network_instance_ids();
    hooks.stop();
    hooks.manager.delete_network_instance(vec![id]).unwrap();
    assert_eq!(claim.unwrap().0.generation, generation);
    assert_eq!(remaining, vec![id]);
}

#[tokio::test(start_paused = true)]
async fn retain_before_post_run_returns_to_waiting_config() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    hooks.manager.retain_network_instance(vec![]).unwrap();
    let monitor = tokio::spawn(hooks.clone().monitor());
    let _ = hooks.wait_for_setup(&id).await;
    let status = hooks.status().unwrap();
    hooks.stop();
    monitor.abort();
    assert_eq!(status.status, "idle", "{:?}", status.error);
    assert!(status.instance_id.is_none());
    assert!(status.options.is_none());
    assert!(status.error.is_none());
}

#[tokio::test]
async fn options_exist_before_instance_rpc_and_status_is_not_running() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    assert!(hooks
        .manager
        .get_instance_service(&config.get_id())
        .is_none());
    let status = hooks.status().unwrap();
    assert_eq!(status.status, "starting");
    assert_eq!(
        status.options.unwrap().ipv4.as_deref(),
        Some("10.42.0.1/24")
    );
}

#[tokio::test]
async fn concurrent_admission_reserves_the_single_instance_slot() {
    let hooks = hooks();
    let first = config();
    let second = config();
    let (a, b) = tokio::join!(hooks.reserve(&first), hooks.reserve(&second));
    assert_ne!(a.is_ok(), b.is_ok());
    assert!(hooks.reserve(&second).await.is_err());
}

async fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn same_id_overwrite_waits_for_the_old_claim_and_only_admits_the_latest() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    let original = hooks.status().unwrap().generation;
    let second = hooks.reserve(&config);
    tokio::pin!(second);
    poll_pending(second.as_mut()).await;
    let third = hooks.reserve(&config);
    tokio::pin!(third);
    poll_pending(third.as_mut()).await;
    assert!(second.await.is_err());
    assert_eq!(
        hooks.state.lock().unwrap().unclaimed_run,
        Some((config.get_id(), original))
    );
    // The old RPC may insert only after the replacements reach pre-run.
    hooks
        .manager
        .run_network_instance(config.clone(), false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    // The stale callback must consume its own reservation, never the new one.
    assert!(hooks.claim_setup(config.get_id()).is_err());
    assert!(hooks.manager.list_network_instance_ids().is_empty());
    third.await.unwrap();
    let (replacement, _) = hooks.claim_setup(config.get_id()).unwrap();
    assert_eq!(replacement.generation, hooks.status().unwrap().generation);
    assert_ne!(replacement.generation, original);
    hooks.fail(original, "obsolete start failed".into());
    assert!(hooks.status().unwrap().error.is_none());
}

#[tokio::test]
async fn stop_and_failure_cancel_a_waiting_overwrite() {
    for action in ["stop", "error"] {
        let hooks = hooks();
        let config = config();
        hooks.reserve(&config).await.unwrap();
        let replacement = hooks.reserve(&config);
        tokio::pin!(replacement);
        poll_pending(replacement.as_mut()).await;
        let generation = hooks.status().unwrap().generation;
        match action {
            "stop" => hooks.stop(),
            _ => hooks.fail(generation, "test failure".into()),
        }
        assert!(tokio::time::timeout(Duration::from_secs(1), replacement)
            .await
            .unwrap()
            .is_err());
        assert!(hooks.manager.list_network_instance_ids().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn waiting_overwrite_times_out_without_releasing_the_old_reservation() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    let original = hooks.status().unwrap().generation;
    let error = hooks.reserve(&config).await.unwrap_err();
    assert_eq!(error, "previous network instance post-run timed out");
    assert_eq!(
        hooks.status().unwrap().error.as_deref(),
        Some(error.as_str())
    );
    assert_eq!(
        hooks.state.lock().unwrap().unclaimed_run,
        Some((config.get_id(), original))
    );
    assert!(hooks.claim_setup(config.get_id()).is_err());
}

#[tokio::test]
async fn delete_hook_does_not_release_or_cancel_an_unclaimed_run() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    let generation = hooks.status().unwrap().generation;
    hooks.reconcile_removed(&[config.get_id()]).await.unwrap();
    assert_eq!(hooks.current_id(), Some(config.get_id()));
    assert_eq!(
        hooks.state.lock().unwrap().unclaimed_run,
        Some((config.get_id(), generation))
    );
    assert!(hooks.reserve(&self::config()).await.is_err());
    hooks
        .manager
        .run_network_instance(config.clone(), false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    assert_eq!(
        hooks.claim_setup(config.get_id()).unwrap().0.generation,
        generation
    );
    hooks.stop();
    hooks
        .manager
        .delete_network_instance(vec![config.get_id()])
        .unwrap();
}

#[tokio::test]
async fn startup_wait_is_cancellable_and_old_ack_cannot_complete_new_session() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    let old_generation = hooks.status().unwrap().generation;
    let id = config.get_id();
    hooks
        .manager
        .run_network_instance(config.clone(), false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    let setup = hooks.wait_for_setup(&id);
    tokio::pin!(setup);
    poll_pending(setup.as_mut()).await;
    hooks.stop();
    assert!(tokio::time::timeout(Duration::from_secs(1), setup)
        .await
        .unwrap()
        .is_err());
    hooks.manager.delete_network_instance(vec![id]).unwrap();
    assert!(hooks.status().unwrap().instance_id.is_none());
    assert!(hooks.reserve(&config).await.is_err());

    let next = Arc::new(InstanceCoordinator::new(
        Arc::new(NetworkInstanceManager::new()),
        None,
    ));
    next.reserve(&config).await.unwrap();
    assert_ne!(old_generation, next.status().unwrap().generation);
    assert!(next.complete_setup(old_generation, Ok(())).is_err());
    assert!(next.set_tun_fd(old_generation, -1).is_err());
}

#[tokio::test]
async fn deletion_cancels_pending_setup_and_clears_error_and_options() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    hooks.claim_setup(config.get_id()).unwrap();
    let generation = hooks.status().unwrap().generation;
    let (sender, receiver) = oneshot::channel();
    hooks.state.lock().unwrap().pending = Some(PendingSetup { generation, sender });
    hooks.reconcile_removed(&[config.get_id()]).await.unwrap();
    assert!(receiver.await.unwrap().is_err());
    assert!(hooks.complete_setup(generation, Ok(())).is_err());
    let status = hooks.status().unwrap();
    assert_eq!(status.status, "idle");
    assert!(status.options.is_none());
    assert!(status.error.is_none());
    assert!(status.generation > generation);
}

#[tokio::test]
async fn acknowledgement_requires_current_generation_and_is_one_shot() {
    let hooks = hooks();
    hooks.reserve(&config()).await.unwrap();
    let generation = hooks.status().unwrap().generation;
    hooks.state.lock().unwrap().active.as_mut().unwrap().phase = InstancePhase::awaiting_ack();
    let (sender, receiver) = oneshot::channel();
    hooks.state.lock().unwrap().pending = Some(PendingSetup { generation, sender });
    assert!(hooks.complete_setup(generation + 1, Ok(())).is_err());
    hooks.complete_setup(generation, Ok(())).unwrap();
    assert_eq!(receiver.await.unwrap(), Ok(()));
    assert!(hooks.complete_setup(generation, Ok(())).is_err());
}

#[tokio::test]
async fn cancelled_rpc_waiter_does_not_lose_successful_local_setup() {
    let hooks = hooks();
    hooks.reserve(&config()).await.unwrap();
    let generation = hooks.status().unwrap().generation;
    hooks.state.lock().unwrap().active.as_mut().unwrap().phase = InstancePhase::awaiting_ack();
    let (sender, receiver) = oneshot::channel();
    hooks.state.lock().unwrap().pending = Some(PendingSetup { generation, sender });
    drop(receiver);
    hooks.complete_setup(generation, Ok(())).unwrap();
    assert_eq!(hooks.status().unwrap().status, "running");
}

#[tokio::test]
async fn monitor_reports_an_early_tun_error_from_the_preinstalled_subscription() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    let (sender, receiver) = tokio::sync::broadcast::channel(8);
    {
        let mut state = hooks.state.lock().unwrap();
        state.active.as_mut().unwrap().phase = InstancePhase::awaiting_ack();
        state.events = Some(receiver);
    }
    sender
        .send(GlobalCtxEvent::TunDeviceError(
            "test TUN setup failure".into(),
        ))
        .unwrap();
    let monitor = tokio::spawn(hooks.clone().monitor());
    tokio::time::timeout(Duration::from_secs(2), async {
        while hooks.status().unwrap().error.is_none() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        hooks.status().unwrap().error.as_deref(),
        Some("test TUN setup failure")
    );
    hooks.stop();
    monitor.abort();
    hooks.manager.delete_network_instance(vec![id]).unwrap();
}

#[tokio::test]
async fn reserved_start_is_preserved_then_core_requires_swift_ack() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    let monitor = tokio::spawn(hooks.clone().monitor());
    // Before post-run, missing from the manager still means reserved.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hooks.status().unwrap().status, "starting");
    assert!(hooks.state.lock().unwrap().pending.is_none());
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    let task_hooks = hooks.clone();
    let task = tokio::spawn(async move { task_hooks.wait_for_setup(&id).await });
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if awaiting_ack(&hooks) {
                break;
            }
            assert!(
                !task.is_finished(),
                "initialization failed before acknowledgement"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(hooks.status().unwrap().status, "starting");
    let generation = hooks.status().unwrap().generation;
    hooks.complete_setup(generation, Ok(())).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(hooks.status().unwrap().status, "running");

    let service = hooks.manager.get_instance_service(&id).unwrap();
    use easytier::proto::{
        api::config::{InstanceConfigPatch, PatchConfigRequest, RoutePatch},
        common::{Ipv4Addr, Ipv4Inet},
    };
    service
        .get_config_service()
        .patch_config(
            Default::default(),
            PatchConfigRequest {
                patch: Some(InstanceConfigPatch {
                    routes: vec![RoutePatch {
                        action: 0,
                        cidr: Some(Ipv4Inet {
                            address: Some(Ipv4Addr { addr: 0x0a2b0000 }),
                            network_length: 16,
                        }),
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while hooks.status().unwrap().options.unwrap().routes != vec!["10.43.0.0/16"] {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    // Retain in 2.6.4 deletes without invoking reconcile_removed.
    hooks.manager.retain_network_instance(Vec::new()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while hooks.status().unwrap().instance_id.is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(hooks.status().unwrap().status, "idle");
    hooks.stop();
    monitor.abort();
    hooks.manager.delete_network_instance(vec![id]).unwrap();
}

#[tokio::test]
async fn ipv6_only_startup_and_hot_patch_keep_the_live_address() {
    use easytier::proto::api::config::{InstanceConfigPatch, PatchConfigRequest};

    let hooks = hooks();
    let monitor = tokio::spawn(hooks.clone().monitor());
    let config = TomlConfigLoader::new_from_str(
        "instance_name = 'ipv6-only'\nipv6 = 'fd42::1234/64'\nlisteners = []\n[flags]\nno_tun = true\nenable_ipv6 = false\n",
    )
    .unwrap();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    let task_hooks = hooks.clone();
    let setup = tokio::spawn(async move { task_hooks.wait_for_setup(&id).await });
    tokio::time::timeout(Duration::from_secs(8), async {
        while !awaiting_ack(&hooks) {
            assert!(!setup.is_finished(), "IPv6-only startup failed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = hooks.status().unwrap();
    let options = status.options.unwrap();
    assert_eq!(options.ipv6.as_deref(), Some("fd42::1234/64"));
    assert!(options.ipv4.is_none());
    hooks.complete_setup(status.generation, Ok(())).unwrap();
    setup.await.unwrap().unwrap();

    // Use the real core patch API, then observe the same snapshot serialized
    // for Swift. A NetworkConfig roundtrip drops this IPv6 field in 2.6.4.
    let service = hooks.manager.get_instance_service(&id).unwrap();
    for address in ["fd43::5678/64", "fd44::9/96"] {
        let patch = TomlConfigLoader::new_from_str(&format!("ipv6 = '{address}'")).unwrap();
        service
            .get_config_service()
            .patch_config(
                Default::default(),
                PatchConfigRequest {
                    patch: Some(InstanceConfigPatch {
                        ipv6: patch.get_ipv6().map(Into::into),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = serde_json::to_value(hooks.status().unwrap()).unwrap();
                if status["options"]["ipv6"] == address {
                    assert_eq!(status["instanceName"], "ipv6-only");
                    assert_eq!(status["status"], "running");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    hooks.stop();
    monitor.abort();
    hooks.manager.delete_network_instance(vec![id]).unwrap();
}

#[tokio::test]
async fn stop_removes_a_late_upstream_insertion() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks.stop();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    assert!(hooks.wait_for_setup(&id).await.is_err());
    assert!(hooks.manager.list_network_instance_ids().is_empty());
}

#[tokio::test]
async fn overwrite_cancels_previous_pending_setup() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    let generation = hooks.status().unwrap().generation;
    hooks.claim_setup(config.get_id()).unwrap();
    let (sender, receiver) = oneshot::channel();
    hooks.state.lock().unwrap().pending = Some(PendingSetup { generation, sender });
    hooks.reserve(&config).await.unwrap();
    assert!(receiver.await.unwrap().is_err());
    assert!(hooks.complete_setup(generation, Ok(())).is_err());
    assert!(hooks.status().unwrap().generation > generation);
}

#[tokio::test]
async fn retained_away_instance_does_not_block_immediate_replacement() {
    for phase in [InstancePhase::initializing(), InstancePhase::Running] {
        let hooks = hooks();
        let first = config();
        hooks.reserve(&first).await.unwrap();
        hooks.claim_setup(first.get_id()).unwrap();
        // Retain removed the instance before the monitor observed it.
        hooks.state.lock().unwrap().active.as_mut().unwrap().phase = phase;
        let replacement = config();
        hooks.reserve(&replacement).await.unwrap();
        assert_eq!(hooks.current_id(), Some(replacement.get_id()));
    }
}

#[tokio::test(start_paused = true)]
async fn cancelled_initialization_wait_is_still_monitored() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    hooks.claim_setup(id).unwrap();
    // The RPC waiter disappears after claiming post-run. The monitor still
    // owns cleanup, even before settings have been sent to Swift.
    hooks.manager.retain_network_instance(vec![]).unwrap();
    let monitor = tokio::spawn(hooks.clone().monitor());
    tokio::time::timeout(Duration::from_secs(1), async {
        while hooks.current_id().is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(hooks.status().unwrap().status, "idle");
    assert!(hooks.status().unwrap().error.is_none());
    hooks.stop();
    monitor.abort();
}

#[tokio::test(start_paused = true)]
async fn stale_phase_timeout_cannot_fail_a_running_instance() {
    let hooks = hooks();
    hooks.reserve(&config()).await.unwrap();
    let generation = hooks.status().unwrap().generation;
    let phase = InstancePhase::awaiting_ack();
    let (sender, receiver) = oneshot::channel();
    {
        let mut state = hooks.state.lock().unwrap();
        state.active.as_mut().unwrap().phase = phase;
        state.pending = Some(PendingSetup { generation, sender });
    }
    tokio::time::advance(SETUP_TIMEOUT - Duration::from_millis(1)).await;
    hooks.complete_setup(generation, Ok(())).unwrap();
    assert_eq!(receiver.await.unwrap(), Ok(()));

    // The monitor's snapshot was taken before the acknowledgement committed.
    tokio::time::advance(Duration::from_millis(1)).await;
    let error = phase.expired_error().unwrap();
    hooks.fail_in_phase(generation, Some(phase), error.into());
    assert_eq!(hooks.status().unwrap().status, "running");
    assert!(hooks.status().unwrap().error.is_none());
}

#[tokio::test]
async fn acknowledgement_timeout_is_unchanged_when_rpc_is_cancelled() {
    for cancel_rpc in [false, true] {
        let hooks = hooks();
        let monitor = tokio::spawn(hooks.clone().monitor());
        let config = config();
        let id = config.get_id();
        hooks.reserve(&config).await.unwrap();
        hooks
            .manager
            .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
            .unwrap();
        let task_hooks = hooks.clone();
        let setup = tokio::spawn(async move { task_hooks.wait_for_setup(&id).await });
        tokio::time::timeout(Duration::from_secs(8), async {
            while !awaiting_ack(&hooks) {
                assert!(
                    !setup.is_finished(),
                    "startup failed before acknowledgement"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if cancel_rpc {
            setup.abort();
        }

        // Freeze only after the real core has initialized on its own runtime.
        tokio::time::pause();
        let deadline = hooks
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .phase
            .timeout()
            .unwrap()
            .0;
        tokio::task::yield_now().await;
        tokio::time::advance(deadline - Instant::now() - Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        assert!(hooks.status().unwrap().error.is_none());
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while hooks.status().unwrap().error.is_none() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("monitor and RPC must expire at the same acknowledgement deadline");
        assert_eq!(
            hooks.status().unwrap().error.as_deref(),
            Some("instance setup timed out")
        );
        if cancel_rpc {
            assert!(setup.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(setup.await.unwrap(), Err("instance setup timed out".into()));
        }
        hooks.stop();
        monitor.abort();
        tokio::time::resume();
        hooks.manager.delete_network_instance(vec![id]).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn missing_post_run_times_out_with_original_error() {
    let hooks = hooks();
    hooks.reserve(&config()).await.unwrap();
    let monitor = tokio::spawn(hooks.clone().monitor());
    tokio::time::timeout(Duration::from_secs(15), async {
        while hooks.status().unwrap().error.is_none() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let status = hooks.status().unwrap();
    assert_eq!(status.status, "error");
    assert_eq!(
        status.error.as_deref(),
        Some("network instance did not finish initialization")
    );
    assert!(hooks.state.lock().unwrap().pending.is_none());
    hooks.stop();
    monitor.abort();
}

#[tokio::test]
async fn stale_failure_and_delete_cannot_change_replacement() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    let old_generation = hooks.status().unwrap().generation;
    hooks.claim_setup(config.get_id()).unwrap();
    hooks.state.lock().unwrap().active.as_mut().unwrap().phase = InstancePhase::Running;
    hooks.reserve(&config).await.unwrap();
    hooks.fail(old_generation, "old failure".into());
    hooks.removed(old_generation);
    let status = hooks.status().unwrap();
    assert_eq!(status.status, "starting");
    assert_eq!(status.instance_id, Some(config.get_id().to_string()));
    assert!(status.error.is_none());
}

#[tokio::test]
async fn cancelled_initialization_rpc_still_emits_run_and_accepts_swift_ack() {
    static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    extern "C" fn record_event(json: *const std::ffi::c_char) {
        let json = unsafe { std::ffi::CStr::from_ptr(json) }.to_str().unwrap();
        let event: serde_json::Value = serde_json::from_str(json).unwrap();
        EVENTS
            .lock()
            .unwrap()
            .push(event["event"].as_str().unwrap().to_owned());
    }
    let hooks = Arc::new(InstanceCoordinator::new(
        Arc::new(NetworkInstanceManager::new()),
        Some(record_event),
    ));
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    // Drop the real post-run future after it claims the instance but before
    // the session monitor can advance initialization.
    let mut rpc = Box::pin(hooks.wait_for_setup(&id));
    poll_pending(rpc.as_mut()).await;
    let generation = hooks.status().unwrap().generation;
    assert!(hooks.complete_setup(generation, Ok(())).is_err());
    drop(rpc);
    let monitor = tokio::spawn(hooks.clone().monitor());
    tokio::time::timeout(Duration::from_secs(8), async {
        while !awaiting_ack(&hooks) {
            assert!(hooks.status().unwrap().error.is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(hooks.manager.get_instance_service(&id).is_some());
    hooks.complete_setup(generation, Ok(())).unwrap();
    tokio::time::pause();
    tokio::time::advance(START_TIMEOUT + SETUP_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert_eq!(hooks.status().unwrap().status, "running");
    assert!(hooks.status().unwrap().error.is_none());
    assert_eq!(
        EVENTS
            .lock()
            .unwrap()
            .iter()
            .filter(|v| *v == "run")
            .count(),
        1
    );
    hooks.stop();
    monitor.abort();
    tokio::time::resume();
    hooks.manager.delete_network_instance(vec![id]).unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_and_failed_monitor_have_no_periodic_wakeups() {
    use std::future::Future;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Wake, Waker};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    for failed in [false, true] {
        let hooks = hooks();
        if failed {
            hooks.reserve(&config()).await.unwrap();
            hooks.fail(hooks.status().unwrap().generation, "test failure".into());
        }
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut context = Context::from_waker(&waker);
        let mut monitor = Box::pin(hooks.clone().monitor());
        assert!(monitor.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
        hooks.stop();
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert!(monitor.as_mut().poll(&mut context).is_ready());
    }
}

#[tokio::test(start_paused = true)]
async fn config_events_and_tun_errors_are_handled_without_a_timer_tick() {
    let hooks = hooks();
    let config = config();
    let id = config.get_id();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config.clone(), false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    let (sender, receiver) = tokio::sync::broadcast::channel(8);
    {
        let mut state = hooks.state.lock().unwrap();
        state.active.as_mut().unwrap().phase = InstancePhase::Running;
        state.events = Some(receiver);
    }
    let mut monitor = Box::pin(hooks.clone().monitor());
    poll_pending(monitor.as_mut()).await;
    let now = Instant::now();
    config.set_routes(Some(vec!["10.99.0.0/16".parse().unwrap()]));
    sender
        .send(GlobalCtxEvent::ProxyCidrsUpdated(vec![], vec![]))
        .unwrap();
    poll_pending(monitor.as_mut()).await;
    assert_eq!(
        hooks.status().unwrap().options.unwrap().routes,
        vec!["10.99.0.0/16"]
    );
    sender
        .send(GlobalCtxEvent::TunDeviceError("immediate TUN error".into()))
        .unwrap();
    poll_pending(monitor.as_mut()).await;
    assert_eq!(
        hooks.status().unwrap().error.as_deref(),
        Some("immediate TUN error")
    );
    assert_eq!(
        Instant::now(),
        now,
        "events must not wait for lifecycle reconciliation"
    );
    hooks.stop();
    monitor.await;
    hooks.manager.delete_network_instance(vec![id]).unwrap();
}

#[tokio::test(start_paused = true)]
async fn retain_without_a_closed_event_channel_uses_lifecycle_fallback() {
    let hooks = hooks();
    let config = config();
    hooks.reserve(&config).await.unwrap();
    hooks
        .manager
        .run_network_instance(config, false, ConfigFileControl::STATIC_CONFIG)
        .unwrap();
    // Keep a sender alive to simulate the upstream lifecycle notification gap.
    let (_sender, receiver) = tokio::sync::broadcast::channel(8);
    {
        let mut state = hooks.state.lock().unwrap();
        state.active.as_mut().unwrap().phase = InstancePhase::Running;
        state.events = Some(receiver);
    }
    let mut monitor = Box::pin(hooks.clone().monitor());
    poll_pending(monitor.as_mut()).await;
    hooks.manager.retain_network_instance(vec![]).unwrap();
    tokio::time::advance(LIFECYCLE_RECHECK - Duration::from_millis(1)).await;
    poll_pending(monitor.as_mut()).await;
    assert_eq!(hooks.status().unwrap().status, "running");
    tokio::time::advance(Duration::from_millis(1)).await;
    poll_pending(monitor.as_mut()).await;
    assert_eq!(hooks.status().unwrap().status, "idle");
    assert!(hooks.status().unwrap().options.is_none());
    hooks.stop();
    monitor.await;
}

#[tokio::test]
async fn local_config_uses_shared_readiness_acknowledgement_and_updates() {
    let coordinator = hooks();
    let config = config();
    let id = config.get_id();
    coordinator.start_local(config).await.unwrap();
    let generation = coordinator.status().unwrap().generation;
    assert_eq!(coordinator.status().unwrap().status, "starting");
    assert!(coordinator.complete_setup(generation, Ok(())).is_err());
    let monitor = tokio::spawn(coordinator.clone().monitor());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !awaiting_ack(&coordinator) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(coordinator.set_tun_fd(generation + 1, -1).is_err());
    coordinator.complete_setup(generation, Ok(())).unwrap();
    assert_eq!(coordinator.status().unwrap().status, "running");

    use easytier::proto::{
        api::config::{InstanceConfigPatch, PatchConfigRequest, RoutePatch},
        common::{Ipv4Addr, Ipv4Inet},
    };
    coordinator
        .manager
        .get_instance_service(&id)
        .unwrap()
        .get_config_service()
        .patch_config(
            Default::default(),
            PatchConfigRequest {
                patch: Some(InstanceConfigPatch {
                    routes: vec![RoutePatch {
                        action: 0,
                        cidr: Some(Ipv4Inet {
                            address: Some(Ipv4Addr { addr: 0x0a630000 }),
                            network_length: 16,
                        }),
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while coordinator.status().unwrap().options.unwrap().routes != vec!["10.99.0.0/16"] {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    coordinator.stop();
    monitor.await.unwrap();
    coordinator
        .manager
        .delete_network_instance(vec![id])
        .unwrap();
    assert!(coordinator.current_id().is_none());
    assert!(coordinator.complete_setup(generation, Ok(())).is_err());
}
