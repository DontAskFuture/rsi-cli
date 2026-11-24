use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Supported target repository languages for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoLanguage {
    Auto,
    Python,
    #[serde(rename = "typescript", alias = "ts", alias = "type_script")]
    TypeScript,
}

impl Default for RepoLanguage {
    fn default() -> Self {
        Self::Auto
    }
}

impl std::fmt::Display for RepoLanguage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Python => write!(f, "python"),
            Self::TypeScript => write!(f, "typescript"),
        }
    }
}

/// How the winning artifact is published when a run completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishMode {
    BundleOnly,
    Branch,
    Tag,
    #[serde(rename = "pull_request", alias = "pr")]
    PullRequest,
    #[serde(rename = "zero_pr", alias = "direct", alias = "trunk")]
    ZeroPr,
}

impl Default for PublishMode {
    fn default() -> Self {
        Self::BundleOnly
    }
}

impl std::fmt::Display for PublishMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::BundleOnly => "bundle_only",
            Self::Branch => "branch",
            Self::Tag => "tag",
            Self::PullRequest => "pull_request",
            Self::ZeroPr => "zero_pr",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub url: String,
    #[serde(rename = "ref", default = "default_repo_ref")]
    pub ref_name: String,
    #[serde(default)]
    pub publish_on_complete: PublishMode,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub github_repo: Option<String>,
    #[serde(default)]
    pub auto_merge_pr: bool,
    #[serde(default)]
    pub force_push: bool,
}

fn default_repo_ref() -> String {
    "HEAD".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpecConfig {
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub inline: Option<String>,
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(default)]
    pub protected_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    Maximize,
    Minimize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CmpOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BreachAction {
    RevertStep,
    RevertAndAbort,
}

impl Default for BreachAction {
    fn default() -> Self {
        Self::RevertStep
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapperKind {
    PythonFn,
    TsFn,
    CliReplay,
    HttpReplay,
}

impl std::fmt::Display for WrapperKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::PythonFn => "python_fn",
            Self::TsFn => "ts_fn",
            Self::CliReplay => "cli_replay",
            Self::HttpReplay => "http_replay",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquivalenceMode {
    ExactOutput,
    JsonSubset,
    StatusOnly,
    None,
}

impl Default for EquivalenceMode {
    fn default() -> Self {
        Self::ExactOutput
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinMetric {
    OpsPerSec,
    P50Ms,
    P95Ms,
    PeakMemMb,
}

impl BuiltinMetric {
    pub fn json_key(self) -> &'static str {
        match self {
            Self::OpsPerSec => "ops_per_sec",
            Self::P50Ms => "p50_ms",
            Self::P95Ms => "p95_ms",
            Self::PeakMemMb => "peak_mem_mb",
        }
    }

    pub fn default_direction(self) -> Direction {
        match self {
            Self::OpsPerSec => Direction::Maximize,
            Self::P50Ms | Self::P95Ms | Self::PeakMemMb => Direction::Minimize,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalWrapperConfig {
    pub kind: WrapperKind,
    pub target: String,
    pub inputs: PathBuf,
    #[serde(default)]
    pub equivalence: EquivalenceMode,
    #[serde(default = "default_warmup_iters")]
    pub warmup_iters: usize,
    #[serde(default = "default_measure_iters")]
    pub measure_iters: usize,
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u64,
    #[serde(default = "default_healthcheck_path")]
    pub healthcheck_path: String,
    #[serde(default = "default_startup_timeout_sec")]
    pub startup_timeout_sec: u64,
}

fn default_warmup_iters() -> usize {
    3
}

fn default_measure_iters() -> usize {
    25
}

fn default_healthcheck_path() -> String {
    "/".to_string()
}

fn default_startup_timeout_sec() -> u64 {
    15
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalConfig {
    #[serde(default)]
    pub wrapper: Option<EvalWrapperConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerMetric {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default = "default_expect_exit_code")]
    pub expect_exit_code: Option<i32>,
    #[serde(default)]
    pub extract_json_path: Option<String>,
    #[serde(default)]
    pub extract_regex: Option<String>,
    #[serde(default)]
    pub operator: Option<CmpOp>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u64,
    #[serde(default)]
    pub on_breach: BreachAction,
}

fn default_expect_exit_code() -> Option<i32> {
    Some(0)
}

fn default_timeout_sec() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectiveMetric {
    pub id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub builtin: Option<BuiltinMetric>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub extract_json_path: Option<String>,
    #[serde(default)]
    pub extract_regex: Option<String>,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub target: Option<f64>,
    #[serde(default)]
    pub target_ratio: Option<f64>,
    #[serde(default)]
    pub min_delta: f64,
    #[serde(default)]
    pub min_delta_ratio: Option<f64>,
    #[serde(default)]
    pub max_regression_tolerance: Option<f64>,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default = "default_trials")]
    pub trials: usize,
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u64,
}

impl ObjectiveMetric {
    pub fn effective_direction(&self) -> Direction {
        self.direction.unwrap_or_else(|| {
            self.builtin
                .map(|b| b.default_direction())
                .unwrap_or(Direction::Maximize)
        })
    }
}

fn default_weight() -> f64 {
    1.0
}

fn default_trials() -> usize {
    3
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsConfig {
    #[serde(default)]
    pub blockers: Vec<BlockerMetric>,
    #[serde(default)]
    pub objectives: Vec<ObjectiveMetric>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionPolicy {
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    #[serde(default = "default_max_consecutive_reverts")]
    pub max_consecutive_reverts: usize,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
}

impl Default for EvolutionPolicy {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            max_consecutive_reverts: default_max_consecutive_reverts(),
            max_budget_usd: None,
        }
    }
}

fn default_max_steps() -> usize {
    10
}

fn default_max_consecutive_reverts() -> usize {
    5
}

/// Customer-provided coding agent subscription or LLM API driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    Auto,
    AgyCli,
    ClaudeCli,
    CodexCli,
    GeminiCli,
    AnthropicApi,
    #[serde(rename = "openai_api", alias = "open_ai_api")]
    OpenAiApi,
    GeminiApi,
    Command,
}

impl Default for DriverKind {
    fn default() -> Self {
        Self::Auto
    }
}

impl std::fmt::Display for DriverKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Auto => "auto",
            Self::AgyCli => "agy_cli",
            Self::ClaudeCli => "claude_cli",
            Self::CodexCli => "codex_cli",
            Self::GeminiCli => "gemini_cli",
            Self::AnthropicApi => "anthropic_api",
            Self::OpenAiApi => "openai_api",
            Self::GeminiApi => "gemini_api",
            Self::Command => "command",
        };
        write!(f, "{label}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub driver: DriverKind,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            driver: DriverKind::Auto,
            model: None,
            command: None,
            max_turns: default_max_turns(),
        }
    }
}

fn default_max_turns() -> usize {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerConfig {
    Cron {
        schedule: String,
        #[serde(default)]
        interval_sec: Option<u64>,
    },
    Webhook {
        #[serde(default)]
        path: Option<String>,
    },
    GithubIssue {
        label: String,
        #[serde(default)]
        mode: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationEvent {
    GoalReached,
    MaxStepsReached,
    AbortedOnBlocker,
    Stalled,
    StepPromoted,
    StepReverted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationChannel {
    SlackWebhook { url: String },
    Webhook { url: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationConfig {
    #[serde(default)]
    pub events: Vec<NotificationEvent>,
    #[serde(default)]
    pub channels: Vec<NotificationChannel>,
}

/// Top-level declarative `rsi.yaml` / `rsi.toml` / `rsi.json` configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RsiConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub language: RepoLanguage,
    pub repo: RepoConfig,
    #[serde(default)]
    pub spec: SpecConfig,
    #[serde(default)]
    pub eval: EvalConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub evolution: EvolutionPolicy,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub triggers: Vec<TriggerConfig>,
    #[serde(default)]
    pub notifications: NotificationConfig,
}

fn default_version() -> u32 {
    1
}

/// Immutable snapshot of behavioral spec and protected checksums for an evolution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecRevision {
    pub id: String,
    pub intent_markdown: String,
    pub invariants: Vec<String>,
    pub protected_paths: Vec<String>,
    pub protected_paths_sha256: BTreeMap<String, String>,
    pub feature_amendments: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    Running,
    GoalReached,
    MaxStepsReached,
    AbortedOnBlocker,
    Stalled,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Running => "RUNNING",
            Self::GoalReached => "GOAL_REACHED",
            Self::MaxStepsReached => "MAX_STEPS_REACHED",
            Self::AbortedOnBlocker => "ABORTED_ON_BLOCKER",
            Self::Stalled => "STALLED",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepOutcome {
    Promoted,
    RevertedBlocker,
    RevertedObjective,
    AbortedSecurity,
    NoChanges,
}

impl std::fmt::Display for StepOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Promoted => "PROMOTED",
            Self::RevertedBlocker => "REVERTED_BLOCKER",
            Self::RevertedObjective => "REVERTED_OBJECTIVE",
            Self::AbortedSecurity => "ABORTED_SECURITY",
            Self::NoChanges => "NO_CHANGES",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevertReason {
    TamperDetected,
    BlockerBreach,
    ObjectiveRegression,
    InsufficientDelta,
    AgentFailed,
    NoChanges,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertRecord {
    pub reverted_at: String,
    pub reason: RevertReason,
    pub failed_metric_id: Option<String>,
    pub observed_value: Option<f64>,
    pub expected_or_baseline: Option<f64>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerResult {
    pub metric_id: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub observed_value: Option<f64>,
    pub duration_ms: u64,
    pub output_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectiveMeasurement {
    pub metric_id: String,
    pub trial_values: Vec<f64>,
    pub median: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffStats {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactSnapshot {
    pub id: String,
    pub run_id: String,
    pub git_ref: String,
    pub commit_sha: String,
    pub tree_sha256: String,
    pub parent_artifact_id: Option<String>,
    pub diff_stats: DiffStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepAttempt {
    pub id: String,
    pub run_id: String,
    pub step_number: usize,
    pub parent_artifact_id: String,
    pub candidate_artifact_id: Option<String>,
    pub hypothesis: String,
    pub outcome: StepOutcome,
    pub blocker_results: Vec<BlockerResult>,
    pub objective_measurements: Vec<ObjectiveMeasurement>,
    pub revert_record: Option<RevertRecord>,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRun {
    pub id: String,
    pub name: String,
    pub repo_url: String,
    pub language: RepoLanguage,
    pub base_sha: String,
    pub spec_revision_id: String,
    pub best_artifact_id: String,
    pub best_commit_sha: String,
    pub current_step: usize,
    pub max_steps: usize,
    pub status: RunStatus,
    pub baseline_scorecard: BTreeMap<String, f64>,
    pub best_scorecard: BTreeMap<String, f64>,
    pub total_cost_usd: f64,
    pub published_ref: Option<String>,
    #[serde(default)]
    pub published_url: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionMemory {
    pub id: String,
    pub run_id: String,
    pub step_number: usize,
    pub outcome: StepOutcome,
    pub hypothesis: String,
    pub diff_summary: String,
    pub metric_deltas: BTreeMap<String, f64>,
    pub lesson: String,
}
