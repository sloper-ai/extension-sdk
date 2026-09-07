#![warn(rust_2018_idioms, unreachable_pub)]

#[path = "generated/api/commands/mod.rs"]
pub mod generated;

use std::{
    collections::BTreeMap,
    env,
    io::{
        self,
        Write as _,
    },
    path::{
        Path,
        PathBuf,
    },
    process::ExitCode,
    time::Duration,
};

use clap::{
    Arg,
    ArgMatches,
    CommandFactory,
    FromArgMatches,
    Parser,
    Subcommand,
};
use generated_client::{
    Client,
    Config,
};
use serde_json::{
    Value,
    json,
};
use sloper_extension_cli::{
    Error,
    PublishOptions,
    RunOptions,
    VerifyOptions,
    Visibility,
};
use sloper_extension_host::TrustRoots;
use sloper_extension_spec::parse_object;
use zeroize::Zeroizing;

// Match the publication workflow's HTTP deadline and bearer bound.
const API_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TOKEN_BYTES: usize = 16_384;

#[derive(Parser)]
#[command(
    name = "sloper-extension",
    version,
    about = "Build, validate, run, and publish Sloper extensions"
)]
struct Cli {
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    json: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(alias = "scaffold", about = "Create a complete SDK extension project")]
    New {
        name: String,
        #[arg(long, default_value = ".")]
        directory: PathBuf,
        #[arg(long)]
        sdk_revision: Option<String>,
    },
    #[command(about = "Build and stamp dist/extension.wasm")]
    Build {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    #[command(about = "Verify that dist matches a fresh SDK assembly")]
    Check {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    #[command(about = "Statically validate a component without executing guest code")]
    Validate { component: PathBuf },
    #[command(about = "Run an action against disposable local fixtures")]
    Run {
        action: String,
        #[arg(long, default_value = ".")]
        directory: PathBuf,
        #[arg(long, default_value = "run")]
        out: PathBuf,
        #[arg(long,default_value="{}",value_parser=json_object)]
        parameters: Value,
        #[arg(long,default_value="{}",value_parser=json_object)]
        configuration: Value,
        #[arg(long="source",value_parser=fixture)]
        sources: Vec<(String, PathBuf)>,
        #[arg(long="resource",value_parser=fixture)]
        resources: Vec<(String, PathBuf)>,
    },
    #[command(about = "Publish existing dist bytes using SLOPER_API_TOKEN")]
    Publish {
        #[arg(default_value = ".")]
        directory: PathBuf,
        #[arg(long,default_value=sloper_extension_cli::DEFAULT_API_URL)]
        api_url: String,
        #[arg(long, value_enum)]
        visibility: Option<Visibility>,
    },
    #[command(about = "Verify a signed release using explicitly trusted public roots")]
    Verify {
        #[arg(default_value = ".")]
        directory: PathBuf,
        #[arg(long)]
        envelope: PathBuf,
        #[arg(long)]
        trust: PathBuf,
        #[arg(long)]
        root_primary: String,
        #[arg(long)]
        root_secondary: String,
    },
}

fn command() -> clap::Command {
    Cli::command().subcommand(
        generated::command().about("Call the extension publication API").arg(
            Arg::new("api-url")
                .long("api-url")
                .env("SLOPER_API_URL")
                .default_value(sloper_extension_cli::DEFAULT_API_URL)
                .global(true),
        ),
    )
}

async fn execute_api(arguments: &ArgMatches) -> Result<Value, Value> {
    let token = Zeroizing::new(
        env::var("SLOPER_API_TOKEN").map_err(|_| usage("SLOPER_API_TOKEN must contain the publishing bearer"))?,
    );
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_control) {
        return Err(usage("publishing token must be a nonempty, bounded HTTP bearer"));
    }
    let api_url = arguments
        .get_one::<String>("api-url")
        .map_or(sloper_extension_cli::DEFAULT_API_URL, String::as_str);
    let url = url::Url::parse(api_url).map_err(|_| usage("API URL must be an absolute HTTP or HTTPS URL"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err(usage("API URL must be an absolute HTTP or HTTPS URL"));
    }
    let mut config = Config::new()
        .with_api_base(format!("{}/api/v1", api_url.trim_end_matches('/')))
        .with_timeout(API_TIMEOUT)
        .with_api_key(token);
    config.headers.insert(
        "origin",
        url.origin()
            .ascii_serialization()
            .parse()
            .map_err(|_| usage("API URL must have a valid HTTP origin"))?,
    );
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| Error::from(error).command_error())?;
    let response = generated::dispatch(arguments, &Client::with_http_client(config, http))
        .await
        .map_err(|error| Error::from(error).command_error())?;
    let json = response
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        });
    let body = if json {
        response
            .decode::<Value>(false)
            .map_err(|error| Error::from(error).command_error())?
            .body
    } else {
        response
            .decode::<String>(true)
            .map_err(|error| Error::from(error).command_error())?
            .body
            .map(Value::String)
    };
    Ok(body.unwrap_or(Value::Null))
}

fn json_object(text: &str) -> Result<Value, String> {
    parse_object(text.as_bytes()).map_err(|error| error.to_string())
}
fn fixture(text: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = text
        .split_once('=')
        .filter(|(name, path)| !name.is_empty() && !path.is_empty())
        .ok_or_else(|| "expected NAME=PATH".to_owned())?;
    Ok((name.to_owned(), PathBuf::from(path)))
}
fn fixtures(values: Vec<(String, PathBuf)>) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut result = BTreeMap::new();
    for (name, path) in values {
        if result.insert(name.clone(), path).is_some() {
            return Err(format!("duplicate fixture key {name}"));
        }
    }
    Ok(result)
}
fn usage(message: &str) -> Value {
    json!({"code":"USAGE","exit":2,"message":message,"retryable":false,"details":{}})
}

#[tokio::main]
async fn main() -> ExitCode {
    let matches = command().get_matches();
    if let Some(("api", arguments)) = matches.subcommand() {
        return show_result(execute_api(arguments).await, matches.get_flag("json"));
    }
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    match cli.command {
        Command::Validate {
            component,
        } => validate_process(&component),
        command => show_result(execute(command).await, cli.json),
    }
}

fn validate_process(component: &Path) -> ExitCode {
    let result = match sloper_extension_cli::validate_file(component) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(10);
        },
    };
    let mut output = io::stdout().lock();
    if serde_json::to_writer(&mut output, &result).is_err()
        || output.write_all(b"\n").is_err()
        || output.flush().is_err()
    {
        eprintln!("validator output could not be written");
        return ExitCode::from(10);
    }
    if result.valid {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    }
}

fn show_result(result: Result<Value, Value>, json: bool) -> ExitCode {
    match result {
        Ok(result) => {
            if json {
                println!("{result}");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
                );
            }
            ExitCode::SUCCESS
        },
        Err(error) => {
            if json {
                println!("{}", json!({"error":error}));
            } else {
                eprintln!("{}", error["message"].as_str().unwrap_or("extension operation failed"));
            }
            ExitCode::from(
                error["exit"]
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .unwrap_or(10),
            )
        },
    }
}

async fn execute(command: Command) -> Result<Value, Value> {
    use sloper_extension_cli as tools;
    match command {
        Command::New {
            name,
            directory,
            sdk_revision,
        } => {
            let revision = sdk_revision
                .as_deref()
                .or(tools::sdk_revision())
                .ok_or_else(|| usage("use --sdk-revision with the immutable SDK commit"))?;
            encode(tools::scaffold(&name, &directory, revision).map_err(|error| error.command_error())?)
        },
        Command::Build {
            directory,
        } => encode(tools::build(&directory).await.map_err(|error| error.command_error())?),
        Command::Check {
            directory,
        } => encode(tools::check(&directory).await.map_err(|error| error.command_error())?),
        Command::Validate {
            ..
        } => unreachable!("validation executes synchronously before the runtime starts"),
        Command::Run {
            action,
            directory,
            out,
            parameters,
            configuration,
            sources,
            resources,
        } => {
            let result = tools::run(RunOptions {
                action,
                directory,
                out,
                parameters,
                configuration,
                sources: fixtures(sources).map_err(|error| usage(&error))?,
                resources: fixtures(resources).map_err(|error| usage(&error))?,
            })
            .await
            .map_err(|error| error.command_error())?;
            let failed = result
                .operation
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|state| state != "SUCCEEDED");
            let result = encode(result)?;
            if failed {
                Err(
                    json!({"code":"EXTENSION_FAILED","exit":3,"message":"local extension operation failed","retryable":false,"details":{"run":result}}),
                )
            } else {
                Ok(result)
            }
        },
        Command::Publish {
            directory,
            api_url,
            visibility,
        } => {
            let token = Zeroizing::new(
                env::var("SLOPER_API_TOKEN")
                    .map_err(|_| usage("SLOPER_API_TOKEN must contain the publishing bearer"))?,
            );
            let result = tools::publish(PublishOptions {
                directory: &directory,
                api_url: &api_url,
                token: &token,
                visibility,
            })
            .await
            .map_err(|error| error.command_error())?;
            encode(result)
        },
        Command::Verify {
            directory,
            envelope,
            trust,
            root_primary,
            root_secondary,
        } => {
            let roots = TrustRoots::from_base64([&root_primary, &root_secondary])
                .map_err(|error| tools::Error::from(error).command_error())?;
            let result = tools::verify(VerifyOptions {
                directory: &directory,
                envelope: &envelope,
                trust: &trust,
                roots: &roots,
            })
            .await
            .map_err(|error| error.command_error())?;
            encode(result)
        },
    }
}

fn encode(result: impl serde::Serialize) -> Result<Value, Value> {
    serde_json::to_value(result).map_err(|error| sloper_extension_cli::Error::from(error).command_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_reject_duplicate_json_keys_and_fixture_keys() {
        assert!(json_object(r#"{"a":1,"a":2}"#).is_err());
        assert!(fixtures(vec![("a".into(), "a.jsonl".into()), ("a".into(), "b.jsonl".into())]).is_err());
    }
    #[test]
    fn explicit_publishing_endpoint_and_audience_parse_without_a_token_argument() {
        let cli = Cli::try_parse_from([
            "sloper-extension",
            "publish",
            ".",
            "--api-url",
            "https://api.example.com",
            "--visibility",
            "public",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Command::Publish {
                visibility: Some(Visibility::Public),
                ..
            }
        ));
        assert!(Cli::try_parse_from(["sloper-extension", "publish", "--token", "secret"]).is_err());
    }

    #[test]
    fn generated_api_commands_share_help_and_require_declared_arguments() {
        command().debug_assert();
        let matches = command()
            .try_get_matches_from([
                "sloper-extension",
                "api",
                "extensions",
                "get-extension-version",
                "--extension",
                "sloper.example",
                "--version",
                "1.0.0",
                "--json",
            ])
            .unwrap();
        assert!(matches.get_flag("json"));
        assert_eq!(matches.subcommand_name(), Some("api"));
        let error = command()
            .try_get_matches_from(["sloper-extension", "api", "extensions", "get-extension-version"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let help = command()
            .try_get_matches_from(["sloper-extension", "api", "extensions", "--help"])
            .unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("get-release-envelope"));
    }
}
