pub(crate) use std::collections::HashMap;
pub(crate) use std::path::Path;
pub(crate) use std::process::Stdio;
pub(crate) use std::time::Duration;

pub(crate) use anyhow::{Context, bail};
pub(crate) use bcp_proto::browsercontrol::v1::global_controller_client::GlobalControllerClient;
pub(crate) use bcp_proto::browsercontrol::v1::machine_controller_client::MachineControllerClient;
pub(crate) use bcp_proto::browsercontrol::v1::upload_artifact_request::Part;
pub(crate) use bcp_proto::browsercontrol::v1::*;
pub(crate) use tokio::process::{Child, Command};

mod harness;
mod modes;
mod scenarios;
pub(crate) use harness::*;
use modes::*;
use scenarios::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::var("BCP_E2E_MODE").as_deref() {
        Ok("real-browser") => real_browser_main().await,
        Ok("vm-fleet") => vm_fleet_main().await,
        Ok("scenarios") => scenarios_main().await,
        Ok("real-web-wsj") => real_web_wsj_main().await,
        Ok("real-web-hn") => real_web_hn_main().await,
        Ok("fake-failures") => fake_failures_main().await,
        Ok("sqlite-persistence") => sqlite_persistence_main().await,
        _ => recording_main().await,
    }
}
