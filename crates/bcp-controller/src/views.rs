use super::*;

pub(crate) fn machine_view(machine: Machine) -> web::MachineView {
    web::MachineView {
        machine_id: machine.machine_id,
        hostname: machine.hostname,
        status: machine_status_label(machine.status),
        agent_grpc_addr: machine.agent_grpc_addr,
        tailscale_host: machine.tailscale_host,
        last_heartbeat_unix_ms: machine.last_heartbeat_unix_ms,
        labels: key_values(machine.labels),
    }
}

pub(crate) fn profile_view(profile: BrowserProfile) -> web::ProfileView {
    web::ProfileView {
        profile_id: profile.profile_id,
        machine_id: profile.machine_id,
        display_name: profile.display_name,
        status: profile_status_label(profile.status),
        cdp_url: profile.cdp_url,
        cdp_port: profile.cdp_port,
        last_seen_unix_ms: profile.last_seen_unix_ms,
        accounts: profile.accounts.into_iter().map(account_view).collect(),
        labels: key_values(profile.labels),
    }
}

pub(crate) fn account_view(account: Account) -> web::AccountView {
    web::AccountView {
        account_id: account.account_id,
        platform: platform_label(account.platform),
        handle: account.handle,
        health: account.health,
        capabilities: account.capabilities,
    }
}

pub(crate) fn account_binding_view(binding: BrowserAccountBinding) -> web::AccountBindingView {
    web::AccountBindingView {
        binding_id: binding.binding_id,
        profile_id: binding.profile_id,
        machine_id: binding.machine_id,
        platform: platform_label(binding.platform),
        account_id: binding.account_id,
        handle: binding.handle,
        account_health: binding.account_health,
        profile_status: profile_status_label(binding.profile_status),
        agent_grpc_addr: binding.agent_grpc_addr,
        cdp_url: binding.cdp_url,
        last_seen_unix_ms: binding.last_seen_unix_ms,
    }
}

pub(crate) fn lease_view(lease: BrowserLease) -> web::LeaseView {
    web::LeaseView {
        lease_id: lease.lease_id,
        profile_id: lease.profile_id,
        machine_id: lease.machine_id,
        client_id: lease.client_id,
        purpose: lease.purpose,
        expires_at_unix_ms: lease.expires_at_unix_ms,
    }
}

pub(crate) fn metric_view(metric: MetricPoint) -> web::MetricView {
    web::MetricView {
        name: metric.name,
        value: metric.value,
        bucket_start_unix_ms: metric.bucket_start_unix_ms,
        machine_id: metric.machine_id,
        profile_id: metric.profile_id,
        platform: platform_label(metric.platform),
        domain: metric.domain,
        action: metric.action,
        status_class: metric.status_class,
        error_class: metric.error_class,
    }
}

pub(crate) fn event_view(event: ControlPlaneEvent) -> web::EventView {
    web::EventView {
        event_type: event.event_type,
        severity: event_severity_label(event.severity),
        observed_at_unix_ms: event.observed_at_unix_ms,
        machine_id: event.machine_id,
        profile_id: event.profile_id,
        message: event.message,
        attributes: key_values(event.attributes),
    }
}

pub(crate) fn artifact_view(artifact: Artifact) -> web::ArtifactView {
    web::ArtifactView {
        artifact_id: artifact.artifact_id,
        machine_id: artifact.machine_id,
        profile_id: artifact.profile_id,
        lease_id: artifact.lease_id,
        original_filename: artifact.original_filename,
        content_type: artifact.content_type,
        purpose: artifact.purpose,
        status: artifact_status_label(artifact.status),
        size_bytes: artifact.size_bytes,
        expires_at_unix_ms: artifact.expires_at_unix_ms,
    }
}

pub(crate) fn key_values(values: HashMap<String, String>) -> Vec<web::KeyValueView> {
    let mut values: Vec<_> = values
        .into_iter()
        .map(|(key, value)| web::KeyValueView { key, value })
        .collect();
    values.sort_by(|left, right| left.key.cmp(&right.key));
    values
}

pub(crate) fn machine_status_label(status: i32) -> String {
    match MachineStatus::try_from(status).unwrap_or(MachineStatus::Unspecified) {
        MachineStatus::Online => "online",
        MachineStatus::Degraded => "degraded",
        MachineStatus::Offline => "offline",
        MachineStatus::Unspecified => "unspecified",
    }
    .to_string()
}

pub(crate) fn profile_status_label(status: i32) -> String {
    match ProfileStatus::try_from(status).unwrap_or(ProfileStatus::Unspecified) {
        ProfileStatus::Available => "available",
        ProfileStatus::Leased => "leased",
        ProfileStatus::Launching => "launching",
        ProfileStatus::Broken => "broken",
        ProfileStatus::Quarantined => "quarantined",
        ProfileStatus::Unspecified => "unspecified",
    }
    .to_string()
}

pub(crate) fn platform_label(platform: i32) -> String {
    match AccountPlatform::try_from(platform).unwrap_or(AccountPlatform::Unspecified) {
        AccountPlatform::Youtube => "youtube",
        AccountPlatform::X => "x",
        AccountPlatform::Douyin => "douyin",
        AccountPlatform::Tiktok => "tiktok",
        AccountPlatform::Reddit => "reddit",
        AccountPlatform::Zhihu => "zhihu",
        AccountPlatform::Weibo => "weibo",
        AccountPlatform::Wsj => "wsj",
        AccountPlatform::HackerNews => "hacker-news",
        AccountPlatform::Unspecified => "unspecified",
    }
    .to_string()
}

pub(crate) fn event_severity_label(severity: i32) -> String {
    match EventSeverity::try_from(severity).unwrap_or(EventSeverity::Unspecified) {
        EventSeverity::Info => "info",
        EventSeverity::Warn => "warn",
        EventSeverity::Error => "error",
        EventSeverity::Unspecified => "unspecified",
    }
    .to_string()
}

pub(crate) fn artifact_status_label(status: i32) -> String {
    match ArtifactStatus::try_from(status).unwrap_or(ArtifactStatus::Unspecified) {
        ArtifactStatus::Uploading => "uploading",
        ArtifactStatus::Available => "available",
        ArtifactStatus::Expired => "expired",
        ArtifactStatus::Deleted => "deleted",
        ArtifactStatus::Failed => "failed",
        ArtifactStatus::Unspecified => "unspecified",
    }
    .to_string()
}
