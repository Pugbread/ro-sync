use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ASSETS_BASE_URL: &str = "https://apis.roblox.com/assets/v1";
const MAX_ASSET_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Creator {
    User(String),
    Group(String),
}

impl Creator {
    fn to_json(&self) -> Value {
        match self {
            Creator::User(id) => serde_json::json!({ "userId": id }),
            Creator::Group(id) => serde_json::json!({ "groupId": id }),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetUploadOutcome {
    pub display_name: String,
    pub asset_type: String,
    pub operation_path: Option<String>,
    pub asset_id: Option<String>,
    pub asset_uri: Option<String>,
    pub done: bool,
    pub initial_operation: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_operation: Option<Value>,
}

pub struct AssetUploadRequest {
    pub file: PathBuf,
    pub api_key: String,
    pub auth_mode: AuthMode,
    pub creator: Creator,
    pub asset_type: String,
    pub content_type: String,
    pub display_name: String,
    pub description: String,
    pub wait: bool,
    pub timeout: Duration,
    pub poll: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    Bearer,
}

pub fn parse_creator(input: &str) -> Result<Creator, String> {
    let (kind, id) = input
        .split_once(':')
        .ok_or_else(|| "expected creator as user:<id> or group:<id>".to_string())?;
    let id = id.trim();
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("creator id must be numeric".to_string());
    }
    match kind.trim().to_ascii_lowercase().as_str() {
        "user" | "u" => Ok(Creator::User(id.to_string())),
        "group" | "g" => Ok(Creator::Group(id.to_string())),
        _ => Err("creator kind must be user or group".to_string()),
    }
}

pub fn default_display_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Ro Sync Asset")
        .to_string()
}

pub fn operation_url(operation_path: &str) -> String {
    let path = operation_path.trim();
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }

    let path = path.trim_start_matches('/');
    if path.starts_with("assets/v1/") {
        format!("https://apis.roblox.com/{path}")
    } else {
        format!("{ASSETS_BASE_URL}/{path}")
    }
}

pub fn extract_asset_id(value: &Value) -> Option<String> {
    for pointer in [
        "/response/assetId",
        "/response/asset/assetId",
        "/response/asset/id",
        "/metadata/assetId",
        "/assetId",
    ] {
        if let Some(id) = json_scalar_to_string(value.pointer(pointer)) {
            return Some(id);
        }
    }
    None
}

pub async fn upload_asset(
    request: AssetUploadRequest,
) -> Result<AssetUploadOutcome, Box<dyn std::error::Error>> {
    validate_file(&request.file)?;
    let bytes = tokio::fs::read(&request.file)
        .await
        .map_err(|e| format!("upload: read {}: {e}", request.file.display()))?;
    let file_name = request
        .file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("asset")
        .to_string();

    let request_json = serde_json::json!({
        "assetType": request.asset_type,
        "displayName": request.display_name,
        "description": request.description,
        "creationContext": {
            "creator": request.creator.to_json(),
            "expectedPrice": 0
        }
    });

    let form = reqwest::multipart::Form::new()
        .part(
            "request",
            reqwest::multipart::Part::text(request_json.to_string())
                .mime_str("application/json")?,
        )
        .part(
            "fileContent",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(file_name)
                .mime_str(&request.content_type)?,
        );

    let client = reqwest::Client::new();
    let initial_operation = response_json(
        apply_auth(
            client.post(format!("{ASSETS_BASE_URL}/assets")),
            request.auth_mode,
            &request.api_key,
        )
        .multipart(form)
        .send()
        .await?,
        "upload",
    )
    .await?;
    if let Some(error) = operation_error(&initial_operation) {
        return Err(format!("upload: Roblox asset operation failed: {error}").into());
    }

    let operation_path = operation_path_from(&initial_operation);
    if !request.wait {
        return Ok(AssetUploadOutcome {
            display_name: request.display_name,
            asset_type: request.asset_type,
            operation_path,
            asset_id: extract_asset_id(&initial_operation),
            asset_uri: extract_asset_id(&initial_operation).map(|id| format!("rbxassetid://{id}")),
            done: initial_operation_done(&initial_operation),
            initial_operation,
            final_operation: None,
        });
    }

    let Some(operation_path) = operation_path else {
        let asset_id = extract_asset_id(&initial_operation);
        return Ok(AssetUploadOutcome {
            display_name: request.display_name,
            asset_type: request.asset_type,
            operation_path: None,
            asset_uri: asset_id.as_ref().map(|id| format!("rbxassetid://{id}")),
            asset_id,
            done: initial_operation_done(&initial_operation),
            initial_operation,
            final_operation: None,
        });
    };

    let started = Instant::now();
    loop {
        if started.elapsed() > request.timeout {
            return Err(format!(
                "upload: timed out waiting for Roblox asset operation {operation_path}"
            )
            .into());
        }

        tokio::time::sleep(request.poll).await;
        let current = response_json(
            apply_auth(
                client.get(operation_url(&operation_path)),
                request.auth_mode,
                &request.api_key,
            )
            .send()
            .await?,
            "operation poll",
        )
        .await?;
        if initial_operation_done(&current) {
            if let Some(error) = operation_error(&current) {
                return Err(format!(
                    "upload: Roblox asset operation {operation_path} failed: {error}"
                )
                .into());
            }
            let asset_id = extract_asset_id(&current);
            return Ok(AssetUploadOutcome {
                display_name: request.display_name,
                asset_type: request.asset_type,
                operation_path: Some(operation_path),
                asset_uri: asset_id.as_ref().map(|id| format!("rbxassetid://{id}")),
                asset_id,
                done: true,
                initial_operation,
                final_operation: Some(current),
            });
        }
    }
}

fn validate_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let meta = std::fs::metadata(path).map_err(|e| {
        format!(
            "upload: asset path is not readable: {}: {e}",
            path.display()
        )
    })?;
    if !meta.is_file() {
        return Err(format!("upload: asset path is not a file: {}", path.display()).into());
    }
    if meta.len() > MAX_ASSET_BYTES {
        return Err(format!(
            "upload: asset is too large ({} bytes); Roblox Open Cloud asset uploads must be <= {} bytes",
            meta.len(),
            MAX_ASSET_BYTES
        )
        .into());
    }
    Ok(())
}

async fn response_json(
    response: reqwest::Response,
    label: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(format!("upload: Roblox Open Cloud {label} failed ({status}): {text}").into());
    }
    serde_json::from_str(&text)
        .map_err(|e| format!("upload: Roblox Open Cloud {label} returned invalid JSON: {e}").into())
}

fn apply_auth(
    request: reqwest::RequestBuilder,
    mode: AuthMode,
    credential: &str,
) -> reqwest::RequestBuilder {
    match mode {
        AuthMode::ApiKey => request.header("x-api-key", credential),
        AuthMode::Bearer => request.bearer_auth(credential.trim_start_matches("Bearer ").trim()),
    }
}

fn operation_path_from(value: &Value) -> Option<String> {
    json_scalar_to_string(value.get("path")).or_else(|| json_scalar_to_string(value.get("name")))
}

fn initial_operation_done(value: &Value) -> bool {
    value.get("done").and_then(Value::as_bool).unwrap_or(false)
}

fn operation_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if error.is_null() {
        return None;
    }
    Some(error.to_string())
}

fn json_scalar_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_creator_targets() {
        assert_eq!(
            parse_creator("user:123").unwrap(),
            Creator::User("123".to_string())
        );
        assert_eq!(
            parse_creator("g:456").unwrap(),
            Creator::Group("456".to_string())
        );
        assert!(parse_creator("place:789").is_err());
        assert!(parse_creator("user:abc").is_err());
    }

    #[test]
    fn builds_operation_urls() {
        assert_eq!(
            operation_url("operations/abc"),
            "https://apis.roblox.com/assets/v1/operations/abc"
        );
        assert_eq!(
            operation_url("assets/v1/operations/abc"),
            "https://apis.roblox.com/assets/v1/operations/abc"
        );
    }

    #[test]
    fn extracts_asset_ids() {
        let value = serde_json::json!({ "response": { "asset": { "assetId": 12345 } } });
        assert_eq!(extract_asset_id(&value).as_deref(), Some("12345"));
    }
}
