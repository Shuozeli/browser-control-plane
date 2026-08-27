pub(crate) use std::collections::HashMap;
pub(crate) use std::sync::{Arc, RwLock};

pub(crate) use async_trait::async_trait;
pub(crate) use bcp_proto::browsercontrol::v1::{
    A11yNode, BrowserProfile, ExecuteActionRequest, ExecuteActionResponse, RunScriptResponse,
};
pub(crate) use thiserror::Error;

mod recording;
pub use recording::{RecordingProfileState, RecordingPwrightGateway};
#[cfg(feature = "real-pwright")]
mod real;
#[cfg(feature = "real-pwright")]
pub use real::{RealPwrightGateway, RealPwrightProfile};

#[derive(Debug, Error)]
pub enum PwrightError {
    #[error("profile '{0}' was not found")]
    ProfileNotFound(String),
    #[error("browser profile '{0}' is unhealthy: {1}")]
    Unhealthy(String, String),
    #[error("browser operation failed for profile '{0}': {1}")]
    OperationFailed(String, String),
}

#[derive(Debug, Clone)]
pub struct BrowserHealth {
    pub healthy: bool,
    pub message: String,
}

#[async_trait]
pub trait PwrightGateway: Send + Sync {
    async fn ensure_browser(&self, profile_id: &str) -> Result<BrowserProfile, PwrightError>;
    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError>;
    async fn check_browser(&self, profile_id: &str) -> Result<BrowserHealth, PwrightError>;
    async fn get_snapshot(&self, profile_id: &str) -> Result<Vec<A11yNode>, PwrightError>;
    async fn execute_action(
        &self,
        profile_id: &str,
        request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError>;
    async fn evaluate(&self, profile_id: &str, expression: &str) -> Result<String, PwrightError>;
    async fn run_script(
        &self,
        profile_id: &str,
        yaml: &str,
        params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError>;
    /// Capture a screenshot, returning base64-encoded image bytes.
    async fn capture_screenshot(
        &self,
        profile_id: &str,
        format: &str,
        full_page: bool,
    ) -> Result<String, PwrightError>;
    /// Print the page to PDF, returning base64-encoded PDF bytes.
    async fn print_pdf(&self, profile_id: &str) -> Result<String, PwrightError>;
    /// Return all cookies visible to the page as a JSON array string. Includes
    /// httpOnly cookies, which page JavaScript cannot read.
    async fn get_cookies(&self, profile_id: &str) -> Result<String, PwrightError>;
    /// Set cookies from a JSON array (subset fields allowed; the rest default).
    /// Returns the number of cookies applied.
    async fn set_cookies(&self, profile_id: &str, cookies_json: &str) -> Result<u32, PwrightError>;
    /// Return the current page's URL, title, and full HTML content.
    async fn get_page(&self, profile_id: &str) -> Result<PageInfo, PwrightError>;
    /// Attach one or more machine-local file paths to a file `<input>` selected
    /// by CSS selector (bridges an out-of-band uploaded artifact into the page).
    async fn set_input_files(
        &self,
        profile_id: &str,
        selector: &str,
        files: &[String],
    ) -> Result<(), PwrightError>;
}

/// Page-level introspection returned by [`PwrightGateway::get_page`].
#[derive(Debug, Clone, Default)]
pub struct PageInfo {
    pub url: String,
    pub title: String,
    pub content: String,
}

pub type SharedPwrightGateway = Arc<dyn PwrightGateway>;
