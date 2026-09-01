use std::{path::PathBuf, sync::Arc, time::Duration};

use clap::Parser;
use yeux_protocol::{ProviderCapabilities, TokenBudget};
use yeux_runtime::{
    OpenAiCompatibleProvider, ProviderConfig, WORKSPACE_SEARCH_DEFAULT_OPERATION_BUDGET,
};
use yeuxd::{
    runner::{AgentLoopLimits, ModelProviderConfig},
    Daemon, DaemonConfig,
};

#[derive(Debug, Parser)]
#[command(name = "yeuxd", version, about = "YeuX Harness local daemon")]
struct Cli {
    /// Serve newline-delimited JSON-RPC over stdin/stdout.
    #[arg(long, conflicts_with = "socket")]
    stdio: bool,

    /// Serve newline-delimited JSON-RPC over this Unix socket.
    #[cfg(unix)]
    #[arg(long, value_name = "PATH", conflicts_with = "stdio")]
    socket: Option<PathBuf>,

    /// Directory containing the event ledger and artifacts.
    #[arg(long, env = "YEUX_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// Base URL of a no-auth OpenAI-compatible API, for example http://127.0.0.1:11434/v1/.
    #[arg(long, env = "YEUX_PROVIDER_BASE_URL", value_name = "URL")]
    provider_base_url: Option<url::Url>,

    /// Stable provider identifier recorded in model requests.
    #[arg(long, env = "YEUX_PROVIDER_ID", default_value = "openai-compatible")]
    provider_id: String,

    /// Model name sent to the configured provider.
    #[arg(long, env = "YEUX_MODEL", value_name = "MODEL")]
    model: Option<String>,

    /// Maximum input-token budget advertised to the provider.
    #[arg(long, default_value_t = 65_536)]
    max_input_tokens: u64,

    /// Maximum generated tokens for one turn.
    #[arg(long, default_value_t = 4_096)]
    max_output_tokens: u64,

    /// Disable the built-in structured workspace read tools for providers that do not support
    /// OpenAI-compatible tool calling.
    #[arg(long, env = "YEUX_DISABLE_PROVIDER_TOOLS")]
    disable_provider_tools: bool,

    /// Maximum provider requests in one agent turn, including the final answer request.
    #[arg(long, default_value_t = 8)]
    max_model_rounds: usize,

    /// Maximum structured tool calls across one agent turn.
    #[arg(long, default_value_t = 32)]
    max_tool_calls: usize,

    /// Maximum serialized tool-result bytes fed back to the provider in one turn.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_tool_result_bytes: usize,

    /// Maximum aggregate matcher operations consumed by workspace.search in one turn.
    /// Values above the runtime hard ceiling are rejected; lower values fail closed sooner.
    #[arg(long, default_value_t = WORKSPACE_SEARCH_DEFAULT_OPERATION_BUDGET)]
    max_search_operations: u64,

    /// Disable background turn execution for deterministic protocol fixtures.
    #[arg(long, hide = true)]
    no_execute_turns: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut config = DaemonConfig::new(cli.state_dir)?;
    if cli.no_execute_turns {
        config = config.without_turn_execution();
    }
    match (cli.provider_base_url, cli.model) {
        (Some(base_url), Some(model)) => {
            let capabilities = ProviderCapabilities {
                tool_calls: !cli.disable_provider_tools,
                parallel_tool_calls: !cli.disable_provider_tools,
                max_context_tokens: cli.max_input_tokens,
                ..ProviderCapabilities::default()
            };
            let provider = OpenAiCompatibleProvider::without_credentials(ProviderConfig {
                provider_id: cli.provider_id,
                base_url,
                credential_handle: None,
                organization: None,
                timeout: Duration::from_secs(120),
                capabilities,
            })?;
            config = config.with_model_provider(
                ModelProviderConfig::new(
                    Arc::new(provider),
                    model,
                    TokenBudget {
                        max_input_tokens: cli.max_input_tokens,
                        max_output_tokens: cli.max_output_tokens,
                    },
                )
                .with_loop_limits(AgentLoopLimits {
                    max_model_rounds: cli.max_model_rounds,
                    max_tool_calls: cli.max_tool_calls,
                    max_tool_result_bytes: cli.max_tool_result_bytes,
                    max_search_operations: cli.max_search_operations,
                }),
            );
        }
        (None, None) => {}
        _ => {
            return Err("--provider-base-url and --model must be provided together".into());
        }
    }
    let daemon = Daemon::open(config)?;

    #[cfg(unix)]
    if let Some(socket) = cli.socket {
        return daemon.serve_unix(socket).await.map_err(Into::into);
    }

    daemon.serve_stdio().await.map_err(Into::into)
}
