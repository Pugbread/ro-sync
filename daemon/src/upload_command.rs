use super::*;

pub(super) async fn run_upload(args: UploadArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_upload_inner(args).await
}

pub(super) async fn run_img(args: ImgArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_upload_inner(UploadArgs {
        inputs: vec![args.path],
        project: args.project,
        creator: args.creator,
        name: args.name,
        description: args.description,
        asset_type: Some(args.asset_type),
        content_type: None,
        auth: args.auth,
        api_key_env: args.api_key_env,
        no_wait: args.no_wait,
        timeout_seconds: args.timeout_seconds,
        poll_seconds: args.poll_seconds,
        concurrency: 1,
        no_recursive: true,
        manifest: None,
        raw: args.raw,
    })
    .await
}

pub(super) async fn run_imgs(args: ImgsArgs) -> Result<(), Box<dyn std::error::Error>> {
    run_upload_inner(UploadArgs {
        inputs: args.inputs,
        project: args.project,
        creator: args.creator,
        name: None,
        description: args.description,
        asset_type: Some(args.asset_type),
        content_type: None,
        auth: args.auth,
        api_key_env: args.api_key_env,
        no_wait: args.no_wait,
        timeout_seconds: args.timeout_seconds,
        poll_seconds: args.poll_seconds,
        concurrency: args.concurrency,
        no_recursive: args.no_recursive,
        manifest: args.manifest,
        raw: args.raw,
    })
    .await
}

pub(super) async fn run_monetization(
    args: MonetizationArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        MonetizationCommand::Gamepass(args) => {
            run_monetization_asset(MonetizationKind::Gamepass, args).await
        }
        MonetizationCommand::Product(args) => {
            run_monetization_asset(MonetizationKind::Product, args).await
        }
    }
}

pub(super) async fn run_monetization_asset(
    kind: MonetizationKind,
    args: MonetizationAssetArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        MonetizationAction::Discover(args) => run_monetization_discover(kind, args),
        MonetizationAction::List(args) => run_monetization_list(kind, args).await,
        MonetizationAction::Create(args) => run_monetization_create(kind, args).await,
        MonetizationAction::Edit(args) => run_monetization_edit(kind, args).await,
        MonetizationAction::Image(args) => run_monetization_image(kind, args).await,
        MonetizationAction::Images(args) => run_monetization_images(kind, args).await,
    }
}

pub(super) fn run_monetization_discover(
    kind: MonetizationKind,
    args: MonetizationDiscoverArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = project_or_cwd(args.project.as_deref(), "monetization discover")?;
    let hits = discover_monetization_files(&project)?;
    let value = serde_json::json!({
        "ok": true,
        "kind": kind.label(),
        "project": project.display().to_string(),
        "universeId": resolve_monetization_universe_id(args.project.as_deref()).ok(),
        "credentialSources": monetization_credential_sources(args.project.as_deref()),
        "matches": hits,
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub(super) async fn run_monetization_list(
    kind: MonetizationKind,
    args: MonetizationCommonArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args)?;
    let value = monetization_list_api(kind, &universe_id, &api_key).await?;
    if args.raw {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        let items = monetization_items_from_response(kind, &value);
        if items.is_empty() {
            println!("no {} entries returned", kind.label());
        } else {
            for item in items {
                println!(
                    "{}\t{}\t{}",
                    item.id.unwrap_or_else(|| "?".into()),
                    item.price
                        .map(|price| price.to_string())
                        .unwrap_or_else(|| "-".into()),
                    item.name.unwrap_or_else(|| "?".into())
                );
            }
        }
    }
    Ok(())
}

pub(super) async fn run_monetization_create(
    kind: MonetizationKind,
    args: MonetizationCreateArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let specs = monetization_create_specs(&args)?;
    let existing = monetization_list_api(kind, &universe_id, &api_key).await?;
    let existing_names = monetization_items_from_response(kind, &existing)
        .into_iter()
        .filter_map(|item| item.name)
        .map(|name| normalize_monetization_name(&name))
        .collect::<std::collections::HashSet<_>>();
    let mut results = Vec::new();
    for spec in specs {
        if existing_names.contains(&normalize_monetization_name(&spec.name)) {
            results.push(serde_json::json!({
                "ok": false,
                "kind": kind.label(),
                "name": spec.name,
                "price": spec.price,
                "error": "asset with this normalized name already exists",
            }));
            continue;
        }
        match monetization_create_one(kind, &universe_id, &api_key, &args, &spec).await {
            Ok(value) => results.push(serde_json::json!({
                "ok": true,
                "kind": kind.label(),
                "name": spec.name,
                "price": spec.price,
                "id": monetization_id_from_value(kind, &value),
                "response": value,
            })),
            Err(e) => results.push(serde_json::json!({
                "ok": false,
                "kind": kind.label(),
                "name": spec.name,
                "price": spec.price,
                "error": e.to_string(),
            })),
        }
    }
    let ok = results.iter().all(|value| value["ok"] == true);
    let out = serde_json::json!({ "ok": ok, "results": results });
    println!("{}", serde_json::to_string_pretty(&out)?);
    if !ok {
        return Err("monetization create: one or more entries failed".into());
    }
    Ok(())
}

pub(super) async fn run_monetization_edit(
    kind: MonetizationKind,
    args: MonetizationEditArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let id =
        resolve_monetization_asset_id(kind, &universe_id, &api_key, args.id, args.name).await?;
    let value = monetization_update_one(kind, &universe_id, &api_key, &id, |mut form| {
        if let Some(name) = &args.new_name {
            form = form.text("name", name.clone());
        }
        if let Some(price) = args.price {
            form = form.text("price", price.to_string());
        }
        if let Some(description) = &args.description {
            form = form.text("description", description.clone());
        }
        if let Some(for_sale) = args.for_sale {
            form = form.text("isForSale", for_sale.to_string());
        }
        form
    })
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": true,
            "kind": kind.label(),
            "id": id,
            "response": value,
        }))?
    );
    Ok(())
}

pub(super) async fn run_monetization_image(
    kind: MonetizationKind,
    args: MonetizationImageArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let id =
        resolve_monetization_asset_id(kind, &universe_id, &api_key, args.id, args.name).await?;
    let value = monetization_update_image(kind, &universe_id, &api_key, &id, &args.file).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": true,
            "kind": kind.label(),
            "id": id,
            "file": args.file,
            "response": value,
        }))?
    );
    Ok(())
}

pub(super) async fn run_monetization_images(
    kind: MonetizationKind,
    args: MonetizationImagesArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let (universe_id, api_key) = monetization_context(&args.common)?;
    let list = monetization_list_api(kind, &universe_id, &api_key).await?;
    let items = monetization_items_from_response(kind, &list);
    let mut by_name = HashMap::new();
    for item in items {
        if let (Some(id), Some(name)) = (item.id, item.name) {
            by_name.insert(normalize_monetization_name(&name), id);
        }
    }
    let mut results = Vec::new();
    let mut entries = std::fs::read_dir(&args.dir)
        .map_err(|e| format!("monetization images: read {}: {e}", args.dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if !path.is_file() || !is_supported_image_path(&path) {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let key = normalize_monetization_name(stem);
        let Some(id) = by_name.get(&key).cloned() else {
            results.push(serde_json::json!({
                "ok": false,
                "file": path,
                "error": "no asset matched normalized filename",
            }));
            continue;
        };
        match monetization_update_image(kind, &universe_id, &api_key, &id, &path).await {
            Ok(value) => results.push(serde_json::json!({
                "ok": true,
                "id": id,
                "file": path,
                "response": value,
            })),
            Err(e) => results.push(serde_json::json!({
                "ok": false,
                "id": id,
                "file": path,
                "error": e.to_string(),
            })),
        }
    }
    let ok = results.iter().all(|value| value["ok"] == true);
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "ok": ok, "results": results }))?
    );
    if !ok {
        return Err("monetization images: one or more images failed".into());
    }
    Ok(())
}

pub(super) async fn run_upload_inner(args: UploadArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.no_wait && args.timeout_seconds == 0 {
        return Err(
            "upload: --timeout-seconds must be greater than 0 unless --no-wait is used".into(),
        );
    }
    if !args.no_wait && args.poll_seconds == 0 {
        return Err(
            "upload: --poll-seconds must be greater than 0 unless --no-wait is used".into(),
        );
    }
    if args.concurrency == 0 {
        return Err("upload: --concurrency must be greater than 0".into());
    }
    if args
        .content_type
        .as_deref()
        .is_some_and(|content_type| content_type.trim().is_empty())
    {
        return Err("upload: --content-type cannot be empty".into());
    }

    let mut failures = Vec::new();
    let jobs = collect_upload_jobs(
        &args.inputs,
        !args.no_recursive,
        args.asset_type,
        args.content_type.as_deref(),
        &mut failures,
    )?;
    let attempted = jobs.len() + failures.len();
    if args.name.is_some() && attempted != 1 {
        return Err("upload: --name can only be used when exactly one file is uploaded".into());
    }
    if jobs.is_empty() && failures.is_empty() {
        return Err("upload: no supported asset files found".into());
    }

    let mut results = failures;
    if !jobs.is_empty() {
        let creator_text = args
            .creator
            .or_else(|| std::env::var("ROBLOX_CREATOR").ok())
            .or_else(|| resolve_img_creator(&args.project))
            .ok_or("upload: missing creator. Pass --creator user:<id> or group:<id>, set ROBLOX_CREATOR, or set a project Group ID.")?;
        let creator = img_upload::parse_creator(&creator_text)
            .map_err(|e| format!("upload: invalid creator {creator_text:?}: {e}"))?;
        let api_key = resolve_img_api_key(args.api_key_env.as_deref())?;

        let semaphore = Arc::new(tokio::sync::Semaphore::new(args.concurrency));
        let mut tasks = futures::stream::FuturesUnordered::new();
        for job in jobs {
            let permit = semaphore.clone().acquire_owned().await?;
            let api_key = api_key.clone();
            let creator = creator.clone();
            let description = args.description.clone();
            let auth_mode = args.auth.as_upload_mode();
            let wait = !args.no_wait;
            let timeout = Duration::from_secs(args.timeout_seconds);
            let poll = Duration::from_secs(args.poll_seconds);
            let display_name = args.name.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                upload_asset_job(
                    job,
                    api_key,
                    auth_mode,
                    creator,
                    description,
                    display_name,
                    wait,
                    timeout,
                    poll,
                )
                .await
            }));
        }

        while let Some(result) = tasks.next().await {
            match result {
                Ok(result) => results.push(result),
                Err(e) => results.push(UploadBulkResult {
                    index: usize::MAX,
                    file: String::new(),
                    display_name: None,
                    asset_type: None,
                    content_type: None,
                    ok: false,
                    asset_id: None,
                    asset_uri: None,
                    operation_path: None,
                    error: Some(format!("task failed: {e}")),
                }),
            }
        }
    }
    results.sort_by_key(|result| result.index);

    if let Some(path) = &args.manifest {
        write_upload_manifest(path, &results)?;
    }

    if args.raw && results.len() == 1 && results[0].ok {
        println!("{}", serde_json::to_string_pretty(&results[0])?);
    } else if args.raw {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print_upload_results(&results);
    }

    let failed = results.iter().filter(|result| !result.ok).count();
    if failed > 0 {
        return Err(format!("upload: {failed} upload(s) failed").into());
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct UploadJob {
    pub(super) index: usize,
    pub(super) file: PathBuf,
    pub(super) media: UploadMedia,
}

#[derive(Clone)]
pub(super) struct UploadMedia {
    pub(super) asset_type: UploadAssetType,
    pub(super) content_type: String,
}

#[derive(Clone, Debug)]
pub(super) struct MonetizationCreateSpec {
    pub(super) name: String,
    pub(super) price: u64,
}

#[derive(Clone, Debug)]
pub(super) struct MonetizationListedItem {
    pub(super) id: Option<String>,
    pub(super) name: Option<String>,
    pub(super) price: Option<u64>,
}

pub(super) fn monetization_context(
    args: &MonetizationCommonArgs,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let universe_id = args
        .universe_id
        .clone()
        .or_else(|| resolve_monetization_universe_id(args.project.as_deref()).ok())
        .ok_or("monetization: missing universe id. Pass --universe-id, set ROBLOX_UNIVERSE_ID/GAMEID, or set ro-sync.json gameId.")?;
    let api_key =
        resolve_monetization_api_key(args.project.as_deref(), args.api_key_env.as_deref())?;
    Ok((universe_id, api_key))
}

pub(super) fn resolve_monetization_universe_id(
    project: Option<&std::path::Path>,
) -> Result<String, String> {
    for env_name in ["ROBLOX_UNIVERSE_ID", "UNIVERSE_ID", "GAMEID", "GAME_ID"] {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    for (key, value) in read_project_env_values(project) {
        if matches!(
            key.as_str(),
            "ROBLOX_UNIVERSE_ID" | "UNIVERSE_ID" | "GAMEID" | "GAME_ID"
        ) && !value.trim().is_empty()
        {
            return Ok(value.trim().to_string());
        }
    }

    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    project_config::read_from_disk(&root)
        .map_err(|e| e.to_string())?
        .and_then(|cfg| cfg.game_id)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "no universe id found".to_string())
}

pub(super) fn resolve_monetization_api_key(
    project: Option<&std::path::Path>,
    preferred_env: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut env_names = Vec::new();
    if let Some(env_name) = preferred_env {
        env_names.push(env_name.to_string());
    }
    for env_name in [
        "ROBLOX_API_KEY",
        "CLOUD_API_KEY",
        "ROBLOX_OPEN_CLOUD_API_KEY",
    ] {
        if !env_names.iter().any(|existing| existing == env_name) {
            env_names.push(env_name.to_string());
        }
    }

    if let Some(env_name) = preferred_env {
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    } else if let Some(value) = find_widget_secret("robloxCloudApiKey") {
        return Ok(value);
    }

    let env_values = read_project_env_values(project);
    if let Some(env_name) = preferred_env {
        if let Some((_, value)) = env_values
            .iter()
            .find(|(key, value)| key == env_name && !value.trim().is_empty())
        {
            return Ok(value.trim().to_string());
        }
    }

    for env_name in &env_names {
        if Some(env_name.as_str()) == preferred_env {
            continue;
        }
        if let Ok(value) = std::env::var(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    for env_name in &env_names {
        if Some(env_name.as_str()) == preferred_env {
            continue;
        }
        if let Some((_, value)) = env_values
            .iter()
            .find(|(key, value)| key == env_name && !value.trim().is_empty())
        {
            return Ok(value.trim().to_string());
        }
    }

    if let Some(value) = find_widget_secret("robloxCloudApiKey") {
        return Ok(value);
    }

    Err(format!(
        "monetization: missing Roblox Open Cloud API key. Save one in Ro Sync Settings, set one of {}, or add it to a project env file.",
        env_names.join(", ")
    )
    .into())
}

pub(super) fn monetization_credential_sources(project: Option<&std::path::Path>) -> Vec<String> {
    let env_values = read_project_env_values(project);
    let mut sources = Vec::new();
    for env_name in [
        "ROBLOX_API_KEY",
        "CLOUD_API_KEY",
        "ROBLOX_OPEN_CLOUD_API_KEY",
        "ROBLOX_UNIVERSE_ID",
        "UNIVERSE_ID",
        "GAMEID",
        "GAME_ID",
    ] {
        if std::env::var(env_name)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        {
            sources.push(format!("env:{env_name}"));
        }
        if env_values
            .iter()
            .any(|(key, value)| key == env_name && !value.trim().is_empty())
        {
            sources.push(format!("project-env:{env_name}"));
        }
    }
    if find_widget_secret("robloxCloudApiKey").is_some() {
        sources.push("rosync-secret:robloxCloudApiKey".to_string());
    }
    sources
}

pub(super) fn read_project_env_values(project: Option<&std::path::Path>) -> Vec<(String, String)> {
    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let mut values = Vec::new();
    for file_name in ["info.env", ".env", ".env.local"] {
        let path = root.join(file_name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().trim_start_matches("export ").to_string();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !key.is_empty() {
                values.push((key, value));
            }
        }
    }
    values
}

pub(super) fn monetization_create_specs(
    args: &MonetizationCreateArgs,
) -> Result<Vec<MonetizationCreateSpec>, Box<dyn std::error::Error>> {
    if let Some(name) = &args.name {
        let price = args
            .price
            .ok_or("monetization create: --price is required with --name")?;
        if !args.entries.is_empty() {
            return Err(
                "monetization create: use either entries or --name/--price, not both".into(),
            );
        }
        return Ok(vec![MonetizationCreateSpec {
            name: name.trim().to_string(),
            price,
        }]);
    }
    if args.price.is_some() {
        return Err("monetization create: --price requires --name".into());
    }

    let mut specs = Vec::new();
    for raw in &args.entries {
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            specs.push(parse_monetization_create_entry(entry)?);
        }
    }
    if specs.is_empty() {
        return Err("monetization create: provide at least one entry like `VIP 499 robux` or --name/--price".into());
    }
    Ok(specs)
}

pub(super) fn parse_monetization_create_entry(
    entry: &str,
) -> Result<MonetizationCreateSpec, Box<dyn std::error::Error>> {
    let mut tokens: Vec<&str> = entry.split_whitespace().collect();
    while tokens
        .last()
        .is_some_and(|token| token.eq_ignore_ascii_case("robux"))
    {
        tokens.pop();
    }
    let Some(price_token) = tokens.pop() else {
        return Err(format!("invalid monetization entry {entry:?}: missing price").into());
    };
    let price = price_token
        .parse::<u64>()
        .map_err(|_| format!("invalid monetization entry {entry:?}: price must be a number"))?;
    let name = tokens.join(" ").trim().to_string();
    if name.is_empty() {
        return Err(format!("invalid monetization entry {entry:?}: missing name").into());
    }
    Ok(MonetizationCreateSpec { name, price })
}

pub(super) async fn monetization_list_api(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let url = format!("{}/creator", kind.base_url(universe_id));
    let response = reqwest::Client::new()
        .get(url)
        .header("x-api-key", api_key)
        .send()
        .await?;
    monetization_response(response, "list").await
}

pub(super) async fn monetization_create_one(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    args: &MonetizationCreateArgs,
    spec: &MonetizationCreateSpec,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut form = reqwest::multipart::Form::new()
        .text("name", spec.name.clone())
        .text("price", spec.price.to_string())
        .text("isForSale", (!args.not_for_sale).to_string());
    if let Some(description) = &args.description {
        form = form.text("description", description.clone());
    }
    if let Some(image) = &args.image {
        form = add_file_part(form, kind.create_image_field(), image)?;
    }
    let response = reqwest::Client::new()
        .post(kind.base_url(universe_id))
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await?;
    monetization_response(response, "create").await
}

pub(super) async fn monetization_update_one<F>(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    id: &str,
    build_form: F,
) -> Result<serde_json::Value, Box<dyn std::error::Error>>
where
    F: FnOnce(reqwest::multipart::Form) -> reqwest::multipart::Form,
{
    let form = build_form(reqwest::multipart::Form::new());
    let response = reqwest::Client::new()
        .patch(format!("{}/{}", kind.base_url(universe_id), id))
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await?;
    monetization_response(response, "update").await
}

pub(super) async fn monetization_update_image(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    id: &str,
    file: &std::path::Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let form = add_file_part(
        reqwest::multipart::Form::new(),
        kind.update_image_field(),
        file,
    )?;
    let response = reqwest::Client::new()
        .patch(format!("{}/{}", kind.base_url(universe_id), id))
        .header("x-api-key", api_key)
        .multipart(form)
        .send()
        .await?;
    monetization_response(response, "image").await
}

pub(super) fn add_file_part(
    form: reqwest::multipart::Form,
    field: &'static str,
    path: &std::path::Path,
) -> Result<reqwest::multipart::Form, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("monetization: read image {}: {e}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image")
        .to_string();
    let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
    Ok(form.part(field, part))
}

pub(super) async fn monetization_response(
    response: reqwest::Response,
    label: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let status = response.status();
    let text = response.text().await?;
    if status.is_success() {
        if text.trim().is_empty() {
            return Ok(serde_json::json!({ "status": status.as_u16() }));
        }
        return serde_json::from_str(&text).map_err(|e| {
            format!("monetization {label}: expected JSON response, got {text:?}: {e}").into()
        });
    }
    let body = if text.trim().is_empty() {
        "<empty response>".to_string()
    } else {
        text
    };
    Err(format!("monetization {label}: Roblox API returned {status}: {body}").into())
}

pub(super) async fn resolve_monetization_asset_id(
    kind: MonetizationKind,
    universe_id: &str,
    api_key: &str,
    id: Option<String>,
    name: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(id) = id {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let name = name.ok_or("monetization: pass --id or --name")?;
    let key = normalize_monetization_name(&name);
    let list = monetization_list_api(kind, universe_id, api_key).await?;
    let mut matches = monetization_items_from_response(kind, &list)
        .into_iter()
        .filter(|item| {
            item.name
                .as_deref()
                .map(normalize_monetization_name)
                .is_some_and(|item_key| item_key == key)
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    if matches.len() > 1 {
        return Err(format!(
            "monetization: name {name:?} matched multiple {} entries; pass --id",
            kind.label()
        )
        .into());
    }
    matches
        .pop()
        .and_then(|item| item.id)
        .ok_or_else(|| format!("monetization: no {} named {name:?} found", kind.label()).into())
}

pub(super) fn monetization_items_from_response(
    kind: MonetizationKind,
    value: &serde_json::Value,
) -> Vec<MonetizationListedItem> {
    let mut out = Vec::new();
    collect_monetization_items(kind, value, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id && a.name == b.name);
    out
}

pub(super) fn collect_monetization_items(
    kind: MonetizationKind,
    value: &serde_json::Value,
    out: &mut Vec<MonetizationListedItem>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_monetization_items(kind, value, out);
            }
        }
        serde_json::Value::Object(map) => {
            let id = monetization_id_from_value(kind, value);
            let name = map
                .get("name")
                .or_else(|| map.get("displayName"))
                .and_then(json_scalar_to_string);
            if id.is_some() || name.is_some() {
                let price = map
                    .get("price")
                    .or_else(|| map.get("priceInRobux"))
                    .and_then(json_u64);
                out.push(MonetizationListedItem { id, name, price });
            }
            for child in map.values() {
                collect_monetization_items(kind, child, out);
            }
        }
        _ => {}
    }
}

pub(super) fn monetization_id_from_value(
    kind: MonetizationKind,
    value: &serde_json::Value,
) -> Option<String> {
    let map = value.as_object()?;
    for key in [kind.id_field(), "id", "assetId"] {
        if let Some(id) = map.get(key).and_then(json_scalar_to_string) {
            if !id.trim().is_empty() {
                return Some(id);
            }
        }
    }
    None
}

pub(super) fn json_scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(super) fn json_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(value) => value.as_u64(),
        serde_json::Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

pub(super) fn normalize_monetization_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn is_supported_image_path(path: &std::path::Path) -> bool {
    matches!(
        upload_extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "tga"
    )
}

pub(super) fn discover_monetization_files(
    project: &std::path::Path,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    discover_monetization_files_inner(project, project, &mut out)?;
    out.sort_by_key(|value| value["path"].as_str().map(str::to_string));
    Ok(out)
}

pub(super) fn discover_monetization_files_inner(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(|e| format!("monetization discover: read {}: {e}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | "node_modules"
                    | "target"
                    | "tools"
                    | "dist"
                    | "build"
                    | ".cursor"
                    | ".vscode"
                    | ".DS_Store"
            )
        }) {
            continue;
        }
        if path.is_dir() {
            discover_monetization_files_inner(root, &path, out)?;
            continue;
        }
        let Some(ext) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
        else {
            continue;
        };
        if !matches!(ext.as_str(), "luau" | "lua" | "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let matches = [
            "GamePass",
            "Gamepass",
            "DeveloperProduct",
            "ProductId",
            "GamePassId",
            "MarketplaceService",
            "ProcessReceipt",
            "PromptGamePassPurchase",
        ]
        .iter()
        .filter(|needle| text.contains(**needle))
        .copied()
        .collect::<Vec<_>>();
        if matches.is_empty() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        out.push(serde_json::json!({
            "path": rel,
            "matches": matches,
        }));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadBulkResult {
    pub(super) index: usize,
    pub(super) file: String,
    pub(super) display_name: Option<String>,
    pub(super) asset_type: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) ok: bool,
    pub(super) asset_id: Option<String>,
    pub(super) asset_uri: Option<String>,
    pub(super) operation_path: Option<String>,
    pub(super) error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn upload_asset_job(
    job: UploadJob,
    api_key: String,
    auth_mode: img_upload::AuthMode,
    creator: img_upload::Creator,
    description: String,
    display_name: Option<String>,
    wait: bool,
    timeout: Duration,
    poll: Duration,
) -> UploadBulkResult {
    let display_name = display_name.unwrap_or_else(|| img_upload::default_display_name(&job.file));
    let file = job.file.display().to_string();
    let asset_type = job.media.asset_type.as_cloud_str().to_string();
    let content_type = job.media.content_type;
    match img_upload::upload_asset(img_upload::AssetUploadRequest {
        file: job.file,
        api_key,
        auth_mode,
        creator,
        asset_type: asset_type.clone(),
        content_type: content_type.clone(),
        display_name: display_name.clone(),
        description,
        wait,
        timeout,
        poll,
    })
    .await
    {
        Ok(outcome) => UploadBulkResult {
            index: job.index,
            file,
            display_name: Some(display_name),
            asset_type: Some(asset_type),
            content_type: Some(content_type),
            ok: true,
            asset_id: outcome.asset_id,
            asset_uri: outcome.asset_uri,
            operation_path: outcome.operation_path,
            error: None,
        },
        Err(e) => UploadBulkResult {
            index: job.index,
            file,
            display_name: Some(display_name),
            asset_type: Some(asset_type),
            content_type: Some(content_type),
            ok: false,
            asset_id: None,
            asset_uri: None,
            operation_path: None,
            error: Some(e.to_string()),
        },
    }
}

pub(super) fn collect_upload_jobs(
    inputs: &[PathBuf],
    recursive: bool,
    asset_type: Option<UploadAssetType>,
    content_type: Option<&str>,
    failures: &mut Vec<UploadBulkResult>,
) -> Result<Vec<UploadJob>, Box<dyn std::error::Error>> {
    let mut jobs = Vec::new();
    let mut index = 0;
    for input in inputs {
        collect_upload_input(
            input,
            recursive,
            true,
            asset_type,
            content_type,
            &mut index,
            &mut jobs,
            failures,
        )?;
    }
    jobs.sort_by(|a, b| a.file.cmp(&b.file));
    for (idx, job) in jobs.iter_mut().enumerate() {
        job.index = idx;
    }
    for (offset, failure) in failures.iter_mut().enumerate() {
        failure.index = jobs.len() + offset;
    }
    Ok(jobs)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_upload_input(
    input: &std::path::Path,
    recursive: bool,
    explicit: bool,
    asset_type: Option<UploadAssetType>,
    content_type: Option<&str>,
    index: &mut usize,
    jobs: &mut Vec<UploadJob>,
    failures: &mut Vec<UploadBulkResult>,
) -> Result<(), Box<dyn std::error::Error>> {
    if input.is_file() {
        match resolve_upload_media(input, asset_type, content_type, explicit) {
            Ok(media) => {
                jobs.push(UploadJob {
                    index: *index,
                    file: input.to_path_buf(),
                    media,
                });
                *index += 1;
            }
            Err(e) if explicit => {
                failures.push(UploadBulkResult {
                    index: *index,
                    file: input.display().to_string(),
                    display_name: None,
                    asset_type: asset_type.map(|asset_type| asset_type.as_cloud_str().to_string()),
                    content_type: content_type.map(|content_type| content_type.to_string()),
                    ok: false,
                    asset_id: None,
                    asset_uri: None,
                    operation_path: None,
                    error: Some(e),
                });
                *index += 1;
            }
            Err(_) => {}
        }
        return Ok(());
    }
    if input.is_dir() {
        let mut entries = std::fs::read_dir(input)
            .map_err(|e| format!("upload: read directory {}: {e}", input.display()))?
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                if recursive {
                    collect_upload_input(
                        &path,
                        recursive,
                        false,
                        asset_type,
                        content_type,
                        index,
                        jobs,
                        failures,
                    )?;
                }
            } else {
                collect_upload_input(
                    &path,
                    recursive,
                    false,
                    asset_type,
                    content_type,
                    index,
                    jobs,
                    failures,
                )?;
            }
        }
        return Ok(());
    }
    failures.push(UploadBulkResult {
        index: *index,
        file: input.display().to_string(),
        display_name: None,
        asset_type: asset_type.map(|asset_type| asset_type.as_cloud_str().to_string()),
        content_type: content_type.map(|content_type| content_type.to_string()),
        ok: false,
        asset_id: None,
        asset_uri: None,
        operation_path: None,
        error: Some("path does not exist".to_string()),
    });
    *index += 1;
    Ok(())
}

pub(super) fn resolve_upload_media(
    path: &std::path::Path,
    requested_asset_type: Option<UploadAssetType>,
    content_type_override: Option<&str>,
    explicit: bool,
) -> Result<UploadMedia, String> {
    let inferred = infer_upload_media(path);
    let asset_type = match requested_asset_type {
        Some(asset_type) => asset_type,
        None => inferred
            .as_ref()
            .map(|media| media.asset_type)
            .ok_or_else(|| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("asset");
                format!(
                    "unsupported or ambiguous asset type for {name}; pass --asset-type and optionally --content-type"
                )
            })?,
    };
    let content_type = match content_type_override {
        Some(content_type) => content_type.trim().to_string(),
        None => match (requested_asset_type, inferred) {
            (None, Some(media)) => media.content_type,
            _ => content_type_for_asset_type(path, asset_type, explicit)?.to_string(),
        },
    };
    Ok(UploadMedia {
        asset_type,
        content_type,
    })
}

pub(super) fn infer_upload_media(path: &std::path::Path) -> Option<UploadMedia> {
    let ext = upload_extension(path);
    let (asset_type, content_type) = match ext.as_str() {
        "png" => (UploadAssetType::Image, "image/png"),
        "jpg" | "jpeg" => (UploadAssetType::Image, "image/jpeg"),
        "bmp" => (UploadAssetType::Image, "image/bmp"),
        "tga" => (UploadAssetType::Image, "image/tga"),
        "mp3" => (UploadAssetType::Audio, "audio/mpeg"),
        "ogg" => (UploadAssetType::Audio, "audio/ogg"),
        "wav" => (UploadAssetType::Audio, "audio/wav"),
        "flac" => (UploadAssetType::Audio, "audio/flac"),
        "fbx" => (UploadAssetType::Model, "model/fbx"),
        "gltf" => (UploadAssetType::Model, "model/gltf+json"),
        "glb" => (UploadAssetType::Model, "model/gltf-binary"),
        "mesh" | "rbxmesh" => (UploadAssetType::Mesh, "model/x-file-mesh-data"),
        "mp4" => (UploadAssetType::Video, "video/mp4"),
        "mov" => (UploadAssetType::Video, "video/mov"),
        _ => return None,
    };
    Some(UploadMedia {
        asset_type,
        content_type: content_type.to_string(),
    })
}

pub(super) fn content_type_for_asset_type(
    path: &std::path::Path,
    asset_type: UploadAssetType,
    explicit: bool,
) -> Result<&'static str, String> {
    let ext = upload_extension(path);
    match asset_type {
        UploadAssetType::Animation => match ext.as_str() {
            "rbxm" | "rbxmx" => Ok("model/x-rbxm"),
            _ => Err(format!(
                "unsupported file type for Animation; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Audio => match ext.as_str() {
            "mp3" => Ok("audio/mpeg"),
            "ogg" => Ok("audio/ogg"),
            "wav" => Ok("audio/wav"),
            "flac" => Ok("audio/flac"),
            _ => Err(format!(
                "unsupported file type for Audio; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Decal | UploadAssetType::Image => match ext.as_str() {
            "png" => Ok("image/png"),
            "jpg" | "jpeg" => Ok("image/jpeg"),
            "bmp" => Ok("image/bmp"),
            "tga" => Ok("image/tga"),
            _ => Err(format!(
                "unsupported file type for {}; expected {}",
                asset_type.as_cloud_str(),
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Mesh => match ext.as_str() {
            "mesh" | "rbxmesh" => Ok("model/x-file-mesh-data"),
            _ if explicit => Ok("model/x-file-mesh-data"),
            _ => Err(format!(
                "unsupported file type for Mesh; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Model => match ext.as_str() {
            "fbx" => Ok("model/fbx"),
            "gltf" => Ok("model/gltf+json"),
            "glb" => Ok("model/gltf-binary"),
            "rbxm" | "rbxmx" => Ok("model/x-rbxm"),
            _ => Err(format!(
                "unsupported file type for Model; expected {}",
                expected_extensions(asset_type)
            )),
        },
        UploadAssetType::Video => match ext.as_str() {
            "mp4" => Ok("video/mp4"),
            "mov" => Ok("video/mov"),
            _ => Err(format!(
                "unsupported file type for Video; expected {}",
                expected_extensions(asset_type)
            )),
        },
    }
}

pub(super) fn expected_extensions(asset_type: UploadAssetType) -> &'static str {
    match asset_type {
        UploadAssetType::Animation => ".rbxm or .rbxmx",
        UploadAssetType::Audio => ".mp3, .ogg, .wav, or .flac",
        UploadAssetType::Decal | UploadAssetType::Image => ".png, .jpg, .jpeg, .bmp, or .tga",
        UploadAssetType::Mesh => ".mesh or .rbxmesh, or pass --content-type model/x-file-mesh-data",
        UploadAssetType::Model => ".fbx, .gltf, .glb, .rbxm, or .rbxmx",
        UploadAssetType::Video => ".mp4 or .mov",
    }
}

pub(super) fn upload_extension(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub(super) fn write_upload_manifest(
    path: &std::path::Path,
    results: &[UploadBulkResult],
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(results)? + "\n")?;
    Ok(())
}

pub(super) fn print_upload_results(results: &[UploadBulkResult]) {
    for result in results {
        if result.ok {
            let uri = result
                .asset_uri
                .as_deref()
                .or(result.operation_path.as_deref())
                .unwrap_or("uploaded");
            let asset_type = result.asset_type.as_deref().unwrap_or("Asset");
            println!(
                "uploaded  {:40} {:9} {}",
                truncate_middle(&result.file, 40),
                asset_type,
                uri
            );
        } else {
            println!(
                "failed    {:40} {}",
                truncate_middle(&result.file, 40),
                result.error.as_deref().unwrap_or("unknown error")
            );
        }
    }
}

pub(super) fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let head_len = (max_chars - 3) / 2;
    let tail_len = max_chars - 3 - head_len;
    let head: String = value.chars().take(head_len).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}
