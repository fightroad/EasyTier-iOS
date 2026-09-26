//! Adapt server mutations to the same instance lifecycle used by local config.
use crate::{
    instance::InstanceCoordinator, InstanceCallback, InstanceStatus, ManagedInstance, CONTEXT,
};
use easytier::{
    common::{config::TomlConfigLoader, MachineIdOptions},
    instance_manager::NetworkInstanceManager,
    tunnel::TunnelScheme,
    web_client::{run_web_client, WebClient, WebClientHooks},
};
use std::sync::Arc;
use uuid::Uuid;

pub(super) struct AppleWebHooks {
    instance: Arc<InstanceCoordinator>,
}

impl AppleWebHooks {
    pub fn new(instance: Arc<InstanceCoordinator>) -> Self {
        Self { instance }
    }
}

#[async_trait::async_trait]
impl WebClientHooks for AppleWebHooks {
    async fn pre_run_network_instance(&self, config: &TomlConfigLoader) -> Result<(), String> {
        self.instance.reserve(config).await
    }

    async fn post_run_network_instance(&self, id: &Uuid) -> Result<(), String> {
        self.instance.wait_for_setup(id).await
    }

    async fn post_remove_network_instances(&self, ids: &[Uuid]) -> Result<(), String> {
        self.instance.reconcile_removed(ids).await
    }
}

pub(super) struct ManagedWebClient {
    client: WebClient,
    pub instance: ManagedInstance,
}

impl ManagedWebClient {
    pub fn start(
        url: &str,
        machine_id: String,
        hostname: Option<String>,
        secure_mode: bool,
        callback: InstanceCallback,
    ) -> Result<Self, String> {
        let url = normalize_config_server_endpoint(url)?;
        let coordinator = Arc::new(InstanceCoordinator::new(
            Arc::new(NetworkInstanceManager::new()),
            callback,
        ));
        let client = CONTEXT
            .runtime
            .block_on(run_web_client(
                &url,
                MachineIdOptions {
                    explicit_machine_id: Some(machine_id),
                    state_dir: None,
                },
                hostname,
                secure_mode,
                coordinator.manager.clone(),
                Some(Arc::new(AppleWebHooks::new(coordinator.clone()))),
            ))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            instance: ManagedInstance::new(coordinator),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    pub fn status(&self) -> Result<InstanceStatus, String> {
        let mut status = self.instance.coordinator.status()?;
        status.server_connected = self.is_connected();
        if status.status == "idle" {
            status.status = if status.server_connected {
                "waiting_config"
            } else {
                "connecting_server"
            };
        }
        Ok(status)
    }

    pub fn stop(self) -> Result<(), String> {
        self.instance.coordinator.stop();
        drop(self.client);
        self.instance.stop()
    }
}

pub(super) fn normalize_config_server_endpoint(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("config server token is empty".to_string());
    }
    let endpoint = if input.contains("://") {
        input.to_string()
    } else {
        if input.contains('/') || input.chars().any(char::is_whitespace) {
            return Err("invalid config server token".to_string());
        }
        let mut endpoint = url::Url::parse("udp://config-server.easytier.cn:22020").unwrap();
        endpoint
            .path_segments_mut()
            .map_err(|_| "invalid config server URL")?
            .push(input);
        endpoint.to_string()
    };
    let url = url::Url::parse(&endpoint).map_err(|error| error.to_string())?;
    TunnelScheme::try_from(&url)
        .map_err(|_| format!("unsupported config server scheme: {}", url.scheme()))?;
    if url
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .unwrap_or_default()
        .is_empty()
    {
        return Err("config server token is empty".to_string());
    }
    Ok(endpoint)
}
