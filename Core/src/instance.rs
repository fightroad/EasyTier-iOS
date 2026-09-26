use super::{InstanceCallback, InstanceStatus, TunnelOptions};
use easytier::{
    common::{
        config::{ConfigLoader, TomlConfigLoader},
        global_ctx::{EventBusSubscriber, GlobalCtxEvent},
    },
    instance_manager::NetworkInstanceManager,
};
use std::{
    ffi::CString,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{oneshot, Notify};
use tokio::time::Instant;
use uuid::Uuid;

// Do not reuse generations when the extension stops and starts another tunnel session.
static GENERATION: AtomicU64 = AtomicU64::new(0);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);
// Upstream 2.6.4 has no API-ready notification or reliable retain/exit hook.
const INITIALIZATION_RECHECK: Duration = Duration::from_millis(250);
const LIFECYCLE_RECHECK: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstancePhase {
    AwaitingAdmission(Instant),
    Reserved(Instant),
    Initializing(Instant),
    AwaitingAck(Instant),
    Running,
}

impl InstancePhase {
    fn awaiting_admission() -> Self {
        Self::AwaitingAdmission(Instant::now() + START_TIMEOUT)
    }

    fn reserved() -> Self {
        Self::Reserved(Instant::now() + START_TIMEOUT)
    }

    fn initializing() -> Self {
        Self::Initializing(Instant::now() + START_TIMEOUT)
    }

    fn awaiting_ack() -> Self {
        Self::AwaitingAck(Instant::now() + SETUP_TIMEOUT)
    }

    fn is_inserted(self) -> bool {
        !matches!(self, Self::AwaitingAdmission(_) | Self::Reserved(_))
    }

    // Admission waits share their deadline with the monitor. After post-run,
    // the monitor owns both initialization and Swift acknowledgement deadlines.
    fn timeout(self) -> Option<(Instant, &'static str)> {
        match self {
            Self::AwaitingAdmission(deadline) => {
                Some((deadline, "previous network instance post-run timed out"))
            }
            Self::Reserved(deadline) => {
                Some((deadline, "network instance did not finish initialization"))
            }
            Self::Initializing(deadline) => {
                Some((deadline, "network instance initialization timed out"))
            }
            Self::AwaitingAck(deadline) => Some((deadline, "instance setup timed out")),
            Self::Running => None,
        }
    }

    fn expired_error(self) -> Option<&'static str> {
        self.timeout()
            .filter(|(deadline, _)| Instant::now() >= *deadline)
            .map(|(_, error)| error)
    }
}

#[derive(Clone)]
struct Instance {
    id: Uuid,
    name: String,
    network: String,
    generation: u64,
    options: TunnelOptions,
    // Clones share the core's live config, including fields omitted by the
    // upstream NetworkConfig RPC representation (notably IPv6).
    config: TomlConfigLoader,
    phase: InstancePhase,
}

struct PendingSetup {
    generation: u64,
    sender: oneshot::Sender<Result<(), String>>,
}

#[derive(Default)]
struct State {
    active: Option<Instance>,
    pending: Option<PendingSetup>,
    // Upstream post-run carries only an ID. Keep its generation reserved until
    // that callback claims it, including across same-ID overwrites.
    unclaimed_run: Option<(Uuid, u64)>,
    stopping: bool,
    error: Option<String>,
    generation: u64,
    events: Option<EventBusSubscriber>,
}

pub(super) struct InstanceCoordinator {
    pub manager: Arc<NetworkInstanceManager>,
    callback: InstanceCallback,
    state: Mutex<State>,
    setup_changed: Notify,
}

impl TunnelOptions {
    fn from_config(config: &TomlConfigLoader) -> Self {
        let flags = config.get_flags();
        Self {
            ipv4: config.get_ipv4().map(|v| v.to_string()),
            ipv6: config.get_ipv6().map(|v| v.to_string()),
            mtu: Some(flags.mtu),
            routes: config
                .get_routes()
                .unwrap_or_default()
                .into_iter()
                .map(|v| v.to_string())
                .collect(),
            magic_dns: flags.accept_dns,
            dns: Vec::new(),
        }
    }
}

impl InstanceCoordinator {
    pub fn new(manager: Arc<NetworkInstanceManager>, callback: InstanceCallback) -> Self {
        Self {
            manager,
            callback,
            state: Mutex::new(State::default()),
            setup_changed: Notify::new(),
        }
    }

    // Both configuration sources reserve and claim the same lifecycle. Local
    // startup returns after insertion; Swift completes setup asynchronously.
    pub async fn start_local(&self, config: TomlConfigLoader) -> Result<(), String> {
        self.reserve(&config).await?;
        let id = self
            .manager
            .run_network_instance(
                config,
                false,
                easytier::common::config::ConfigFileControl::STATIC_CONFIG,
            )
            .map_err(|error| error.to_string())?;
        let _ = self.claim_setup(id)?;
        Ok(())
    }

    pub fn current_id(&self) -> Option<Uuid> {
        self.state.lock().ok()?.active.as_ref().map(|v| v.id)
    }

    pub fn set_tun_fd(&self, generation: u64, fd: i32) -> Result<(), String> {
        let state = self.state.lock().map_err(|e| e.to_string())?;
        let active = state
            .active
            .as_ref()
            .filter(|v| {
                v.generation == generation
                    && matches!(
                        v.phase,
                        InstancePhase::AwaitingAck(_) | InstancePhase::Running
                    )
            })
            .ok_or("network instance was superseded")?;
        if state.stopping || state.error.is_some() {
            return Err("network instance is stopping".into());
        }
        self.manager
            .set_tun_fd(&active.id, fd)
            .map_err(|e| e.to_string())
    }

    fn emit(&self, event: &str, instance: &Instance) {
        let Some(callback) = self.callback else {
            return;
        };
        let json = serde_json::json!({
            "event": event, "instance_id": instance.id, "instance_name": instance.name,
            "network_name": instance.network, "generation": instance.generation,
        });
        if let Ok(json) = CString::new(json.to_string()) {
            callback(json.as_ptr());
        }
    }

    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopping = true;
        if let Some(pending) = state.pending.take() {
            let _ = pending.sender.send(Err("tunnel session stopped".into()));
        }
        state.active = None;
        state.events = None;
        self.setup_changed.notify_waiters();
    }

    fn claim_setup(
        &self,
        id: Uuid,
    ) -> Result<(Instance, oneshot::Receiver<Result<(), String>>), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if state.stopping {
            // Upstream may insert after stop() drained this old manager.
            drop(state);
            self.manager
                .delete_network_instance(vec![id])
                .map_err(|e| e.to_string())?;
            return Err("tunnel session is stopping".into());
        }
        let (reserved_id, generation) = state
            .unclaimed_run
            .ok_or("network instance setup is already being handled")?;
        if reserved_id != id {
            return Err("network instance was superseded".into());
        }
        state.unclaimed_run = None;
        self.setup_changed.notify_waiters();
        if let Some(active) = state
            .active
            .as_mut()
            .filter(|v| v.id == id && v.generation == generation)
        {
            // Upstream calls post-run only after insertion. From this point,
            // absence from the manager means removal, not delayed insertion.
            active.phase = InstancePhase::initializing();
            let instance = active.clone();
            let (sender, receiver) = oneshot::channel();
            state.pending = Some(PendingSetup { generation, sender });
            return Ok((instance, receiver));
        }
        // An overwrite may supersede this reservation before insertion.
        // No replacement can be admitted until this lock is released.
        self.manager
            .delete_network_instance(vec![id])
            .map_err(|e| e.to_string())?;
        Err("network instance was superseded".into())
    }

    pub fn complete_setup(
        &self,
        generation: u64,
        result: Result<(), String>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if state.stopping
            || state.pending.as_ref().map(|v| v.generation) != Some(generation)
            || !state.active.as_ref().is_some_and(|v| {
                v.generation == generation && matches!(v.phase, InstancePhase::AwaitingAck(_))
            })
        {
            return Err("stale or cancelled instance setup acknowledgement".into());
        }
        let pending = state.pending.take().unwrap();
        let error = result.as_ref().err().cloned();
        if error.is_none() {
            if let Some(active) = state.active.as_mut() {
                active.phase = InstancePhase::Running;
            }
        }
        drop(state);
        self.setup_changed.notify_waiters();
        // A disconnected server may cancel the RPC waiter, while local setup
        // still succeeds. Commit the acknowledgement independently of it.
        let _ = pending.sender.send(result);
        if let Some(error) = error {
            self.fail(generation, error);
        }
        Ok(())
    }

    pub fn status(&self) -> Result<InstanceStatus, String> {
        let state = self.state.lock().map_err(|e| e.to_string())?;
        let active = state.active.as_ref();
        Ok(InstanceStatus {
            status: if state.error.is_some() {
                "error"
            } else if active.is_some_and(|v| v.phase == InstancePhase::Running) {
                "running"
            } else if active.is_some() {
                "starting"
            } else {
                "idle"
            },
            server_connected: false,
            instance_id: active.map(|v| v.id.to_string()),
            instance_name: active.map(|v| v.name.clone()),
            network_name: active.map(|v| v.network.clone()),
            generation: state.generation,
            error: state.error.clone(),
            options: active.map(|v| v.options.clone()),
        })
    }

    fn fail(&self, generation: u64, message: String) {
        self.fail_in_phase(generation, None, message);
    }

    fn fail_in_phase(&self, generation: u64, phase: Option<InstancePhase>, message: String) {
        let instance = {
            let mut state = self.state.lock().unwrap();
            if state.stopping
                || state.error.is_some()
                || state.active.as_ref().map(|v| v.generation) != Some(generation)
                || phase.is_some_and(|phase| state.active.as_ref().map(|v| v.phase) != Some(phase))
            {
                return;
            }
            state.error = Some(message.clone());
            if let Some(pending) = state.pending.take() {
                let _ = pending.sender.send(Err(message));
            }
            state.active.as_ref().unwrap().clone()
        };
        self.setup_changed.notify_waiters();
        self.emit("error", &instance);
    }

    fn removed(&self, generation: u64) {
        let instance = {
            let mut state = self.state.lock().unwrap();
            if state.stopping || state.active.as_ref().map(|v| v.generation) != Some(generation) {
                return;
            }
            if let Some(pending) = state.pending.take() {
                let _ = pending
                    .sender
                    .send(Err("network instance was removed during setup".into()));
            }
            let mut instance = state.active.take().unwrap();
            instance.generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
            state.generation = instance.generation;
            state.error = None;
            state.events = None;
            instance
        };
        self.setup_changed.notify_waiters();
        self.emit("delete", &instance);
    }

    // Notifications drive configuration changes and exact phase deadlines drive
    // timeouts. Poll only the lifecycle facts not exposed by upstream 2.6.4:
    // API readiness during initialization and retain/exit reconciliation later.
    pub async fn monitor(self: Arc<Self>) {
        let mut subscription: Option<EventBusSubscriber> = None;
        let mut subscribed_generation = 0;
        let mut next_check = Instant::now();
        loop {
            // Create before reading state: notify_waiters must not be lost while
            // choosing which event/deadline to await.
            let changed = self.setup_changed.notified();
            let instance = {
                let state = self.state.lock().unwrap();
                if state.stopping {
                    return;
                }
                if state.error.is_some() {
                    None
                } else {
                    state.active.clone()
                }
            };
            let Some(instance) = instance else {
                subscription = None;
                subscribed_generation = 0;
                changed.await;
                continue;
            };
            if !instance.phase.is_inserted() {
                subscription = None;
                subscribed_generation = 0;
                let (deadline, error) = instance.phase.timeout().unwrap();
                tokio::select! {
                    _ = changed => {},
                    _ = tokio::time::sleep_until(deadline) => {
                        self.fail_in_phase(instance.generation, Some(instance.phase), error.into());
                    }
                }
                continue;
            }
            if subscribed_generation != instance.generation {
                let mut state = self.state.lock().unwrap();
                if state.active.as_ref().map(|v| v.generation) != Some(instance.generation) {
                    continue;
                }
                subscription = state.events.take().or_else(|| {
                    self.manager
                        .iter()
                        .find(|v| *v.key() == instance.id)
                        .and_then(|v| v.subscribe_event())
                });
                subscribed_generation = instance.generation;
                next_check = Instant::now();
            }
            if Instant::now() >= next_check {
                let health = self
                    .manager
                    .iter()
                    .find(|v| *v.key() == instance.id)
                    .map(|v| (v.is_easytier_running(), v.get_latest_error_msg()));
                match health {
                    None => {
                        self.removed(instance.generation);
                        continue;
                    }
                    Some((false, error)) => {
                        self.fail(
                            instance.generation,
                            error.unwrap_or_else(|| "network instance stopped unexpectedly".into()),
                        );
                        continue;
                    }
                    _ => {}
                }
                if let Some(error) = instance.phase.expired_error() {
                    self.fail_in_phase(instance.generation, Some(instance.phase), error.into());
                    continue;
                }
                if matches!(instance.phase, InstancePhase::Initializing(_)) {
                    if self.manager.get_instance_service(&instance.id).is_some() {
                        let metadata = {
                            let mut state = self.state.lock().unwrap();
                            if state.stopping || state.error.is_some() {
                                continue;
                            }
                            let Some(active) = state.active.as_mut().filter(|v| {
                                v.generation == instance.generation && v.phase == instance.phase
                            }) else {
                                continue;
                            };
                            active.options = TunnelOptions::from_config(&active.config);
                            active.phase = InstancePhase::awaiting_ack();
                            active.clone()
                        };
                        // The event subscription is installed before Swift can
                        // attach its descriptor, including synchronous callbacks.
                        next_check = Instant::now() + LIFECYCLE_RECHECK;
                        self.emit("run", &metadata);
                        continue;
                    }
                    next_check = Instant::now() + INITIALIZATION_RECHECK;
                } else {
                    next_check = Instant::now() + LIFECYCLE_RECHECK;
                }
            }
            let deadline = instance.phase.timeout().map(|v| v.0);
            let wake_at = deadline.map_or(next_check, |v| v.min(next_check));
            tokio::select! {
                _ = changed => {},
                _ = tokio::time::sleep_until(wake_at) => {
                    if let Some(error) = instance.phase.expired_error() {
                        self.fail_in_phase(instance.generation, Some(instance.phase), error.into());
                    }
                },
                event = async {
                    match subscription.as_mut() {
                        Some(events) => events.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Ok(GlobalCtxEvent::TunDeviceError(error)) => self.fail(instance.generation, error),
                        Ok(GlobalCtxEvent::DhcpIpv4Changed(_, _)
                            | GlobalCtxEvent::DhcpIpv4Conflicted(_)
                            | GlobalCtxEvent::PublicIpv6Changed(_, _)
                            | GlobalCtxEvent::PublicIpv6RoutesUpdated(_, _)
                            | GlobalCtxEvent::ProxyCidrsUpdated(_, _)
                            | GlobalCtxEvent::ConfigPatched(_))
                        | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Before run, refresh only the cached options. Swift
                            // must not receive update before initialization completes.
                            self.refresh_options(instance.generation);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            subscription = None;
                            next_check = Instant::now();
                        }
                        Ok(_) => {}
                    }
                }
            }
        }
    }

    fn refresh_options(&self, generation: u64) {
        let updated = {
            let mut state = self.state.lock().unwrap();
            if state.stopping || state.error.is_some() {
                return;
            }
            let Some(active) = state.active.as_mut().filter(|v| v.generation == generation) else {
                return;
            };
            active.options = TunnelOptions::from_config(&active.config);
            active.name = active.config.get_inst_name();
            active.network = active.config.get_network_identity().network_name;
            if !matches!(
                active.phase,
                InstancePhase::AwaitingAck(_) | InstancePhase::Running
            ) {
                return;
            }
            active.clone()
        };
        self.emit("update", &updated);
    }
}

impl InstanceCoordinator {
    pub async fn reserve(&self, config: &TomlConfigLoader) -> Result<(), String> {
        let (generation, phase) = {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            if state.stopping {
                return Err("tunnel session is stopping".into());
            }
            if state.unclaimed_run.is_some_and(|(id, _)| {
                id != config.get_id() || state.active.as_ref().map(|v| v.id) != Some(id)
            }) {
                return Err("another network instance start is awaiting its post-run hook".into());
            }
            if let Some(active) = &state.active {
                if active.id != config.get_id()
                    && (!active.phase.is_inserted()
                        || self
                            .manager
                            .list_network_instance_ids()
                            .contains(&active.id))
                {
                    return Err("Apple client supports one network instance".into());
                }
            }
            if let Some(pending) = state.pending.take() {
                let _ = pending
                    .sender
                    .send(Err("network instance was superseded".into()));
            }
            let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
            state.generation = generation;
            // Keep the old callback's reservation until it is claimed. Upstream
            // has already deleted the old instance before invoking this hook for
            // an overwrite, so rejecting the same ID would strand both requests.
            // Publishing the replacement now invalidates old acknowledgements.
            let waiting_for_post = state.unclaimed_run.is_some();
            let phase = if waiting_for_post {
                InstancePhase::awaiting_admission()
            } else {
                InstancePhase::reserved()
            };
            if !waiting_for_post {
                state.unclaimed_run = Some((config.get_id(), generation));
            }
            state.error = None;
            state.events = None;
            // Keep the received config available even before the core RPC exists.
            state.active = Some(Instance {
                id: config.get_id(),
                name: config.get_inst_name(),
                network: config.get_network_identity().network_name,
                generation,
                options: TunnelOptions::from_config(config),
                config: config.clone(),
                phase,
            });
            if !waiting_for_post {
                // Once reserved, let upstream reach post-run even if another
                // thread supersedes us immediately after releasing this lock.
                // Returning an error would leave a reservation nobody claims.
                self.setup_changed.notify_waiters();
                return Ok(());
            }
            (generation, phase)
        };
        self.setup_changed.notify_waiters();
        let (deadline, timeout_error) = phase.timeout().expect("admission has a deadline");
        let admission = tokio::time::timeout_at(deadline, async {
            loop {
                // Register before checking state so a claim/stop cannot lose
                // its wakeup between unlocking and awaiting the notification.
                let changed = self.setup_changed.notified();
                {
                    let mut state = self.state.lock().map_err(|e| e.to_string())?;
                    if state.stopping
                        || state.error.is_some()
                        || state.active.as_ref().map(|v| v.generation) != Some(generation)
                    {
                        return Err("network instance setup was cancelled".to_string());
                    }
                    match state.unclaimed_run {
                        None => {
                            state.unclaimed_run = Some((config.get_id(), generation));
                            state.active.as_mut().unwrap().phase = InstancePhase::reserved();
                            self.setup_changed.notify_waiters();
                            return Ok(());
                        }
                        Some((_, reserved)) if reserved == generation => return Ok(()),
                        _ => {}
                    }
                }
                // Do not let upstream insert the replacement until the old
                // post-run has consumed its ID-only callback and cleaned up.
                changed.await;
            }
        })
        .await;
        admission.unwrap_or_else(|_| {
            let error = timeout_error.to_string();
            self.fail(generation, error.clone());
            Err(error)
        })
    }

    pub async fn wait_for_setup(&self, id: &Uuid) -> Result<(), String> {
        let (instance, receiver) = self.claim_setup(*id)?;
        // The session monitor owns initialization and acknowledgement deadlines.
        // Dropping this RPC waiter must not cancel the local tunnel setup.
        receiver
            .await
            .map_err(|_| "instance setup acknowledgement was cancelled".to_string())??;
        let state = self.state.lock().map_err(|e| e.to_string())?;
        if state.stopping || state.error.is_some() {
            return Err("network instance setup was cancelled".into());
        }
        state
            .active
            .as_ref()
            .filter(|v| v.generation == instance.generation)
            .ok_or("network instance setup was superseded")?;
        Ok(())
    }

    pub async fn reconcile_removed(&self, ids: &[Uuid]) -> Result<(), String> {
        let active = self.state.lock().map_err(|e| e.to_string())?.active.clone();
        if let Some(active) = active.filter(|v| v.phase.is_inserted() && ids.contains(&v.id)) {
            // This ID-only callback may belong to an older delete RPC. A
            // reservation has not reached post-run yet, so absence is expected.
            // Once insertion is confirmed, reconcile against the actual manager.
            if !self
                .manager
                .list_network_instance_ids()
                .contains(&active.id)
            {
                self.removed(active.generation);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
