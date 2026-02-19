use std::collections::HashMap;
use std::fmt;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, ValueEnum};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use zbus::Proxy;
use zvariant::{OwnedValue, Str};

const NOTIFY_DEST: &str = "org.freedesktop.Notifications";
const NOTIFY_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFY_IFACE: &str = "org.freedesktop.Notifications";

#[derive(Debug, Clone, Copy, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    fn as_hint_value(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "notify",
    about = "dispatch dbus notifications",
    long_about = None,
    color = clap::ColorChoice::Never,
    help_template = "{name} - {about}\n\nUsage:\n  notify [options] [summary] [body...]\n\nArguments:\n\n  summary   notification title (overrides YAML summary)\n  body...   notification body text; use '-' to read body text from stdin\n\nOptions:\n{options}"
)]
struct Cli {
    #[arg(value_name = "summary", help = "notification title (overrides YAML summary)")]
    summary: Option<String>,

    #[arg(value_name = "body", trailing_var_arg = true, allow_hyphen_values = true, help = "notification body text; use '-' to read body text from stdin")]
    body: Vec<String>,

    #[arg(long = "file", value_name = "path", help = "read YAML payload from file path, or '-' for stdin")]
    file: Option<PathBuf>,

    #[arg(short = 'u', long = "urgency", value_enum, value_name = "URGENCY", help = "urgency level")]
    urgency: Option<Urgency>,

    #[arg(short = 'i', long = "icon", value_name = "ICON", help = "icon name or icon file path")]
    icon: Option<String>,

    #[arg(short = 'a', long = "app-name", value_name = "APP_NAME", help = "application name shown by notification daemon")]
    app_name: Option<String>,

    #[arg(short = 'c', long = "category", value_name = "CATEGORY", help = "notification category hint")]
    category: Option<String>,

    #[arg(long = "hint", value_name = "key:value", help = "custom hint (repeatable)")]
    hints: Vec<String>,

    #[arg(long = "action", value_name = "id:label", help = "add action button (repeatable)")]
    actions: Vec<String>,

    #[arg(long = "progress", value_name = "0-100", help = "progress value hint")]
    progress: Option<u8>,

    #[arg(short = 't', long = "timeout", value_name = "ms", help = "auto-close timeout in milliseconds; with --await also sets await cap to ms+1000")]
    expire_time: Option<i32>,

    #[arg(long = "id", aliases = ["replace"], value_name = "id", help = "replace existing notification id")]
    replace_id: Option<u32>,

    #[arg(long = "print-id", help = "print returned notification id to stdout")]
    print_id: bool,

    #[arg(long = "await", help = "wait until notification closes or an action is selected")]
    await_result: bool,
}

impl Cli {
    fn is_empty_invocation(&self) -> bool {
        self.summary.is_none()
            && self.body.is_empty()
            && self.file.is_none()
            && self.urgency.is_none()
            && self.icon.is_none()
            && self.app_name.is_none()
            && self.category.is_none()
            && self.hints.is_empty()
            && self.actions.is_empty()
            && self.progress.is_none()
            && self.expire_time.is_none()
            && self.replace_id.is_none()
            && !self.print_id
            && !self.await_result
    }
}

#[derive(Debug, Default, Deserialize)]
struct YamlPayload {
    summary: Option<String>,
    body: Option<String>,
    urgency: Option<Urgency>,
    icon: Option<String>,
    app_name: Option<String>,
    category: Option<String>,
    #[serde(default)]
    hints: HashMap<String, serde_yaml::Value>,
    #[serde(default)]
    actions: Vec<YamlAction>,
    progress: Option<u8>,
    timeout: Option<i32>,
    expire_time: Option<i32>,
    id: Option<u32>,
    replace: Option<u32>,
    print_id: Option<bool>,
    #[serde(rename = "await")]
    await_result: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum YamlAction {
    Pair(String),
    Object { id: String, label: String },
}

#[derive(Debug)]
struct Request {
    app_name: String,
    replaces_id: u32,
    icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: HashMap<String, OwnedValue>,
    expire_timeout: i32,
    print_id: bool,
    await_result: bool,
    await_timeout_ms: Option<u64>,
}

#[derive(Debug)]
struct AwaitTimeoutError {
    timeout_ms: u64,
}

impl fmt::Display for AwaitTimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "--await timed out after {}ms", self.timeout_ms)
    }
}

impl std::error::Error for AwaitTimeoutError {}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        if error.downcast_ref::<AwaitTimeoutError>().is_some() {
            eprintln!("error: {error:#}");
            std::process::exit(124);
        }
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.file.is_some() && cli.body.len() == 1 && cli.body[0] == "-" {
        bail!("cannot use BODY='-' together with --file");
    }

    if cli.is_empty_invocation() && io::stdin().is_terminal() {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    }

    if let Some(value) = cli.progress
        && value > 100
    {
        bail!("--progress must be between 0 and 100");
    }

    let stdin_body = load_stdin_body_if_requested(&cli)?;
    let payload = load_yaml_payload(&cli)?;
    let request = merge_request(cli, payload, stdin_body)?;

    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to session D-Bus")?;

    let proxy = Proxy::new(&connection, NOTIFY_DEST, NOTIFY_PATH, NOTIFY_IFACE)
        .await
        .context("failed to create notifications proxy")?;

    let notification_id: u32 = proxy
        .call(
            "Notify",
            &(
                request.app_name,
                request.replaces_id,
                request.icon,
                request.summary,
                request.body,
                request.actions,
                request.hints,
                request.expire_timeout,
            ),
        )
        .await
        .context("failed to send desktop notification")?;

    if request.print_id {
        println!("{notification_id}");
    }

    if request.await_result {
        await_notification_result(
            &proxy,
            notification_id,
            request.print_id,
            request.await_timeout_ms,
        )
        .await?;
    }

    Ok(())
}

fn load_yaml_payload(cli: &Cli) -> Result<Option<YamlPayload>> {
    let mut input = String::new();

    if let Some(path) = &cli.file {
        if path.as_os_str() == "-" {
            io::stdin()
                .read_to_string(&mut input)
                .context("failed to read YAML from stdin")?;
        } else {
            input = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read YAML file: {}", path.display()))?;
        }
    } else if !io::stdin().is_terminal() && cli.body.is_empty() {
        io::stdin()
            .read_to_string(&mut input)
            .context("failed to read YAML from stdin")?;
    }

    if input.trim().is_empty() {
        return Ok(None);
    }

    let payload: YamlPayload =
        serde_yaml::from_str(&input).context("failed to parse YAML payload")?;
    Ok(Some(payload))
}

fn load_stdin_body_if_requested(cli: &Cli) -> Result<Option<String>> {
    if !(cli.body.len() == 1 && cli.body[0] == "-") {
        return Ok(None);
    }

    let mut body = String::new();
    io::stdin()
        .read_to_string(&mut body)
        .context("failed to read body from stdin")?;
    Ok(Some(body))
}

fn merge_request(cli: Cli, payload: Option<YamlPayload>, stdin_body: Option<String>) -> Result<Request> {
    let payload = payload.unwrap_or_default();

    let mut hints = HashMap::<String, OwnedValue>::new();
    for (key, value) in payload.hints {
        hints.insert(key, yaml_value_to_owned_value(value)?);
    }

    let mut actions = Vec::<String>::new();
    for action in payload.actions {
        let (id, label) = parse_yaml_action(action)?;
        actions.push(id);
        actions.push(label);
    }
    for action in cli.actions {
        let (id, label) = parse_cli_action(&action)?;
        actions.push(id);
        actions.push(label);
    }

    let body_from_cli = if cli.body.is_empty() || (cli.body.len() == 1 && cli.body[0] == "-") {
        None
    } else {
        Some(cli.body.join(" "))
    };

    let summary = sanitize_text(cli.summary.or(payload.summary).unwrap_or_default());
    let body = sanitize_text(
        stdin_body
            .or(body_from_cli)
            .or(payload.body)
            .unwrap_or_default(),
    );
    let app_name = sanitize_text(
        cli.app_name
            .or(payload.app_name)
            .unwrap_or_else(|| "notify".to_string()),
    );
    let icon = sanitize_text(cli.icon.or(payload.icon).unwrap_or_default());

    let urgency = cli.urgency.or(payload.urgency).unwrap_or(Urgency::Normal);
    hints.insert(
        "urgency".to_string(),
        OwnedValue::from(urgency.as_hint_value()),
    );

    if let Some(category) = cli.category.or(payload.category) {
        let category = sanitize_text(category);
        hints.insert(
            "category".to_string(),
            OwnedValue::from(Str::from(category.as_str())),
        );
    }

    let progress = cli.progress.or(payload.progress);
    if let Some(value) = progress {
        if value > 100 {
            bail!("progress must be between 0 and 100");
        }
        hints.insert("value".to_string(), OwnedValue::from(i32::from(value)));
    }

    for raw_hint in cli.hints {
        let (key, value) = parse_cli_hint(&raw_hint)?;
        hints.insert(key, value);
    }

    let replaces_id = cli
        .replace_id
        .or(payload.replace)
        .or(payload.id)
        .unwrap_or(0);
    let expire_timeout = cli
        .expire_time
        .or(payload.expire_time)
        .or(payload.timeout)
        .unwrap_or(-1);
    let print_id = cli.print_id || payload.print_id.unwrap_or(false);
    let await_result = cli.await_result || payload.await_result.unwrap_or(false);
    let await_timeout_ms = if await_result && expire_timeout >= 0 {
        Some(expire_timeout as u64 + 1000)
    } else {
        None
    };

    Ok(Request {
        app_name,
        replaces_id,
        icon,
        summary,
        body,
        actions,
        hints,
        expire_timeout,
        print_id,
        await_result,
        await_timeout_ms,
    })
}

fn parse_yaml_action(action: YamlAction) -> Result<(String, String)> {
    match action {
        YamlAction::Pair(value) => parse_cli_action(&value),
        YamlAction::Object { id, label } => Ok((sanitize_text(id), sanitize_text(label))),
    }
}

fn parse_cli_action(input: &str) -> Result<(String, String)> {
    let (id, label) = input
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid --action '{input}', expected ID:LABEL"))?;
    let id = sanitize_text(id.trim().to_string());
    let label = sanitize_text(label.trim().to_string());
    if id.is_empty() || label.is_empty() {
        bail!("invalid --action '{input}', ID and LABEL must be non-empty");
    }
    Ok((id, label))
}

fn parse_cli_hint(input: &str) -> Result<(String, OwnedValue)> {
    let (key, raw_value) = input
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid --hint '{input}', expected KEY:VALUE"))?;
    let key = key.trim();
    if key.is_empty() {
        bail!("invalid --hint '{input}', key cannot be empty");
    }
    let value = parse_string_value(raw_value.trim());
    Ok((key.to_string(), value))
}

fn parse_string_value(value: &str) -> OwnedValue {
    if value.eq_ignore_ascii_case("true") {
        return OwnedValue::from(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return OwnedValue::from(false);
    }
    if let Ok(number) = value.parse::<i64>() {
        return OwnedValue::from(number);
    }
    if let Ok(number) = value.parse::<f64>() {
        return OwnedValue::from(number);
    }
    let string_value = sanitize_text(value.to_string());
    OwnedValue::from(Str::from(string_value.as_str()))
}

fn yaml_value_to_owned_value(value: serde_yaml::Value) -> Result<OwnedValue> {
    match value {
        serde_yaml::Value::Bool(v) => Ok(OwnedValue::from(v)),
        serde_yaml::Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                Ok(OwnedValue::from(i))
            } else if let Some(u) = v.as_u64() {
                Ok(OwnedValue::from(u))
            } else if let Some(f) = v.as_f64() {
                Ok(OwnedValue::from(f))
            } else {
                Err(anyhow!("unsupported numeric hint value"))
            }
        }
        serde_yaml::Value::String(v) => {
            let value = sanitize_text(v);
            Ok(OwnedValue::from(Str::from(value.as_str())))
        }
        serde_yaml::Value::Null => Ok(OwnedValue::from(Str::from(""))),
        _ => Err(anyhow!(
            "unsupported YAML hint type; only scalar values are allowed"
        )),
    }
}

fn sanitize_text(value: String) -> String {
    value.replace('\0', "")
}

async fn await_notification_result(
    proxy: &Proxy<'_>,
    id: u32,
    print_id: bool,
    await_timeout: Option<u64>,
) -> Result<()> {
    let mut action_stream = proxy
        .receive_signal("ActionInvoked")
        .await
        .context("failed to subscribe to ActionInvoked signal")?;
    let mut closed_stream = proxy
        .receive_signal("NotificationClosed")
        .await
        .context("failed to subscribe to NotificationClosed signal")?;

    let wait_future = async {
        loop {
            tokio::select! {
                maybe_msg = action_stream.next() => {
                    let msg = maybe_msg.context("action signal stream ended")?;
                    let (signal_id, action_key): (u32, String) = msg.body().deserialize().context("failed to decode ActionInvoked")?;
                    if signal_id == id {
                        let output = if print_id {
                            json!({"event":"action","id": id, "action": action_key})
                        } else {
                            json!({"event":"action","action": action_key})
                        };
                        println!("{}", output);
                        return Ok(());
                    }
                }
                maybe_msg = closed_stream.next() => {
                    let msg = maybe_msg.context("closed signal stream ended")?;
                    let (signal_id, reason): (u32, u32) = msg.body().deserialize().context("failed to decode NotificationClosed")?;
                    if signal_id == id {
                        let output = if print_id {
                            json!({"event":"closed","id": id, "reason": reason})
                        } else {
                            json!({"event":"closed","reason": reason})
                        };
                        println!("{}", output);
                        return Ok(());
                    }
                }
            }
        }
    };

    if let Some(ms) = await_timeout {
        match tokio::time::timeout(Duration::from_millis(ms), wait_future).await {
            Ok(result) => result,
            Err(_) => {
                let output = if print_id {
                    json!({"event":"await-timeout","id": id, "timeout_ms": ms})
                } else {
                    json!({"event":"await-timeout","timeout_ms": ms})
                };
                println!("{}", output);
                Err(AwaitTimeoutError { timeout_ms: ms }.into())
            }
        }
    } else {
        wait_future.await
    }
}
