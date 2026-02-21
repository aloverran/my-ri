use clap::Parser;
use color_eyre::eyre::Result;
use ri::{SessionStore, ThinkingLevel};
use std::collections::HashSet;

mod agent;
mod interactive;
mod print_mode;
mod rpc_mode;

#[derive(Parser)]
#[command(name = "ri", about = "A Rust coding agent")]
struct Cli {
    #[arg(long, default_value = "interactive")]
    mode: String,

    #[arg(long)]
    model: Option<String>,

    #[arg(short, long)]
    prompt: Option<String>,

    #[arg(short = 'C', long)]
    cwd: Option<String>,

    #[arg(long, default_value = "text")]
    output: String,

    #[arg(long)]
    thinking: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let cwd = cli.cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .expect("could not determine current directory")
            .display()
            .to_string()
    });

    tracing::info!("ri starting in {}", cwd);

    let cwd_path = std::path::PathBuf::from(&cwd);
    let settings = ri_tools::resources::load_settings();
    let context_files = ri_tools::resources::discover_context_files(&cwd_path);

    let model_id = cli.model
        .or_else(|| settings.default_model.clone())
        .unwrap_or_else(|| ri_ai::registry::default_model_id().to_string());

    let (provider, model) = ri_ai::registry::resolve(&model_id).await?;
    let system_prompt = ri_tools::resources::build_system_prompt(&context_files);
    let tools = ri_tools::all_tools();

    // Resolve thinking level: CLI flag > settings > default (medium).
    let thinking = resolve_thinking(
        cli.thinking.as_deref(),
        settings.default_thinking.as_deref(),
    );

    let mut templates = Vec::new();
    if let Some(global) = ri_tools::resources::config_dir() {
        templates.extend(ri_tools::prompts::load_templates(&global.join("prompts")));
    }
    {
        let mut dir = cwd_path.canonicalize().ok().or(Some(cwd_path.clone()));
        while let Some(d) = dir {
            templates.extend(ri_tools::prompts::load_templates(&d.join(".agents").join("prompts")));
            if d.join(".git").exists() { break; }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }

    match cli.mode.as_str() {
        "print" | "json" => {
            use futures::StreamExt;

            let raw_prompt = cli.prompt
                .ok_or_else(|| eyre::eyre!("Print mode requires --prompt (-p)"))?;
            let prompt = match ri_tools::prompts::parse_command(&raw_prompt) {
                Some(cmd) => {
                    match templates.iter().rfind(|t| t.name == cmd.name) {
                        Some(t) => {
                            let args: Vec<&str> = cmd.args_str.split_whitespace().collect();
                            ri_tools::prompts::substitute_args(&t.content, &args)
                        }
                        None => raw_prompt,
                    }
                }
                None => raw_prompt,
            };

            let is_json = cli.mode == "json" || cli.output == "json";

            let (mut store, mut message_ids) = SessionStore::init("print", &cwd_path, &system_prompt)?;

            let cancel = tokio_util::sync::CancellationToken::new();
            let mut seen_agents = HashSet::new();
            let handler: fn(&agent::AgentEvent) = if is_json {
                print_mode::on_event_json
            } else {
                print_mode::on_event_text
            };

            let events = agent::submit(
                &prompt, provider.as_ref(), &model, &system_prompt, &tools,
                &mut store, &mut message_ids, &cwd_path, thinking, &mut seen_agents, cancel,
            )?;
            tokio::pin!(events);
            while let Some(evt) = events.next().await {
                handler(&evt);
            }
            println!();
        }
        "rpc" => {
            rpc_mode::run(provider, model, system_prompt, tools, cwd_path, cli.prompt, thinking).await;
        }
        "interactive" => {
            let seen_agents = HashSet::new();
            interactive::run(provider, model, system_prompt, tools, cwd_path, cli.prompt, thinking, seen_agents).await?;
        }
        other => {
            eyre::bail!("Unknown mode '{}'. Expected: interactive, print, json, rpc", other);
        }
    }

    Ok(())
}

fn resolve_thinking(cli_flag: Option<&str>, settings: Option<&str>) -> ThinkingLevel {
    let raw = cli_flag.or(settings).unwrap_or("medium");
    match raw {
        "off" => ThinkingLevel::Off,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        other => {
            eprintln!("Unknown thinking level '{}', using medium", other);
            ThinkingLevel::Medium
        }
    }
}
