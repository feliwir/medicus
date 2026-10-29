//! `medicus`: the core server binary (CLI over the `medicus_server` library).

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use medicus_server::{Config, Exit, LogConfig, LogFormat, Node, SettingsStore, UserStore};

#[derive(Debug, Parser)]
#[command(version, about = "medicus — a lightweight, modular PACS")]
struct Cli {
    /// Configuration file (TOML). Values can be overridden with MEDICUS_* environment variables.
    #[arg(short, long, env = "MEDICUS_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server (default).
    Run,
    /// Validate the configuration and print the effective result as JSON.
    CheckConfig,
    /// Import DICOM Part 10 files from a file or directory tree and print a summary as JSON.
    Import {
        path: PathBuf,
        /// Files ingested concurrently.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Partition to import into.
        #[arg(long, default_value = medicus_core::Partition::DEFAULT)]
        partition: medicus_core::Partition,
    },
    /// Addon tools.
    Addon {
        #[command(subcommand)]
        command: AddonCommand,
    },
    /// The settings stored in the index. The configuration file's settings sections are only
    /// imported on the first start; after that, change the settings here or in the admin UI.
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// User accounts. Use these on a server without the admin UI, or to regain access.
    Users {
        #[command(subcommand)]
        command: UsersCommand,
    },
}

#[derive(Debug, Subcommand)]
enum UsersCommand {
    /// Create a user. The password is read from the terminal (hidden) or the first line of stdin.
    Create {
        name: String,
        /// Give the user the built-in `admin` role: every permission.
        #[arg(long)]
        admin: bool,
        /// A role from the settings; repeat for several.
        #[arg(long = "role")]
        roles: Vec<String>,
        #[arg(long)]
        display_name: Option<String>,
        /// Create the user without a password: it can only use API tokens.
        #[arg(long)]
        no_password: bool,
    },
    /// Set a user's password (read like for `create`).
    Passwd {
        name: String,
    },
    /// List the users as JSON.
    List,
    /// Disable a user: their sessions end, and their tokens are refused.
    Disable {
        name: String,
    },
    Enable {
        name: String,
    },
    /// Delete a user with their sessions and tokens.
    Delete {
        name: String,
    },
    /// Create an API token for a user and print it. It is shown only once.
    Token {
        name: String,
        /// What the token is for.
        #[arg(long)]
        label: Option<String>,
        /// Days until the token expires; without it, it never does.
        #[arg(long)]
        days: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Print the settings (by default the latest revision) as TOML, in the format of the
    /// configuration file.
    Export {
        #[arg(long)]
        revision: Option<u64>,
        /// Print JSON instead of TOML.
        #[arg(long)]
        json: bool,
        /// Include the secrets of addon configurations, e.g. for a backup. Without it they are
        /// placeholders, which `import` replaces with the stored values.
        #[arg(long)]
        with_secrets: bool,
    },
    /// Validate a settings file (TOML, or JSON if it ends in `.json`) and store it as a new
    /// revision. It takes effect when the server starts next.
    Import {
        file: PathBuf,
        /// Why the settings are changed; recorded with the revision.
        #[arg(long)]
        comment: Option<String>,
    },
    /// List the latest revisions as JSON (without the documents).
    History {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Subcommand)]
enum AddonCommand {
    /// Write the manifest file (`<name>.addon.toml`) of an addon library, so the server can
    /// discover the addon in its search paths without loading it. Reads the library's embedded
    /// manifest, which runs the library's static initializers: only use it on trusted libraries.
    Manifest {
        /// The addon library (`.so`, `.dylib` or `.dll`).
        library: PathBuf,
        /// Directory to write the file into; default: the library's directory.
        #[arg(long, conflicts_with = "print")]
        out: Option<PathBuf>,
        /// Print the file instead of writing it.
        #[arg(long)]
        print: bool,
    },
}

fn init_tracing(cfg: &LogConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.filter));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match cfg.format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn run(cfg: Config) -> anyhow::Result<Exit> {
    tracing::info!(
        name = %cfg.server.name,
        data_dir = %cfg.server.data_dir.display(),
        http = %cfg.http.listen,
        version = env!("CARGO_PKG_VERSION"),
        "medicus starting"
    );
    let node = Node::open(cfg).await?;
    if let Some(token) = node.setup_token() {
        tracing::warn!("no user exists: the server is in setup mode");
        // On the console as well, whatever the log filter: the operator needs it to set up.
        eprintln!(
            "\n  medicus is not set up yet. Create the first administrator in the admin UI with the\n  \
             setup token below, or with `medicus users create <name> --admin`.\n\n    setup token: \
             {token}\n"
        );
    }
    std::sync::Arc::new(node).serve(shutdown_signal()).await
}

/// Exit code that asks the service manager (or parent process) to start the server again, where
/// the process cannot replace itself.
#[cfg(not(unix))]
const RESTART_EXIT_CODE: i32 = 75;

/// Starts the server again after a requested restart. On Unix the process replaces itself with
/// the executable at its path (after an upgrade, the new one), keeping its arguments and
/// environment; file descriptors are closed on exec. Elsewhere it exits with
/// `RESTART_EXIT_CODE` (75).
fn restart() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let exe = std::env::current_exe().context("finding the executable to restart")?;
        tracing::info!(exe = %exe.display(), "restarting");
        let err = std::process::Command::new(&exe)
            .args(std::env::args_os().skip(1))
            .exec();
        Err(err).with_context(|| format!("restarting {}", exe.display()))
    }
    #[cfg(not(unix))]
    {
        tracing::info!(code = RESTART_EXIT_CODE, "exiting to be restarted");
        std::process::exit(RESTART_EXIT_CODE)
    }
}

/// A password from the terminal without echo, or else the first line of stdin.
fn read_password(prompt: &str) -> anyhow::Result<String> {
    use std::io::IsTerminal as _;
    if std::io::stdin().is_terminal() {
        let password = rpassword::prompt_password(prompt)?;
        let again = rpassword::prompt_password("Repeat: ")?;
        anyhow::ensure!(password == again, "the passwords differ");
        Ok(password)
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }
}

/// Runs a `users` command; returns its output.
async fn users(cfg: Config, command: UsersCommand) -> anyhow::Result<String> {
    let store = UserStore::open(cfg).await?;
    match command {
        UsersCommand::Create {
            name,
            admin,
            roles,
            display_name,
            no_password,
        } => {
            let password = if no_password {
                None
            } else {
                Some(read_password("Password: ")?)
            };
            let user = store
                .create(&name, password.as_deref(), admin, roles, display_name)
                .await?;
            Ok(serde_json::to_string_pretty(&user)? + "\n")
        }
        UsersCommand::Passwd { name } => {
            store
                .set_password(&name, &read_password("New password: ")?)
                .await?;
            eprintln!("password of {name:?} changed");
            Ok(String::new())
        }
        UsersCommand::List => Ok(serde_json::to_string_pretty(&store.list().await?)? + "\n"),
        UsersCommand::Disable { name } => {
            store.set_disabled(&name, true).await?;
            Ok(String::new())
        }
        UsersCommand::Enable { name } => {
            store.set_disabled(&name, false).await?;
            Ok(String::new())
        }
        UsersCommand::Delete { name } => {
            store.delete(&name).await?;
            Ok(String::new())
        }
        UsersCommand::Token { name, label, days } => {
            Ok(store.create_token(&name, label, days).await? + "\n")
        }
    }
}

/// Runs a `settings` command; returns its output.
async fn settings(cfg: Config, command: SettingsCommand) -> anyhow::Result<String> {
    let store = SettingsStore::open(cfg)?;
    match command {
        SettingsCommand::Export {
            revision,
            json,
            with_secrets,
        } => {
            let found = store.get(revision).await?.with_context(|| match revision {
                Some(n) => format!("there is no settings revision {n}"),
                None => "no settings are stored yet (the server has not started)".into(),
            })?;
            let found = if with_secrets {
                found
            } else {
                store.masked(found)?
            };
            medicus_server::format_settings(&found, json)
        }
        SettingsCommand::Import { file, comment } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let json = file.extension().is_some_and(|e| e == "json");
            let parsed = medicus_server::parse_settings(&text, json)
                .with_context(|| format!("parsing {}", file.display()))?;
            let comment = comment.or_else(|| Some(format!("imported from {}", file.display())));
            let stored = store.put(&parsed, None, comment).await?;
            eprintln!(
                "stored settings revision {}; it takes effect when the server starts next",
                stored.revision
            );
            Ok(String::new())
        }
        SettingsCommand::History { limit } => {
            let history: Vec<_> = store
                .history(limit)
                .await?
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "revision": r.revision,
                        "author": r.author,
                        "comment": r.comment,
                        "created_at": r.created_at.format(&time::format_description::well_known::Rfc3339).ok(),
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&history)? + "\n")
        }
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref()).context("loading configuration")?;
    match cli.command.unwrap_or(Command::Run) {
        Command::CheckConfig => {
            let out = serde_json::to_string_pretty(&cfg)?;
            #[allow(clippy::print_stdout)] // the command's output
            {
                println!("{out}");
            }
            Ok(())
        }
        Command::Run => {
            init_tracing(&cfg.log);
            let rt = runtime()?;
            let exit = rt.block_on(run(cfg))?;
            rt.shutdown_timeout(std::time::Duration::from_secs(10));
            match exit {
                Exit::Shutdown => Ok(()),
                Exit::Restart => restart(),
            }
        }
        Command::Import {
            path,
            concurrency,
            partition,
        } => {
            init_tracing(&cfg.log);
            let report = runtime()?.block_on(async {
                Node::open(cfg)
                    .await?
                    .import(&partition, &path, concurrency)
                    .await
            })?;
            let out = serde_json::to_string_pretty(&report)?;
            #[allow(clippy::print_stdout)] // the command's output
            {
                println!("{out}");
            }
            Ok(())
        }
        Command::Users { command } => {
            init_tracing(&LogConfig {
                filter: "warn".into(),
                ..cfg.log.clone()
            });
            let out = runtime()?.block_on(users(cfg, command))?;
            #[allow(clippy::print_stdout)] // the command's output
            {
                print!("{out}");
            }
            Ok(())
        }
        Command::Settings { command } => {
            init_tracing(&LogConfig {
                filter: "warn".into(),
                ..cfg.log.clone()
            });
            let out = runtime()?.block_on(settings(cfg, command))?;
            #[allow(clippy::print_stdout)] // the command's output
            {
                print!("{out}");
            }
            Ok(())
        }
        Command::Addon {
            command:
                AddonCommand::Manifest {
                    library,
                    out,
                    print,
                },
        } => {
            let dir = out.unwrap_or_else(|| {
                library
                    .parent()
                    .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf)
            });
            let (file_name, text) =
                runtime()?.block_on(async { medicus_server::manifest_file(&library, &dir) })?;
            if print {
                #[allow(clippy::print_stdout)] // the command's output
                {
                    print!("{text}");
                }
            } else {
                let path = dir.join(file_name);
                std::fs::write(&path, text)
                    .with_context(|| format!("writing {}", path.display()))?;
                eprintln!("wrote {}", path.display());
            }
            Ok(())
        }
    }
}
