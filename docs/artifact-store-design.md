# Artifact Store Design

## Purpose

Browser automation often needs local files on the machine that owns a browser
profile. Examples include uploading videos to YouTube, attachments to social
platforms, or temporary media used by scripted workflows.

Clients should upload files to the target machine controller before executing
browser work. The machine controller stores files in a dedicated local folder,
records TTL metadata, exposes file metadata through the control plane, and
deletes expired files automatically.

The global controller stores metadata only. It must not store video binaries.

## Placement

Artifacts are local to each machine controller.

```text
Client
  |
  | AcquireBrowser
  v
Global Controller
  |
  | route + lease
  v
Client
  |
  | UploadArtifact(stream, lease)
  v
Machine Controller
  |
  | file path passed to pwright upload action
  v
Chrome profile
```

## Storage Layout

Each machine controller owns a dedicated directory:

```text
~/.bcp/artifacts/
  ├── artifact_abc123/
  │   └── input.mp4
  └── artifact_def456/
      └── thumbnail.png
```

The path is configurable:

```text
BCP_ARTIFACT_DIR=/var/lib/bcp/artifacts
```

Files should not be stored in profile directories.

## Metadata

Each artifact needs durable metadata, backed by SQLite.

Table shape:

```text
artifacts
  artifact_id TEXT PRIMARY KEY
  machine_id TEXT NOT NULL
  profile_id TEXT NOT NULL
  lease_id TEXT NOT NULL
  original_filename TEXT NOT NULL
  stored_path TEXT NOT NULL
  content_type TEXT NOT NULL
  size_bytes INTEGER NOT NULL
  uploaded_at_unix_ms INTEGER NOT NULL
  expires_at_unix_ms INTEGER NOT NULL
  purpose TEXT NOT NULL
  status TEXT NOT NULL
```

Statuses:

- `available`
- `expired`
- `deleted`

## TTL Rules

All uploaded artifacts require TTL.

Rules:

- Reject uploads without TTL.
- Enforce a maximum TTL.
- Default max TTL: 24 hours.
- Expired files are not returned for browser actions.
- Cleanup scanner deletes expired files and marks metadata deleted.

Suggested environment variables:

```text
BCP_ARTIFACT_DIR
BCP_ARTIFACT_MAX_TTL_SECONDS
BCP_ARTIFACT_CLEANUP_SECONDS
```

## APIs

Machine controller APIs:

- `UploadArtifact(stream UploadArtifactRequest) returns UploadArtifactResponse`
- `ListLocalArtifacts(ListLocalArtifactsRequest) returns ListLocalArtifactsResponse`
- `DeleteArtifact(DeleteArtifactRequest) returns DeleteArtifactResponse`

Global controller APIs:

- `ReportArtifacts(ReportArtifactsRequest) returns ReportArtifactsResponse`
- `ListArtifacts(ListArtifactsRequest) returns ListArtifactsResponse`

The global API is metadata-only and should be populated by machine heartbeat or
telemetry reporting.

## Upload Flow

1. Client acquires browser lease from global controller.
2. Client opens a stream to the routed machine controller.
3. First stream message contains metadata and lease context.
4. Subsequent stream messages contain bytes.
5. Machine controller validates lease.
6. Machine controller writes to a temp file.
7. Machine controller atomically moves temp file into artifact directory.
8. Machine controller records SQLite metadata.
9. Client uses returned `artifact_id` or `stored_path` in a browser upload step.

## Cleanup Flow

The machine controller runs a periodic cleanup loop:

```text
scan metadata where expires_at < now and status=available
  -> remove file from disk
  -> mark status=deleted
  -> emit metric/event
```

Metrics:

```text
bcp.artifact.upload.count
bcp.artifact.upload.bytes
bcp.artifact.active.count
bcp.artifact.cleanup.count
bcp.artifact.cleanup.bytes
```

Events:

- `bcp.artifact.uploaded`
- `bcp.artifact.deleted`
- `bcp.artifact.cleanup_failed`

## Safety

- Never allow path traversal from uploaded filenames.
- Store files under generated artifact IDs.
- Never overwrite existing artifacts.
- Do not keep expired artifacts available to browser actions.
- Cleanup failure should be reported, not silently ignored.
- Artifacts are machine-local; moving work to another machine requires
  re-uploading or future replication support.
