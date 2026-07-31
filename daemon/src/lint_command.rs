use super::*;

pub(super) async fn run_lint(args: LintArgs) -> Result<(), Box<dyn std::error::Error>> {
    let project = match args.project {
        Some(p) => p,
        None => std::env::current_dir().map_err(|e| format!("lint: current directory: {e}"))?,
    };
    let project = lifecycle::canonical_project(&project)
        .map_err(|e| format!("lint: validate project {}: {e}", project.display()))?;

    if args.scope_only && args.paths.is_empty() {
        return Err("lint: --scope-only requires at least one --path".into());
    }
    if extra_args_use_plain_formatter(&args.extra_args) {
        return Err(
            "lint: --formatter=plain does not preserve analyzer failure exit codes; use the default or GNU formatter"
                .into(),
        );
    }

    let explicit_targets = !args.paths.is_empty();
    let mut targets = if args.paths.is_empty() {
        vec![project.clone()]
    } else {
        args.paths
            .iter()
            .map(|path| lint_target_path(&project, path))
            .collect()
    };
    targets = targets
        .into_iter()
        .map(|target| validate_lint_target(&project, &target))
        .collect::<Result<Vec<_>, _>>()?;

    let compile_report = run_lint_compiler(
        &project,
        &targets,
        explicit_targets,
        args.compile,
        args.luau_compile.clone(),
        args.no_vendor_ignores,
        &args.ignores,
    )?;
    report_lint_compiler(&compile_report, args.raw, args.summary);

    let luau_lsp = resolve_luau_lsp(args.luau_lsp);
    warn_if_old_luau_lsp(&luau_lsp, &project);
    let user_sourcemap = extra_args_include_sourcemap(&args.extra_args);
    if (args.no_sourcemap || user_sourcemap)
        && matches!(
            args.data_model,
            LintDataModelMode::Studio | LintDataModelMode::Filesystem
        )
    {
        return Err(format!(
            "lint: --data-model {} requires Ro Sync's generated sourcemap; remove --no-sourcemap/custom --sourcemap",
            args.data_model.as_str()
        )
        .into());
    }

    let (sourcemap, mut coverage) = if args.no_sourcemap || user_sourcemap {
        (
            None,
            LintDataModelCoverage::external(args.data_model, user_sourcemap),
        )
    } else {
        let (map, coverage) = prepare_lint_sourcemap(&project, args.port, args.data_model).await?;
        (Some(write_temp_sourcemap_value(&map)?), coverage)
    };
    let definitions = if extra_args_include_roblox_definitions(&args.extra_args) {
        None
    } else {
        find_luau_definitions(&project)
            .map_err(|error| format!("lint: locate Roblox definitions: {error}"))?
    };
    let strict_settings = if coverage.strict
        && !extra_args_include_settings(&args.extra_args)
        && !extra_args_disable_strict_datamodel(&args.extra_args)
    {
        Some(write_temp_lint_settings()?)
    } else {
        if coverage.strict && extra_args_include_settings(&args.extra_args) {
            coverage.note = Some(
                "A caller-supplied --settings file controls strict DataModel diagnostics."
                    .to_string(),
            );
            coverage.strict = false;
        }
        if extra_args_disable_strict_datamodel(&args.extra_args) {
            coverage.note = Some(
                "Strict DataModel diagnostics were disabled by --no-strict-dm-types.".to_string(),
            );
            coverage.strict = false;
        }
        None
    };

    report_lint_coverage(&coverage, args.raw);
    let mut cmd = std::process::Command::new(&luau_lsp);
    cmd.arg("analyze");
    if !extra_args_include_platform(&args.extra_args) {
        cmd.arg("--platform=roblox");
    }
    if let Some(path) = &sourcemap {
        cmd.arg(format!("--sourcemap={}", path.display()));
    }
    if let Some(path) = &strict_settings {
        cmd.arg(format!("--settings={}", path.display()));
    }
    if let Some(path) = &definitions {
        cmd.arg(format!("--definitions=@roblox={}", path.display()));
    }

    // An explicit --path is an explicit ownership boundary and must never be
    // silently swallowed by the default vendor filters.
    if !args.no_vendor_ignores && !explicit_targets {
        for pattern in DEFAULT_LINT_VENDOR_IGNORES {
            cmd.arg(format!("--ignore={pattern}"));
        }
    }
    for pattern in &args.ignores {
        cmd.arg(format!("--ignore={pattern}"));
    }

    cmd.args(&args.extra_args)
        .args(&targets)
        .current_dir(&project)
        .stdin(Stdio::null());

    let capture_output = args.scope_only || args.summary || args.raw;
    let (status, effective_success) = if capture_output {
        let output = match cmd.output() {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                print_luau_lsp_missing(&luau_lsp);
                std::process::exit(127);
            }
            Err(e) => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                return Err(
                    format!("lint: failed to run {}: {e}", luau_lsp.to_string_lossy()).into(),
                );
            }
        };
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        let all_diagnostics = lint_diagnostics(&project, &combined);
        let rendered = if args.scope_only {
            filter_lint_output_to_targets(&project, &targets, &combined)
        } else {
            combined
        };
        let shown_diagnostics = lint_diagnostics(&project, &rendered);
        let suppressed = all_diagnostics
            .len()
            .saturating_sub(shown_diagnostics.len());
        let retained_unparsed_failure =
            args.scope_only && lint_has_unparsed_failure(&project, &rendered);
        let effective_success = lint_analyzer_effective_success(
            args.scope_only,
            output.status.success(),
            all_diagnostics.len(),
            shown_diagnostics.len(),
            retained_unparsed_failure,
        );
        if args.raw {
            print_lint_json(
                &project,
                &coverage,
                &compile_report,
                LintAnalyzerJson {
                    output: &rendered,
                    diagnostics: &shown_diagnostics,
                    suppressed,
                    exit_code: output.status.code(),
                    ok: effective_success && compile_report.is_success(),
                },
            )?;
        } else {
            print!("{rendered}");
            if args.summary {
                print_lint_summary(&project, &rendered, &compile_report, suppressed);
            }
        }
        (output.status, effective_success)
    } else {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let status = match cmd.status() {
            Ok(status) => status,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                print_luau_lsp_missing(&luau_lsp);
                std::process::exit(127);
            }
            Err(e) => {
                cleanup_temp_file(&sourcemap);
                cleanup_temp_file(&strict_settings);
                return Err(
                    format!("lint: failed to run {}: {e}", luau_lsp.to_string_lossy()).into(),
                );
            }
        };
        let success = status.success();
        (status, success)
    };

    cleanup_temp_file(&sourcemap);
    cleanup_temp_file(&strict_settings);
    if !effective_success || !compile_report.is_success() {
        let exit_code = if effective_success {
            compile_report.exit_code().unwrap_or(1)
        } else {
            status.code().filter(|code| *code != 0).unwrap_or(1)
        };
        std::process::exit(exit_code);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LintDataModelCoverage {
    requested: String,
    source: String,
    strict: bool,
    live_nodes: Option<usize>,
    note: Option<String>,
}

impl LintDataModelCoverage {
    fn external(mode: LintDataModelMode, user_sourcemap: bool) -> Self {
        Self {
            requested: mode.as_str().to_string(),
            source: if user_sourcemap {
                "caller-supplied".to_string()
            } else {
                "disabled".to_string()
            },
            strict: false,
            live_nodes: None,
            note: Some(if user_sourcemap {
                "Ro Sync cannot determine strict DataModel coverage for a caller-supplied sourcemap."
                    .to_string()
            } else {
                "DataModel sourcemap generation was disabled.".to_string()
            }),
        }
    }
}

pub(super) const LINT_COMPILE_OPTIMIZATIONS: &[u8] = &[0, 1, 2];
pub(super) const LINT_COMPILE_BATCH_MAX_FILES: usize = 128;
pub(super) const LINT_COMPILE_BATCH_MAX_ARG_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LintCompileReport {
    pub(super) requested: String,
    pub(super) status: String,
    pub(super) executable: Option<String>,
    pub(super) source_files: usize,
    pub(super) optimizations_checked: Vec<u8>,
    pub(super) failures: Vec<LintCompileFailure>,
    pub(super) note: Option<String>,
}

impl LintCompileReport {
    fn disabled(mode: LintCompileMode) -> Self {
        Self {
            requested: mode.as_str().to_string(),
            status: "disabled".to_string(),
            executable: None,
            source_files: 0,
            optimizations_checked: Vec::new(),
            failures: Vec::new(),
            note: None,
        }
    }

    fn skipped(mode: LintCompileMode, executable: Option<&OsString>, note: String) -> Self {
        Self {
            requested: mode.as_str().to_string(),
            status: "skipped".to_string(),
            executable: executable.map(|value| value.to_string_lossy().into_owned()),
            source_files: 0,
            optimizations_checked: Vec::new(),
            failures: Vec::new(),
            note: Some(note),
        }
    }

    fn is_success(&self) -> bool {
        self.status != "failed"
    }

    fn exit_code(&self) -> Option<i32> {
        self.failures.iter().find_map(|failure| failure.exit_code)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LintCompileFailure {
    pub(super) optimization: u8,
    pub(super) batch: usize,
    pub(super) exit_code: Option<i32>,
    pub(super) output: String,
}

pub(super) fn run_lint_compiler(
    project: &std::path::Path,
    targets: &[PathBuf],
    explicit_targets: bool,
    mode: LintCompileMode,
    explicit_executable: Option<PathBuf>,
    no_vendor_ignores: bool,
    ignores: &[String],
) -> Result<LintCompileReport, Box<dyn std::error::Error>> {
    if mode == LintCompileMode::Off {
        return Ok(LintCompileReport::disabled(mode));
    }

    let executable = resolve_luau_compile(explicit_executable);
    let Some(executable) = executable else {
        let note = "luau-compile was not found; install the Luau compiler, set ROSYNC_LUAU_COMPILE, or pass --luau-compile"
            .to_string();
        if mode == LintCompileMode::Required {
            return Err(format!("lint: {note}").into());
        }
        return Ok(LintCompileReport::skipped(mode, None, note));
    };

    match std::process::Command::new(&executable)
        .arg("--help")
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            return Err(format!(
                "lint: {} did not accept --help (exit {}); pass a valid luau-compile executable",
                executable.to_string_lossy(),
                status.code().unwrap_or(1)
            )
            .into());
        }
        Err(error) => {
            let note = format!(
                "could not run luau-compile at {}: {error}",
                executable.to_string_lossy()
            );
            if mode == LintCompileMode::Required {
                return Err(format!("lint: {note}").into());
            }
            return Ok(LintCompileReport::skipped(mode, Some(&executable), note));
        }
    }

    let use_default_ignores = !no_vendor_ignores && !explicit_targets;
    let sources = collect_lint_compile_sources(project, targets, use_default_ignores, ignores)?;
    let mut report = LintCompileReport {
        requested: mode.as_str().to_string(),
        status: "passed".to_string(),
        executable: Some(executable.to_string_lossy().into_owned()),
        source_files: sources.len(),
        optimizations_checked: Vec::new(),
        failures: Vec::new(),
        note: None,
    };
    if sources.is_empty() {
        report.note = Some("No executable .lua or .luau source files were in scope.".to_string());
        return Ok(report);
    }

    let batches = lint_compile_batches(project, &sources);
    for &optimization in LINT_COMPILE_OPTIMIZATIONS {
        report.optimizations_checked.push(optimization);
        for (batch_index, batch) in batches.iter().enumerate() {
            let output = std::process::Command::new(&executable)
                .arg("--null")
                .arg(format!("-O{optimization}"))
                .args(batch)
                .current_dir(project)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| {
                    format!(
                        "lint: failed to run {} during bytecode compilation: {error}",
                        executable.to_string_lossy()
                    )
                })?;
            if output.status.success() {
                continue;
            }
            report.status = "failed".to_string();
            report.failures.push(LintCompileFailure {
                optimization,
                batch: batch_index + 1,
                exit_code: output.status.code(),
                output: lint_compile_failure_output(&output),
            });
        }
    }
    Ok(report)
}

pub(super) fn report_lint_compiler(report: &LintCompileReport, raw: bool, summary: bool) {
    if raw {
        return;
    }
    match report.status.as_str() {
        "skipped" => {
            if let Some(note) = &report.note {
                eprintln!("[rosync lint] bytecode check skipped: {note}");
            }
        }
        "failed" => {
            for failure in &report.failures {
                eprintln!(
                    "[rosync lint] bytecode compilation failed at -O{} (batch {}):",
                    failure.optimization, failure.batch
                );
                eprint!("{}", failure.output);
                if !failure.output.ends_with('\n') {
                    eprintln!();
                }
            }
        }
        "passed" if summary => {
            let modes = report
                .optimizations_checked
                .iter()
                .map(|optimization| format!("O{optimization}"))
                .collect::<Vec<_>>()
                .join("/");
            if modes.is_empty() {
                eprintln!(
                    "[rosync lint] bytecode: {}",
                    report.note.as_deref().unwrap_or("nothing to compile")
                );
            } else {
                eprintln!(
                    "[rosync lint] bytecode: {} source file{} passed {modes}",
                    report.source_files,
                    plural_s(report.source_files)
                );
            }
        }
        _ => {}
    }
}

pub(super) fn collect_lint_compile_sources(
    project: &std::path::Path,
    targets: &[PathBuf],
    use_default_ignores: bool,
    ignores: &[String],
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut sources = Vec::new();
    for target in targets {
        let metadata = crate::fs_safety::require_metadata_no_follow(target)
            .map_err(|error| format!("lint: inspect {}: {error}", target.display()))?;
        if metadata.is_file() {
            if is_lint_compile_source(project, target, true)
                && !lint_compile_path_ignored(project, target, use_default_ignores, ignores)
            {
                sources.push(validate_lint_target(project, target)?);
            }
            continue;
        }
        if !metadata.is_dir() {
            return Err(format!(
                "lint: target is not a regular file or directory: {}",
                target.display()
            )
            .into());
        }
        collect_lint_compile_directory(
            project,
            target,
            use_default_ignores,
            ignores,
            &mut sources,
        )?;
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

pub(super) fn collect_lint_compile_directory(
    project: &std::path::Path,
    directory: &std::path::Path,
    use_default_ignores: bool,
    ignores: &[String],
    sources: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut pending = vec![directory.to_path_buf()];
    let mut visited = 0usize;
    while let Some(current) = pending.pop() {
        let relative = current.strip_prefix(project).unwrap_or(&current);
        let first = relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str());
        let index = if current == project {
            crate::fs_safety::PortableDirectoryIndex::read_project_root(&current)
        } else if first.is_some_and(|service| crate::fs_safety::SYNCED_SERVICES.contains(&service))
        {
            crate::fs_safety::PortableDirectoryIndex::read(&current)
        } else {
            crate::fs_safety::PortableDirectoryIndex::read_raw(&current)
        }
        .map_err(|error| format!("lint: scan {}: {error}", current.display()))?;
        visited = visited.saturating_add(index.entries().len());
        if visited > crate::fs_safety::MAX_SERVICE_TREE_NODES {
            return Err(format!(
                "lint: source scan exceeds the {} node safety limit",
                crate::fs_safety::MAX_SERVICE_TREE_NODES
            )
            .into());
        }

        for entry in index.entries().iter().rev() {
            let path = &entry.path;
            if lint_compile_path_ignored(project, path, use_default_ignores, ignores) {
                continue;
            }
            match entry.kind {
                crate::fs_safety::SafeEntryKind::Directory => pending.push(path.clone()),
                crate::fs_safety::SafeEntryKind::File
                    if is_lint_compile_source(project, path, false) =>
                {
                    sources.push(validate_lint_target(project, path)?);
                }
                crate::fs_safety::SafeEntryKind::File => {}
            }
        }
    }
    Ok(())
}

pub(super) fn is_lint_compile_source(
    project: &std::path::Path,
    path: &std::path::Path,
    explicit_file: bool,
) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if fs_map::classify_script_file(name).is_none() {
        return false;
    }
    if name.ends_with(".d.luau") || name.ends_with(".d.lua") {
        // Declaration files outside the mirrored DataModel are analyzer inputs,
        // not executable chunks. Inside a synced service, however, `Foo.d.luau`
        // is a perfectly valid ModuleScript named `Foo.d`; an explicit file is
        // likewise an unambiguous request to run the compiler.
        if explicit_file {
            return true;
        }
        let relative = path.strip_prefix(project).unwrap_or(path);
        return relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|service| snapshot::SYNCED_SERVICES.contains(&service));
    }
    true
}

pub(super) fn lint_compile_path_ignored(
    project: &std::path::Path,
    path: &std::path::Path,
    use_default_ignores: bool,
    ignores: &[String],
) -> bool {
    let relative = path.strip_prefix(project).unwrap_or(path);
    let relative = relative.to_string_lossy().replace('\\', "/");
    let absolute = path.to_string_lossy().replace('\\', "/");
    let matches = |pattern: &str| {
        lint_glob_matches(pattern, &relative)
            || lint_glob_matches(pattern, &absolute)
            || (!pattern.contains('/')
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| lint_glob_matches(pattern, name)))
    };
    (use_default_ignores
        && DEFAULT_LINT_VENDOR_IGNORES
            .iter()
            .any(|pattern| matches(pattern)))
        || ignores.iter().any(|pattern| matches(pattern))
}

pub(super) fn lint_glob_matches(pattern: &str, value: &str) -> bool {
    fn recurse(
        pattern: &[u8],
        value: &[u8],
        pattern_index: usize,
        value_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, value_index)) {
            return *result;
        }
        let result = if pattern_index == pattern.len() {
            value_index == value.len()
        } else if pattern[pattern_index..].starts_with(b"**/") {
            recurse(pattern, value, pattern_index + 3, value_index, memo)
                || (value_index < value.len()
                    && recurse(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index..].starts_with(b"**") {
            recurse(pattern, value, pattern_index + 2, value_index, memo)
                || (value_index < value.len()
                    && recurse(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index] == b'*' {
            recurse(pattern, value, pattern_index + 1, value_index, memo)
                || (value_index < value.len()
                    && value[value_index] != b'/'
                    && recurse(pattern, value, pattern_index, value_index + 1, memo))
        } else if pattern[pattern_index] == b'?' {
            value_index < value.len()
                && value[value_index] != b'/'
                && recurse(pattern, value, pattern_index + 1, value_index + 1, memo)
        } else {
            value_index < value.len()
                && pattern[pattern_index] == value[value_index]
                && recurse(pattern, value, pattern_index + 1, value_index + 1, memo)
        };
        memo.insert((pattern_index, value_index), result);
        result
    }

    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    recurse(
        pattern.as_bytes(),
        value.as_bytes(),
        0,
        0,
        &mut HashMap::new(),
    )
}

pub(super) fn lint_compile_batches(
    project: &std::path::Path,
    sources: &[PathBuf],
) -> Vec<Vec<OsString>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut argument_bytes = 0usize;
    for source in sources {
        let argument_path = source.strip_prefix(project).unwrap_or(source);
        let argument = argument_path.as_os_str().to_os_string();
        let bytes = argument.to_string_lossy().len() + 1;
        if !batch.is_empty()
            && (batch.len() >= LINT_COMPILE_BATCH_MAX_FILES
                || argument_bytes.saturating_add(bytes) > LINT_COMPILE_BATCH_MAX_ARG_BYTES)
        {
            batches.push(std::mem::take(&mut batch));
            argument_bytes = 0;
        }
        argument_bytes = argument_bytes.saturating_add(bytes);
        batch.push(argument);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

pub(super) fn lint_compile_failure_output(output: &std::process::Output) -> String {
    let mut rendered = String::new();
    rendered.push_str(&String::from_utf8_lossy(&output.stderr));
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.starts_with("Compiled ") {
            rendered.push_str(line);
            rendered.push('\n');
        }
    }
    if rendered.trim().is_empty() {
        rendered = format!(
            "luau-compile exited with status {} without an error message\n",
            output.status.code().unwrap_or(1)
        );
    }
    rendered
}

pub(super) async fn prepare_lint_sourcemap(
    project: &std::path::Path,
    port: u16,
    mode: LintDataModelMode,
) -> Result<(serde_json::Value, LintDataModelCoverage), Box<dyn std::error::Error>> {
    let mut map = sourcemap::generate(project)?;
    let mut coverage = LintDataModelCoverage {
        requested: mode.as_str().to_string(),
        source: "filesystem".to_string(),
        strict: mode == LintDataModelMode::Filesystem,
        live_nodes: None,
        note: None,
    };

    match mode {
        LintDataModelMode::Loose => {
            coverage.note = Some(
                "DataModel-derived expressions remain gradual/any in diagnostics.".to_string(),
            );
            return Ok((map, coverage));
        }
        LintDataModelMode::Filesystem => {
            coverage.note = Some(
                "Strict filesystem types can report unknown children for Studio-only instances."
                    .to_string(),
            );
            return Ok((map, coverage));
        }
        LintDataModelMode::Auto | LintDataModelMode::Studio => {}
    }

    let hello = fetch_daemon_hello(port).ok();
    let matching_daemon = hello
        .as_ref()
        .is_some_and(|hello| daemon_hello_matches_project(hello, project));
    let plugin_connected = hello
        .as_ref()
        .and_then(|hello| hello.get("pluginConnected"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if matching_daemon && plugin_connected {
        match live_tree(port, "lint").await {
            Ok(tree) => {
                if diff::has_truncated_tree(&tree) {
                    return Err("lint: Studio returned a truncated DataModel tree".into());
                }
                let live_nodes = count_json_tree_nodes(&tree);
                sourcemap::merge_live_tree(&mut map, &tree);
                coverage.source = "studio".to_string();
                coverage.strict = true;
                coverage.live_nodes = Some(live_nodes);
                coverage.note = Some(
                    "Strict DataModel diagnostics use live Studio classes plus disk file mappings."
                        .to_string(),
                );
                return Ok((map, coverage));
            }
            Err(error) if mode == LintDataModelMode::Studio => {
                return Err(format!("lint: live Studio DataModel request failed: {error}").into());
            }
            Err(error) => {
                coverage.note = Some(format!(
                    "Live Studio DataModel request failed ({error}); using relaxed filesystem types."
                ));
                return Ok((map, coverage));
            }
        }
    }

    if mode == LintDataModelMode::Studio {
        let reason = if !matching_daemon {
            format!("no matching Ro Sync daemon is reachable on port {port}")
        } else {
            "the Studio plugin is not connected".to_string()
        };
        return Err(format!("lint: --data-model studio requires live Studio: {reason}").into());
    }

    coverage.note = Some(if !matching_daemon {
        "Studio is unavailable; using relaxed filesystem types. Use --data-model filesystem for an offline strict audit."
            .to_string()
    } else {
        "Studio plugin is disconnected; using relaxed filesystem types. Use --data-model filesystem for an offline strict audit."
            .to_string()
    });
    Ok((map, coverage))
}

pub(super) fn count_json_tree_nodes(node: &serde_json::Value) -> usize {
    1 + node
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map(|children| children.iter().map(count_json_tree_nodes).sum::<usize>())
        .unwrap_or(0)
}

pub(super) fn report_lint_coverage(coverage: &LintDataModelCoverage, raw: bool) {
    if raw {
        return;
    }
    let node_detail = coverage
        .live_nodes
        .map(|count| format!(", {count} live instances"))
        .unwrap_or_default();
    let strict = if coverage.strict { "strict" } else { "relaxed" };
    eprintln!(
        "[rosync lint] DataModel: {} ({strict}{node_detail})",
        coverage.source
    );
    if let Some(note) = &coverage.note {
        eprintln!("[rosync lint] {note}");
    }
}

pub(super) const DEFAULT_LINT_VENDOR_IGNORES: &[&str] = &[
    "**/Packages/**",
    "**/_Index/**",
    "**/Madwork*/**",
    "**/PlayerModule/**",
    "**/node_modules/**",
    "**/.git/**",
    "**/.codex/**",
    "**/.vscode/**",
    "**/.rosync-artifacts/**",
    "**/.rosync-backups/**",
    "**/.rosync-workflows/**",
    "**/tools/**",
];

pub(super) fn lint_target_path(project: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project.join(path)
    }
}

pub(super) fn validate_lint_target(
    project: &std::path::Path,
    target: &std::path::Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if target == project {
        return Ok(project.to_path_buf());
    }

    let validated = if let Ok(relative) = target.strip_prefix(project) {
        let synced = relative
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|service| crate::fs_safety::SYNCED_SERVICES.contains(&service));
        if synced {
            crate::fs_safety::validate_synced_path(project, target, false)
        } else {
            crate::fs_safety::validate_descendant_no_follow(project, relative, false)
        }
        .map_err(|error| format!("lint: validate target {}: {error}", target.display()))?
    } else {
        let metadata = crate::fs_safety::require_metadata_no_follow(target)
            .map_err(|error| format!("lint: inspect target {}: {error}", target.display()))?;
        if metadata.is_dir() {
            crate::fs_safety::stable_canonical_directory(target).map_err(|error| {
                format!(
                    "lint: validate target directory {}: {error}",
                    target.display()
                )
            })?
        } else if metadata.is_file() {
            target.to_path_buf()
        } else {
            return Err(format!(
                "lint: target is not a regular file or directory: {}",
                target.display()
            )
            .into());
        }
    };

    let metadata = crate::fs_safety::require_metadata_no_follow(&validated)
        .map_err(|error| format!("lint: inspect target {}: {error}", target.display()))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(format!(
            "lint: target is not a regular file or directory: {}",
            target.display()
        )
        .into());
    }
    Ok(validated)
}

#[derive(Debug, Clone)]
pub(super) struct LintDiagnostic {
    pub(super) path: PathBuf,
    pub(super) category: String,
    pub(super) message: String,
    pub(super) line: usize,
    pub(super) column: usize,
    pub(super) end_line: Option<usize>,
    pub(super) end_column: Option<usize>,
}

pub(super) fn filter_lint_output_to_targets(
    project: &std::path::Path,
    targets: &[PathBuf],
    output: &str,
) -> String {
    let scopes: Vec<PathBuf> = targets.to_vec();
    let mut filtered = String::new();
    for line in output.lines() {
        match parse_lint_diagnostic(project, line) {
            Some(diag) if lint_path_in_scopes(&diag.path, &scopes) => {
                filtered.push_str(line);
                filtered.push('\n');
            }
            Some(_) => {}
            None => {
                filtered.push_str(line);
                filtered.push('\n');
            }
        }
    }
    filtered
}

pub(super) fn lint_path_in_scopes(path: &std::path::Path, scopes: &[PathBuf]) -> bool {
    scopes.iter().any(|scope| {
        if scope.is_dir() {
            path.starts_with(scope)
        } else {
            path == scope
        }
    })
}

#[cfg(test)]
pub(super) fn normalize_existing_path(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn parse_lint_diagnostic(
    project: &std::path::Path,
    line: &str,
) -> Option<LintDiagnostic> {
    let (file_part, coordinates, message) = split_lint_diagnostic_line(line)?;
    let (category, diagnostic_message) = split_lint_diagnostic_message(message)?;
    // With a sourcemap, luau-lsp appends its virtual DataModel location to the
    // real filename: `Main.luau [game/ReplicatedStorage/Main]`. Keep the disk
    // path for ownership filtering and structured output.
    let file_label = strip_lint_virtual_path_suffix(file_part);
    let file_path = std::path::Path::new(file_label);
    let absolute = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        project.join(file_path)
    };
    Some(LintDiagnostic {
        path: validate_lint_target(project, &absolute).unwrap_or(absolute),
        category: category.trim().to_string(),
        message: diagnostic_message.trim().to_string(),
        line: coordinates[0],
        column: coordinates[1],
        end_line: coordinates.get(2).copied(),
        end_column: coordinates.get(3).copied(),
    })
}

pub(super) fn split_lint_diagnostic_line(line: &str) -> Option<(&str, Vec<usize>, &str)> {
    // Search for a numeric `(line,column[,endLine,endColumn]): ` suffix rather
    // than splitting at the first `(`. Ro Sync's script-with-children files
    // intentionally contain parentheses, e.g. `init (MarketService).luau`.
    for (location_end, _) in line.rmatch_indices("): ") {
        let prefix = &line[..location_end];
        let Some(location_start) = prefix.rfind('(') else {
            continue;
        };
        let Ok(coordinates) = prefix[location_start + 1..]
            .split(',')
            .map(str::parse::<usize>)
            .collect::<Result<Vec<_>, _>>()
        else {
            continue;
        };
        let file_part = &prefix[..location_start];
        let message = &line[location_end + 3..];
        if (coordinates.len() == 2 || coordinates.len() == 4)
            && lint_file_part_is_plausible(file_part)
            && split_lint_diagnostic_message(message).is_some()
        {
            return Some((file_part, coordinates, message));
        }
    }

    // `--formatter=gnu` uses `path:line.column-endLine.endColumn: ...`, while
    // `--formatter=plain` uses `path:line:column-endColumn: (Wn) ...`.
    for (location_end, _) in line.rmatch_indices(": ") {
        let prefix = &line[..location_end];
        let message = &line[location_end + 2..];
        if split_lint_diagnostic_message(message).is_none() {
            continue;
        }
        if let Some((file_part, coordinates)) = split_gnu_lint_location(prefix) {
            if lint_file_part_is_plausible(file_part) {
                return Some((file_part, coordinates, message));
            }
        }
        if let Some((file_part, coordinates)) = split_plain_lint_location(prefix) {
            if lint_file_part_is_plausible(file_part) {
                return Some((file_part, coordinates, message));
            }
        }
    }
    None
}

pub(super) fn lint_file_part_is_plausible(file_part: &str) -> bool {
    for marker in [" [game/", " [game]"] {
        if let Some(index) = file_part.rfind(marker) {
            let disk_label = &file_part[..index];
            if disk_label.ends_with(".lua") || disk_label.ends_with(".luau") {
                if !file_part.ends_with(']') {
                    return false;
                }
                break;
            }
        }
    }
    let disk_label = strip_lint_virtual_path_suffix(file_part);
    disk_label.ends_with(".lua") || disk_label.ends_with(".luau")
}

pub(super) fn split_lint_diagnostic_message(message: &str) -> Option<(&str, &str)> {
    let mut message = message.trim();
    if let Some(after_open) = message.strip_prefix('(') {
        if let Some((severity, rest)) = after_open.split_once(") ") {
            let has_digit = severity.bytes().any(|byte| byte.is_ascii_digit());
            if has_digit
                && severity
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                message = rest;
            }
        }
    }
    let (category, diagnostic_message) = message.split_once(':')?;
    let category = category.trim();
    if category.is_empty()
        || !category
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some((category, diagnostic_message.trim()))
}

pub(super) fn split_gnu_lint_location(prefix: &str) -> Option<(&str, Vec<usize>)> {
    let location_start = prefix.rfind(':')?;
    let coordinates = parse_gnu_lint_location(&prefix[location_start + 1..])?;
    Some((&prefix[..location_start], coordinates))
}

pub(super) fn split_plain_lint_location(prefix: &str) -> Option<(&str, Vec<usize>)> {
    let (file_and_line, column_range) = prefix.rsplit_once(':')?;
    let (file_part, line) = file_and_line.rsplit_once(':')?;
    let line = line.parse::<usize>().ok()?;
    let (column, end_column) = match column_range.split_once('-') {
        Some((column, end_column)) => (
            column.parse::<usize>().ok()?,
            Some(end_column.parse::<usize>().ok()?),
        ),
        None => (column_range.parse::<usize>().ok()?, None),
    };
    let coordinates = match end_column {
        Some(end_column) => vec![line, column, line, end_column],
        None => vec![line, column],
    };
    Some((file_part, coordinates))
}

pub(super) fn parse_gnu_lint_location(location: &str) -> Option<Vec<usize>> {
    fn point(value: &str) -> Option<(usize, usize)> {
        let (line, column) = value.split_once('.')?;
        Some((line.parse().ok()?, column.parse().ok()?))
    }

    if let Some((start, end)) = location.split_once('-') {
        let (line, column) = point(start)?;
        let (end_line, end_column) = point(end)?;
        Some(vec![line, column, end_line, end_column])
    } else {
        let (line, column) = point(location)?;
        Some(vec![line, column])
    }
}

pub(super) fn strip_lint_virtual_path_suffix(label: &str) -> &str {
    if !label.ends_with(']') {
        return label;
    }
    for marker in [" [game/", " [game]"] {
        if let Some(index) = label.rfind(marker) {
            return &label[..index];
        }
    }
    label
}

pub(super) fn lint_diagnostics(project: &std::path::Path, output: &str) -> Vec<LintDiagnostic> {
    output
        .lines()
        .filter_map(|line| parse_lint_diagnostic(project, line))
        .collect()
}

pub(super) fn lint_summary_counts(
    project: &std::path::Path,
    analyzer_output: &str,
    compiler: &LintCompileReport,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let project = lifecycle::canonical_project(project).unwrap_or_else(|_| project.to_path_buf());
    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    let analyzer_diagnostics = lint_diagnostics(&project, analyzer_output);
    let compiler_diagnostics = compiler
        .failures
        .iter()
        .flat_map(|failure| lint_diagnostics(&project, &failure.output))
        .collect::<Vec<_>>();
    for diag in analyzer_diagnostics.into_iter().chain(compiler_diagnostics) {
        *by_category.entry(diag.category).or_insert(0) += 1;
        let file = diag
            .path
            .strip_prefix(&project)
            .unwrap_or(&diag.path)
            .to_string_lossy()
            .replace('\\', "/");
        *by_file.entry(file).or_insert(0) += 1;
    }
    (by_category, by_file)
}

pub(super) fn print_lint_summary(
    project: &std::path::Path,
    analyzer_output: &str,
    compiler: &LintCompileReport,
    suppressed: usize,
) {
    let (by_category, by_file) = lint_summary_counts(project, analyzer_output, compiler);
    let total: usize = by_category.values().sum();
    if total == 0 {
        println!("\nSummary: 0 diagnostics");
        if suppressed > 0 {
            println!("Suppressed outside requested scopes: {suppressed}");
        }
        return;
    }
    println!("\nSummary: {total} diagnostic{}", plural_s(total));
    println!("By category:");
    for (category, count) in by_category {
        println!("  {count:>4} {category}");
    }
    println!("By file:");
    for (file, count) in by_file {
        println!("  {count:>4} {file}");
    }
    if suppressed > 0 {
        println!("Suppressed outside requested scopes: {suppressed}");
    }
}

pub(super) struct LintAnalyzerJson<'a> {
    output: &'a str,
    diagnostics: &'a [LintDiagnostic],
    suppressed: usize,
    exit_code: Option<i32>,
    ok: bool,
}

pub(super) fn print_lint_json(
    project: &std::path::Path,
    coverage: &LintDataModelCoverage,
    compiler: &LintCompileReport,
    analyzer: LintAnalyzerJson<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let analyzer_messages = lint_unparsed_lines(project, analyzer.output);
    let analyzer_diagnostic_count = analyzer.diagnostics.len();
    let mut diagnostics = analyzer
        .diagnostics
        .iter()
        .map(|diagnostic| lint_diagnostic_json(project, diagnostic, "analyzer", None))
        .collect::<Vec<_>>();
    let mut compiler_diagnostic_count = 0usize;
    for failure in &compiler.failures {
        for diagnostic in lint_diagnostics(project, &failure.output) {
            compiler_diagnostic_count += 1;
            diagnostics.push(lint_diagnostic_json(
                project,
                &diagnostic,
                "compiler",
                Some(failure.optimization),
            ));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": analyzer.ok,
            "project": project,
            "analyzerExitCode": analyzer.exit_code,
            "dataModel": coverage,
            "compiler": compiler,
            "analyzerDiagnosticCount": analyzer_diagnostic_count,
            "analyzerMessages": analyzer_messages,
            "compilerDiagnosticCount": compiler_diagnostic_count,
            "diagnosticCount": diagnostics.len(),
            "suppressedDiagnostics": analyzer.suppressed,
            "diagnostics": diagnostics,
        }))?
    );
    Ok(())
}

pub(super) fn lint_unparsed_lines(project: &std::path::Path, output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty() && parse_lint_diagnostic(project, line).is_none())
        .map(str::to_string)
        .collect()
}

pub(super) fn lint_has_unparsed_failure(project: &std::path::Path, output: &str) -> bool {
    lint_unparsed_lines(project, output)
        .iter()
        .any(|line| !lint_unparsed_line_is_benign(line))
}

pub(super) fn lint_analyzer_effective_success(
    scope_only: bool,
    process_success: bool,
    all_diagnostics: usize,
    shown_diagnostics: usize,
    retained_unparsed_failure: bool,
) -> bool {
    if process_success {
        return true;
    }
    scope_only && all_diagnostics > 0 && shown_diagnostics == 0 && !retained_unparsed_failure
}

pub(super) fn lint_unparsed_line_is_benign(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("[INFO]")
        || line.starts_with("[WARN] client does not allow didChangeWatchedFiles registration")
}

pub(super) fn lint_diagnostic_json(
    project: &std::path::Path,
    diagnostic: &LintDiagnostic,
    stage: &str,
    optimization: Option<u8>,
) -> serde_json::Value {
    let path = diagnostic
        .path
        .strip_prefix(project)
        .unwrap_or(&diagnostic.path)
        .to_string_lossy()
        .replace('\\', "/");
    serde_json::json!({
        "stage": stage,
        "optimization": optimization,
        "path": path,
        "line": diagnostic.line,
        "column": diagnostic.column,
        "endLine": diagnostic.end_line,
        "endColumn": diagnostic.end_column,
        "category": diagnostic.category,
        "message": diagnostic.message,
    })
}

pub(super) fn plural_s(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

pub(super) fn write_temp_sourcemap_value(
    map: &serde_json::Value,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rosync-sourcemap-{}-{}.json",
        std::process::id(),
        unix_nanos()
    ));
    let text = serde_json::to_string_pretty(&map)?;
    std::fs::write(&path, text).map_err(|e| format!("lint: write {}: {e}", path.display()))?;
    Ok(path)
}

pub(super) fn write_temp_lint_settings() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rosync-lint-settings-{}-{}.json",
        std::process::id(),
        unix_nanos()
    ));
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "luau-lsp.diagnostics.strictDatamodelTypes": true,
        "luau-lsp.platform.type": "roblox",
    }))?;
    std::fs::write(&path, text).map_err(|e| format!("lint: write {}: {e}", path.display()))?;
    Ok(path)
}

pub(super) fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(super) fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn extra_args_include_sourcemap(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--sourcemap" || arg.starts_with("--sourcemap="))
}

pub(super) fn extra_args_include_platform(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--platform" || arg.starts_with("--platform="))
}

pub(super) fn extra_args_use_plain_formatter(args: &[String]) -> bool {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--formatter" {
            if args.get(index + 1).is_some_and(|value| value == "plain") {
                return true;
            }
            continue;
        }
        for prefix in ["--formatter=", "--formatter:"] {
            if arg.strip_prefix(prefix) == Some("plain") {
                return true;
            }
        }
    }
    false
}

pub(super) fn extra_args_include_settings(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--settings" || arg.starts_with("--settings="))
}

pub(super) fn extra_args_disable_strict_datamodel(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--no-strict-dm-types")
}

pub(super) fn extra_args_include_roblox_definitions(args: &[String]) -> bool {
    for (index, arg) in args.iter().enumerate() {
        if arg == "--definitions" || arg == "--defs" {
            if args
                .get(index + 1)
                .is_some_and(|value| definition_value_replaces_roblox(value))
            {
                return true;
            }
            continue;
        }
        for prefix in ["--definitions=", "--defs=", "--definitions:", "--defs:"] {
            if let Some(value) = arg.strip_prefix(prefix) {
                if definition_value_replaces_roblox(value) {
                    return true;
                }
            }
        }
    }
    false
}

pub(super) fn definition_value_replaces_roblox(value: &str) -> bool {
    !value.starts_with('@') || value.starts_with("@roblox=")
}

pub(super) fn cleanup_temp_file(path: &Option<PathBuf>) {
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn resolve_luau_lsp(explicit: Option<PathBuf>) -> OsString {
    if let Some(path) = explicit {
        return path.into_os_string();
    }
    if let Ok(path) = std::env::var("ROSYNC_LUAU_LSP") {
        if !path.trim().is_empty() {
            return OsString::from(path);
        }
    }
    if let Some(path) = find_bundled_luau_lsp() {
        return path.into_os_string();
    }
    if let Some(path) = find_aftman_luau_lsp() {
        return path.into_os_string();
    }
    OsString::from("luau-lsp")
}

pub(super) fn resolve_luau_compile(explicit: Option<PathBuf>) -> Option<OsString> {
    if let Some(path) = explicit {
        return Some(path.into_os_string());
    }
    for variable in ["ROSYNC_LUAU_COMPILE", "LUAU_COMPILE"] {
        if let Some(path) = std::env::var_os(variable) {
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    if let Some(path) = find_bundled_luau_compile() {
        return Some(path.into_os_string());
    }
    if let Some(path) = find_aftman_luau_compile() {
        return Some(path.into_os_string());
    }
    find_executable_on_path(if cfg!(windows) {
        "luau-compile.exe"
    } else {
        "luau-compile"
    })
    .map(PathBuf::into_os_string)
}

pub(super) fn find_bundled_luau_compile() -> Option<PathBuf> {
    find_in_tool_bases(&bundled_luau_compile_relative_path())
}

pub(super) fn bundled_luau_compile_relative_path() -> PathBuf {
    PathBuf::from("tools")
        .join("luau")
        .join(platform_tool_triple())
        .join(if cfg!(windows) {
            "luau-compile.exe"
        } else {
            "luau-compile"
        })
}

pub(super) fn find_bundled_luau_lsp() -> Option<PathBuf> {
    let rel = PathBuf::from("tools")
        .join("luau-lsp")
        .join(platform_tool_triple())
        .join(if cfg!(windows) {
            "luau-lsp.exe"
        } else {
            "luau-lsp"
        });
    find_in_tool_bases(&rel)
}

pub(super) fn find_aftman_luau_lsp() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "luau-lsp.exe"
    } else {
        "luau-lsp"
    };
    let path = dirs::home_dir()?
        .join(".aftman")
        .join("bin")
        .join(executable);
    path.is_file().then_some(path)
}

pub(super) fn find_aftman_luau_compile() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "luau-compile.exe"
    } else {
        "luau-compile"
    };
    let path = dirs::home_dir()?
        .join(".aftman")
        .join("bin")
        .join(executable);
    path.is_file().then_some(path)
}

pub(super) fn find_executable_on_path(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(executable);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        if std::path::Path::new(executable).extension().is_none() {
            let extensions = std::env::var_os("PATHEXT")
                .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
            for extension in extensions.to_string_lossy().split(';') {
                let candidate = directory.join(format!("{executable}{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

pub(super) fn find_luau_definitions(project: &std::path::Path) -> Result<Option<PathBuf>, String> {
    // The widget snapshot is paired with the analyzer version Ro Sync tests.
    // A project copy exists for editor tooling and as a standalone fallback,
    // but it can be stale until the next `rosync refresh`.
    if let Some(definitions) = find_bundled_luau_definitions() {
        let metadata = crate::fs_safety::require_metadata_no_follow(&definitions)
            .map_err(|error| format!("inspect bundled definitions: {error}"))?;
        if !metadata.is_file() {
            return Err(format!(
                "bundled definitions are not a regular file: {}",
                definitions.display()
            ));
        }
        return Ok(Some(definitions));
    }
    let project_definitions = project.join(snapshot::ROBLOX_DEFINITIONS_PATH);
    if snapshot::project_tool_file_exists(project, &project_definitions)
        .map_err(|error| format!("inspect project definitions: {error}"))?
    {
        return Ok(Some(project_definitions));
    }
    Ok(None)
}

pub(super) fn find_bundled_luau_definitions() -> Option<PathBuf> {
    let rel = PathBuf::from("tools")
        .join("luau-lsp")
        .join("roblox")
        .join("globalTypes.d.luau");
    find_in_tool_bases(&rel)
}

pub(super) fn warn_if_old_luau_lsp(executable: &OsString, project: &std::path::Path) {
    let Ok(output) = std::process::Command::new(executable)
        .arg("--version")
        .current_dir(project)
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let Some(parsed) = parse_semver_triplet(&version) else {
        return;
    };
    if parsed < RECOMMENDED_LUAU_LSP_VERSION {
        eprintln!(
            "[rosync lint] warning: luau-lsp {version} is older than tested {}.{}.{}; run `aftman install` after `rosync refresh`",
            RECOMMENDED_LUAU_LSP_VERSION.0,
            RECOMMENDED_LUAU_LSP_VERSION.1,
            RECOMMENDED_LUAU_LSP_VERSION.2,
        );
    }
}

pub(super) fn parse_semver_triplet(value: &str) -> Option<(u64, u64, u64)> {
    let version = value.trim().trim_start_matches('v');
    let mut parts = version.split(|character: char| !character.is_ascii_digit());
    let major = parts.find(|part| !part.is_empty())?.parse().ok()?;
    let minor = parts.find(|part| !part.is_empty())?.parse().ok()?;
    let patch = parts.find(|part| !part.is_empty())?.parse().ok()?;
    Some((major, minor, patch))
}

pub(super) fn resolve_img_api_key(
    preferred_env: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
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

    if preferred_env.is_some() {
        if let Some(value) = find_widget_secret("robloxCloudApiKey") {
            return Ok(value);
        }
    }

    Err(format!(
        "upload: missing Roblox Open Cloud credential. Save one in Ro Sync Settings, set one of {}, or pass --api-key-env with a populated environment variable.",
        env_names.join(", ")
    )
    .into())
}

pub(super) fn resolve_img_creator(project: &Option<PathBuf>) -> Option<String> {
    if let Some(group_id) = project_group_id(project.as_deref()) {
        return Some(format!("group:{group_id}"));
    }
    if let Some(group_id) = active_widget_project_group_id() {
        return Some(format!("group:{group_id}"));
    }
    None
}

pub(super) fn project_group_id(project: Option<&std::path::Path>) -> Option<String> {
    let root = match project {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    project_config::read_from_disk(&root)
        .ok()
        .flatten()
        .and_then(|cfg| cfg.group_id)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

pub(super) fn active_widget_project_group_id() -> Option<String> {
    for state_file in widget_state_file_candidates() {
        let Ok(text) = std::fs::read_to_string(&state_file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(group_id) = group_id_from_widget_state(&value) {
            return Some(group_id);
        }
    }
    None
}

pub(super) fn group_id_from_widget_state(value: &serde_json::Value) -> Option<String> {
    let state = value.get("state").unwrap_or(value);
    let active_id = state
        .get("activeProjectId")
        .and_then(serde_json::Value::as_str)?;
    let projects = state
        .get("projects")
        .and_then(serde_json::Value::as_array)?;
    projects
        .iter()
        .find(|project| {
            project
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id == active_id)
        })
        .and_then(|project| project.get("groupId"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(super) fn find_widget_secret(key: &str) -> Option<String> {
    if let Ok(state_dir) = lifecycle::state_dir(None) {
        let path = lifecycle::credentials_path(&state_dir);
        if let Ok(Some(secret)) = lifecycle::read_credential(&path, key) {
            let secret = secret.trim();
            if !secret.is_empty() {
                return Some(secret.to_string());
            }
        }
    }
    for state_file in widget_state_file_candidates() {
        let Ok(text) = std::fs::read_to_string(&state_file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str(&text) else {
            continue;
        };
        if let Some(secret) = secret_from_widget_state(&value, key) {
            return Some(secret);
        }
    }
    None
}

pub(super) fn secret_from_widget_state(value: &serde_json::Value, key: &str) -> Option<String> {
    for pointer in [
        format!("/state/secrets/{key}"),
        format!("/secrets/{key}"),
        format!("/{key}"),
    ] {
        if let Some(secret) = value
            .pointer(&pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
        {
            return Some(secret.to_string());
        }
    }
    None
}

pub(super) fn widget_state_file_candidates() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    let mut files = Vec::new();

    if let Ok(path) = std::env::var("ROSYNC_WIDGET_STATE") {
        push_unique_path(&mut files, PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        push_unique_path(
            &mut files,
            home.join(".terminal64")
                .join("widgets")
                .join("ro-sync")
                .join("state.json"),
        );
    }

    if let Ok(cwd) = std::env::current_dir() {
        push_ancestors(&mut bases, cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        push_exe_ancestors(&mut bases, &exe);
        if let Ok(canonical) = std::fs::canonicalize(&exe) {
            push_exe_ancestors(&mut bases, &canonical);
        }
        if let Ok(target) = std::fs::read_link(&exe) {
            let resolved = if target.is_absolute() {
                target
            } else {
                exe.parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join(target)
            };
            push_exe_ancestors(&mut bases, &resolved);
            if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                push_exe_ancestors(&mut bases, &canonical);
            }
        }
    }

    for base in bases {
        push_unique_path(&mut files, base.join("state.json"));
    }
    files
}

pub(super) fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

pub(super) fn push_exe_ancestors(paths: &mut Vec<PathBuf>, exe: &std::path::Path) {
    if let Some(parent) = exe.parent() {
        push_ancestors(paths, parent.to_path_buf());
    }
}

pub(super) fn push_ancestors(paths: &mut Vec<PathBuf>, start: PathBuf) {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if !paths.contains(&dir) {
            paths.push(dir.clone());
        }
        cur = dir.parent().map(std::path::Path::to_path_buf);
    }
}

pub(super) fn find_in_tool_bases(rel: &std::path::Path) -> Option<PathBuf> {
    let mut bases = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(dir) = cur {
            bases.push(dir.to_path_buf());
            cur = dir.parent();
        }
    }

    for base in bases {
        let candidate = base.join(rel);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(super) fn platform_tool_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x86_64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else {
        "unknown"
    }
}

pub(super) fn print_luau_lsp_missing(luau_lsp: &OsString) {
    eprintln!("luau-lsp not found: {}", luau_lsp.to_string_lossy());
    eprintln!();
    eprintln!("Install luau-lsp and make it available on PATH:");
    eprintln!("https://github.com/JohnnyMorganz/luau-lsp");
    eprintln!();
    eprintln!("Ro-Sync also checks this bundled tool path:");
    eprintln!(
        "tools/luau-lsp/{}/{}",
        platform_tool_triple(),
        if cfg!(windows) {
            "luau-lsp.exe"
        } else {
            "luau-lsp"
        }
    );
    eprintln!();
    eprintln!("Or pass an explicit executable path:");
    eprintln!("rosync lint --luau-lsp /path/to/luau-lsp");
}
