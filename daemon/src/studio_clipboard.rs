//! Cross-project Roblox Studio clipboard.
//!
//! The payload is an opaque engine-produced `.rbxm` buffer. Ro Sync never
//! attempts to understand or rewrite that format; Roblox's SerializationService
//! is the source of truth on both sides of the transfer.

use base64::Engine as _;
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIPBOARD_SCHEMA: &str = "ro-sync.instance-clipboard.v1";
const CLIPBOARD_DIR: &str = "clipboard";
const CLIPBOARD_MANIFEST: &str = "current.json";
const CLIPBOARD_MIME: &str = "application/octet-stream";
const MAX_CLIPBOARD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ROOTS: usize = 256;
const TRANSFER_CHUNK_BYTES: usize = 384 * 1024;
const DEFAULT_PORT: u16 = 7878;

#[derive(ClapArgs, Debug)]
pub struct CopyArgs {
    /// Project directory. Defaults to the current directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Studio instance path. May be repeated; when omitted, copies Selection.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Vec<String>,
    /// Studio instance paths may also be supplied positionally.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
    /// End-to-end transfer deadline in seconds.
    #[arg(long, default_value_t = 120.0)]
    pub timeout: f64,
    /// Print machine-readable JSON.
    #[arg(long)]
    pub raw: bool,
}

#[derive(ClapArgs, Debug)]
pub struct PasteArgs {
    /// Project directory. Defaults to the current directory.
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Parent all pasted roots here. Without it, recorded parent routes are used.
    #[arg(long, visible_alias = "parent", value_name = "PARENT")]
    pub to: Option<String>,
    /// Do not select the newly pasted roots in Studio Explorer.
    #[arg(long)]
    pub no_select: bool,
    /// End-to-end transfer deadline in seconds.
    #[arg(long, default_value_t = 120.0)]
    pub timeout: f64,
    /// Print machine-readable JSON.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardRouteSegment {
    pub name: String,
    pub class: String,
    pub ordinal: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardRoot {
    pub source_path: String,
    pub parent_path: String,
    pub parent_route: Vec<ClipboardRouteSegment>,
    pub class: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ClipboardSource {
    project: String,
    game_id: Option<String>,
    place_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ClipboardManifest {
    schema: String,
    created_at_unix_ms: u64,
    serializer: String,
    byte_length: u64,
    sha256: String,
    payload: String,
    source: ClipboardSource,
    roots: Vec<ClipboardRoot>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactMetadata {
    id: String,
    mime: String,
    path: PathBuf,
    size: u64,
    sha256: String,
}

struct LoadedClipboard {
    manifest: ClipboardManifest,
    bytes: Vec<u8>,
}

pub async fn run_copy(args: CopyArgs) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = transfer_deadline(args.timeout, "copy")?;
    let project = canonical_project(args.project.as_deref(), "copy")?;
    let mut paths = args.path;
    paths.extend(args.paths);
    if paths.len() > MAX_ROOTS {
        return Err(format!("copy: at most {MAX_ROOTS} root paths may be copied").into());
    }

    let client = local_http_client()?;
    let lease_response = post_json_until(
        &client,
        args.port,
        "/artifacts/lease",
        &json!({
            "filename": "rosync-clipboard.rbxm",
            "mime": CLIPBOARD_MIME,
        }),
        deadline,
    )
    .await?;
    let lease = success_field(&lease_response, "lease", "copy artifact lease")?.clone();
    let lease_id = lease
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_artifact_id(id))
        .ok_or("copy: artifact lease returned an invalid id")?
        .to_string();
    let lease_token = lease
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("copy: artifact lease omitted its token")?
        .to_string();

    let request_timeout = remaining(deadline, "Studio serialization")?;
    let response = crate::remote::request_with_timeout(
        args.port,
        "clipboard_copy",
        json!({
            "paths": paths,
            "lease": lease,
            "timeoutSeconds": request_timeout.as_secs_f64(),
        }),
        request_timeout,
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            abort_lease(&client, args.port, &lease_id, &lease_token, deadline).await;
            return Err(format!("copy: {error}").into());
        }
    };
    let value = match plugin_value(&response, "copy") {
        Ok(value) => value,
        Err(error) => {
            abort_lease(&client, args.port, &lease_id, &lease_token, deadline).await;
            return Err(error.into());
        }
    };
    let returned_id = value
        .get("artifact")
        .and_then(|artifact| artifact.get("id"))
        .and_then(Value::as_str)
        .ok_or("copy: plugin response omitted artifact id")?;
    if returned_id != lease_id {
        abort_lease(&client, args.port, &lease_id, &lease_token, deadline).await;
        return Err(
            format!("copy: plugin finalized artifact {returned_id}, expected {lease_id}").into(),
        );
    }

    let roots: Vec<ClipboardRoot> = serde_json::from_value(
        value
            .get("roots")
            .cloned()
            .ok_or("copy: plugin response omitted root metadata")?,
    )?;
    validate_roots(&roots)?;
    let metadata = lookup_artifact(&client, args.port, &lease_id, deadline).await?;
    let materialized = read_and_verify_artifact(&metadata)?;
    let manifest = ClipboardManifest {
        schema: CLIPBOARD_SCHEMA.to_string(),
        created_at_unix_ms: now_unix_ms(),
        serializer: "Roblox.SerializationService".to_string(),
        byte_length: metadata.size,
        sha256: metadata.sha256.clone(),
        payload: format!("{}.rbxm", metadata.sha256),
        source: ClipboardSource {
            project: project.display().to_string(),
            game_id: value
                .get("gameId")
                .and_then(Value::as_str)
                .map(str::to_string),
            place_id: value
                .get("placeId")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        roots,
    };

    let install_result = install_clipboard(&clipboard_dir()?, &manifest, &materialized);
    consume_artifact(&client, args.port, &lease_id, deadline).await;
    install_result?;

    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "clipboard": manifest,
            }))?
        );
    } else {
        println!(
            "copied {} Studio instance{} ({} bytes)",
            manifest.roots.len(),
            if manifest.roots.len() == 1 { "" } else { "s" },
            manifest.byte_length,
        );
        for root in &manifest.roots {
            println!("  {}  ({})", root.source_path, root.class);
        }
    }
    Ok(())
}

pub async fn run_paste(args: PasteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = transfer_deadline(args.timeout, "paste")?;
    let loaded = load_clipboard(&clipboard_dir()?)?;
    let client = local_http_client()?;
    let lease_response = post_json_until(
        &client,
        args.port,
        "/artifacts/lease",
        &json!({
            "filename": "rosync-clipboard.rbxm",
            "mime": CLIPBOARD_MIME,
            "expectedSize": loaded.manifest.byte_length,
        }),
        deadline,
    )
    .await?;
    let lease = success_field(&lease_response, "lease", "paste artifact lease")?.clone();
    let lease_id = lease
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_artifact_id(id))
        .ok_or("paste: artifact lease returned an invalid id")?
        .to_string();
    let lease_token = lease
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("paste: artifact lease omitted its token")?
        .to_string();

    let upload_result = upload_artifact(
        &client,
        args.port,
        &lease_id,
        &lease_token,
        &loaded.bytes,
        &loaded.manifest.sha256,
        deadline,
    )
    .await;
    if let Err(error) = upload_result {
        abort_lease(&client, args.port, &lease_id, &lease_token, deadline).await;
        return Err(format!("paste: {error}").into());
    }

    let request_timeout = remaining(deadline, "Studio paste")?;
    let response = crate::remote::request_with_timeout(
        args.port,
        "clipboard_paste",
        json!({
            "artifactId": lease_id,
            "byteLength": loaded.manifest.byte_length,
            "sha256": loaded.manifest.sha256,
            "roots": loaded.manifest.roots,
            "parent": args.to,
            "select": !args.no_select,
            "timeoutSeconds": request_timeout.as_secs_f64(),
        }),
        request_timeout,
    )
    .await;
    consume_artifact(&client, args.port, &lease_id, deadline).await;
    let response = response.map_err(|error| format!("paste: {error}"))?;
    let value = plugin_value(&response, "paste")?;

    if args.raw {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        let count = value
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(loaded.manifest.roots.len() as u64);
        println!(
            "pasted {count} Studio instance{}",
            if count == 1 { "" } else { "s" }
        );
        if let Some(paths) = value.get("paths").and_then(Value::as_array) {
            for path in paths.iter().filter_map(Value::as_str) {
                println!("  {path}");
            }
        }
    }
    Ok(())
}

fn transfer_deadline(
    timeout_seconds: f64,
    operation: &str,
) -> Result<Instant, Box<dyn std::error::Error>> {
    if !timeout_seconds.is_finite() || !(1.0..=300.0).contains(&timeout_seconds) {
        return Err(
            format!("{operation}: --timeout must be finite and between 1 and 300 seconds").into(),
        );
    }
    Instant::now()
        .checked_add(Duration::from_secs_f64(timeout_seconds))
        .ok_or_else(|| format!("{operation}: timeout overflow").into())
}

fn remaining(deadline: Instant, phase: &str) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(format!("clipboard deadline expired before {phase}"))
    } else {
        Ok(remaining)
    }
}

fn canonical_project(
    project: Option<&Path>,
    operation: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    fs::canonicalize(&path)
        .map_err(|error| format!("{operation}: project {}: {error}", path.display()).into())
}

fn clipboard_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(crate::lifecycle::state_dir(None)?.join(CLIPBOARD_DIR))
}

fn local_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .build()
}

async fn post_json_until(
    client: &reqwest::Client,
    port: u16,
    path: &str,
    body: &Value,
    deadline: Instant,
) -> Result<Value, String> {
    let timeout = remaining(deadline, path)?;
    let response = client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .timeout(timeout)
        .json(body)
        .send()
        .await
        .map_err(|error| format!("POST {path}: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("POST {path} response: {error}"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(format!("POST {path} response exceeded 4 MiB"));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("POST {path} returned invalid JSON ({status}): {error}"))?;
    if !status.is_success() {
        return Err(format!("POST {path} returned {status}: {value}"));
    }
    Ok(value)
}

async fn get_json_until(
    client: &reqwest::Client,
    port: u16,
    path: &str,
    deadline: Instant,
) -> Result<Value, String> {
    let timeout = remaining(deadline, path)?;
    let response = client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| format!("GET {path}: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("GET {path} response: {error}"))?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(format!("GET {path} response exceeded 4 MiB"));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("GET {path} returned invalid JSON ({status}): {error}"))?;
    if !status.is_success() {
        return Err(format!("GET {path} returned {status}: {value}"));
    }
    Ok(value)
}

fn success_field<'a>(response: &'a Value, field: &str, context: &str) -> Result<&'a Value, String> {
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!("{context} rejected: {response}"));
    }
    response
        .get(field)
        .ok_or_else(|| format!("{context} omitted {field}"))
}

fn plugin_value<'a>(response: &'a Value, operation: &str) -> Result<&'a Value, String> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return response
            .get("value")
            .ok_or_else(|| format!("{operation}: plugin response omitted value"));
    }
    let error = crate::remote::plugin_error(response)
        .map(|error| error.to_string())
        .unwrap_or_else(|| "request failed".to_string());
    if response
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        == Some("UNKNOWN_OP")
    {
        Err(format!(
            "{operation}: connected Studio plugin does not support copy/paste; reinstall the current Ro Sync plugin"
        ))
    } else {
        Err(format!("{operation}: {error}"))
    }
}

fn valid_artifact_id(id: &str) -> bool {
    id.len() == 48 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn lookup_artifact(
    client: &reqwest::Client,
    port: u16,
    id: &str,
    deadline: Instant,
) -> Result<ArtifactMetadata, String> {
    if !valid_artifact_id(id) {
        return Err("invalid artifact id".to_string());
    }
    let response = get_json_until(client, port, &format!("/artifacts/{id}"), deadline).await?;
    let metadata: ArtifactMetadata =
        serde_json::from_value(success_field(&response, "artifact", "artifact lookup")?.clone())
            .map_err(|error| format!("artifact lookup returned invalid metadata: {error}"))?;
    if metadata.id != id
        || metadata.mime != CLIPBOARD_MIME
        || metadata.size == 0
        || metadata.size > MAX_CLIPBOARD_BYTES
        || !valid_sha256(&metadata.sha256)
        || !metadata.path.is_absolute()
    {
        return Err("artifact lookup returned unsafe clipboard metadata".to_string());
    }
    Ok(metadata)
}

fn read_and_verify_artifact(metadata: &ArtifactMetadata) -> Result<Vec<u8>, String> {
    let bytes = fs::read(&metadata.path).map_err(|error| {
        format!(
            "read clipboard artifact {}: {error}",
            metadata.path.display()
        )
    })?;
    verify_payload(&bytes, metadata.size, &metadata.sha256)?;
    Ok(bytes)
}

async fn upload_artifact(
    client: &reqwest::Client,
    port: u16,
    id: &str,
    token: &str,
    bytes: &[u8],
    sha256: &str,
    deadline: Instant,
) -> Result<(), String> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + TRANSFER_CHUNK_BYTES).min(bytes.len());
        let response = post_json_until(
            client,
            port,
            &format!("/artifacts/{id}/chunk"),
            &json!({
                "token": token,
                "offset": offset,
                "bytesBase64": base64::engine::general_purpose::STANDARD.encode(&bytes[offset..end]),
            }),
            deadline,
        )
        .await?;
        let receipt = success_field(&response, "receipt", "artifact upload")?;
        let total = receipt
            .get("totalBytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| "artifact upload receipt omitted totalBytes".to_string())?;
        if total != end as u64 {
            return Err(format!(
                "artifact upload advanced to {total} bytes, expected {end}"
            ));
        }
        offset = end;
    }
    let response = post_json_until(
        client,
        port,
        &format!("/artifacts/{id}/finalize"),
        &json!({ "token": token, "expectedSha256": sha256 }),
        deadline,
    )
    .await?;
    let artifact = success_field(&response, "artifact", "artifact finalization")?;
    if artifact.get("id").and_then(Value::as_str) != Some(id)
        || artifact.get("sha256").and_then(Value::as_str) != Some(sha256)
    {
        return Err("artifact finalization returned mismatched metadata".to_string());
    }
    Ok(())
}

async fn abort_lease(
    client: &reqwest::Client,
    port: u16,
    id: &str,
    token: &str,
    deadline: Instant,
) {
    if remaining(deadline, "artifact abort").is_ok() {
        let _ = post_json_until(
            client,
            port,
            &format!("/artifacts/{id}/abort"),
            &json!({ "token": token }),
            deadline,
        )
        .await;
    }
}

async fn consume_artifact(client: &reqwest::Client, port: u16, id: &str, deadline: Instant) {
    if remaining(deadline, "artifact consume").is_ok() {
        let _ = post_json_until(
            client,
            port,
            &format!("/artifacts/{id}/consume"),
            &json!({}),
            deadline,
        )
        .await;
    }
}

fn install_clipboard(dir: &Path, manifest: &ClipboardManifest, bytes: &[u8]) -> Result<(), String> {
    validate_manifest(manifest)?;
    verify_payload(bytes, manifest.byte_length, &manifest.sha256)?;
    crate::lifecycle::create_private_dir(dir)
        .map_err(|error| format!("create private clipboard directory: {error}"))?;
    let _install_lock =
        crate::lifecycle::StartLock::acquire_named(&dir.join(".install.lock"), "clipboard update")
            .map_err(|error| format!("lock private clipboard: {error}"))?;
    let payload_path = dir.join(&manifest.payload);
    crate::lifecycle::write_private_atomic_exact(&payload_path, bytes)
        .map_err(|error| format!("write clipboard payload: {error}"))?;
    let manifest_bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("encode clipboard manifest: {error}"))?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("clipboard manifest exceeds 1 MiB".to_string());
    }
    crate::lifecycle::write_private_atomic(&dir.join(CLIPBOARD_MANIFEST), &manifest_bytes)
        .map_err(|error| format!("write clipboard manifest: {error}"))?;

    // The manifest is the atomic pointer. Once it is durable, older immutable
    // payload generations are no longer needed. A paste that already loaded an
    // older generation owns its bytes in memory and is unaffected by cleanup.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("rbxm") && path != payload_path
            {
                let _ = fs::remove_file(path);
            }
        }
    }
    Ok(())
}

fn load_clipboard(dir: &Path) -> Result<LoadedClipboard, String> {
    let manifest_path = dir.join(CLIPBOARD_MANIFEST);
    for attempt in 0..2 {
        let metadata = fs::metadata(&manifest_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "clipboard is empty; run `rosync copy` first".to_string()
            } else {
                format!("read clipboard manifest metadata: {error}")
            }
        })?;
        if metadata.len() == 0 || metadata.len() > MAX_MANIFEST_BYTES {
            return Err("clipboard manifest has an invalid size".to_string());
        }
        let manifest: ClipboardManifest = serde_json::from_slice(
            &fs::read(&manifest_path)
                .map_err(|error| format!("read clipboard manifest: {error}"))?,
        )
        .map_err(|error| format!("parse clipboard manifest: {error}"))?;
        validate_manifest(&manifest)?;
        match fs::read(dir.join(&manifest.payload)) {
            Ok(bytes) => match verify_payload(&bytes, manifest.byte_length, &manifest.sha256) {
                Ok(()) => return Ok(LoadedClipboard { manifest, bytes }),
                Err(error) if attempt == 0 => {
                    let _ = error;
                    continue;
                }
                Err(error) => return Err(error),
            },
            Err(_) if attempt == 0 => continue,
            Err(error) => return Err(format!("read clipboard payload: {error}")),
        }
    }
    Err("clipboard changed while it was being read; retry paste".to_string())
}

fn validate_manifest(manifest: &ClipboardManifest) -> Result<(), String> {
    if manifest.schema != CLIPBOARD_SCHEMA
        || manifest.serializer != "Roblox.SerializationService"
        || manifest.byte_length == 0
        || manifest.byte_length > MAX_CLIPBOARD_BYTES
        || !valid_sha256(&manifest.sha256)
        || manifest.payload != format!("{}.rbxm", manifest.sha256)
    {
        return Err("clipboard manifest is invalid or unsupported".to_string());
    }
    validate_roots(&manifest.roots)
}

fn validate_roots(roots: &[ClipboardRoot]) -> Result<(), String> {
    if roots.is_empty() {
        return Err("copy: Studio selection is empty".to_string());
    }
    if roots.len() > MAX_ROOTS {
        return Err(format!("clipboard contains more than {MAX_ROOTS} roots"));
    }
    for root in roots {
        if root.source_path.is_empty()
            || root.parent_path.is_empty()
            || root.parent_route.is_empty()
            || root.parent_route.len() > 256
            || root.class.is_empty()
            || root.name.is_empty()
            || root.source_path.len() > 4096
            || root.parent_path.len() > 4096
            || root.class.len() > 128
            || root.name.len() > 256
        {
            return Err("clipboard root metadata is invalid".to_string());
        }
        for segment in &root.parent_route {
            if segment.name.is_empty()
                || segment.class.is_empty()
                || segment.name.len() > 256
                || segment.class.len() > 128
                || segment.ordinal == 0
                || segment.ordinal > 100_000
            {
                return Err("clipboard parent route metadata is invalid".to_string());
            }
        }
    }
    Ok(())
}

fn verify_payload(bytes: &[u8], expected_size: u64, expected_sha256: &str) -> Result<(), String> {
    if expected_size == 0 || expected_size > MAX_CLIPBOARD_BYTES {
        return Err("clipboard payload size is outside the 128 MiB limit".to_string());
    }
    if bytes.len() as u64 != expected_size {
        return Err(format!(
            "clipboard payload has {} bytes; expected {expected_size}",
            bytes.len()
        ));
    }
    let actual = sha256_hex(bytes);
    if actual != expected_sha256 {
        return Err(format!(
            "clipboard payload SHA-256 mismatch: expected {expected_sha256}, got {actual}"
        ));
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(bytes: &[u8]) -> ClipboardManifest {
        let sha256 = sha256_hex(bytes);
        ClipboardManifest {
            schema: CLIPBOARD_SCHEMA.to_string(),
            created_at_unix_ms: 123,
            serializer: "Roblox.SerializationService".to_string(),
            byte_length: bytes.len() as u64,
            payload: format!("{sha256}.rbxm"),
            sha256,
            source: ClipboardSource {
                project: "/tmp/source".to_string(),
                game_id: Some("123".to_string()),
                place_id: Some("456".to_string()),
            },
            roots: vec![ClipboardRoot {
                source_path: "Workspace/Model".to_string(),
                parent_path: "Workspace".to_string(),
                parent_route: vec![ClipboardRouteSegment {
                    name: "Workspace".to_string(),
                    class: "Workspace".to_string(),
                    ordinal: 1,
                }],
                class: "Model".to_string(),
                name: "Model".to_string(),
            }],
        }
    }

    #[test]
    fn clipboard_round_trip_is_content_addressed() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"native-rbxm";
        let manifest = sample_manifest(bytes);
        install_clipboard(dir.path(), &manifest, bytes).unwrap();
        let loaded = load_clipboard(dir.path()).unwrap();
        assert_eq!(loaded.manifest, manifest);
        assert_eq!(loaded.bytes, bytes);
        assert!(dir.path().join(&manifest.payload).is_file());
    }

    #[test]
    fn failed_install_preserves_previous_clipboard() {
        let dir = tempfile::tempdir().unwrap();
        let original = sample_manifest(b"first");
        install_clipboard(dir.path(), &original, b"first").unwrap();
        let replacement = sample_manifest(b"second");
        assert!(install_clipboard(dir.path(), &replacement, b"corrupt").is_err());
        let loaded = load_clipboard(dir.path()).unwrap();
        assert_eq!(loaded.manifest, original);
        assert_eq!(loaded.bytes, b"first");
    }

    #[test]
    fn corrupt_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = sample_manifest(b"good");
        install_clipboard(dir.path(), &manifest, b"good").unwrap();
        fs::write(dir.path().join(&manifest.payload), b"evil").unwrap();
        let error = load_clipboard(dir.path()).err().unwrap();
        assert!(error.contains("SHA-256 mismatch") || error.contains("expected"));
    }

    #[test]
    fn concurrent_install_refuses_to_corrupt_current_generation() {
        let dir = tempfile::tempdir().unwrap();
        let original = sample_manifest(b"original");
        install_clipboard(dir.path(), &original, b"original").unwrap();
        let _held = crate::lifecycle::StartLock::acquire_named(
            &dir.path().join(".install.lock"),
            "clipboard update",
        )
        .unwrap();
        let replacement = sample_manifest(b"replacement");
        let error = install_clipboard(dir.path(), &replacement, b"replacement")
            .err()
            .unwrap();
        assert!(error.contains("another clipboard update"));
        let loaded = load_clipboard(dir.path()).unwrap();
        assert_eq!(loaded.manifest, original);
        assert_eq!(loaded.bytes, b"original");
    }

    #[test]
    fn manifest_payload_cannot_escape_clipboard_directory() {
        let mut manifest = sample_manifest(b"payload");
        manifest.payload = "../escape.rbxm".to_string();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_files_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("clipboard");
        let manifest = sample_manifest(b"private");
        install_clipboard(&dir, &manifest, b"private").unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join(CLIPBOARD_MANIFEST))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.join(&manifest.payload))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn timeout_and_root_limits_are_enforced() {
        assert!(transfer_deadline(f64::NAN, "copy").is_err());
        assert!(transfer_deadline(0.5, "copy").is_err());
        assert!(transfer_deadline(301.0, "copy").is_err());
        assert!(validate_roots(&[]).is_err());
        let root = sample_manifest(b"x").roots[0].clone();
        assert!(validate_roots(&vec![root; MAX_ROOTS + 1]).is_err());
    }
}
