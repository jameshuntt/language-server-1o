#![forbid(unsafe_code)]
use anyhow::{Context, Result};
use language_server_1o::lsp::{Backend, Client};
use lsp_server::{Connection, Message, Response};
use lsp_types::InitializeParams;
use core_1o::{engine::{diagnostics_for_text_workspace, EngineSeverity, Workspace}, ir::parse_document, utils::parse_filename};
use std::{env, fs, path::{Path, PathBuf}};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") => { println!("language-server-1o {}", env!("CARGO_PKG_VERSION")); Ok(()) }
        Some("parse") | Some("emit-ast") => {
            let file = args.next().context("usage: language-server-1o parse <file>")?;
            let source = fs::read_to_string(&file).with_context(|| format!("reading {file}"))?;
            let ast = parse_document(&source).map_err(|e| anyhow::anyhow!("parse error: {e:?}"))?;
            println!("{}", serde_json::to_string_pretty(&ast)?);
            Ok(())
        }
        Some("check") => {
            let file = PathBuf::from(args.next().context("usage: language-server-1o check <file> [--workspace dir]")?);
            let mut workspace_dir: Option<PathBuf> = None;
            while let Some(flag) = args.next() {
                if flag == "--workspace" { workspace_dir = Some(PathBuf::from(args.next().context("missing --workspace directory")?)); }
                else { anyhow::bail!("unknown argument: {flag}"); }
            }
            let root = match workspace_dir { Some(root) => root, None => discover_workspace(file.parent().unwrap_or(Path::new("."))) };
            let name = file.file_name().and_then(|v|v.to_str()).context("invalid filename")?;
            let record = parse_filename(name).context("expected v1.<name>.1o.<modifier> filename")?;
            let source = fs::read_to_string(&file)?;
            let workspace = Workspace::new(root);
            let diagnostics = diagnostics_for_text_workspace(&workspace, &record.modifier, &record.version, &source);
            for diagnostic in &diagnostics { eprintln!("{:?}: {}", diagnostic.severity, diagnostic.message); }
            if diagnostics.iter().any(|d| matches!(d.severity, EngineSeverity::Error)) { anyhow::bail!("validation failed"); }
            println!("1o: validation passed ({} warning(s))", diagnostics.len());
            Ok(())
        }
        None | Some("--stdio") => run_lsp(),
        Some(other) => anyhow::bail!("unknown command '{other}'; use --version, check, parse or --stdio"),
    }
}

fn discover_workspace(from: &Path) -> PathBuf {
    for path in from.ancestors() {
        if path.join("oneo.project.json").is_file() || path.join("schema/1o.version.json").is_file() { return path.to_owned(); }
    }
    from.to_owned()
}

fn run_lsp() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    let capabilities = serde_json::to_value(Backend::capabilities())?;
    let init_params = connection.initialize(capabilities)?;
    let client = Client::new(connection.sender.clone());
    let init_params: InitializeParams = serde_json::from_value(init_params)?;
    let backend = Backend::new(client, init_params);
    backend.start_semantic_refresh_loop();
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? { break; }
                let id = request.id.clone();
                let response = match backend.handle_request(request) {
                    Ok(result) => Response { id, result, error: None },
                    Err(error) => Response { id, result: None, error: Some(lsp_server::ResponseError {
                        code: -32603, message: format!("Internal server error: {error}"), data: None
                    }) },
                };
                connection.sender.send(Message::Response(response))?;
            }
            Message::Notification(notification) => backend.handle_notification(notification)?,
            Message::Response(_) => {}
        }
    }
    io_threads.join()?;
    Ok(())
}
