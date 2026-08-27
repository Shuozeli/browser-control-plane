use super::*;

pub(crate) fn init_store(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS controller_records (
            kind TEXT NOT NULL,
            id TEXT NOT NULL,
            payload BLOB NOT NULL,
            PRIMARY KEY (kind, id)
        );",
    )?;
    Ok(())
}

pub(crate) fn persist_state(conn: &mut Connection, state: &ControllerState) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    for kind in [
        "machine", "profile", "binding", "lease", "metric", "event", "artifact",
    ] {
        tx.execute("DELETE FROM controller_records WHERE kind = ?1", [kind])?;
    }

    {
        let mut stmt =
            tx.prepare("INSERT INTO controller_records (kind, id, payload) VALUES (?1, ?2, ?3)")?;
        for machine in state.machines.values() {
            insert_message(&mut stmt, "machine", &machine.machine_id, machine)?;
        }
        for profile in state.profiles.values() {
            insert_message(&mut stmt, "profile", &profile.profile_id, profile)?;
        }
        for binding in state.account_bindings.values() {
            insert_message(&mut stmt, "binding", &binding.binding_id, binding)?;
        }
        for lease in state.leases.values() {
            insert_message(&mut stmt, "lease", &lease.lease_id, lease)?;
        }
        for point in state.metrics.values() {
            insert_message(&mut stmt, "metric", &metric_point_id(point), point)?;
        }
        for (index, event) in state.events.iter().enumerate() {
            let id = if event.event_id.is_empty() {
                format!("{}:{index}", event.observed_at_unix_ms)
            } else {
                event.event_id.clone()
            };
            insert_message(&mut stmt, "event", &id, event)?;
        }
        for artifact in state.artifacts.values() {
            insert_message(&mut stmt, "artifact", &artifact.artifact_id, artifact)?;
        }
    }

    tx.commit()?;
    Ok(())
}

pub(crate) fn insert_message<M: Message>(
    stmt: &mut rusqlite::Statement<'_>,
    kind: &str,
    id: &str,
    message: &M,
) -> anyhow::Result<()> {
    stmt.execute(params![kind, id, message.encode_to_vec()])?;
    Ok(())
}

pub(crate) fn load_state(conn: &Connection) -> anyhow::Result<ControllerState> {
    let mut state = ControllerState::default();

    for machine in load_messages::<Machine>(conn, "machine")? {
        state.machines.insert(machine.machine_id.clone(), machine);
    }
    for lease in load_messages::<BrowserLease>(conn, "lease")? {
        state.leases.insert(lease.lease_id.clone(), lease);
    }
    for profile in load_messages::<BrowserProfile>(conn, "profile")? {
        state.profiles.insert(profile.profile_id.clone(), profile);
    }
    for binding in load_messages::<BrowserAccountBinding>(conn, "binding")? {
        if let Some(key) =
            ControllerService::account_binding_key(binding.platform, &binding.account_id)
        {
            state.account_bindings.insert(key, binding);
        }
    }
    for point in load_messages::<MetricPoint>(conn, "metric")? {
        state.metrics.insert(metric_key_from_point(&point), point);
    }
    state.events = load_messages::<ControlPlaneEvent>(conn, "event")?;
    for artifact in load_messages::<Artifact>(conn, "artifact")? {
        state
            .artifacts
            .insert(artifact.artifact_id.clone(), artifact);
    }

    Ok(state)
}

pub(crate) fn load_messages<M: Message + Default>(
    conn: &Connection,
    kind: &str,
) -> anyhow::Result<Vec<M>> {
    let mut stmt =
        conn.prepare("SELECT payload FROM controller_records WHERE kind = ?1 ORDER BY id ASC")?;
    let rows = stmt.query_map([kind], |row| row.get::<_, Vec<u8>>(0))?;
    let mut messages = Vec::new();
    for row in rows {
        let payload = row?;
        messages.push(M::decode(payload.as_slice())?);
    }
    Ok(messages)
}

pub(crate) fn metric_key_from_point(point: &MetricPoint) -> MetricBucketKey {
    MetricBucketKey {
        name: point.name.clone(),
        bucket_start_unix_ms: point.bucket_start_unix_ms,
        machine_id: point.machine_id.clone(),
        profile_id: point.profile_id.clone(),
        platform: point.platform,
        domain: point.domain.clone(),
        action: point.action.clone(),
        status_class: point.status_class.clone(),
        error_class: point.error_class.clone(),
    }
}

pub(crate) fn metric_point_id(point: &MetricPoint) -> String {
    let key = metric_key_from_point(point);
    [
        key.name,
        key.bucket_start_unix_ms.to_string(),
        key.machine_id,
        key.profile_id,
        key.platform.to_string(),
        key.domain,
        key.action,
        key.status_class,
        key.error_class,
    ]
    .join("\u{1f}")
}
