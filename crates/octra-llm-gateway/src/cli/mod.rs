//! CLI provider transport.
//!
//! Some models are only reachable through a local agent CLI (Claude Code,
//! Codex, or a custom binary speaking Octra's generic JSONL contract). These
//! providers share routing, accounts, plugins and usage accounting with HTTP
//! providers; only the transport differs: an argv array is spawned, the
//! canonical request is rendered to a prompt, and stdout JSONL is mapped back
//! to canonical stream events.

pub mod adapter;
pub mod process;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::stream::{BoxStream, StreamExt, unfold};
use octra_config::Workspace;
use octra_llm_core::{
    AssistantContent, ChatRequest, ChatResponse, ContentPart, FinishReason, GatewayError, Message,
    Role, StreamEvent, Usage,
};
use octra_storage::LlmProviderAccountRecord;
use serde_json::Value;

use self::adapter::{AdapterFinish, CliFormat, StreamAdapter, adapter_for};
use self::process::{ChildSession, SpawnSpec};

pub const DEFAULT_ENV_ALLOW: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM", "USER", "SHELL",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    Generic,
    ClaudeCode,
    Codex,
}

impl CliKind {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "generic" => Some(Self::Generic),
            "claude" | "claude_code" | "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
        }
    }

    fn default_command(self) -> Option<&'static str> {
        match self {
            Self::Generic => None,
            Self::ClaudeCode => Some("claude"),
            Self::Codex => Some("codex"),
        }
    }

    fn default_args(self) -> &'static [&'static str] {
        match self {
            Self::Generic => &["{prompt}"],
            Self::ClaudeCode => &["-p", "--output-format", "stream-json", "--verbose"],
            Self::Codex => &["exec", "--json"],
        }
    }

    fn default_model_args(self) -> &'static [&'static str] {
        match self {
            Self::Generic => &[],
            Self::ClaudeCode => &["--model", "{model}"],
            Self::Codex => &["-m", "{model}"],
        }
    }

    fn default_format(self) -> CliFormat {
        match self {
            Self::Generic => CliFormat::Generic,
            Self::ClaudeCode => CliFormat::ClaudeCode,
            Self::Codex => CliFormat::Codex,
        }
    }

    fn config_env(self) -> &'static str {
        match self {
            Self::Generic => "OCTRA_CLI_CONFIG_DIR",
            Self::ClaudeCode => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }

    fn isolates_by_default(self) -> bool {
        !matches!(self, Self::Generic)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Arg,
    Stdin,
}

/// Provider-level CLI settings parsed from `config_json.cli`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliConfig {
    pub kind: CliKind,
    pub command: String,
    pub args: Vec<String>,
    pub model_args: Vec<String>,
    pub prompt_mode: PromptMode,
    pub format: CliFormat,
    pub env: Vec<(String, String)>,
    pub env_allow: Vec<String>,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
    pub working_dir: Option<String>,
    pub config_dir: Option<String>,
    pub isolate_config: bool,
    pub version_args: Vec<String>,
}

impl CliConfig {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "`cli` must be an object".to_string())?;
        let string = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let strings = |key: &str, default: &[&str]| -> Result<Vec<String>, String> {
            match object.get(key) {
                None => Ok(default.iter().map(|item| item.to_string()).collect()),
                Some(value) => {
                    let items = value
                        .as_array()
                        .ok_or_else(|| format!("`cli.{key}` must be an array of strings"))?;
                    let mut out = Vec::new();
                    for item in items {
                        let item = item
                            .as_str()
                            .ok_or_else(|| format!("`cli.{key}` must be an array of strings"))?;
                        out.push(item.to_string());
                    }
                    Ok(out)
                }
            }
        };

        let kind = match string("kind") {
            None => CliKind::Generic,
            Some(kind) => CliKind::parse(&kind).ok_or_else(|| {
                format!("unknown `cli.kind` '{kind}' (generic, claude_code, codex)")
            })?,
        };
        let command = match string("command").or_else(|| kind.default_command().map(str::to_string))
        {
            Some(command) => command,
            None => {
                return Err("`cli.command` is required for a generic CLI provider".to_string());
            }
        };
        if command.split_whitespace().count() != 1 {
            return Err(
                "`cli.command` must be a single executable, not a command line".to_string(),
            );
        }
        let args = strings("args", kind.default_args())?;
        let model_args = strings("model_args", kind.default_model_args())?;
        let prompt_mode = match string("prompt_mode").as_deref() {
            None | Some("arg") => PromptMode::Arg,
            Some("stdin") => PromptMode::Stdin,
            Some(other) => {
                return Err(format!(
                    "`cli.prompt_mode` must be arg or stdin, got '{other}'"
                ));
            }
        };
        let format = match string("format").as_deref() {
            None => kind.default_format(),
            Some(format) => CliFormat::parse(format)
                .ok_or_else(|| format!("unknown `cli.format` '{format}'"))?,
        };
        let env = match object.get("env") {
            None => Vec::new(),
            Some(Value::Null) => Vec::new(),
            Some(value) => value
                .as_object()
                .ok_or_else(|| "`cli.env` must be an object of strings".to_string())?
                .iter()
                .map(|(key, value)| match value.as_str() {
                    Some(value) => Ok((key.clone(), value.to_string())),
                    None => Err(format!("`cli.env.{key}` must be a string")),
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let env_allow = match object.get("env_allow") {
            None => DEFAULT_ENV_ALLOW
                .iter()
                .map(|item| item.to_string())
                .collect(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| "`cli.env_allow` must be an array of strings".to_string())?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "`cli.env_allow` must be an array of strings".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let timeout_secs = integer(object, "timeout_secs")?.unwrap_or(300);
        if !(1..=3600).contains(&timeout_secs) {
            return Err("`cli.timeout_secs` must be between 1 and 3600".to_string());
        }
        let max_output_bytes = integer(object, "max_output_bytes")?.unwrap_or(1_048_576) as usize;
        if !(1024..=64 * 1024 * 1024).contains(&max_output_bytes) {
            return Err("`cli.max_output_bytes` must be between 1024 and 67108864".to_string());
        }
        let isolate_config = object
            .get("isolate_config")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| kind.isolates_by_default());
        let version_args = strings("version_args", &["--version"])?;

        Ok(Self {
            kind,
            command,
            args,
            model_args,
            prompt_mode,
            format,
            env,
            env_allow,
            timeout_secs,
            max_output_bytes,
            working_dir: string("working_dir"),
            config_dir: string("config_dir"),
            isolate_config,
            version_args,
        })
    }

    pub fn allows_partial_messages(&self) -> bool {
        self.args
            .iter()
            .any(|arg| arg == "--include-partial-messages")
    }
}

fn integer(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`cli.{key}` must be a positive integer")),
    }
}

/// Per-account CLI overrides parsed from the account's `config_json.cli`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliAccountConfig {
    pub working_dir: Option<String>,
    pub config_dir: Option<String>,
    pub env: Vec<(String, String)>,
}

impl CliAccountConfig {
    pub fn from_record(record: &LlmProviderAccountRecord) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(&record.config_json) else {
            return Self::default();
        };
        let Some(object) = value.get("cli").and_then(Value::as_object) else {
            return Self::default();
        };
        let string = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let env = object
            .get("env")
            .and_then(Value::as_object)
            .map(|env| {
                env.iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            working_dir: string("working_dir"),
            config_dir: string("config_dir"),
            env,
        }
    }
}

/// Renders the canonical request as one plain-text prompt. CLI agents are
/// single-turn text endpoints here: tool definitions cannot be honored.
pub fn render_prompt(request: &ChatRequest) -> Result<String, GatewayError> {
    if !request.tools.is_empty() {
        return Err(
            GatewayError::invalid_request("CLI providers do not support tool definitions")
                .with_code("cli_unsupported"),
        );
    }
    let mut out = String::new();
    for block in &request.system.blocks {
        if !block.trim().is_empty() {
            out.push_str("System:\n");
            out.push_str(block.trim_end());
            out.push_str("\n\n");
        }
    }
    for message in &request.messages {
        let mut body = String::new();
        if let Some(content) = &message.content {
            for part in &content.parts {
                match part {
                    ContentPart::Text { text, .. } => {
                        body.push_str(text);
                        body.push('\n');
                    }
                    ContentPart::Reasoning { text, .. } => {
                        body.push_str(text);
                        body.push('\n');
                    }
                    ContentPart::Image(_) | ContentPart::File(_) => {
                        return Err(GatewayError::invalid_request(
                            "CLI providers do not support images or file attachments",
                        )
                        .with_code("cli_unsupported"));
                    }
                    ContentPart::Unsupported { .. } => {}
                }
            }
        }
        for block in &message.assistant {
            match block {
                AssistantContent::Text { text } => {
                    body.push_str(text);
                    body.push('\n');
                }
                AssistantContent::Reasoning { text, .. } => {
                    body.push_str(text);
                    body.push('\n');
                }
                AssistantContent::Refusal { text } => {
                    body.push_str(text);
                    body.push('\n');
                }
                AssistantContent::ToolCall {
                    name, arguments, ..
                } => {
                    body.push_str(&format!("[tool call: {name} {arguments}]\n"));
                }
                AssistantContent::ToolResult { content, .. } => {
                    body.push_str(&format!("[tool result: {content}]\n"));
                }
                AssistantContent::Raw { .. } => {}
            }
        }
        for result in &message.tool_results {
            let text = result
                .content
                .parts
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join("\n");
            body.push_str(&format!("[tool result {}: {text}]\n", result.tool_call_id));
        }
        if !body.trim().is_empty() {
            out.push_str(message.role.as_str());
            out.push_str(":\n");
            out.push_str(body.trim_end());
            out.push_str("\n\n");
        }
    }
    Ok(out.trim_end().to_string())
}

/// One resolved invocation.
struct Invocation {
    spec: SpawnSpec,
    format: CliFormat,
    partial: bool,
}

/// Executes a request through the configured CLI binary.
#[derive(Debug, Clone)]
pub struct CliRunner {
    config: CliConfig,
}

impl CliRunner {
    pub fn new(config: CliConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &CliConfig {
        &self.config
    }

    fn invocation(
        &self,
        request: &ChatRequest,
        account: Option<&LlmProviderAccountRecord>,
    ) -> Result<Invocation, GatewayError> {
        let prompt = render_prompt(request)?;
        let account_config = account
            .map(CliAccountConfig::from_record)
            .unwrap_or_default();
        let model = request.model.as_str();

        let mut args: Vec<String> = self
            .config
            .args
            .iter()
            .map(|arg| arg.replace("{model}", model))
            .collect();
        let mut prompt_in_args = false;
        for arg in &mut args {
            if arg.contains("{prompt}") {
                prompt_in_args = true;
                *arg = arg.replace("{prompt}", &prompt);
            }
        }
        args.extend(
            self.config
                .model_args
                .iter()
                .map(|arg| arg.replace("{model}", model).replace("{prompt}", &prompt)),
        );
        let stdin = match self.config.prompt_mode {
            PromptMode::Stdin => Some(prompt.clone()),
            PromptMode::Arg => {
                if !prompt_in_args {
                    args.push(prompt.clone());
                }
                None
            }
        };

        let mut env = Vec::new();
        for key in &self.config.env_allow {
            if let Ok(value) = std::env::var(key) {
                env.push((key.clone(), value));
            }
        }
        env.extend(self.config.env.iter().cloned());
        env.extend(account_config.env.iter().cloned());

        let account_id = account
            .map(|record| record.id.as_str())
            .unwrap_or("default");
        let config_dir = account_config
            .config_dir
            .clone()
            .or_else(|| {
                self.config
                    .config_dir
                    .as_ref()
                    .map(|dir| dir.replace("{account_id}", account_id))
            })
            .or_else(|| {
                self.config.isolate_config.then(|| {
                    Workspace::resolve()
                        .map(|workspace| {
                            workspace
                                .home
                                .join("llm-cli")
                                .join(self.config.kind.as_str())
                                .join(account_id)
                                .to_string_lossy()
                                .to_string()
                        })
                        .unwrap_or_else(|_| {
                            std::env::temp_dir()
                                .join("octra-llm-cli")
                                .join(self.config.kind.as_str())
                                .join(account_id)
                                .to_string_lossy()
                                .to_string()
                        })
                })
            });
        if let Some(dir) = &config_dir {
            env.push((self.config.kind.config_env().to_string(), dir.clone()));
        }

        let working_dir = account_config
            .working_dir
            .clone()
            .or_else(|| self.config.working_dir.clone())
            .map(PathBuf::from);

        Ok(Invocation {
            spec: SpawnSpec {
                command: self.config.command.clone(),
                args,
                env,
                working_dir,
                stdin,
                timeout: Duration::from_secs(self.config.timeout_secs),
                max_output_bytes: self.config.max_output_bytes,
            },
            format: self.config.format,
            partial: self.config.allows_partial_messages(),
        })
    }

    /// Streams canonical events from the CLI process.
    pub fn stream(
        &self,
        request: ChatRequest,
        account: Option<&LlmProviderAccountRecord>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, GatewayError>>, GatewayError> {
        let invocation = self.invocation(&request, account)?;
        let start = StreamEvent::MessageStart {
            id: format!("chatcmpl-cli-{}", uuid::Uuid::new_v4().simple()),
            model: request.model.clone(),
        };
        let pending = VecDeque::from([Ok(start)]);
        let state = EventStream {
            command: self.config.command.clone(),
            invocation: Some(invocation),
            session: None,
            adapter: None,
            pending,
            finished: false,
        };
        Ok(unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        })
        .boxed())
    }

    /// Runs the CLI to completion and returns the assembled response.
    pub async fn execute(
        &self,
        request: ChatRequest,
        account: Option<&LlmProviderAccountRecord>,
    ) -> Result<ChatResponse, GatewayError> {
        let model = request.model.clone();
        let mut stream = self.stream(request, account)?;
        let mut text = String::new();
        let mut usage = Usage::default();
        let mut finish_reason = FinishReason::Stop;
        let mut id = format!("chatcmpl-cli-{}", uuid::Uuid::new_v4().simple());
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::MessageStart { id: start_id, .. } => id = start_id,
                StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
                StreamEvent::Usage(found) => usage = found,
                StreamEvent::Finish { reason } => finish_reason = reason,
                StreamEvent::Error(error) => return Err(error),
                _ => {}
            }
        }
        let mut message = Message::new(Role::Assistant);
        message.assistant.push(AssistantContent::Text { text });
        let mut response = ChatResponse::new(id, model, message);
        response.finish_reason = finish_reason;
        response.usage = usage;
        Ok(response)
    }
}

struct EventStream {
    command: String,
    invocation: Option<Invocation>,
    session: Option<ChildSession>,
    adapter: Option<Box<dyn StreamAdapter>>,
    pending: VecDeque<Result<StreamEvent, GatewayError>>,
    finished: bool,
}

impl EventStream {
    async fn next_event(&mut self) -> Option<Result<StreamEvent, GatewayError>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            if self.session.is_none() {
                let invocation = self.invocation.take().expect("invocation is consumed once");
                match ChildSession::spawn(&invocation.spec) {
                    Ok(session) => {
                        self.session = Some(session);
                        self.adapter = Some(adapter_for(invocation.format, invocation.partial));
                    }
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error));
                    }
                }
            }

            let line = match self
                .session
                .as_mut()
                .expect("session just started")
                .next_line()
                .await
            {
                Ok(line) => line,
                Err(error) => {
                    self.finished = true;
                    self.pending.push_back(Err(error));
                    continue;
                }
            };
            match line {
                Some(line) => {
                    let adapter = self.adapter.as_mut().expect("adapter just started");
                    let events = adapter.push_line(&line);
                    self.pending.extend(events.into_iter().map(Ok));
                }
                None => self.finish().await,
            }
        }
    }

    async fn finish(&mut self) {
        self.finished = true;
        let exit = match self.session.as_mut() {
            Some(session) => session.finish().await,
            None => return,
        };
        let finish = match self.adapter.as_mut() {
            Some(adapter) => adapter.finish(),
            None => AdapterFinish::default(),
        };
        let AdapterFinish {
            usage,
            finish_reason,
            error,
            terminal,
            ..
        } = finish;

        if let Some(error) = error {
            self.pending.push_back(Err(error));
            return;
        }
        match exit {
            Err(error) => self.pending.push_back(Err(error)),
            Ok(exit) if !exit.success && !terminal => {
                let mut message = format!(
                    "CLI '{}' failed with status {}",
                    self.command,
                    exit.code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_string())
                );
                if !exit.stderr.is_empty() {
                    message.push_str(": ");
                    message.push_str(&exit.stderr);
                }
                self.pending.push_back(Err(
                    GatewayError::upstream(502, message).with_code("cli_failed")
                ));
            }
            Ok(_) => {
                if !usage.is_empty() {
                    self.pending.push_back(Ok(StreamEvent::Usage(usage)));
                }
                self.pending.push_back(Ok(StreamEvent::Finish {
                    reason: finish_reason,
                }));
            }
        }
    }
}

/// Validates a CLI binary through the control plane before enabling it.
pub async fn probe(config: &CliConfig) -> Result<String, String> {
    process::probe(
        &config.command,
        &config.version_args,
        Duration::from_secs(10),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(messages: Vec<Message>) -> ChatRequest {
        ChatRequest::new("m", messages)
    }

    #[test]
    fn generic_requires_a_command() {
        assert!(CliConfig::from_value(&json!({})).is_err());
        let config = CliConfig::from_value(&json!({"command": "/bin/echo"})).unwrap();
        assert_eq!(config.kind, CliKind::Generic);
        assert_eq!(config.args, vec!["{prompt}"]);
        assert_eq!(config.timeout_secs, 300);
        assert!(!config.isolate_config);
    }

    #[test]
    fn claude_defaults_are_applied() {
        let config = CliConfig::from_value(&json!({"kind": "claude_code"})).unwrap();
        assert_eq!(config.command, "claude");
        assert_eq!(config.format, CliFormat::ClaudeCode);
        assert!(config.isolate_config);
        assert_eq!(config.model_args, vec!["--model", "{model}"]);
        assert!(!config.allows_partial_messages());

        let partial = CliConfig::from_value(&json!({
            "kind": "claude_code",
            "args": ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]
        }))
        .unwrap();
        assert!(partial.allows_partial_messages());
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(CliConfig::from_value(&json!({"kind": "nope", "command": "x"})).is_err());
        assert!(CliConfig::from_value(&json!({"command": "a b"})).is_err());
        assert!(CliConfig::from_value(&json!({"command": "x", "timeout_secs": 0})).is_err());
        assert!(CliConfig::from_value(&json!({"command": "x", "max_output_bytes": 10})).is_err());
        assert!(CliConfig::from_value(&json!({"command": "x", "prompt_mode": "pipe"})).is_err());
    }

    #[test]
    fn prompt_renders_roles_and_system() {
        let mut request = request(vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: None,
                assistant: vec![AssistantContent::Text {
                    text: "hi there".to_string(),
                }],
                tool_results: Vec::new(),
                name: None,
                extra: serde_json::Map::new(),
            },
        ]);
        request.system = octra_llm_core::SystemPrompt::text("be brief");
        let prompt = render_prompt(&request).unwrap();
        assert_eq!(
            prompt,
            "System:\nbe brief\n\nuser:\nhello\n\nassistant:\nhi there"
        );
    }

    #[test]
    fn prompt_rejects_tools_and_images() {
        let mut with_tools = request(vec![Message::user("hello")]);
        with_tools.tools.push(octra_llm_core::Tool {
            kind: octra_llm_core::ToolCallKind::Function,
            name: None,
            function: Some(octra_llm_core::FunctionDefinition {
                name: "f".to_string(),
                description: None,
                parameters: None,
                strict: None,
            }),
            provider_type: None,
            provider_config: None,
            extra: serde_json::Map::new(),
        });
        assert!(render_prompt(&with_tools).is_err());

        let image = Message::user_parts(vec![ContentPart::Image(octra_llm_core::Attachment {
            media_type: "image/png".to_string(),
            data: None,
            url: Some("https://example.test/a.png".to_string()),
            name: None,
            extra: serde_json::Map::new(),
        })]);
        assert!(render_prompt(&request(vec![image])).is_err());
    }

    #[test]
    fn account_overrides_are_parsed() {
        let record = LlmProviderAccountRecord {
            id: "a1".into(),
            provider_id: "p".into(),
            name: "a1".into(),
            secret_ref: None,
            weight: 1,
            priority: 0,
            enabled: true,
            quota_limit: None,
            quota_unit: "requests".into(),
            quota_period: "none".into(),
            quota_used: 0,
            quota_reset_at: None,
            cooldown_until: None,
            last_error: None,
            config_json:
                r#"{"cli":{"working_dir":"/tmp/w","config_dir":"/tmp/c","env":{"TOKEN":"t"}}}"#
                    .into(),
            credential_kind: "static".into(),
            oauth_status: "none".into(),
            oauth_expires_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let overrides = CliAccountConfig::from_record(&record);
        assert_eq!(overrides.working_dir.as_deref(), Some("/tmp/w"));
        assert_eq!(overrides.config_dir.as_deref(), Some("/tmp/c"));
        assert_eq!(overrides.env, vec![("TOKEN".to_string(), "t".to_string())]);
    }
}
