//! Native command-line access to the tool handlers, without MCP framing.
//!
//! `call` runs one tool through a per-user session daemon so element indices
//! from `get_app_state` and the lazily created uinput pointer/keyboard persist
//! across invocations, exactly as they do inside one MCP session. `tools`
//! prints the catalogue from the same router the MCP server uses. Images are
//! written to files and reported by path instead of inline base64.

use super::*;
use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const SOCKET_ENV: &str = "COMPUTER_USE_LINUX_SOCKET";
const IDLE_ENV: &str = "COMPUTER_USE_LINUX_DAEMON_IDLE_SECS";
const SCREENSHOT_DIR_ENV: &str = "COMPUTER_USE_LINUX_SCREENSHOT_DIR";
const DEFAULT_IDLE_SECS: u64 = 1800;
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Tools reachable from the CLI. `run_shell` is excluded: every agent that
/// drives this CLI already has a shell.
pub(crate) const CLI_TOOLS: &[&str] = &[
    "doctor",
    "setup_accessibility",
    "setup_window_targeting",
    "list_apps",
    "list_desktop_apps",
    "launch_app",
    "list_windows",
    "focused_window",
    "activate_window",
    "move_window",
    "resize_window",
    "get_app_state",
    "screenshot",
    "click",
    "drag",
    "scroll",
    "press_key",
    "type_text",
    "paste",
    "perform_action",
    "set_value",
    "focus_element",
    "select_text",
    "complete_interaction",
];

/// Exit codes: 0 success, 1 the tool ran and reported failure, 2 the request
/// was rejected before dispatch or the daemon could not be reached.
pub(crate) async fn run(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("tools") => {
            print_json(&tool_catalogue(args.get(1).map(String::as_str))?);
            Ok(0)
        }
        Some("call") => {
            let mut rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            let in_process = take_flag(&mut rest, "--no-daemon");
            let (tool, arguments) = match rest.as_slice() {
                [tool] => (*tool, "{}"),
                [tool, arguments] => (*tool, *arguments),
                _ => {
                    return Ok(usage_error(
                        "usage: call TOOL [JSON_ARGUMENTS] [--no-daemon]",
                    ))
                }
            };
            let arguments = match parse_arguments(tool, arguments) {
                Ok(arguments) => arguments,
                Err(error) => return Ok(usage_error(&format!("{error:#}"))),
            };
            let response = if in_process {
                execute(&ComputerUseLinux::default(), tool, arguments).await
            } else {
                request_daemon(tool, arguments).await
            };
            Ok(emit(response))
        }
        Some("daemon") => {
            serve(&socket_path()).await?;
            Ok(0)
        }
        Some("stop") => {
            let stopped = match UnixStream::connect(socket_path()).await {
                Ok(stream) => exchange(stream, &json!({"tool": "__stop"})).await.is_ok(),
                Err(_) => false,
            };
            print_json(&json!({"ok": true, "stopped": stopped}));
            Ok(0)
        }
        _ => Ok(usage_error(
            "usage: tools [TOOL] | call TOOL [JSON] | daemon | stop",
        )),
    }
}

fn take_flag(args: &mut Vec<&str>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|arg| *arg != flag);
    args.len() != before
}

fn print_json(value: &Value) {
    println!("{}", serde_json::to_string(value).unwrap_or_default());
}

fn usage_error(message: &str) -> i32 {
    print_json(&json!({"ok": false, "error": message}));
    2
}

/// Print the response and map it to an exit code.
fn emit(response: Result<Value>) -> i32 {
    match response {
        Ok(value) => {
            let failed = value.get("ok") == Some(&Value::Bool(false))
                || value.get("is_error") == Some(&Value::Bool(true));
            print_json(&value);
            i32::from(failed)
        }
        Err(error) => usage_error(&format!("{error:#}")),
    }
}

/// Validate before anything reaches the desktop: known tool, JSON object,
/// and only argument names the tool's schema declares. Upstream parameter
/// structs ignore unknown fields, which would turn a typo such as `windw_id`
/// into an untargeted action.
fn parse_arguments(tool: &str, raw: &str) -> Result<Value> {
    if !CLI_TOOLS.contains(&tool) {
        bail!(
            "unknown tool '{tool}'. Run `tools` for the list; run_shell is not exposed, use your own shell"
        );
    }
    let arguments: Value =
        serde_json::from_str(raw).with_context(|| format!("arguments for {tool} are not JSON"))?;
    let Value::Object(fields) = &arguments else {
        bail!("arguments for {tool} must be a JSON object");
    };
    let known = schema_properties(tool);
    let unknown: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|key| !known.iter().any(|name| name == key))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "unknown argument(s) for {tool}: {}. Accepted: {}",
            unknown.join(", "),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    }
    Ok(arguments)
}

fn cli_router() -> rmcp::handler::server::router::tool::ToolRouter<ComputerUseLinux> {
    ComputerUseLinux::default().router_with_completion(true)
}

fn schema_properties(tool: &str) -> Vec<String> {
    cli_router()
        .list_all()
        .into_iter()
        .find(|entry| entry.name == tool)
        .and_then(|entry| entry.input_schema.get("properties").cloned())
        .and_then(|properties| {
            properties
                .as_object()
                .map(|map| map.keys().cloned().collect())
        })
        .unwrap_or_default()
}

fn tool_catalogue(tool: Option<&str>) -> Result<Value> {
    let tools: Vec<_> = cli_router()
        .list_all()
        .into_iter()
        .filter(|entry| CLI_TOOLS.contains(&entry.name.as_ref()))
        .collect();
    match tool {
        None => Ok(Value::Array(
            tools
                .iter()
                .map(|entry| {
                    let description = entry.description.as_deref().unwrap_or_default();
                    let summary = description.split(". ").next().unwrap_or(description);
                    json!({
                        "name": entry.name,
                        "summary": summary.trim_end_matches('.'),
                        "read_only": entry.annotations.as_ref().and_then(|a| a.read_only_hint),
                    })
                })
                .collect(),
        )),
        Some(name) => {
            let entry = tools
                .into_iter()
                .find(|entry| entry.name == name)
                .ok_or_else(|| anyhow!("unknown tool '{name}'"))?;
            Ok(json!({
                "name": entry.name,
                "description": entry.description,
                "arguments": entry.input_schema.get("properties"),
                "required": entry.input_schema.get("required"),
            }))
        }
    }
}

fn params<T: DeserializeOwned>(tool: &str, arguments: Value) -> Result<Parameters<T>> {
    serde_json::from_value(arguments)
        .map(Parameters)
        .with_context(|| format!("invalid arguments for {tool}"))
}

fn output<T: Serialize>(Json(value): Json<T>) -> Result<Value> {
    serde_json::to_value(value).context("failed to serialize tool output")
}

/// Run one tool against a server instance. Argument errors surface as `Err`.
async fn execute(server: &ComputerUseLinux, tool: &str, arguments: Value) -> Result<Value> {
    let a = arguments;
    match tool {
        "doctor" => output(server.doctor().await),
        "setup_accessibility" => output(server.setup_accessibility().await),
        "setup_window_targeting" => output(server.setup_window_targeting().await),
        "list_apps" => output(server.list_apps().await),
        "list_desktop_apps" => output(server.list_desktop_apps().await),
        "launch_app" => output(server.launch_app(params(tool, a)?).await),
        "list_windows" => output(server.list_windows().await),
        "focused_window" => output(server.focused_window().await),
        "activate_window" => output(server.activate_window(params(tool, a)?).await),
        "move_window" => output(server.move_window(params(tool, a)?).await),
        "resize_window" => output(server.resize_window(params(tool, a)?).await),
        "get_app_state" => content_result(server.get_app_state(params(tool, a)?).await),
        "screenshot" => content_result(server.screenshot(params(tool, a)?).await),
        "click" => output(server.click(params(tool, a)?).await),
        "drag" => output(server.drag(params(tool, a)?).await),
        "scroll" => output(server.scroll(params(tool, a)?).await),
        "press_key" => output(server.press_key(params(tool, a)?).await),
        "type_text" => output(server.type_text(params(tool, a)?).await),
        "paste" => output(server.paste(params(tool, a)?).await),
        "perform_action" => output(server.perform_action(params(tool, a)?).await),
        "set_value" => output(server.set_value(params(tool, a)?).await),
        "focus_element" => output(server.focus_element(params(tool, a)?).await),
        "select_text" => output(server.select_text(params(tool, a)?).await),
        // An explicit CLI call is the opt-in, so the MCP env gate does not apply.
        "complete_interaction" => serde_json::to_value(
            send_completion_notification(Path::new("notify-send"), Duration::from_secs(2)).await,
        )
        .context("failed to serialize completion output"),
        _ => bail!("unknown tool '{tool}'"),
    }
}

/// Flatten a content-bearing result into one JSON object: structured content
/// or JSON text blocks become fields, images become files under `images`.
fn content_result(result: Result<CallToolResult, ErrorData>) -> Result<Value> {
    let result = result.map_err(|error| anyhow!("{}", error.message))?;
    let mut merged = match result.structured_content {
        Some(Value::Object(map)) => map,
        Some(other) => serde_json::Map::from_iter([("result".to_string(), other)]),
        None => serde_json::Map::new(),
    };
    let mut images = Vec::new();
    let mut texts = Vec::new();
    for block in serde_json::to_value(&result.content)?
        .as_array()
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => {
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png");
                images.push(json!({"path": write_image(data, mime)?, "mime_type": mime}));
            }
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match serde_json::from_str::<Value>(text) {
                    Ok(Value::Object(fields)) => {
                        for (key, value) in fields {
                            merged.entry(key).or_insert(value);
                        }
                    }
                    _ => texts.push(Value::String(text.to_string())),
                }
            }
            _ => {}
        }
    }
    if !images.is_empty() {
        merged.insert("images".into(), Value::Array(images));
    }
    if !texts.is_empty() && merged.is_empty() {
        merged.insert("text".into(), Value::Array(texts));
    }
    if result.is_error == Some(true) {
        merged.insert("is_error".into(), Value::Bool(true));
    }
    Ok(Value::Object(merged))
}

fn screenshot_dir() -> PathBuf {
    if let Some(dir) = env::var_os(SCREENSHOT_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(env::temp_dir);
    cache.join("computer-use-linux").join("screenshots")
}

fn write_image(base64_data: &str, mime: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .context("image block is not valid base64")?;
    let extension = if mime == "image/jpeg" { "jpg" } else { "png" };
    let dir = screenshot_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let path = dir.join(format!(
        "{}-{:09}.{extension}",
        stamp.as_secs(),
        stamp.subsec_nanos()
    ));
    std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path.display().to_string())
}

fn socket_path() -> PathBuf {
    if let Some(path) = env::var_os(SOCKET_ENV) {
        return PathBuf::from(path);
    }
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::temp_dir().join(format!("computer-use-linux-{}", unsafe { libc::getuid() }))
        });
    base.join("computer-use-linux").join("cli.sock")
}

/// One JSON line out, one JSON line back.
async fn exchange(mut stream: UnixStream, request: &Value) -> Result<Value> {
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.shutdown().await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    serde_json::from_str(&response).context("daemon returned an unreadable response")
}

async fn request_daemon(tool: &str, arguments: Value) -> Result<Value> {
    let socket = socket_path();
    let request = json!({"tool": tool, "arguments": arguments});
    for _ in 0..2 {
        let stream = connect_or_start(&socket).await?;
        let reply = exchange(stream, &request).await?;
        // The daemon exits instead of answering when its binary was replaced.
        if reply.get("restart") == Some(&Value::Bool(true)) {
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        return match reply.get("error").and_then(Value::as_str) {
            Some(error) if reply.get("result").is_none() => Err(anyhow!("{error}")),
            _ => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        };
    }
    bail!("daemon kept restarting; run `stop` and retry")
}

async fn connect_or_start(socket: &Path) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket).await {
        return Ok(stream);
    }
    let log = socket.with_file_name("daemon.log");
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    Command::new(env::current_exe()?)
        .arg("daemon")
        .env(SOCKET_ENV, socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .context("failed to start the computer-use-linux daemon")?;
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Ok(stream);
        }
        sleep(Duration::from_millis(50)).await;
    }
    bail!("daemon did not start within 5s; see {}", log.display())
}

fn binary_stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

async fn serve(socket: &Path) -> Result<()> {
    if UnixStream::connect(socket).await.is_ok() {
        return Ok(()); // another daemon already owns the session
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    let exe = env::current_exe()?;
    let started_from = binary_stamp(&exe);
    let idle = Duration::from_secs(
        env::var(IDLE_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_IDLE_SECS),
    );
    let server = ComputerUseLinux::default();
    loop {
        let stream = match timeout(idle, listener.accept()).await {
            Err(_) => break,
            Ok(accepted) => accepted?.0,
        };
        let stale = binary_stamp(&exe) != started_from;
        match handle(&server, stream, stale).await {
            Ok(true) => continue,
            Ok(false) => break,
            Err(error) => eprintln!("computer-use-linux daemon: {error:#}"),
        }
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}

/// Returns whether the daemon should keep serving.
async fn handle(server: &ComputerUseLinux, stream: UnixStream, stale: bool) -> Result<bool> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    let request: Value = serde_json::from_str(&line).context("unreadable request")?;
    let tool = request
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (reply, keep_serving) = if stale {
        (json!({"restart": true}), false)
    } else if tool == "__stop" {
        (json!({"result": {"ok": true}}), false)
    } else {
        let arguments = request.get("arguments").cloned().unwrap_or(json!({}));
        let reply = match execute(server, tool, arguments).await {
            Ok(result) => json!({"result": result}),
            Err(error) => json!({"error": format!("{error:#}")}),
        };
        (reply, true)
    };
    let mut bytes = serde_json::to_vec(&reply)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(keep_serving)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exposes_every_router_tool_except_run_shell() {
        let mut routed: Vec<String> = cli_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .filter(|name| name != "run_shell")
            .collect();
        let mut exposed: Vec<String> = CLI_TOOLS.iter().map(|name| name.to_string()).collect();
        routed.sort();
        exposed.sort();
        assert_eq!(routed, exposed);
    }

    #[test]
    fn arguments_reject_unknown_tool_fields_and_non_objects() {
        assert!(parse_arguments("run_shell", "{}").is_err());
        assert!(parse_arguments("click", "[1]").is_err());
        let typo = parse_arguments("click", r#"{"windw_id": 4}"#).unwrap_err();
        assert!(format!("{typo:#}").contains("windw_id"));
        assert!(parse_arguments("doctor", r#"{"x": 1}"#).is_err());
        assert!(parse_arguments("click", r#"{"x": 1, "y": 2}"#).is_ok());
    }

    #[test]
    fn image_blocks_become_files_and_text_json_becomes_fields() {
        let dir = std::env::temp_dir().join(format!("cul-cli-test-{}", std::process::id()));
        std::env::set_var(SCREENSHOT_DIR_ENV, &dir);
        let result = CallToolResult::success(vec![
            ContentBlock::image("aGVsbG8=".to_string(), "image/png".to_string()),
            ContentBlock::text(r#"{"width": 2, "scale": 1.5}"#.to_string()),
        ]);
        let value = content_result(Ok(result)).unwrap();
        std::env::remove_var(SCREENSHOT_DIR_ENV);
        assert_eq!(value["width"], 2);
        let path = value["images"][0]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(dir);
    }
}
