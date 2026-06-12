use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use bcp_proto::browsercontrol::v1::Machine;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("machine '{0}' was not found in network topology")]
    MachineNotFound(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineEndpoint {
    pub machine_id: String,
    pub agent_grpc_addr: String,
    pub tailscale_host: String,
}

#[async_trait]
pub trait NetworkDirectory: Send + Sync {
    async fn endpoint_for_machine(&self, machine_id: &str)
    -> Result<MachineEndpoint, NetworkError>;
}

#[derive(Debug, Default)]
pub struct StaticNetworkDirectory {
    endpoints: RwLock<HashMap<String, MachineEndpoint>>,
}

impl StaticNetworkDirectory {
    pub fn from_machines(machines: impl IntoIterator<Item = Machine>) -> Self {
        let directory = Self::default();
        for machine in machines {
            directory.upsert_machine(machine);
        }
        directory
    }

    pub fn upsert_machine(&self, machine: Machine) {
        self.endpoints
            .write()
            .expect("static network directory lock poisoned")
            .insert(
                machine.machine_id.clone(),
                MachineEndpoint {
                    machine_id: machine.machine_id,
                    agent_grpc_addr: machine.agent_grpc_addr,
                    tailscale_host: machine.tailscale_host,
                },
            );
    }
}

#[async_trait]
impl NetworkDirectory for StaticNetworkDirectory {
    async fn endpoint_for_machine(
        &self,
        machine_id: &str,
    ) -> Result<MachineEndpoint, NetworkError> {
        self.endpoints
            .read()
            .expect("static network directory lock poisoned")
            .get(machine_id)
            .cloned()
            .ok_or_else(|| NetworkError::MachineNotFound(machine_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bcp_proto::browsercontrol::v1::{Machine, MachineStatus};

    use super::*;

    fn machine(machine_id: &str, endpoint: &str) -> Machine {
        Machine {
            machine_id: machine_id.to_string(),
            hostname: machine_id.to_string(),
            tailscale_host: format!("{machine_id}.tail.test"),
            agent_grpc_addr: endpoint.to_string(),
            status: MachineStatus::Online as i32,
            labels: HashMap::new(),
            last_heartbeat_unix_ms: 0,
        }
    }

    #[tokio::test]
    async fn static_network_directory_resolves_registered_machine_endpoint() {
        // Arrange
        let directory =
            StaticNetworkDirectory::from_machines([machine("machine-a", "http://machine-a:7100")]);

        // Act
        let endpoint = directory.endpoint_for_machine("machine-a").await.unwrap();

        // Assert
        assert_eq!(endpoint.machine_id, "machine-a");
        assert_eq!(endpoint.agent_grpc_addr, "http://machine-a:7100");
        assert_eq!(endpoint.tailscale_host, "machine-a.tail.test");
    }

    #[tokio::test]
    async fn static_network_directory_returns_not_found_for_unknown_machine() {
        // Arrange
        let directory = StaticNetworkDirectory::default();

        // Act
        let result = directory.endpoint_for_machine("missing").await;

        // Assert
        assert!(matches!(result, Err(NetworkError::MachineNotFound(id)) if id == "missing"));
    }
}
