//! Versioned, LLM-oriented workflow schema.
//!
//! This module deliberately contains no transport or Studio execution code. It
//! is the boundary between untrusted JSON supplied by an agent and an executor:
//! deserialize, validate the entire plan, then resolve references immediately
//! before executing each ordered step.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;

pub const WORKFLOW_VERSION: u32 = 1;
pub const MAX_WORKFLOW_STEPS: usize = 1_024;
pub const MAX_STEP_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
pub const MAX_CAPTURE_DIMENSION: u32 = 16_384;
pub const MAX_CAPTURE_PIXELS: u64 = 67_108_864;

/// Results are keyed by workflow step id. A reference such as
/// `$camera.value.properties.CFrame` walks the JSON value stored for `camera`.
pub type StepResults = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workflow {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_mode: Option<ExpectedMode>,
    #[serde(
        default,
        alias = "expectedPlace",
        skip_serializing_if = "Option::is_none"
    )]
    pub expected_place_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transactions: Vec<TransactionGroup>,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExpectedMode {
    Edit,
    Play,
    Run,
    PlayServer,
    PlayClient,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionGroup {
    pub id: String,
    /// Atomic groups are intended to map to one Studio change-history
    /// recording. Set this to false for a logical/non-atomic result group.
    #[serde(default = "default_true")]
    pub atomic: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStep {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub verify: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
    #[serde(flatten)]
    pub operation: StepOperation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StepOperation {
    Get {
        path: String,
        #[serde(default, alias = "prop", skip_serializing_if = "Option::is_none")]
        property: Option<String>,
    },
    Set {
        path: String,
        #[serde(alias = "prop")]
        property: String,
        value: Value,
    },
    New {
        /// Parent instance path.
        path: String,
        class: String,
        name: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        props: BTreeMap<String, Value>,
    },
    Rm {
        path: String,
    },
    Mv {
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    AttrSet {
        path: String,
        name: String,
        value: Value,
    },
    AttrRm {
        path: String,
        name: String,
    },
    AttrLs {
        path: String,
    },
    TagAdd {
        path: String,
        tag: String,
    },
    TagRm {
        path: String,
        tag: String,
    },
    Assert {
        actual: Value,
        check: Assertion,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Wait {
        path: String,
        #[serde(default, alias = "prop", skip_serializing_if = "Option::is_none")]
        property: Option<String>,
        check: Assertion,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        poll_interval_ms: Option<u64>,
    },
    Eval {
        source: String,
    },
    Capture {
        #[serde(default)]
        target: CaptureTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<CaptureRect>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_size: Option<CaptureSize>,
        #[serde(default)]
        ui: CaptureUi,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Method calls are intentionally available only outside atomic groups:
    /// the executor cannot know whether an arbitrary method yields or mutates.
    Call {
        path: String,
        method: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<Value>,
    },
    /// Typed passthrough for the playtest controller. The action remains typed
    /// while action-specific arguments can evolve without changing schema v1.
    Playtest {
        action: PlaytestAction,
        #[serde(default, skip_serializing_if = "is_null")]
        args: Value,
    },
    Upload {
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asset_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        creator: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Assertion {
    Equals {
        expected: Value,
    },
    NotEquals {
        expected: Value,
    },
    Exists {
        #[serde(default = "default_true")]
        expected: bool,
    },
    Truthy {
        #[serde(default = "default_true")]
        expected: bool,
    },
    Contains {
        expected: Value,
    },
    GreaterThan {
        expected: f64,
    },
    GreaterThanOrEqual {
        expected: f64,
    },
    LessThan {
        expected: f64,
    },
    LessThanOrEqual {
        expected: f64,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureTarget {
    #[default]
    Screen,
    Scene,
    Viewport,
    Desktop,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureUi {
    #[default]
    None,
    Game,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaytestAction {
    Start,
    Stop,
    Status,
    Contexts,
    Wait,
    Exec,
    Logs,
    Ui,
    Input,
    Capture,
    Request,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationIssue {
    pub code: String,
    pub location: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowValidationErrors {
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for WorkflowValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "workflow validation failed with {} issue(s)",
            self.issues.len()
        )?;
        for issue in &self.issues {
            write!(
                f,
                "\n- {} at {}: {}",
                issue.code, issue.location, issue.message
            )?;
        }
        Ok(())
    }
}

impl Error for WorkflowValidationErrors {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonReference {
    pub step_id: String,
    pub path: Vec<String>,
}

#[derive(Debug)]
pub enum ResolveError {
    InvalidReference {
        reference: String,
        reason: String,
    },
    MissingStep {
        reference: String,
        step_id: String,
    },
    MissingPath {
        reference: String,
        segment: String,
    },
    InvalidResolvedStep {
        step_id: String,
        source: serde_json::Error,
    },
    InvalidResolvedValidation {
        step_id: String,
        issues: Vec<ValidationIssue>,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReference { reference, reason } => {
                write!(f, "invalid workflow reference {reference:?}: {reason}")
            }
            Self::MissingStep { reference, step_id } => {
                write!(
                    f,
                    "reference {reference:?} has no result for step {step_id:?}"
                )
            }
            Self::MissingPath { reference, segment } => {
                write!(
                    f,
                    "reference {reference:?} is missing path segment {segment:?}"
                )
            }
            Self::InvalidResolvedStep { step_id, source } => {
                write!(
                    f,
                    "resolved step {step_id:?} no longer matches schema: {source}"
                )
            }
            Self::InvalidResolvedValidation { step_id, issues } => {
                write!(
                    f,
                    "resolved step {step_id:?} failed {} validation check(s)",
                    issues.len()
                )?;
                for issue in issues {
                    write!(
                        f,
                        "\n- {} at {}: {}",
                        issue.code, issue.location, issue.message
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ResolveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidResolvedStep { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Workflow {
    /// Parse and validate a workflow in one call. Executors should not run a
    /// workflow that has merely deserialized: cross-step invariants live in
    /// [`Workflow::validate`].
    pub fn from_json(source: &str) -> Result<Self, WorkflowParseError> {
        let workflow: Self = serde_json::from_str(source).map_err(WorkflowParseError::Json)?;
        workflow
            .validate()
            .map_err(WorkflowParseError::Validation)?;
        Ok(workflow)
    }

    pub fn validate(&self) -> Result<(), WorkflowValidationErrors> {
        let issues = self.validation_issues();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(WorkflowValidationErrors { issues })
        }
    }

    pub fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.version != WORKFLOW_VERSION {
            push_issue(
                &mut issues,
                "unsupported_version",
                "$.version",
                format!(
                    "expected workflow version {WORKFLOW_VERSION}, got {}",
                    self.version
                ),
            );
        }
        validate_optional_label(&self.name, "$.name", 128, &mut issues);
        validate_optional_key(&self.idempotency_key, &mut issues);
        if let Some(place_id) = &self.expected_place_id {
            if place_id.is_empty() || !place_id.bytes().all(|byte| byte.is_ascii_digit()) {
                push_issue(
                    &mut issues,
                    "invalid_place_id",
                    "$.expectedPlaceId",
                    "expectedPlaceId must be a decimal Roblox PlaceId string",
                );
            }
        }
        if self.steps.is_empty() {
            push_issue(
                &mut issues,
                "empty_workflow",
                "$.steps",
                "a workflow must contain at least one step",
            );
        } else if self.steps.len() > MAX_WORKFLOW_STEPS {
            push_issue(
                &mut issues,
                "too_many_steps",
                "$.steps",
                format!("at most {MAX_WORKFLOW_STEPS} steps are allowed"),
            );
        }

        let mut transaction_by_id = HashMap::new();
        for (index, transaction) in self.transactions.iter().enumerate() {
            let location = format!("$.transactions[{index}].id");
            validate_identifier(&transaction.id, &location, &mut issues);
            if transaction_by_id
                .insert(transaction.id.as_str(), (index, transaction))
                .is_some()
            {
                push_issue(
                    &mut issues,
                    "duplicate_transaction_id",
                    location,
                    format!(
                        "transaction id {:?} is declared more than once",
                        transaction.id
                    ),
                );
            }
        }

        let mut first_step_index = HashMap::new();
        for (index, step) in self.steps.iter().enumerate() {
            let location = format!("$.steps[{index}].id");
            validate_identifier(&step.id, &location, &mut issues);
            if let Some(first) = first_step_index.insert(step.id.as_str(), index) {
                push_issue(
                    &mut issues,
                    "duplicate_step_id",
                    location,
                    format!("step id {:?} was first used at index {first}", step.id),
                );
            }
        }

        let mut transaction_members: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, step) in self.steps.iter().enumerate() {
            let base = format!("$.steps[{index}]");
            validate_step_common(step, &base, &mut issues);
            validate_operation(&step.operation, step.timeout_ms, &base, &mut issues);

            if let Some(transaction_id) = &step.transaction {
                transaction_members
                    .entry(transaction_id.as_str())
                    .or_default()
                    .push(index);
                match transaction_by_id.get(transaction_id.as_str()) {
                    None => push_issue(
                        &mut issues,
                        "unknown_transaction",
                        format!("{base}.transaction"),
                        format!("transaction {transaction_id:?} is not declared"),
                    ),
                    Some((_, transaction))
                        if transaction.atomic && !step.operation.atomic_safe() =>
                    {
                        push_issue(
                            &mut issues,
                            "unsafe_atomic_operation",
                            format!("{base}.op"),
                            format!(
                                "operation {:?} cannot run inside atomic transaction {transaction_id:?}",
                                step.operation.op_name()
                            ),
                        );
                    }
                    _ => {}
                }
            }

            let value = serde_json::to_value(step).expect("workflow steps always serialize");
            let mut references = Vec::new();
            scan_references(&value, &base, &mut references, &mut issues);
            for (location, reference) in references {
                match first_step_index.get(reference.step_id.as_str()).copied() {
                    None => push_issue(
                        &mut issues,
                        "unknown_reference",
                        location,
                        format!("reference names unknown step {:?}", reference.step_id),
                    ),
                    Some(dependency_index) if dependency_index == index => push_issue(
                        &mut issues,
                        "self_reference",
                        location,
                        format!("step {:?} cannot reference its own result", step.id),
                    ),
                    Some(dependency_index) if dependency_index > index => push_issue(
                        &mut issues,
                        "forward_reference",
                        location,
                        format!(
                            "step {:?} appears after this step; workflows execute in order",
                            reference.step_id
                        ),
                    ),
                    _ => {}
                }
            }
        }

        for (transaction_index, transaction) in self.transactions.iter().enumerate() {
            let members = transaction_members
                .get(transaction.id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if members.is_empty() {
                push_issue(
                    &mut issues,
                    "empty_transaction",
                    format!("$.transactions[{transaction_index}]"),
                    format!("transaction {:?} has no member steps", transaction.id),
                );
            }
            if transaction.atomic && !indices_are_contiguous(members) {
                push_issue(
                    &mut issues,
                    "non_contiguous_atomic_transaction",
                    format!("$.transactions[{transaction_index}]"),
                    format!(
                        "atomic transaction {:?} must occupy one contiguous range of steps",
                        transaction.id
                    ),
                );
            }
        }

        issues
    }

    /// Return direct dependencies of a step in stable first-seen order.
    pub fn dependencies_for(&self, step_index: usize) -> Result<Vec<String>, ResolveError> {
        let step = self
            .steps
            .get(step_index)
            .ok_or_else(|| ResolveError::InvalidReference {
                reference: format!("step index {step_index}"),
                reason: "step index is out of range".into(),
            })?;
        let value = serde_json::to_value(step).expect("workflow steps always serialize");
        let mut ordered = Vec::new();
        let mut seen = BTreeSet::new();
        collect_dependencies(&value, &mut ordered, &mut seen)?;
        Ok(ordered)
    }
}

#[derive(Debug)]
pub enum WorkflowParseError {
    Json(serde_json::Error),
    Validation(WorkflowValidationErrors),
}

impl fmt::Display for WorkflowParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "invalid workflow JSON: {error}"),
            Self::Validation(error) => error.fmt(f),
        }
    }
}

impl Error for WorkflowParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Validation(error) => Some(error),
        }
    }
}

impl WorkflowStep {
    pub fn resolve(&self, results: &StepResults) -> Result<Self, ResolveError> {
        let mut value = serde_json::to_value(self).expect("workflow steps always serialize");
        resolve_references(&mut value, results)?;
        let resolved: Self =
            serde_json::from_value(value).map_err(|source| ResolveError::InvalidResolvedStep {
                step_id: self.id.clone(),
                source,
            })?;
        // Reference substitution can turn an otherwise valid placeholder into
        // a forbidden or oversized value (for example, property="Parent").
        // Re-run every per-step check after substitution before execution.
        let mut issues = Vec::new();
        validate_step_common(&resolved, "$.resolvedStep", &mut issues);
        validate_operation(
            &resolved.operation,
            resolved.timeout_ms,
            "$.resolvedStep",
            &mut issues,
        );
        if issues.is_empty() {
            Ok(resolved)
        } else {
            Err(ResolveError::InvalidResolvedValidation {
                step_id: resolved.id.clone(),
                issues,
            })
        }
    }
}

impl StepOperation {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Get { .. } => "get",
            Self::Set { .. } => "set",
            Self::New { .. } => "new",
            Self::Rm { .. } => "rm",
            Self::Mv { .. } => "mv",
            Self::AttrSet { .. } => "attr-set",
            Self::AttrRm { .. } => "attr-rm",
            Self::AttrLs { .. } => "attr-ls",
            Self::TagAdd { .. } => "tag-add",
            Self::TagRm { .. } => "tag-rm",
            Self::Assert { .. } => "assert",
            Self::Wait { .. } => "wait",
            Self::Eval { .. } => "eval",
            Self::Capture { .. } => "capture",
            Self::Call { .. } => "call",
            Self::Playtest { .. } => "playtest",
            Self::Upload { .. } => "upload",
        }
    }

    /// Safe means the operation is bounded, non-yielding, and has understood
    /// change-history behavior. Unknown-side-effect calls and external/runtime
    /// operations are excluded even if a particular invocation looks harmless.
    pub fn atomic_safe(&self) -> bool {
        !matches!(
            self,
            Self::Eval { .. }
                | Self::Call { .. }
                | Self::Wait { .. }
                | Self::Capture { .. }
                | Self::Playtest { .. }
                | Self::Upload { .. }
        )
    }

    pub fn supports_verify(&self) -> bool {
        matches!(
            self,
            Self::Set { .. }
                | Self::New { .. }
                | Self::Rm { .. }
                | Self::Mv { .. }
                | Self::AttrSet { .. }
                | Self::AttrRm { .. }
                | Self::TagAdd { .. }
                | Self::TagRm { .. }
        )
    }

    pub fn supports_target_precondition(&self) -> bool {
        !matches!(
            self,
            Self::Assert { .. } | Self::Eval { .. } | Self::Playtest { .. } | Self::Upload { .. }
        )
    }

    /// Split the schema operation into an operation name and argument object.
    /// This is intentionally transport-neutral; executors may map names such
    /// as `attr-set` onto the current plugin wire spelling.
    pub fn request_parts(&self) -> (String, Value) {
        let mut value = serde_json::to_value(self).expect("step operations always serialize");
        let object = value
            .as_object_mut()
            .expect("internally tagged operation serializes as an object");
        let op = object
            .remove("op")
            .and_then(|value| value.as_str().map(str::to_owned))
            .expect("internally tagged operation contains op");
        (op, Value::Object(std::mem::take(object)))
    }
}

/// Resolve references recursively in JSON values. Object keys are deliberately
/// never templated. A referenced object is inserted verbatim and is not scanned
/// a second time, preventing data returned by Studio from becoming executable
/// workflow syntax.
pub fn resolve_references(value: &mut Value, results: &StepResults) -> Result<(), ResolveError> {
    match value {
        Value::String(text) if text.starts_with("$$") => {
            text.remove(0);
            Ok(())
        }
        Value::String(text) => {
            let Some(reference) =
                parse_reference(text).map_err(|reason| ResolveError::InvalidReference {
                    reference: text.clone(),
                    reason,
                })?
            else {
                return Ok(());
            };
            let resolved = resolve_reference(text, &reference, results)?;
            *value = resolved;
            Ok(())
        }
        Value::Array(values) => {
            for value in values {
                resolve_references(value, results)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                resolve_references(value, results)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Parse an exact JSON reference. Strings that do not use reference syntax
/// return `Ok(None)`. A valid step id without a suffix (`$step`) references the
/// complete step result; `$step.a.0.b` traverses objects and arrays.
pub fn parse_reference(text: &str) -> Result<Option<JsonReference>, String> {
    if !text.starts_with('$') || text.starts_with("$$") {
        return Ok(None);
    }
    let body = &text[1..];
    let mut parts = body.split('.');
    let step_id = parts.next().unwrap_or_default();
    // Currency and similar literal strings (e.g. "$100") are not references.
    if !step_id
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
    {
        return Ok(None);
    }
    if !is_valid_identifier(step_id) {
        return Err("step id must use letters, digits, '_' or '-'".into());
    }
    let path: Vec<String> = parts.map(str::to_owned).collect();
    if path.iter().any(String::is_empty) {
        return Err("reference path contains an empty segment".into());
    }
    Ok(Some(JsonReference {
        step_id: step_id.to_owned(),
        path,
    }))
}

fn resolve_reference(
    source: &str,
    reference: &JsonReference,
    results: &StepResults,
) -> Result<Value, ResolveError> {
    let mut current = results
        .get(&reference.step_id)
        .ok_or_else(|| ResolveError::MissingStep {
            reference: source.to_owned(),
            step_id: reference.step_id.clone(),
        })?;
    for segment in &reference.path {
        current = match current {
            Value::Object(object) => object.get(segment),
            Value::Array(array) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| array.get(index)),
            _ => None,
        }
        .ok_or_else(|| ResolveError::MissingPath {
            reference: source.to_owned(),
            segment: segment.clone(),
        })?;
    }
    Ok(current.clone())
}

fn scan_references(
    value: &Value,
    location: &str,
    references: &mut Vec<(String, JsonReference)>,
    issues: &mut Vec<ValidationIssue>,
) {
    match value {
        Value::String(text) if !text.starts_with("$$") => match parse_reference(text) {
            Ok(Some(reference)) => references.push((location.to_owned(), reference)),
            Ok(None) => {}
            Err(reason) => push_issue(
                issues,
                "invalid_reference",
                location,
                format!("invalid reference {text:?}: {reason}"),
            ),
        },
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                scan_references(value, &format!("{location}[{index}]"), references, issues);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                scan_references(
                    value,
                    &format!("{location}.{}", json_path_key(key)),
                    references,
                    issues,
                );
            }
        }
        _ => {}
    }
}

fn collect_dependencies(
    value: &Value,
    ordered: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) -> Result<(), ResolveError> {
    match value {
        Value::String(text) if !text.starts_with("$$") => {
            if let Some(reference) =
                parse_reference(text).map_err(|reason| ResolveError::InvalidReference {
                    reference: text.clone(),
                    reason,
                })?
            {
                if seen.insert(reference.step_id.clone()) {
                    ordered.push(reference.step_id);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_dependencies(value, ordered, seen)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_dependencies(value, ordered, seen)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_step_common(step: &WorkflowStep, base: &str, issues: &mut Vec<ValidationIssue>) {
    if let Some(timeout_ms) = step.timeout_ms {
        if timeout_ms == 0 || timeout_ms > MAX_STEP_TIMEOUT_MS {
            push_issue(
                issues,
                "invalid_timeout",
                format!("{base}.timeoutMs"),
                format!("timeoutMs must be between 1 and {MAX_STEP_TIMEOUT_MS}"),
            );
        }
    }
    if step.verify && !step.operation.supports_verify() {
        push_issue(
            issues,
            "unsupported_verification",
            format!("{base}.verify"),
            format!(
                "operation {:?} does not have deterministic generic verification",
                step.operation.op_name()
            ),
        );
    }
    if (step.expected_class.is_some() || step.etag.is_some())
        && !step.operation.supports_target_precondition()
    {
        push_issue(
            issues,
            "unsupported_precondition",
            base,
            format!(
                "operation {:?} has no target for expectedClass/etag",
                step.operation.op_name()
            ),
        );
    }
    if let Some(expected_class) = &step.expected_class {
        validate_resolvable_nonempty(expected_class, &format!("{base}.expectedClass"), issues);
    }
    if let Some(etag) = &step.etag {
        validate_resolvable_nonempty(etag, &format!("{base}.etag"), issues);
    }
}

fn validate_operation(
    operation: &StepOperation,
    timeout_ms: Option<u64>,
    base: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let path = |value: &str, field: &str, issues: &mut Vec<ValidationIssue>| {
        validate_resolvable_nonempty(value, &format!("{base}.{field}"), issues)
    };
    match operation {
        StepOperation::Get {
            path: value,
            property,
        } => {
            path(value, "path", issues);
            if let Some(property) = property {
                validate_resolvable_nonempty(property, &format!("{base}.property"), issues);
            }
        }
        StepOperation::Set {
            path: value,
            property,
            ..
        } => {
            path(value, "path", issues);
            validate_resolvable_nonempty(property, &format!("{base}.property"), issues);
            if parse_reference(property).is_ok_and(|reference| reference.is_some()) {
                push_issue(
                    issues,
                    "dynamic_set_property",
                    format!("{base}.property"),
                    "set.property must be static so guarded properties such as Parent are rejected before execution",
                );
            }
            if property == "Parent" {
                push_issue(
                    issues,
                    "unsafe_parent_set",
                    format!("{base}.property"),
                    "use the mv operation instead of setting Parent",
                );
            }
        }
        StepOperation::New {
            path: value,
            class,
            name,
            props,
        } => {
            path(value, "path", issues);
            validate_resolvable_nonempty(class, &format!("{base}.class"), issues);
            validate_resolvable_nonempty(name, &format!("{base}.name"), issues);
            if props.len() > 64 {
                push_issue(
                    issues,
                    "too_many_properties",
                    format!("{base}.props"),
                    "new supports at most 64 initial properties",
                );
            }
            for property in props.keys() {
                validate_resolvable_nonempty(
                    property,
                    &format!("{base}.props.{}", json_path_key(property)),
                    issues,
                );
                if property == "Parent" {
                    push_issue(
                        issues,
                        "unsafe_parent_set",
                        format!("{base}.props.Parent"),
                        "use the mv operation instead of setting Parent",
                    );
                }
            }
        }
        StepOperation::Rm { path: value } | StepOperation::AttrLs { path: value } => {
            path(value, "path", issues)
        }
        StepOperation::Mv { from, to, .. } => {
            path(from, "from", issues);
            path(to, "to", issues);
        }
        StepOperation::AttrSet {
            path: value, name, ..
        }
        | StepOperation::AttrRm {
            path: value, name, ..
        } => {
            path(value, "path", issues);
            validate_resolvable_nonempty(name, &format!("{base}.name"), issues);
        }
        StepOperation::TagAdd {
            path: value, tag, ..
        }
        | StepOperation::TagRm {
            path: value, tag, ..
        } => {
            path(value, "path", issues);
            validate_resolvable_nonempty(tag, &format!("{base}.tag"), issues);
        }
        StepOperation::Assert { message, .. } => {
            validate_optional_label(message, &format!("{base}.message"), 1_024, issues);
        }
        StepOperation::Wait {
            path: value,
            property,
            poll_interval_ms,
            ..
        } => {
            path(value, "path", issues);
            if let Some(property) = property {
                validate_resolvable_nonempty(property, &format!("{base}.property"), issues);
            }
            if let Some(poll_interval_ms) = poll_interval_ms {
                if *poll_interval_ms == 0 || *poll_interval_ms > 60_000 {
                    push_issue(
                        issues,
                        "invalid_poll_interval",
                        format!("{base}.pollIntervalMs"),
                        "pollIntervalMs must be between 1 and 60000",
                    );
                }
                if timeout_ms.is_some_and(|timeout| *poll_interval_ms > timeout) {
                    push_issue(
                        issues,
                        "poll_exceeds_timeout",
                        format!("{base}.pollIntervalMs"),
                        "pollIntervalMs cannot exceed timeoutMs",
                    );
                }
            }
        }
        StepOperation::Eval { source } => {
            if source.trim().is_empty() {
                push_issue(
                    issues,
                    "empty_source",
                    format!("{base}.source"),
                    "eval source cannot be empty",
                );
            }
            if source.len() > 256 * 1_024 {
                push_issue(
                    issues,
                    "source_too_large",
                    format!("{base}.source"),
                    "eval source cannot exceed 256 KiB",
                );
            }
        }
        StepOperation::Capture {
            path: capture_path,
            region,
            output_size,
            output,
            ..
        } => {
            if let Some(value) = capture_path {
                path(value, "path", issues);
            }
            if let Some(region) = region {
                validate_capture_size(
                    region.width,
                    region.height,
                    &format!("{base}.region"),
                    issues,
                );
            }
            if let Some(size) = output_size {
                validate_capture_size(
                    size.width,
                    size.height,
                    &format!("{base}.outputSize"),
                    issues,
                );
            }
            if let Some(output) = output {
                validate_resolvable_nonempty(output, &format!("{base}.output"), issues);
            }
        }
        StepOperation::Call {
            path: value,
            method,
            ..
        } => {
            path(value, "path", issues);
            validate_resolvable_nonempty(method, &format!("{base}.method"), issues);
        }
        StepOperation::Playtest { .. } => {}
        StepOperation::Upload {
            paths,
            asset_type,
            creator,
        } => {
            if paths.is_empty() {
                push_issue(
                    issues,
                    "empty_upload",
                    format!("{base}.paths"),
                    "upload requires at least one path",
                );
            }
            for (index, value) in paths.iter().enumerate() {
                path(value, &format!("paths[{index}]"), issues);
            }
            if let Some(asset_type) = asset_type {
                validate_resolvable_nonempty(asset_type, &format!("{base}.assetType"), issues);
            }
            if let Some(creator) = creator {
                validate_resolvable_nonempty(creator, &format!("{base}.creator"), issues);
            }
        }
    }
}

fn validate_capture_size(
    width: u32,
    height: u32,
    location: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    if width == 0
        || height == 0
        || width > MAX_CAPTURE_DIMENSION
        || height > MAX_CAPTURE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_CAPTURE_PIXELS
    {
        push_issue(
            issues,
            "invalid_capture_size",
            location,
            format!(
                "capture dimensions must be non-zero, at most {MAX_CAPTURE_DIMENSION} per side, and at most {MAX_CAPTURE_PIXELS} pixels"
            ),
        );
    }
}

fn validate_optional_key(value: &Option<String>, issues: &mut Vec<ValidationIssue>) {
    let Some(value) = value else {
        return;
    };
    if value.is_empty() || value.len() > 128 {
        push_issue(
            issues,
            "invalid_idempotency_key",
            "$.idempotencyKey",
            "idempotencyKey must contain between 1 and 128 bytes",
        );
    } else if value.chars().any(char::is_control) {
        push_issue(
            issues,
            "invalid_idempotency_key",
            "$.idempotencyKey",
            "idempotencyKey cannot contain control characters",
        );
    }
}

fn validate_optional_label(
    value: &Option<String>,
    location: &str,
    max_len: usize,
    issues: &mut Vec<ValidationIssue>,
) {
    if let Some(value) = value {
        if value.trim().is_empty() || value.len() > max_len {
            push_issue(
                issues,
                "invalid_label",
                location,
                format!("value must be non-empty and at most {max_len} bytes"),
            );
        }
    }
}

fn validate_identifier(value: &str, location: &str, issues: &mut Vec<ValidationIssue>) {
    if !is_valid_identifier(value) {
        push_issue(
            issues,
            "invalid_identifier",
            location,
            "id must start with a letter or '_' and contain at most 64 letters, digits, '_' or '-'",
        );
    }
}

fn validate_resolvable_nonempty(value: &str, location: &str, issues: &mut Vec<ValidationIssue>) {
    if value.trim().is_empty() {
        push_issue(issues, "empty_value", location, "value cannot be empty");
    }
}

fn is_valid_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

fn indices_are_contiguous(indices: &[usize]) -> bool {
    indices
        .windows(2)
        .all(|pair| pair[1] == pair[0].saturating_add(1))
}

fn json_path_key(key: &str) -> String {
    if is_valid_identifier(key) {
        key.to_owned()
    } else {
        format!(
            "[{}]",
            serde_json::to_string(key).expect("string serializes")
        )
    }
}

fn push_issue(
    issues: &mut Vec<ValidationIssue>,
    code: impl Into<String>,
    location: impl Into<String>,
    message: impl Into<String>,
) {
    issues.push(ValidationIssue {
        code: code.into(),
        location: location.into(),
        message: message.into(),
    });
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_null(value: &Value) -> bool {
    value.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_step(id: &str, operation: StepOperation) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            timeout_ms: None,
            verify: false,
            expected_class: None,
            etag: None,
            transaction: None,
            operation,
        }
    }

    fn workflow(steps: Vec<WorkflowStep>) -> Workflow {
        Workflow {
            version: WORKFLOW_VERSION,
            name: None,
            idempotency_key: Some("request-123".into()),
            expected_mode: Some(ExpectedMode::Edit),
            expected_place_id: Some("123456789".into()),
            transactions: Vec::new(),
            steps,
        }
    }

    #[test]
    fn parses_and_round_trips_v1_json() {
        let source = json!({
            "version": 1,
            "idempotencyKey": "make-box-v1",
            "expectedMode": "edit",
            "expectedPlaceId": "123",
            "transactions": [{"id": "edit", "atomic": true}],
            "steps": [
                {"id": "parent", "op": "get", "path": "Workspace", "expectedClass": "Workspace"},
                {
                    "id": "box",
                    "op": "new",
                    "path": "$parent.value.path",
                    "class": "Part",
                    "name": "AgentBox",
                    "props": {"Anchored": true},
                    "transaction": "edit",
                    "verify": true,
                    "timeoutMs": 5000
                },
                {
                    "id": "color",
                    "op": "set",
                    "path": "$box.value.path",
                    "property": "Color",
                    "value": {"__type": "Color3", "r": 1, "g": 0, "b": 0},
                    "transaction": "edit",
                    "etag": "$box.value.etag"
                }
            ]
        });
        let parsed = Workflow::from_json(&source.to_string()).expect("valid workflow");
        assert_eq!(parsed.steps.len(), 3);
        assert_eq!(parsed.dependencies_for(2).unwrap(), vec!["box"]);

        let round_trip: Workflow =
            serde_json::from_value(serde_json::to_value(&parsed).unwrap()).unwrap();
        assert_eq!(round_trip, parsed);
    }

    #[test]
    fn validation_reports_duplicate_forward_and_unknown_references() {
        let workflow = workflow(vec![
            minimal_step(
                "first",
                StepOperation::Set {
                    path: "$later.value.path".into(),
                    property: "Name".into(),
                    value: json!("$missing.value"),
                },
            ),
            minimal_step(
                "first",
                StepOperation::Get {
                    path: "Workspace".into(),
                    property: None,
                },
            ),
            minimal_step(
                "later",
                StepOperation::Get {
                    path: "Workspace/Part".into(),
                    property: None,
                },
            ),
        ]);
        let errors = workflow.validate().unwrap_err();
        let codes: BTreeSet<_> = errors
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect();
        assert!(codes.contains("duplicate_step_id"));
        assert!(codes.contains("forward_reference"));
        assert!(codes.contains("unknown_reference"));
    }

    #[test]
    fn resolves_references_with_original_json_types() {
        let step = minimal_step(
            "write",
            StepOperation::Set {
                path: "$lookup.value.path".into(),
                property: "Transparency".into(),
                value: json!("$lookup.value.properties.Transparency"),
            },
        );
        let mut results = StepResults::new();
        results.insert(
            "lookup".into(),
            json!({
                "value": {
                    "path": "Workspace/Part",
                    "properties": {"Transparency": 0.25}
                }
            }),
        );
        let resolved = step.resolve(&results).expect("resolve");
        match resolved.operation {
            StepOperation::Set { path, value, .. } => {
                assert_eq!(path, "Workspace/Part");
                assert_eq!(value, json!(0.25));
            }
            other => panic!("unexpected operation: {other:?}"),
        }
    }

    #[test]
    fn escaped_reference_is_a_literal_and_substituted_data_is_not_reprocessed() {
        let mut value = json!({
            "literal": "$$prior.value",
            "data": "$prior.value"
        });
        let results = BTreeMap::from([("prior".into(), json!({"value": "$another.value"}))]);
        resolve_references(&mut value, &results).unwrap();
        assert_eq!(value["literal"], "$prior.value");
        assert_eq!(value["data"], "$another.value");
    }

    #[test]
    fn missing_reference_path_is_precise() {
        let mut value = json!("$prior.value.nope");
        let results = BTreeMap::from([("prior".into(), json!({"value": {}}))]);
        let error = resolve_references(&mut value, &results).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::MissingPath { segment, .. } if segment == "nope"
        ));
    }

    #[test]
    fn every_unbounded_operation_is_rejected_in_atomic_group() {
        let unsafe_operations = vec![
            StepOperation::Eval {
                source: "return true".into(),
            },
            StepOperation::Call {
                path: "Workspace".into(),
                method: "GetChildren".into(),
                args: vec![],
            },
            StepOperation::Wait {
                path: "Workspace/Part".into(),
                property: Some("Transparency".into()),
                check: Assertion::Equals { expected: json!(0) },
                poll_interval_ms: Some(50),
            },
            StepOperation::Capture {
                target: CaptureTarget::Screen,
                path: None,
                context: None,
                region: None,
                output_size: None,
                ui: CaptureUi::None,
                output: None,
            },
            StepOperation::Playtest {
                action: PlaytestAction::Start,
                args: Value::Null,
            },
            StepOperation::Upload {
                paths: vec!["asset.png".into()],
                asset_type: None,
                creator: None,
            },
        ];

        for (index, operation) in unsafe_operations.into_iter().enumerate() {
            let mut step = minimal_step(&format!("step{index}"), operation);
            step.transaction = Some("atomic".into());
            let mut workflow = workflow(vec![step]);
            workflow.transactions = vec![TransactionGroup {
                id: "atomic".into(),
                atomic: true,
            }];
            let errors = workflow.validate().unwrap_err();
            assert!(
                errors
                    .issues
                    .iter()
                    .any(|issue| issue.code == "unsafe_atomic_operation"),
                "operation was not rejected: {:?}",
                workflow.steps[0].operation
            );
        }
    }

    #[test]
    fn unsafe_operations_are_allowed_in_non_atomic_groups() {
        let mut step = minimal_step(
            "run",
            StepOperation::Eval {
                source: "return true".into(),
            },
        );
        step.transaction = Some("sequence".into());
        let mut workflow = workflow(vec![step]);
        workflow.transactions = vec![TransactionGroup {
            id: "sequence".into(),
            atomic: false,
        }];
        workflow.validate().unwrap();
    }

    #[test]
    fn atomic_members_must_be_contiguous() {
        let mut first = minimal_step(
            "one",
            StepOperation::Set {
                path: "Workspace/A".into(),
                property: "Name".into(),
                value: json!("A1"),
            },
        );
        first.transaction = Some("atomic".into());
        let middle = minimal_step(
            "middle",
            StepOperation::Get {
                path: "Workspace".into(),
                property: None,
            },
        );
        let mut last = minimal_step(
            "two",
            StepOperation::Rm {
                path: "Workspace/B".into(),
            },
        );
        last.transaction = Some("atomic".into());
        let mut workflow = workflow(vec![first, middle, last]);
        workflow.transactions = vec![TransactionGroup {
            id: "atomic".into(),
            atomic: true,
        }];
        let errors = workflow.validate().unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.code == "non_contiguous_atomic_transaction"));
    }

    #[test]
    fn validates_capture_bounds_timeout_verify_and_parent_guardrail() {
        let mut capture = minimal_step(
            "capture",
            StepOperation::Capture {
                target: CaptureTarget::Screen,
                path: None,
                context: None,
                region: Some(CaptureRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 100,
                }),
                output_size: None,
                ui: CaptureUi::None,
                output: None,
            },
        );
        capture.timeout_ms = Some(0);
        capture.verify = true;
        let set_parent = minimal_step(
            "parent",
            StepOperation::Set {
                path: "Workspace/Part".into(),
                property: "Parent".into(),
                value: json!("ServerStorage"),
            },
        );
        let errors = workflow(vec![capture, set_parent]).validate().unwrap_err();
        let codes: BTreeSet<_> = errors
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect();
        assert!(codes.contains("invalid_capture_size"));
        assert!(codes.contains("invalid_timeout"));
        assert!(codes.contains("unsupported_verification"));
        assert!(codes.contains("unsafe_parent_set"));
    }

    #[test]
    fn request_parts_preserve_nested_values() {
        let operation = StepOperation::AttrSet {
            path: "Workspace/Part".into(),
            name: "Config".into(),
            value: json!({"enabled": true}),
        };
        let (op, args) = operation.request_parts();
        assert_eq!(op, "attr-set");
        assert_eq!(args["path"], "Workspace/Part");
        assert_eq!(args["value"]["enabled"], true);
    }

    #[test]
    fn currency_is_not_a_reference_and_malformed_reference_is() {
        assert_eq!(parse_reference("$100").unwrap(), None);
        assert_eq!(
            parse_reference("$read.value").unwrap(),
            Some(JsonReference {
                step_id: "read".into(),
                path: vec!["value".into()]
            })
        );
        assert!(parse_reference("$read..value").is_err());
    }

    #[test]
    fn rejects_unknown_fields_instead_of_silently_dropping_guardrails() {
        let top_level = json!({
            "version": 1,
            "expectedPlaecId": "123",
            "steps": [{"id": "read", "op": "get", "path": "Workspace"}]
        });
        assert!(matches!(
            Workflow::from_json(&top_level.to_string()),
            Err(WorkflowParseError::Json(_))
        ));

        let step_typo = json!({
            "version": 1,
            "steps": [{
                "id": "write",
                "op": "set",
                "path": "Workspace/Part",
                "property": "Transparency",
                "value": 0.5,
                "verfiy": true
            }]
        });
        assert!(matches!(
            Workflow::from_json(&step_typo.to_string()),
            Err(WorkflowParseError::Json(_))
        ));
    }

    #[test]
    fn resolved_values_are_revalidated_before_execution() {
        let step = minimal_step(
            "write",
            StepOperation::Set {
                path: "Workspace/Part".into(),
                property: "$lookup.value".into(),
                value: json!({"__type": "Instance", "path": "ServerStorage"}),
            },
        );
        let results = BTreeMap::from([("lookup".into(), json!({"value": "Parent"}))]);
        let error = step.resolve(&results).unwrap_err();
        assert!(matches!(
            error,
            ResolveError::InvalidResolvedValidation { ref issues, .. }
                if issues.iter().any(|issue| issue.code == "unsafe_parent_set")
        ));
    }

    #[test]
    fn dynamic_set_property_is_rejected_before_transport() {
        let step = minimal_step(
            "write",
            StepOperation::Set {
                path: "Workspace/Part".into(),
                property: "$lookup.value".into(),
                value: json!(true),
            },
        );
        let errors = workflow(vec![step]).validate().unwrap_err();
        assert!(errors
            .issues
            .iter()
            .any(|issue| issue.code == "dynamic_set_property"));
    }
}
