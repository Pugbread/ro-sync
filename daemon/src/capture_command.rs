use super::*;

pub(super) async fn run_capture(args: CaptureArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        CaptureCommand::Status(args) => run_capture_status(args).await,
        CaptureCommand::Authorize(args) => run_capture_authorize(args).await,
        CaptureCommand::Screen(args) => run_capture_screen(args).await,
        CaptureCommand::Photo(args) => run_capture_photo(args).await,
        CaptureCommand::Scene(args) => run_capture_scene(args).await,
    }
}

pub(super) async fn run_capture_scene(
    args: CaptureSceneArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    if !(1.0..=4.0).contains(&args.padding) || !args.padding.is_finite() {
        return Err("capture scene: --padding must be between 1.0 and 4.0".into());
    }
    parse_capture_size(&args.size)?;
    if args.resample != CaptureResampleMode::Default {
        return Err("capture scene: --resample pixelated is not supported by the Photo engine; use `capture photo` and resize the PNG after capture".into());
    }
    run_capture_photo(capture_scene_photo_args(args)).await
}

pub(super) fn capture_scene_photo_args(args: CaptureSceneArgs) -> CapturePhotoArgs {
    CapturePhotoArgs {
        project: args.project,
        port: args.port,
        focus: Some(args.focus),
        region: None,
        size: Some(args.size),
        view: args.view,
        direction: None,
        camera_cframe: None,
        padding: args.padding,
        fov: 32.0,
        background: CapturePhotoBackground::Transparent,
        alpha_bleed: true,
        include_world: false,
        no_tight_crop: args.no_tight_crop,
        ui: None,
        ui_target: None,
        include_ui: false,
        delay: 0.05,
        output: args.output,
        timeout: args.timeout,
        raw: args.raw,
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct PhotoPrepared {
    #[serde(rename = "sessionId")]
    pub(super) session_id: String,
    pub(super) width: u32,
    pub(super) height: u32,
    #[serde(rename = "byteLength")]
    pub(super) byte_length: usize,
    #[serde(default)]
    pub(super) background: Option<String>,
    #[serde(default, rename = "uiMode")]
    pub(super) ui_mode: Option<String>,
    #[serde(default, rename = "cameraCFrame")]
    pub(super) camera_cframe: Option<serde_json::Value>,
    #[serde(default, rename = "uiTarget")]
    pub(super) ui_target: Option<String>,
    #[serde(default, rename = "uiTargetClass")]
    pub(super) ui_target_class: Option<String>,
    #[serde(default, rename = "fieldOfView")]
    pub(super) field_of_view: Option<f64>,
    #[serde(default)]
    pub(super) isolated: Option<bool>,
    #[serde(default, rename = "tightCrop")]
    pub(super) tight_crop: Option<bool>,
    #[serde(default, rename = "fullSize")]
    pub(super) full_size: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) region: Option<serde_json::Value>,
    #[serde(default, rename = "regionSource")]
    pub(super) region_source: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PhotoChunk {
    offset: usize,
    #[serde(rename = "nextOffset")]
    next_offset: usize,
    eof: bool,
    #[serde(rename = "bytesBase64")]
    bytes_base64: String,
}

pub(super) fn parse_capture_direction(value: &str) -> Result<[f64; 3], Box<dyn std::error::Error>> {
    let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err("capture photo: --direction must be x,y,z".into());
    }
    let mut direction = [0.0; 3];
    for (index, field) in fields.iter().enumerate() {
        direction[index] = field.parse::<f64>().map_err(|error| {
            format!(
                "capture photo: invalid direction component {}: {error}",
                index + 1
            )
        })?;
        if !direction[index].is_finite() {
            return Err("capture photo: --direction components must be finite".into());
        }
    }
    let magnitude = direction[0].hypot(direction[1]).hypot(direction[2]);
    if !magnitude.is_finite() {
        return Err("capture photo: --direction magnitude must be finite".into());
    }
    if magnitude <= 1e-6 {
        return Err("capture photo: --direction cannot be the zero vector".into());
    }
    for component in &mut direction {
        *component /= magnitude;
    }
    Ok(direction)
}

pub(super) fn parse_capture_camera_cframe(
    value: &str,
) -> Result<[f64; 12], Box<dyn std::error::Error>> {
    const ORTHONORMAL_EPSILON: f64 = 1e-3;

    let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 12 {
        return Err(
            "capture photo: --camera-cframe must contain the 12 comma-separated values returned by CFrame:GetComponents()"
                .into(),
        );
    }

    let mut components = [0.0; 12];
    for (index, field) in fields.iter().enumerate() {
        components[index] = field.parse::<f64>().map_err(|error| {
            format!(
                "capture photo: invalid --camera-cframe component {}: {error}",
                index + 1
            )
        })?;
        if !components[index].is_finite() {
            return Err("capture photo: --camera-cframe components must be finite".into());
        }
    }

    let rows = [
        [components[3], components[4], components[5]],
        [components[6], components[7], components[8]],
        [components[9], components[10], components[11]],
    ];
    let dot = |left: [f64; 3], right: [f64; 3]| {
        left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
    };
    let rows_are_unit = rows
        .iter()
        .all(|row| (dot(*row, *row) - 1.0).abs() <= ORTHONORMAL_EPSILON);
    let rows_are_orthogonal = dot(rows[0], rows[1]).abs() <= ORTHONORMAL_EPSILON
        && dot(rows[0], rows[2]).abs() <= ORTHONORMAL_EPSILON
        && dot(rows[1], rows[2]).abs() <= ORTHONORMAL_EPSILON;
    let determinant = rows[0][0] * (rows[1][1] * rows[2][2] - rows[1][2] * rows[2][1])
        - rows[0][1] * (rows[1][0] * rows[2][2] - rows[1][2] * rows[2][0])
        + rows[0][2] * (rows[1][0] * rows[2][1] - rows[1][1] * rows[2][0]);
    if !rows_are_unit
        || !rows_are_orthogonal
        || !determinant.is_finite()
        || (determinant - 1.0).abs() > ORTHONORMAL_EPSILON
    {
        return Err(
            "capture photo: --camera-cframe rotation must be an orthonormal right-handed matrix from CFrame:GetComponents()"
                .into(),
        );
    }

    Ok(components)
}

pub(super) fn build_capture_photo_request(
    args: &CapturePhotoArgs,
    ui_mode: CapturePhotoUiMode,
    region: Option<CaptureRegion>,
    size: Option<[u32; 2]>,
    direction: Option<[f64; 3]>,
    camera_cframe: Option<[f64; 12]>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut request = serde_json::Map::new();
    request.insert(
        "background".into(),
        serde_json::Value::String(args.background.as_wire_str().to_string()),
    );
    request.insert("alphaBleed".into(), serde_json::json!(args.alpha_bleed));
    request.insert(
        "tightCrop".into(),
        serde_json::json!(capture_photo_uses_tight_crop(args)),
    );
    request.insert(
        "uiMode".into(),
        serde_json::Value::String(ui_mode.as_wire_str().to_string()),
    );
    request.insert(
        "hideUI".into(),
        serde_json::json!(ui_mode == CapturePhotoUiMode::None),
    );
    request.insert("delay".into(), serde_json::json!(args.delay));
    request.insert("timeoutSeconds".into(), serde_json::json!(args.timeout));
    if let Some(ui_target) = &args.ui_target {
        request.insert(
            "uiTarget".into(),
            serde_json::Value::String(ui_target.clone()),
        );
    }
    if let Some(focus) = &args.focus {
        request.insert("focus".into(), serde_json::Value::String(focus.clone()));
        request.insert("fieldOfView".into(), serde_json::json!(args.fov));
        request.insert("isolate".into(), serde_json::json!(!args.include_world));
        if let Some(components) = camera_cframe {
            request.insert(
                "cameraCFrame".into(),
                serde_json::json!({
                    "__type": "CFrame",
                    "components": components,
                }),
            );
        } else {
            request.insert(
                "view".into(),
                serde_json::Value::String(args.view.as_plugin_str().to_string()),
            );
            request.insert("padding".into(), serde_json::json!(args.padding));
            if let Some(direction) = direction {
                request.insert(
                    "direction".into(),
                    serde_json::json!({ "x": direction[0], "y": direction[1], "z": direction[2] }),
                );
            }
        }
    }
    if let Some(region) = region {
        request.insert(
            "nativeRect".into(),
            serde_json::json!({
                "x": region.x,
                "y": region.y,
                "width": region.width,
                "height": region.height,
            }),
        );
    }
    if let Some([width, height]) = size {
        request.insert(
            "outputSize".into(),
            serde_json::json!({ "x": width, "y": height }),
        );
    }
    request
}

pub(super) fn capture_photo_uses_tight_crop(args: &CapturePhotoArgs) -> bool {
    args.focus.is_some()
        && !args.include_world
        && !args.no_tight_crop
        && args.background == CapturePhotoBackground::Transparent
}

pub(super) fn validate_photo_dimensions(
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    if width == 0 || height == 0 {
        return Err("capture photo: dimensions must be positive".into());
    }
    validate_capture_dimensions(width, height)?;
    if width > PHOTO_MAX_DIMENSION || height > PHOTO_MAX_DIMENSION {
        return Err(format!(
            "capture photo: dimensions {width}x{height} exceed the {PHOTO_MAX_DIMENSION}px Photo limit"
        )
        .into());
    }
    if u64::from(width) * u64::from(height) > PHOTO_MAX_PIXELS {
        return Err(format!(
            "capture photo: dimensions {width}x{height} exceed the {PHOTO_MAX_PIXELS}-pixel Photo limit"
        )
        .into());
    }
    Ok(())
}

pub(super) fn encode_photo_png(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| "capture photo: RGBA byte length does not fit this platform")?;
    if rgba.len() != expected {
        return Err(format!(
            "capture photo: received {} RGBA bytes, expected {expected} for {width}x{height}",
            rgba.len()
        )
        .into());
    }
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("capture photo: encode PNG header: {error}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| format!("capture photo: encode PNG pixels: {error}"))?;
    }
    Ok(png_bytes)
}

pub(super) async fn capture_remote_session_connect_until(
    port: u16,
    deadline: Instant,
    phase: &str,
) -> Result<remote::RemoteSession, String> {
    let remaining = capture_deadline_remaining(deadline, phase)?;
    tokio::time::timeout(remaining, remote::RemoteSession::connect(port))
        .await
        .map_err(|_| format!("capture deadline expired during {phase}"))?
}

pub(super) async fn capture_remote_session_request_until(
    session: &mut remote::RemoteSession,
    op: &str,
    args: serde_json::Value,
    deadline: Instant,
    phase: &str,
) -> Result<serde_json::Value, String> {
    let remaining = capture_deadline_remaining(deadline, phase)?;
    tokio::time::timeout(remaining, session.request(op, args, remaining))
        .await
        .map_err(|_| format!("capture deadline expired during {phase}"))?
}

pub(super) fn confirm_photo_close_response(response: &serde_json::Value) -> Result<(), String> {
    let value = response_value_or_err(response, "capture photo close")
        .map_err(|error| error.to_string())?;
    if value.as_bool() == Some(true) {
        Ok(())
    } else {
        Err("capture photo close: plugin did not confirm session cleanup".into())
    }
}

pub(super) async fn close_photo_session_until(
    session: &mut remote::RemoteSession,
    session_id: &str,
    deadline: Instant,
) -> Result<(), String> {
    let response = capture_remote_session_request_until(
        session,
        "photo_close",
        serde_json::json!({ "sessionId": session_id }),
        deadline,
        "capture photo close",
    )
    .await?;
    confirm_photo_close_response(&response)
}

pub(super) async fn run_capture_photo(
    args: CapturePhotoArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};

    if !args.timeout.is_finite() || !(1.0..=120.0).contains(&args.timeout) {
        return Err("capture photo: --timeout must be between 1 and 120 seconds".into());
    }
    let deadline = capture_deadline(args.timeout, "capture photo")?;
    if !(1.0..=4.0).contains(&args.padding) || !args.padding.is_finite() {
        return Err("capture photo: --padding must be between 1.0 and 4.0".into());
    }
    if !(1.0..=120.0).contains(&args.fov) || !args.fov.is_finite() {
        return Err("capture photo: --fov must be between 1 and 120 degrees".into());
    }
    if !(0.0..=5.0).contains(&args.delay) || !args.delay.is_finite() {
        return Err("capture photo: --delay must be between 0 and 5 seconds".into());
    }
    if args.delay >= args.timeout {
        return Err("capture photo: --delay must be shorter than --timeout".into());
    }
    if args.no_tight_crop && args.focus.is_none() {
        return Err("capture photo: --no-tight-crop requires --focus".into());
    }
    if let Some(ui_target) = &args.ui_target {
        if ui_target.trim().is_empty() {
            return Err(
                "capture photo: --ui-target must be a non-empty Studio instance path".into(),
            );
        }
        if let Some(mode) = args.ui {
            if mode != CapturePhotoUiMode::Only {
                return Err("capture photo: --ui-target implies --ui only and cannot be combined with --ui none or --ui overlay".into());
            }
        }
        if args.include_ui {
            return Err(
                "capture photo: --ui-target cannot be combined with the --include-ui overlay alias"
                    .into(),
            );
        }
        if args.focus.is_some() {
            return Err("capture photo: --ui-target cannot be combined with --focus".into());
        }
        if args.background != CapturePhotoBackground::Transparent {
            return Err("capture photo: --ui-target requires --background transparent".into());
        }
    }
    let ui_mode = if args.ui_target.is_some() {
        CapturePhotoUiMode::Only
    } else {
        args.ui.unwrap_or(if args.include_ui {
            CapturePhotoUiMode::Overlay
        } else {
            CapturePhotoUiMode::None
        })
    };
    if ui_mode == CapturePhotoUiMode::Only {
        if args.focus.is_some() {
            return Err(
                "capture photo: --ui only captures the current viewport and cannot be combined with --focus"
                    .into(),
            );
        }
        if args.background != CapturePhotoBackground::Transparent {
            return Err("capture photo: --ui only requires --background transparent".into());
        }
    }
    if args.camera_cframe.is_some() && args.focus.is_none() {
        return Err("capture photo: --camera-cframe requires --focus".into());
    }
    if args.camera_cframe.is_some()
        && (args.direction.is_some()
            || args.view != CaptureView::Isometric
            || (args.padding - 1.25).abs() > f64::EPSILON)
    {
        return Err(
            "capture photo: --camera-cframe cannot be combined with --view, --direction, or --padding"
                .into(),
        );
    }
    if args.focus.is_none()
        && (args.direction.is_some() || args.include_world || args.view != CaptureView::Isometric)
    {
        return Err(
            "capture photo: --view, --direction, and --include-world require --focus".into(),
        );
    }
    if args.focus.is_some() && args.region.is_some() {
        return Err(
            "capture photo: --region captures the current viewport and cannot be combined with --focus; use --size to frame a subject".into(),
        );
    }

    let region = args
        .region
        .as_deref()
        .map(parse_capture_region)
        .transpose()?;
    if let Some(region) = region {
        if region.x < 0 || region.y < 0 {
            return Err(
                "capture photo: viewport-native --region x and y must be non-negative".into(),
            );
        }
        validate_photo_dimensions(region.width, region.height)?;
    }
    let size = match args.size.as_deref() {
        Some(value) => Some(parse_capture_size(value)?),
        None if args.focus.is_some() => Some([1024, 1024]),
        None => None,
    };
    if let Some([width, height]) = size {
        validate_photo_dimensions(width, height)?;
    }
    let direction = args
        .direction
        .as_deref()
        .map(parse_capture_direction)
        .transpose()?;
    let camera_cframe = args
        .camera_cframe
        .as_deref()
        .map(parse_capture_camera_cframe)
        .transpose()?;
    let tight_crop = capture_photo_uses_tight_crop(&args);

    let request =
        build_capture_photo_request(&args, ui_mode, region, size, direction, camera_cframe);

    let work_deadline = capture_work_deadline(deadline);
    let mut photo_remote =
        capture_remote_session_connect_until(args.port, work_deadline, "capture photo connect")
            .await?;
    if ui_mode == CapturePhotoUiMode::Only
        || camera_cframe.is_some()
        || args.ui_target.is_some()
        || tight_crop
    {
        let capability_response = capture_remote_session_request_until(
            &mut photo_remote,
            "capabilities",
            serde_json::json!({}),
            work_deadline,
            "capture photo capabilities",
        )
        .await?;
        let capability_value =
            response_value_or_err(&capability_response, "capture photo capabilities")?;
        let features = capability_value
            .get("features")
            .and_then(serde_json::Value::as_object);
        let supported = |name: &str| {
            features
                .and_then(|features| features.get(name))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        };
        if camera_cframe.is_some() && !supported("photoCameraCFrame") {
            return Err(
                "capture photo: the connected Studio plugin does not support --camera-cframe; reinstall the current Ro Sync plugin and reload Studio"
                    .into(),
            );
        }
        if args.ui_target.is_some() && !supported("photoUiTarget") {
            return Err(
                "capture photo: the connected Studio plugin does not support --ui-target; reinstall the current Ro Sync plugin and reload Studio"
                    .into(),
            );
        }
        if ui_mode == CapturePhotoUiMode::Only && !supported("photoUiOnly") {
            return Err(
                "capture photo: the connected Studio plugin does not support --ui only; reinstall the current Ro Sync plugin and reload Studio"
                    .into(),
            );
        }
        if tight_crop && !supported("photoInstanceTightCrop") {
            return Err(
                "capture photo: the connected Studio plugin does not support automatic instance tight-cropping; reinstall the current Ro Sync plugin and reload Studio, or pass --no-tight-crop"
                    .into(),
            );
        }
    }
    let prepare_response = capture_remote_session_request_until(
        &mut photo_remote,
        "photo_prepare",
        serde_json::Value::Object(request),
        work_deadline,
        "capture photo prepare",
    )
    .await?;
    let prepared_value = response_value_or_err(&prepare_response, "capture photo prepare")?;
    let session_hint = prepared_value
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let prepared: PhotoPrepared = match serde_json::from_value(prepared_value) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(session_id) = session_hint {
                if let Err(cleanup) =
                    close_photo_session_until(&mut photo_remote, &session_id, deadline).await
                {
                    return Err(format!(
                        "capture photo: plugin returned invalid metadata: {error}; session cleanup also failed: {cleanup}"
                    )
                    .into());
                }
            }
            return Err(format!("capture photo: plugin returned invalid metadata: {error}").into());
        }
    };
    let session_id = prepared.session_id.clone();

    let flow: Result<(PathBuf, usize, String), Box<dyn std::error::Error>> = async {
        validate_photo_dimensions(prepared.width, prepared.height)?;
        let expected_u64 = u64::from(prepared.width)
            .checked_mul(u64::from(prepared.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("capture photo: RGBA byte length overflowed")?;
        let expected = usize::try_from(expected_u64)
            .map_err(|_| "capture photo: RGBA byte length does not fit this platform")?;
        if expected_u64 > CAPTURE_MAX_ARTIFACT_BYTES || prepared.byte_length != expected {
            return Err(format!(
                "capture photo: plugin reported {} bytes for {}x{} RGBA; expected {expected}",
                prepared.byte_length, prepared.width, prepared.height
            )
            .into());
        }

        let mut rgba = Vec::with_capacity(expected);
        let mut offset = 0usize;
        while offset < expected {
            let response = capture_remote_session_request_until(
                &mut photo_remote,
                "photo_read",
                serde_json::json!({
                    "sessionId": session_id,
                    "offset": offset,
                    "maxBytes": 384 * 1024,
                }),
                capture_work_deadline(deadline),
                "capture photo read",
            )
            .await?;
            let value = response_value_or_err(&response, "capture photo read")?;
            let chunk: PhotoChunk = serde_json::from_value(value)
                .map_err(|error| format!("capture photo: invalid chunk metadata: {error}"))?;
            if chunk.offset != offset
                || chunk.next_offset <= chunk.offset
                || chunk.next_offset > expected
            {
                return Err(format!(
                    "capture photo: invalid chunk range {}..{} at expected offset {offset}",
                    chunk.offset, chunk.next_offset
                )
                .into());
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&chunk.bytes_base64)
                .map_err(|error| format!("capture photo: decode RGBA chunk: {error}"))?;
            let declared = chunk.next_offset - chunk.offset;
            if decoded.len() != declared || decoded.len() > 384 * 1024 {
                return Err(format!(
                    "capture photo: chunk decoded to {} bytes, expected {declared}",
                    decoded.len()
                )
                .into());
            }
            rgba.extend_from_slice(&decoded);
            offset = chunk.next_offset;
            if chunk.eof != (offset == expected) {
                return Err(
                    "capture photo: plugin returned inconsistent end-of-file metadata".into(),
                );
            }
        }

        let png_bytes = encode_photo_png(prepared.width, prepared.height, &rgba)?;
        if u64::try_from(png_bytes.len()).unwrap_or(u64::MAX) > CAPTURE_MAX_ARTIFACT_BYTES {
            return Err("capture photo: encoded PNG exceeds the artifact byte limit".into());
        }
        verify_capture_png(
            &png_bytes,
            Some((prepared.width, prepared.height)),
            capture_work_deadline(deadline),
        )?;
        if let Some(parent) = args.output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("capture photo: create {}: {error}", parent.display())
                })?;
            }
        }
        std::fs::write(&args.output, &png_bytes)
            .map_err(|error| format!("capture photo: write {}: {error}", args.output.display()))?;
        let absolute = std::fs::canonicalize(&args.output).unwrap_or_else(|_| args.output.clone());
        let sha256 = format!("{:x}", Sha256::digest(&png_bytes));
        Ok((absolute, png_bytes.len(), sha256))
    }
    .await;

    let close_result = close_photo_session_until(&mut photo_remote, &session_id, deadline).await;
    let ((absolute, png_size, sha256), consumed) = match (flow, close_result) {
        (Ok(result), Ok(())) => (result, true),
        (Ok(result), Err(cleanup)) => {
            eprintln!("capture photo: warning: session cleanup failed: {cleanup}");
            (result, false)
        }
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cleanup)) => {
            return Err(format!("{error}; session cleanup also failed: {cleanup}").into());
        }
    };
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "artifact": {
                    "path": absolute,
                    "provider": "rosync-photo",
                    "source": "locally-packaged",
                    "mime": "image/png",
                    "size": png_size,
                    "sha256": sha256,
                    "width": prepared.width,
                    "height": prepared.height,
                    "background": prepared.background,
                    "uiMode": prepared.ui_mode,
                    "cameraCFrame": prepared.camera_cframe,
                    "uiTarget": prepared.ui_target,
                    "uiTargetClass": prepared.ui_target_class,
                    "fieldOfView": prepared.field_of_view,
                    "isolated": prepared.isolated,
                    "tightCrop": prepared.tight_crop,
                    "fullSize": prepared.full_size,
                    "region": prepared.region,
                    "regionSource": prepared.region_source,
                    "transport": {
                        "kind": "bounded-rgba-chunks",
                        "consumed": consumed,
                    },
                }
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {}; locally packaged Photo engine)",
            absolute.display(),
            prepared.width,
            prepared.height,
            png_size,
            sha256
        );
    }
    Ok(())
}

pub(super) async fn run_capture_status(
    args: CaptureStatusArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut resp = remote::request(args.port, "capture_status", serde_json::json!({})).await?;
    let native = native_capture::screen_capture_permission_status();
    if resp.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        let mut value = resp
            .get("value")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let studio_authorized = value
            .get("authorized")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let provider_unsupported = value
            .get("providerUnsupported")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        value.insert(
            "nativeFallback".into(),
            serde_json::json!({
                "available": native.available,
                "authorized": native.authorized,
                "scope": "screen-ui-all",
            }),
        );
        value.insert(
            "effectiveProvider".into(),
            serde_json::Value::String(
                capture_effective_provider(studio_authorized, provider_unsupported, native)
                    .to_string(),
            ),
        );
        resp["value"] = serde_json::Value::Object(value);
    }
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        let value = response_value_or_err(&resp, "capture status")?;
        let available = value
            .get("available")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let authorized = value
            .get("authorized")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let provider = value
            .get("effectiveProvider")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("none");
        let photo_available = value
            .get("photoAvailable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let photo_ui_only_available = value
            .get("photoUiOnlyAvailable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        println!(
            "capture API: {}; Studio permission: {}; effective provider: {}; packaged Photo: {}; UI-only: {}",
            if available {
                "available"
            } else {
                "unavailable"
            },
            if authorized { "granted" } else { "not granted" },
            provider,
            if photo_available {
                "available"
            } else {
                "unavailable"
            },
            if photo_ui_only_available {
                "available"
            } else {
                "unavailable"
            },
        );
    }
    ok_or_err(&resp)
}

pub(super) fn capture_effective_provider(
    studio_authorized: bool,
    provider_unsupported: bool,
    native: native_capture::NativePermissionStatus,
) -> &'static str {
    if studio_authorized {
        "studio"
    } else if provider_unsupported && native.available && native.authorized {
        "macos-window"
    } else {
        "none"
    }
}

pub(super) async fn run_capture_authorize(
    args: CaptureAuthorizeArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = remote::request_with_timeout(
        args.port,
        "capture_authorize",
        serde_json::json!({}),
        Duration::from_secs(120),
    )
    .await?;
    let value = match response_value_or_err(&resp, "capture authorize") {
        Ok(value) => value,
        Err(error) => {
            if args.raw {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            }
            return Err(error);
        }
    };
    let studio_authorized = value
        .get("authorized")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let provider_unsupported = value
        .get("providerUnsupported")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let provider_error = value.get("providerError").cloned();
    let native_before = native_capture::screen_capture_permission_status();
    let mut native = native_before;
    let mut native_prompted = false;
    if provider_unsupported && native.available && !native.authorized {
        native_prompted = true;
        native = native_capture::request_screen_capture_permission()?;
    }
    let provider = capture_effective_provider(studio_authorized, provider_unsupported, native);
    let authorized = provider != "none";
    let aggregate = serde_json::json!({
        "ok": authorized,
        "provider": provider,
        "studio": {
            "available": true,
            "authorized": studio_authorized,
            "providerUnsupported": provider_unsupported,
            "providerError": provider_error,
        },
        "nativeFallback": {
            "available": native.available,
            "authorized": native.authorized,
            "prompted": native_prompted,
            "scope": "screen-ui-all",
        }
    });
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&aggregate)?);
    } else if studio_authorized {
        println!("screenshot permission: granted (Studio provider)");
    } else if provider_unsupported && native.authorized {
        println!(
            "screenshot permission: granted (macOS Roblox Studio window fallback; Studio provider unsupported)"
        );
    } else if provider_unsupported && native.available {
        println!(
            "screenshot permission: denied (Studio provider unsupported; macOS Screen & System Audio Recording permission not granted)"
        );
    } else {
        println!("screenshot permission: denied");
    }
    if authorized {
        Ok(())
    } else if provider_unsupported && !native.available {
        Err("capture authorize: Studio screenshot provider is unsupported and the native fallback is only available on macOS".into())
    } else if provider_unsupported {
        Err("capture authorize: macOS Screen & System Audio Recording permission was not granted; enable it for the app running rosync, then retry".into())
    } else {
        Err("capture authorize: Studio screenshot permission was denied".into())
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct CapturePrepared {
    #[serde(rename = "sessionId")]
    pub(super) session_id: String,
    pub(super) width: u32,
    pub(super) height: u32,
    #[serde(rename = "byteLength")]
    pub(super) byte_length: usize,
    #[serde(default)]
    pub(super) position: Option<CapturePoint>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct CapturePoint {
    pub(super) x: f64,
    pub(super) y: f64,
}

#[derive(Clone, Copy)]
pub(super) struct CaptureRegion {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) fn parse_capture_region(
    value: &str,
) -> Result<CaptureRegion, Box<dyn std::error::Error>> {
    let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 4 {
        return Err("capture: --region must be x,y,width,height with positive dimensions".into());
    }
    let x = fields[0]
        .parse::<i32>()
        .map_err(|e| format!("capture: invalid region x coordinate: {e}"))?;
    let y = fields[1]
        .parse::<i32>()
        .map_err(|e| format!("capture: invalid region y coordinate: {e}"))?;
    let width = fields[2]
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid region width: {e}"))?;
    let height = fields[3]
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid region height: {e}"))?;
    if width == 0 || height == 0 {
        return Err("capture: region dimensions must be positive".into());
    }
    Ok(CaptureRegion {
        x,
        y,
        width,
        height,
    })
}

pub(super) fn parse_capture_size(value: &str) -> Result<[u32; 2], Box<dyn std::error::Error>> {
    let normalized = value.trim().to_ascii_lowercase();
    let Some((width, height)) = normalized.split_once('x') else {
        return Err("capture: --output-size must be WIDTHxHEIGHT".into());
    };
    let width = width
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid output width: {e}"))?;
    let height = height
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("capture: invalid output height: {e}"))?;
    if width == 0 || height == 0 {
        return Err("capture: output dimensions must be positive".into());
    }
    Ok([width, height])
}

pub(super) fn validate_capture_dimensions(
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    if width > CAPTURE_MAX_DIMENSION || height > CAPTURE_MAX_DIMENSION {
        return Err(format!(
            "capture: dimensions {width}x{height} exceed the {CAPTURE_MAX_DIMENSION}px per-axis limit"
        )
        .into());
    }
    if u64::from(width) * u64::from(height) > CAPTURE_MAX_PIXELS {
        return Err(format!(
            "capture: dimensions {width}x{height} exceed the {CAPTURE_MAX_PIXELS}-pixel limit"
        )
        .into());
    }
    Ok(())
}

pub(super) fn capture_deadline(
    timeout_seconds: f64,
    context: &str,
) -> Result<Instant, Box<dyn std::error::Error>> {
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 || timeout_seconds > 120.0 {
        return Err(format!(
            "{context}: timeout must be finite, greater than zero, and at most 120 seconds"
        )
        .into());
    }
    Instant::now()
        .checked_add(Duration::from_secs_f64(timeout_seconds))
        .ok_or_else(|| format!("{context}: timeout is too large").into())
}

pub(super) fn capture_deadline_remaining(
    deadline: Instant,
    phase: &str,
) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(format!("capture deadline expired before {phase}"))
    } else {
        Ok(remaining)
    }
}

pub(super) fn capture_work_deadline(deadline: Instant) -> Instant {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let reserve = CAPTURE_CLEANUP_RESERVE.min(remaining / 5);
    deadline.checked_sub(reserve).unwrap_or(deadline)
}

pub(super) async fn capture_remote_request_until(
    port: u16,
    op: &str,
    args: serde_json::Value,
    deadline: Instant,
    phase: &str,
) -> Result<serde_json::Value, String> {
    let remaining = capture_deadline_remaining(deadline, phase)?;
    tokio::time::timeout(
        remaining,
        remote::request_with_timeout(port, op, args, remaining),
    )
    .await
    .map_err(|_| format!("capture deadline expired during {phase}"))?
}

pub(super) fn validate_artifact_id(id: &str) -> Result<&str, String> {
    if id.len() == 48 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err("capture artifact id must be exactly 48 hexadecimal characters".into())
    }
}

pub(super) fn plugin_artifact_id<'a>(
    artifact: &'a serde_json::Value,
    context: &str,
) -> Result<&'a str, String> {
    let id = artifact
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{context}: response omitted artifact id"))?;
    validate_artifact_id(id).map_err(|error| format!("{context}: {error}"))
}

pub(super) async fn lookup_artifact_transport_until(
    port: u16,
    id: &str,
    deadline: Instant,
) -> Result<artifact::ArtifactMetadata, String> {
    validate_artifact_id(id)?;
    let response = http_get_json_until(port, &format!("/artifacts/{id}"), deadline).await?;
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("artifact lookup rejected: {response}"));
    }
    let metadata: artifact::ArtifactMetadata = serde_json::from_value(
        response
            .get("artifact")
            .cloned()
            .ok_or_else(|| "artifact lookup omitted metadata".to_string())?,
    )
    .map_err(|error| format!("artifact lookup returned invalid metadata: {error}"))?;
    if metadata.id != id {
        return Err(format!(
            "artifact lookup returned id {}, expected {id}",
            metadata.id
        ));
    }
    if metadata.mime != "image/png" {
        return Err(format!(
            "artifact {id} has MIME {}, expected image/png",
            metadata.mime
        ));
    }
    if metadata.size == 0 || metadata.size > CAPTURE_MAX_ARTIFACT_BYTES {
        return Err(format!(
            "artifact {id} size {} is outside the capture limit",
            metadata.size
        ));
    }
    if metadata.sha256.len() != 64 || !metadata.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("artifact {id} has an invalid SHA-256 digest"));
    }
    if !metadata.path.is_absolute() {
        return Err(format!("artifact {id} path is not absolute"));
    }
    Ok(metadata)
}

pub(super) fn read_bounded_capture_file(
    metadata: &artifact::ArtifactMetadata,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    if metadata.size == 0 || metadata.size > CAPTURE_MAX_ARTIFACT_BYTES {
        return Err(format!(
            "artifact size {} is outside the capture limit",
            metadata.size
        ));
    }
    let expected = usize::try_from(metadata.size)
        .map_err(|_| "artifact size does not fit this platform".to_string())?;
    let file_metadata = std::fs::metadata(&metadata.path).map_err(|error| {
        format!(
            "read artifact metadata {}: {error}",
            metadata.path.display()
        )
    })?;
    if !file_metadata.is_file() {
        return Err(format!(
            "artifact path is not a regular file: {}",
            metadata.path.display()
        ));
    }
    if file_metadata.len() != metadata.size {
        return Err(format!(
            "artifact file size {} does not match daemon metadata {}",
            file_metadata.len(),
            metadata.size
        ));
    }
    let mut file = std::fs::File::open(&metadata.path)
        .map_err(|error| format!("open artifact {}: {error}", metadata.path.display()))?;
    let mut bytes = Vec::with_capacity(expected);
    let mut buffer = [0u8; 64 * 1024];
    let bounded_length = expected + 1;
    while bytes.len() < bounded_length {
        capture_deadline_remaining(deadline, "artifact read")?;
        let available = (bounded_length - bytes.len()).min(buffer.len());
        let count = file
            .read(&mut buffer[..available])
            .map_err(|error| format!("read artifact {}: {error}", metadata.path.display()))?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != expected {
        return Err(format!(
            "artifact read {} bytes, expected {expected}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

pub(super) fn verify_capture_png(
    bytes: &[u8],
    expected_dimensions: Option<(u32, u32)>,
    deadline: Instant,
) -> Result<(u32, u32), String> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("capture artifact is not a PNG".into());
    }
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("decode capture PNG header: {error}"))?;
    let width = reader.info().width;
    let height = reader.info().height;
    validate_capture_dimensions(width, height).map_err(|error| error.to_string())?;
    if let Some((expected_width, expected_height)) = expected_dimensions {
        if (width, height) != (expected_width, expected_height) {
            return Err(format!(
                "capture PNG dimensions {width}x{height} do not match reported {expected_width}x{expected_height}"
            ));
        }
    }
    loop {
        capture_deadline_remaining(deadline, "PNG verification")?;
        match reader
            .next_row()
            .map_err(|error| format!("decode capture PNG: {error}"))?
        {
            Some(_) => {}
            None => break,
        }
    }
    Ok((width, height))
}

#[derive(Debug)]
pub(super) struct MaterializedCapture {
    pub(super) metadata: artifact::ArtifactMetadata,
    pub(super) output_path: Option<PathBuf>,
    pub(super) size: usize,
    pub(super) sha256: String,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) consumed: bool,
}

pub(super) async fn materialize_capture_artifact(
    port: u16,
    id: &str,
    expected_size: Option<u64>,
    expected_dimensions: Option<(u32, u32)>,
    destination: Option<&std::path::Path>,
    deadline: Instant,
    context: &str,
) -> Result<MaterializedCapture, Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};

    validate_artifact_id(id).map_err(|error| format!("{context}: {error}"))?;
    let work_deadline = capture_work_deadline(deadline);
    let primary: Result<MaterializedCapture, Box<dyn std::error::Error>> = async {
        let metadata = lookup_artifact_transport_until(port, id, work_deadline).await?;
        if let Some(expected_size) = expected_size {
            if metadata.size != expected_size {
                return Err(format!(
                    "{context}: daemon artifact size {} does not match reported {expected_size}",
                    metadata.size
                )
                .into());
            }
        }
        let bytes = read_bounded_capture_file(&metadata, work_deadline)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if !sha256.eq_ignore_ascii_case(&metadata.sha256) {
            return Err(format!(
                "{context}: SHA-256 mismatch (computed {sha256}, daemon {})",
                metadata.sha256
            )
            .into());
        }
        let (width, height) = verify_capture_png(&bytes, expected_dimensions, work_deadline)
            .map_err(|error| format!("{context}: {error}"))?;
        let output_path = if let Some(destination) = destination {
            capture_deadline_remaining(work_deadline, "capture output")?;
            if let Some(parent) = destination.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("{context}: create {}: {error}", parent.display())
                    })?;
                }
            }
            std::fs::write(destination, &bytes)
                .map_err(|error| format!("{context}: write {}: {error}", destination.display()))?;
            Some(std::fs::canonicalize(destination).unwrap_or_else(|_| destination.to_path_buf()))
        } else {
            None
        };
        Ok(MaterializedCapture {
            metadata,
            output_path,
            size: bytes.len(),
            sha256,
            width,
            height,
            consumed: false,
        })
    }
    .await;

    let consume_result = consume_artifact_transport_until(port, id, deadline).await;
    match primary {
        Ok(mut materialized) => {
            materialized.consumed = consume_result.is_ok();
            if let Err(error) = consume_result {
                eprintln!("{context}: warning: could not remove transport artifact: {error}");
            }
            Ok(materialized)
        }
        Err(error) => {
            if let Err(cleanup) = consume_result {
                Err(format!("{error}; artifact cleanup also failed: {cleanup}").into())
            } else {
                Err(error)
            }
        }
    }
}

pub(super) async fn cleanup_artifact_lease_until(
    port: u16,
    id: &str,
    token: &str,
    deadline: Instant,
) {
    if consume_artifact_transport_until(port, id, deadline)
        .await
        .is_ok()
    {
        return;
    }
    if capture_deadline_remaining(deadline, "artifact abort").is_ok() {
        let _ = http_post_json_until(
            port,
            &format!("/artifacts/{id}/abort"),
            &serde_json::json!({ "token": token }),
            deadline,
        )
        .await;
    }
}

pub(super) fn capture_error_allows_macos_window_fallback(
    args: &CaptureScreenArgs,
    error: &str,
) -> bool {
    if !cfg!(target_os = "macos")
        || args.ui != CaptureUiMode::All
        || args.focus.is_some()
        || args.view.is_some()
        || args.padding.is_some()
    {
        return false;
    }
    let normalized = error.to_ascii_lowercase();
    normalized
        .contains("studio screenshot provider is unsupported after explicit capture authorization")
        && normalized.contains("feature not supported yet")
}

pub(super) async fn run_macos_window_capture_fallback(
    args: &CaptureScreenArgs,
    region: Option<CaptureRegion>,
    output_size: Option<[u32; 2]>,
    deadline: Instant,
    studio_error: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};

    eprintln!(
        "capture: Studio screenshot provider unavailable ({studio_error}); using the macOS Roblox Studio window fallback for --ui all"
    );
    let project_hint = args
        .project
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str);
    let result = native_capture::capture_studio_window(native_capture::NativeCaptureRequest {
        project_hint,
        region: region.map(|region| native_capture::CaptureRegion {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        }),
        output_size,
        pixelated: args.resample == CaptureResampleMode::Pixelated,
        output: &args.output,
        deadline,
        limits: native_capture::CaptureLimits {
            max_dimension: CAPTURE_MAX_DIMENSION,
            max_pixels: CAPTURE_MAX_PIXELS,
            max_bytes: CAPTURE_MAX_ARTIFACT_BYTES,
        },
    })
    .await
    .map_err(|native_error| {
        format!(
            "capture: Studio provider failed ({studio_error}); macOS window fallback failed: {native_error}"
        )
    })?;

    // Run the same structural/decode verification used for Studio transport
    // artifacts before reporting a native capture as successful.
    let bytes = std::fs::read(&result.output_path).map_err(|error| {
        format!(
            "capture: verify native output {}: {error}",
            result.output_path.display()
        )
    })?;
    if bytes.len() != result.size
        || bytes.is_empty()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CAPTURE_MAX_ARTIFACT_BYTES
    {
        let _ = std::fs::remove_file(&result.output_path);
        return Err("capture: native output changed size before verification".into());
    }
    let (width, height) = match verify_capture_png(
        &bytes,
        Some((result.width, result.height)),
        capture_work_deadline(deadline),
    ) {
        Ok(dimensions) => dimensions,
        Err(error) => {
            let _ = std::fs::remove_file(&result.output_path);
            return Err(format!("capture: verify native output: {error}").into());
        }
    };
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if sha256 != result.sha256 {
        let _ = std::fs::remove_file(&result.output_path);
        return Err("capture: native output SHA-256 changed before verification".into());
    }
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "artifact": {
                    "path": result.output_path,
                    "transport": {
                        "kind": "direct-local",
                        "consumed": true,
                    },
                    "provider": "macos-window",
                    "fallbackFrom": "StudioCaptureService",
                    "mime": "image/png",
                    "size": result.size,
                    "sha256": sha256,
                    "width": width,
                    "height": height,
                    "position": {
                        "x": result.position[0],
                        "y": result.position[1],
                    },
                    "window": result.window,
                }
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {}; macOS Roblox Studio window fallback)",
            result.output_path.display(),
            width,
            height,
            result.size,
            sha256
        );
    }
    Ok(())
}

pub(super) async fn run_capture_screen(
    args: CaptureScreenArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = capture_deadline(args.timeout, "capture")?;

    let region = args
        .region
        .as_deref()
        .map(parse_capture_region)
        .transpose()?;
    let output_size = args
        .output_size
        .as_deref()
        .map(parse_capture_size)
        .transpose()?;
    if let Some(region) = region {
        validate_capture_dimensions(region.width, region.height)?;
    }
    if let Some([width, height]) = output_size {
        validate_capture_dimensions(width, height)?;
    }
    let mut request = serde_json::Map::new();
    request.insert(
        "ui".into(),
        serde_json::Value::String(args.ui.as_plugin_str().to_string()),
    );
    request.insert(
        "resample".into(),
        serde_json::Value::String(args.resample.as_plugin_str().to_string()),
    );
    request.insert("timeoutSeconds".into(), serde_json::json!(args.timeout));
    if let Some(region) = region {
        request.insert(
            "position".into(),
            serde_json::json!({ "x": region.x, "y": region.y }),
        );
        request.insert(
            "captureSize".into(),
            serde_json::json!({ "x": region.width, "y": region.height }),
        );
    }
    if let Some([width, height]) = output_size {
        request.insert(
            "outputSize".into(),
            serde_json::json!({ "x": width, "y": height }),
        );
    }
    if let Some(focus) = &args.focus {
        request.insert("focus".into(), serde_json::Value::String(focus.clone()));
    }
    if let Some(view) = args.view {
        request.insert(
            "view".into(),
            serde_json::Value::String(view.as_plugin_str().to_string()),
        );
    }
    if let Some(padding) = args.padding {
        request.insert("padding".into(), serde_json::json!(padding));
    }

    let work_deadline = capture_work_deadline(deadline);
    let prepare_resp = capture_remote_request_until(
        args.port,
        "capture_prepare",
        serde_json::Value::Object(request),
        work_deadline,
        "capture prepare",
    )
    .await?;
    let prepared_value = match response_value_or_err(&prepare_resp, "capture prepare") {
        Ok(value) => value,
        Err(error) => {
            let error = error.to_string();
            if capture_error_allows_macos_window_fallback(&args, &error) {
                return run_macos_window_capture_fallback(
                    &args,
                    region,
                    output_size,
                    deadline,
                    &error,
                )
                .await;
            }
            return Err(error.into());
        }
    };
    let session_hint = prepared_value
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let prepared: CapturePrepared = match serde_json::from_value(prepared_value) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(session_id) = session_hint {
                let _ = capture_remote_request_until(
                    args.port,
                    "capture_close",
                    serde_json::json!({ "sessionId": session_id }),
                    deadline,
                    "capture close",
                )
                .await;
            }
            return Err(format!("capture: plugin returned invalid metadata: {error}").into());
        }
    };
    let session_id = prepared.session_id.clone();
    let mut lease_credentials: Option<(String, String)> = None;
    let flow: Result<MaterializedCapture, Box<dyn std::error::Error>> = async {
        validate_capture_dimensions(prepared.width, prepared.height)?;
        let prepared_size = u64::try_from(prepared.byte_length)
            .map_err(|_| "capture: reported artifact size does not fit u64")?;
        if prepared_size == 0 || prepared_size > CAPTURE_MAX_ARTIFACT_BYTES {
            return Err(format!(
                "capture: plugin reported an invalid artifact size of {} bytes",
                prepared.byte_length
            )
            .into());
        }
        let lease_response = http_post_json_until(
            args.port,
            "/artifacts/lease",
            &serde_json::json!({
                "filename": "studio-capture.png",
                "mime": "image/png",
                "expectedSize": prepared_size,
            }),
            work_deadline,
        )
        .await
        .map_err(|error| format!("capture: create artifact lease: {error}"))?;
        if lease_response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(format!("capture: artifact lease rejected: {lease_response}").into());
        }
        let lease = lease_response
            .get("lease")
            .cloned()
            .ok_or("capture: artifact lease response omitted lease")?;
        let lease_id = plugin_artifact_id(&lease, "capture lease")?.to_string();
        let lease_token = lease
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or("capture: artifact lease omitted token")?
            .to_string();
        lease_credentials = Some((lease_id.clone(), lease_token));
        let export_timeout = capture_deadline_remaining(work_deadline, "capture export")?;
        let export_response = capture_remote_request_until(
            args.port,
            "capture_export",
            serde_json::json!({
                "sessionId": session_id,
                "lease": lease,
                "timeoutSeconds": export_timeout.as_secs_f64(),
            }),
            work_deadline,
            "capture export",
        )
        .await?;
        let plugin_artifact = response_value_or_err(&export_response, "capture export")?;
        let returned_id = plugin_artifact_id(&plugin_artifact, "capture export")?;
        if returned_id != lease_id {
            return Err(format!(
                "capture: plugin finalized artifact {returned_id}, expected lease {lease_id}"
            )
            .into());
        }
        materialize_capture_artifact(
            args.port,
            &lease_id,
            Some(prepared_size),
            Some((prepared.width, prepared.height)),
            Some(&args.output),
            deadline,
            "capture",
        )
        .await
    }
    .await;

    if capture_deadline_remaining(deadline, "capture close").is_ok() {
        let _ = capture_remote_request_until(
            args.port,
            "capture_close",
            serde_json::json!({ "sessionId": session_id }),
            deadline,
            "capture close",
        )
        .await;
    }
    let mut materialized = match flow {
        Ok(materialized) => materialized,
        Err(error) => {
            if let Some((id, token)) = &lease_credentials {
                cleanup_artifact_lease_until(args.port, id, token, deadline).await;
            }
            return Err(error);
        }
    };
    if !materialized.consumed {
        if consume_artifact_transport_until(args.port, &materialized.metadata.id, deadline)
            .await
            .is_ok()
        {
            materialized.consumed = true;
        } else if let Some((id, token)) = &lease_credentials {
            cleanup_artifact_lease_until(args.port, id, token, deadline).await;
        }
    }
    let absolute = materialized
        .output_path
        .clone()
        .ok_or("capture: output path was not materialized")?;
    if args.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "artifact": {
                    "path": absolute,
                    "provider": "studio",
                    "transport": {
                        "metadata": materialized.metadata,
                        "consumed": materialized.consumed,
                    },
                    "mime": "image/png",
                    "size": materialized.size,
                    "sha256": materialized.sha256,
                    "width": materialized.width,
                    "height": materialized.height,
                    "position": prepared.position,
                }
            }))?
        );
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {})",
            absolute.display(),
            materialized.width,
            materialized.height,
            materialized.size,
            materialized.sha256
        );
    }
    Ok(())
}
