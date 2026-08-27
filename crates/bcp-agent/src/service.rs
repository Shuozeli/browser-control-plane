use super::*;

#[tonic::async_trait]
impl MachineController for AgentService {
    async fn install_lease(
        &self,
        request: Request<InstallLeaseRequest>,
    ) -> Result<Response<InstallLeaseResponse>, Status> {
        let lease = request
            .into_inner()
            .lease
            .ok_or_else(|| Status::invalid_argument("lease is required"))?;
        if lease.lease_id.is_empty() {
            return Err(Status::invalid_argument("lease_id is required"));
        }
        if lease.profile_id.is_empty() {
            return Err(Status::invalid_argument("profile_id is required"));
        }
        if lease.fencing_token.is_empty() {
            return Err(Status::invalid_argument("fencing_token is required"));
        }
        self.install_lease(
            &lease.lease_id,
            &lease.profile_id,
            &lease.fencing_token,
            lease.expires_at_unix_ms,
        );
        Ok(Response::new(InstallLeaseResponse { installed: true }))
    }

    async fn uninstall_lease(
        &self,
        request: Request<UninstallLeaseRequest>,
    ) -> Result<Response<UninstallLeaseResponse>, Status> {
        let lease = request
            .into_inner()
            .lease
            .ok_or_else(|| Status::invalid_argument("lease is required"))?;
        if lease.lease_id.is_empty() {
            return Err(Status::invalid_argument("lease_id is required"));
        }
        let uninstalled = self.uninstall_lease(&lease.lease_id, &lease.fencing_token);
        Ok(Response::new(UninstallLeaseResponse { uninstalled }))
    }

    async fn list_local_profiles(
        &self,
        _request: Request<ListLocalProfilesRequest>,
    ) -> Result<Response<ListLocalProfilesResponse>, Status> {
        Ok(Response::new(ListLocalProfilesResponse {
            profiles: self.fleet.list_profiles(),
        }))
    }

    async fn ensure_browser(
        &self,
        request: Request<EnsureBrowserRequest>,
    ) -> Result<Response<EnsureBrowserResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let profile = self
            .pwright
            .ensure_browser(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(EnsureBrowserResponse {
            profile: Some(profile),
        }))
    }

    async fn stop_browser(
        &self,
        request: Request<StopBrowserRequest>,
    ) -> Result<Response<StopBrowserResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let stopped = self
            .pwright
            .stop_browser(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(StopBrowserResponse { stopped }))
    }

    async fn check_browser(
        &self,
        request: Request<CheckBrowserRequest>,
    ) -> Result<Response<CheckBrowserResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let health = self
            .pwright
            .check_browser(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(CheckBrowserResponse {
            healthy: health.healthy,
            message: health.message,
        }))
    }

    async fn get_snapshot(
        &self,
        request: Request<GetSnapshotRequest>,
    ) -> Result<Response<GetSnapshotResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let nodes = self
            .pwright
            .get_snapshot(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet.record_operation(
            "browser.snapshot",
            &profile_id,
            &format!("snapshot returned {} nodes", nodes.len()),
        );
        Ok(Response::new(GetSnapshotResponse { nodes }))
    }

    async fn execute_action(
        &self,
        request: Request<ExecuteActionRequest>,
    ) -> Result<Response<ExecuteActionResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease.clone())?;
        let action = request.action.clone();
        let response = self
            .pwright
            .execute_action(&profile_id, request)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet
            .record_operation("browser.action", &profile_id, &action);
        Ok(Response::new(response))
    }

    async fn evaluate(
        &self,
        request: Request<EvaluateRequest>,
    ) -> Result<Response<EvaluateResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let json_result = self
            .pwright
            .evaluate(&profile_id, &request.expression)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet
            .record_operation("browser.eval", &profile_id, "evaluate expression");
        Ok(Response::new(EvaluateResponse { json_result }))
    }

    async fn capture_screenshot(
        &self,
        request: Request<CaptureScreenshotRequest>,
    ) -> Result<Response<CaptureScreenshotResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let base64_data = self
            .pwright
            .capture_screenshot(&profile_id, &request.format, request.full_page)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet
            .record_operation("browser.screenshot", &profile_id, "captured screenshot");
        let format = if request.format.is_empty() {
            "png".to_string()
        } else {
            request.format
        };
        Ok(Response::new(CaptureScreenshotResponse {
            base64_data,
            format,
        }))
    }

    async fn print_pdf(
        &self,
        request: Request<PrintPdfRequest>,
    ) -> Result<Response<PrintPdfResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let base64_data = self
            .pwright
            .print_pdf(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet
            .record_operation("browser.pdf", &profile_id, "printed pdf");
        Ok(Response::new(PrintPdfResponse { base64_data }))
    }

    async fn get_cookies(
        &self,
        request: Request<GetCookiesRequest>,
    ) -> Result<Response<GetCookiesResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let cookies_json = self
            .pwright
            .get_cookies(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet
            .record_operation("browser.get_cookies", &profile_id, "read cookies");
        Ok(Response::new(GetCookiesResponse { cookies_json }))
    }

    async fn set_cookies(
        &self,
        request: Request<SetCookiesRequest>,
    ) -> Result<Response<SetCookiesResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let count = self
            .pwright
            .set_cookies(&profile_id, &request.cookies_json)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet.record_operation(
            "browser.set_cookies",
            &profile_id,
            &format!("set {count} cookie(s)"),
        );
        Ok(Response::new(SetCookiesResponse { count }))
    }

    async fn get_page(
        &self,
        request: Request<GetPageRequest>,
    ) -> Result<Response<GetPageResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let info = self
            .pwright
            .get_page(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet
            .record_operation("browser.get_page", &profile_id, "read page info");
        Ok(Response::new(GetPageResponse {
            url: info.url,
            title: info.title,
            content: info.content,
        }))
    }

    async fn set_input_files(
        &self,
        request: Request<SetInputFilesRequest>,
    ) -> Result<Response<SetInputFilesResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        self.pwright
            .set_input_files(&profile_id, &request.selector, &request.files)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet.record_operation(
            "browser.set_input_files",
            &profile_id,
            &format!("attached {} file(s)", request.files.len()),
        );
        Ok(Response::new(SetInputFilesResponse { ok: true }))
    }

    type DownloadArtifactStream = Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<
                    Item = Result<DownloadArtifactResponse, Status>,
                > + Send
                + 'static,
        >,
    >;

    async fn download_artifact(
        &self,
        request: Request<DownloadArtifactRequest>,
    ) -> Result<Response<Self::DownloadArtifactStream>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let (artifact, path) = self
            .artifacts
            .open_for_read(&request.artifact_id)
            .map_err(Self::artifact_error_to_status)?;
        // An artifact is only retrievable through the lease that owns its profile.
        if artifact.profile_id != profile_id {
            return Err(Status::permission_denied(
                "artifact does not belong to this lease's profile",
            ));
        }
        let bytes = std::fs::read(&path).map_err(|error| Status::internal(error.to_string()))?;
        self.fleet.record_operation(
            "browser.download_artifact",
            &profile_id,
            &format!("streamed {} bytes", bytes.len()),
        );
        let mut messages = Vec::new();
        messages.push(DownloadArtifactResponse {
            part: Some(DownloadPart::Metadata(ArtifactDownloadMetadata {
                artifact_id: artifact.artifact_id,
                original_filename: artifact.original_filename,
                content_type: artifact.content_type,
                size_bytes: artifact.size_bytes,
            })),
        });
        for chunk in bytes.chunks(64 * 1024) {
            messages.push(DownloadArtifactResponse {
                part: Some(DownloadPart::Chunk(chunk.to_vec())),
            });
        }
        let stream = tonic::codegen::tokio_stream::iter(messages.into_iter().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    type RunScriptStream = Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<RunScriptResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn run_script(
        &self,
        request: Request<RunScriptRequest>,
    ) -> Result<Response<Self::RunScriptStream>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let lines = self
            .pwright
            .run_script(&profile_id, &request.yaml, request.params)
            .await
            .map_err(Self::pwright_error_to_status)?;
        self.fleet.record_operation(
            "browser.run_script",
            &profile_id,
            &format!("ran script with {} result line(s)", lines.len()),
        );
        let stream = tonic::codegen::tokio_stream::iter(lines.into_iter().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn upload_artifact(
        &self,
        request: Request<tonic::Streaming<UploadArtifactRequest>>,
    ) -> Result<Response<UploadArtifactResponse>, Status> {
        let mut stream = request.into_inner();
        let metadata = match stream.message().await? {
            Some(UploadArtifactRequest {
                part: Some(Part::Metadata(metadata)),
            }) => metadata,
            Some(_) => {
                return Err(Status::invalid_argument(
                    "first upload message must contain metadata",
                ));
            }
            None => return Err(Status::invalid_argument("upload stream is empty")),
        };
        let lease = metadata
            .lease
            .clone()
            .ok_or_else(|| Status::invalid_argument("lease is required"))?;
        let profile_id = self.validate_lease(Some(lease.clone()))?;
        if profile_id != lease.profile_id {
            return Err(Status::permission_denied("lease profile mismatch"));
        }

        let ticket = self
            .artifacts
            .reserve(
                &lease,
                &metadata.original_filename,
                &metadata.content_type,
                &metadata.purpose,
                metadata.ttl_seconds,
            )
            .map_err(Self::artifact_error_to_status)?;
        let mut file = std::fs::File::create(&ticket.temp_path)
            .map_err(|error| Status::internal(error.to_string()))?;
        let mut size_bytes = 0_i64;

        while let Some(message) = stream.message().await? {
            match message.part {
                Some(Part::Chunk(chunk)) => {
                    size_bytes += chunk.len() as i64;
                    if let Err(error) = file.write_all(&chunk) {
                        let _ = self.artifacts.fail_upload(&ticket);
                        return Err(Status::internal(error.to_string()));
                    }
                }
                Some(Part::Metadata(_)) => {
                    let _ = self.artifacts.fail_upload(&ticket);
                    return Err(Status::invalid_argument(
                        "metadata is only allowed as the first upload message",
                    ));
                }
                None => {}
            }
        }
        drop(file);

        let artifact = self
            .artifacts
            .commit(&ticket, size_bytes)
            .map_err(Self::artifact_error_to_status)?;
        Ok(Response::new(UploadArtifactResponse {
            artifact: Some(artifact),
        }))
    }

    async fn list_local_artifacts(
        &self,
        request: Request<ListLocalArtifactsRequest>,
    ) -> Result<Response<ListLocalArtifactsResponse>, Status> {
        let request = request.into_inner();
        let profile_id = (!request.profile_id.is_empty()).then_some(request.profile_id.as_str());
        let lease_id = (!request.lease_id.is_empty()).then_some(request.lease_id.as_str());
        let artifacts = self
            .artifacts
            .list(profile_id, lease_id, request.include_expired)
            .map_err(Self::artifact_error_to_status)?;
        Ok(Response::new(ListLocalArtifactsResponse { artifacts }))
    }

    async fn delete_artifact(
        &self,
        request: Request<DeleteArtifactRequest>,
    ) -> Result<Response<DeleteArtifactResponse>, Status> {
        let artifact_id = request.into_inner().artifact_id;
        if artifact_id.is_empty() {
            return Err(Status::invalid_argument("artifact_id is required"));
        }
        let deleted = self
            .artifacts
            .delete(&artifact_id)
            .map_err(Self::artifact_error_to_status)?;
        Ok(Response::new(DeleteArtifactResponse { deleted }))
    }
}
