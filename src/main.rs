use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod config;
mod db;
mod links;
mod passwd;
mod paste;
mod server;
mod templates;
mod watcher;
mod shortcode;
mod wordlist;

#[derive(Parser)]
#[command(name = "zigi", version, about = "zigi.zip file upload service")]
struct Cli {
    #[arg(long, env = "ZIGI_CONFIG", default_value = "~/zigi-data/config.toml")]
    config: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (default if no subcommand given)
    Serve,
    /// Manage upload passwords
    Passwd {
        #[command(subcommand)]
        action: PasswdAction,
    },
    /// Run database migrations
    Migrate,
}

#[derive(Subcommand)]
enum PasswdAction {
    /// Add a new password (label must be unique). If no password is provided, generates a two-word one.
    Add {
        label: String,
        /// Provide password non-interactively
        #[arg(long)]
        password: Option<String>,
        /// Generate a two-word password and print it
        #[arg(long)]
        generate: bool,
    },
    /// List passwords (labels, created/revoked timestamps)
    List,
    /// Revoke a password by label
    Revoke { label: String },
    /// Promote a password to admin
    Promote { label: String },
    /// Demote a password (remove admin)
    Demote { label: String },
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(stripped) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(p)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let cli = Cli::parse();
    let config_path = expand_tilde(&cli.config);
    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let pool = db::connect(&cfg.database_url).await?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => server::run(cfg, pool).await?,
        Command::Migrate => db::migrate(&pool).await?,
        Command::Passwd { action } => match action {
            PasswdAction::Add { label, password, generate } => {
                let (pw, generated) = match (password, generate) {
                    (Some(p), _) => (p, false),
                    (None, true) => (wordlist::two_word(), true),
                    (None, false) => (rpassword::prompt_password("New password: ")?, false),
                };
                passwd::add(&pool, &label, &pw).await?;
                if generated {
                    println!("Added password '{}': {}", label, pw);
                } else {
                    println!("Added password '{}'", label);
                }
            }
            PasswdAction::List => passwd::list(&pool).await?,
            PasswdAction::Revoke { label } => {
                passwd::revoke(&pool, &label).await?;
                println!("Revoked '{}'", label);
            }
            PasswdAction::Promote { label } => {
                passwd::set_admin(&pool, &label, true).await?;
                println!("Promoted '{}' to admin", label);
            }
            PasswdAction::Demote { label } => {
                passwd::set_admin(&pool, &label, false).await?;
                println!("Demoted '{}'", label);
            }
        },
    }
    Ok(())
}
