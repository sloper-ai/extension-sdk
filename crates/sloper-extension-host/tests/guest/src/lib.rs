#![cfg(target_arch = "wasm32")]

use std::{
    env::{
        args_os,
        current_dir,
        set_current_dir,
        vars_os,
    },
    fmt::Debug,
    fs,
    hint::{
        black_box,
        spin_loop,
    },
    io,
    io::{
        Read,
        Write,
    },
    net::{
        TcpListener,
        TcpStream,
        ToSocketAddrs,
        UdpSocket,
    },
    sync::atomic::{
        AtomicU32,
        Ordering,
    },
    time::{
        Instant,
        SystemTime,
    },
};

#[allow(
    clippy::all,
    clippy::module_name_repetitions,
    clippy::pedantic,
    reason = "Generated canonical ABI bindings are owned by wit-bindgen."
)]
mod bindings {
    wit_bindgen::generate!({
        path: concat!(env!("OUT_DIR"), "/wit"),
        world: "extension",
        generate_all,
    });
}
use bindings::{
    exports,
    sloper,
    wasi,
    wit_stream,
};
use exports::sloper::extension::action::{
    Failure,
    Guest,
    Request,
    Retry,
};
use sloper::api::{
    credentials,
    errors,
    log,
    resources,
    sources,
};

// SAFETY: this custom section contains metadata only and creates no executable
// symbols.
#[used]
#[unsafe(link_section = "sloper:parts")]
static PARTS: [u8; include_bytes!("parts.json").len()] = *include_bytes!("parts.json");

static RUNS: AtomicU32 = AtomicU32::new(0);

struct Extension;
bindings::export!(Extension with_types_in bindings);

impl Guest for Extension {
    async fn run(request: Request) -> Result<(), Failure> {
        match request.action.as_str() {
            "probe" => probe(),
            "scratch" => scratch(&request.parameters),
            "tcp" => tcp(&request.parameters),
            "http" => http(&request.parameters),
            "allowed" => {
                let token = credentials::access_token("ledger".into()).await.map_err(failed)?;
                log::write(log::Level::Info, &format!("issued {}", token.value));
                Ok(())
            },
            "log-flood" => {
                let parameters: serde_json::Value = serde_json::from_str(&request.parameters).map_err(failed)?;
                if parameters["redact"].as_bool().unwrap_or(false) {
                    credentials::access_token("ledger".into()).await.map_err(failed)?;
                }
                let count = parameters["count"]
                    .as_u64()
                    .ok_or_else(|| failed("missing log count"))?;
                let message = parameters["message"]
                    .as_str()
                    .ok_or_else(|| failed("missing log message"))?;
                for _ in 0..count {
                    log::write(log::Level::Info, message);
                }
                log::write(log::Level::Info, "must-not-appear-after-drop");
                Ok(())
            },
            "denied" => {
                require(
                    credentials::access_token("ledger".into()).await.is_err(),
                    "other-action connection granted",
                )?;
                require(sources::open("unlent".into()).await.is_err(), "unlent source granted")?;
                require(
                    resources::open("items".into(), None).await.is_err(),
                    "other-action writer granted",
                )?;
                require(
                    resources::read("items".into()).await.is_err(),
                    "other-action reader granted",
                )
            },
            "writer" => {
                let writer = resources::open("items".into(), None).await.map_err(failed)?;
                require(
                    resources::open("items".into(), None).await.is_err(),
                    "duplicate writer granted",
                )?;
                drop(writer);
                require(
                    resources::open("items".into(), None).await.is_err(),
                    "dropped writer reopened",
                )
            },
            "write-bytes" => write_bytes(&request.parameters).await,
            "operation-write" | "operation-retry" | "operation-cancel" => operation(&request).await,
            "read-bytes" => {
                resources::read("items".into()).await.map_err(failed)?;
                Ok(())
            },
            "source-failed" => {
                let _ignored = produce_source("document", b"source content".to_vec()).await;
                Ok(())
            },
            "source-produced" => {
                let id = produce_source("document", b"source content".to_vec()).await?;
                let input = sources::open(id).await.map_err(failed)?;
                require(
                    input.collect().await == b"source content",
                    "produced source content differs",
                )
            },
            "source-property" => {
                let parameters: serde_json::Value = serde_json::from_str(&request.parameters)
                    .map_err(|error| Failure::InvalidParameters(error.to_string()))?;
                let property = parameters["property"]
                    .as_str()
                    .ok_or_else(|| Failure::InvalidParameters("property is required".into()))?;
                produce_source(property, b"source content".to_vec()).await?;
                Ok(())
            },
            "source-oversize" => {
                let _ignored = produce_source("document", vec![0; 1025]).await;
                Ok(())
            },
            "reader" => {
                let page = resources::read("items".into()).await.map_err(failed)?;
                require(page.is_some(), "projected item page absent")?;
                let input = sources::open("projected".into()).await.map_err(failed)?;
                require(
                    input.collect().await == b"projected content",
                    "projected source content differs",
                )
            },
            "memory" => {
                let data = vec![1_u8; 600 * 1024 * 1024];
                black_box(data);
                Err(Failure::Internal("memory ceiling was not enforced".into()))
            },
            "source-read" => {
                let input = sources::open("lent".into()).await.map_err(failed)?;
                let _ignored = input.collect().await;
                Ok(())
            },
            "spin" => {
                log::write(log::Level::Info, "spinning");
                loop {
                    spin_loop();
                }
            },
            "wait" => {
                credentials::access_token("ledger".into()).await.map_err(failed)?;
                Ok(())
            },
            "counter" => require(RUNS.fetch_add(1, Ordering::Relaxed) == 0, "guest instance was reused"),
            _ => Err(Failure::InvalidParameters("unknown test action".into())),
        }
    }
}

async fn produce_source(property: &str, bytes: Vec<u8>) -> Result<String, Failure> {
    let writer = resources::open("items".into(), None).await.map_err(failed)?;
    let (mut output, input) = wit_stream::new::<u8>();
    wit_bindgen::spawn_local(async move {
        output.write_all(bytes).await;
    });
    writer
        .source(property.into(), "file.txt".into(), input)
        .await
        .map_err(failed)
}

async fn write_bytes(parameters: &str) -> Result<(), Failure> {
    let parameters: serde_json::Value = serde_json::from_str(parameters).map_err(failed)?;
    if parameters["redact"].as_bool().unwrap_or(false) {
        credentials::access_token("ledger".into()).await.map_err(failed)?;
    }
    let token_count = parameters["tokenCount"].as_u64().unwrap_or(0) as usize;
    let mut items = Vec::new();
    for size in parameters["sizes"]
        .as_array()
        .ok_or_else(|| Failure::Internal("sizes missing".into()))?
    {
        let size = size.as_u64().ok_or_else(|| Failure::Internal("size invalid".into()))? as usize;
        let empty = r#"{"id":"one","value":""}"#;
        let mut item = String::with_capacity(size);
        item.push_str(r#"{"id":"one","value":""#);
        item.push_str(&"x".repeat(token_count));
        item.push_str(&"z".repeat(size - empty.len() - token_count));
        item.push_str("\"}");
        items.push(item);
    }
    let writer = resources::open("items".into(), Some("ledger".into()))
        .await
        .map_err(failed)?;
    let (mut output, input) = wit_stream::new::<String>();
    wit_bindgen::spawn_local(async move {
        output.write_all(items).await;
    });
    writer.write(input).await.map_err(failed)
}

async fn operation(request: &Request) -> Result<(), Failure> {
    let parameters: serde_json::Value = serde_json::from_str(&request.parameters).map_err(failed)?;
    let configuration: serde_json::Value = serde_json::from_str(&request.configuration).map_err(failed)?;
    let id = parameters["id"].as_str().unwrap_or("operation-item");
    let value = configuration["label"]
        .as_str()
        .or_else(|| parameters["value"].as_str())
        .unwrap_or("operation-value");
    let item = serde_json::json!({"id":id,"value":value}).to_string();
    let writer = resources::open("items".into(), None).await.map_err(failed)?;
    let (mut output, input) = wit_stream::new::<String>();
    wit_bindgen::spawn_local(async move {
        output.write_all(vec![item]).await;
    });
    writer.write(input).await.map_err(failed)?;
    writer.checkpoint("banked".into()).await.map_err(failed)?;
    if request.action == "operation-retry"
        && !request
            .cursors
            .iter()
            .any(|cursor| cursor.name == "items" && cursor.value == "banked")
    {
        return Err(Failure::Unavailable(Retry {
            message: "retry after checkpoint".into(),
            not_before: None,
        }));
    }
    if request.action == "operation-cancel" {
        loop {
            match writer.checkpoint("banked".into()).await {
                Ok(()) => {},
                Err(errors::Error::Cancelled) => return Ok(()),
                Err(error) => return Err(failed(error)),
            }
        }
    }
    Ok(())
}

fn require(condition: bool, message: &str) -> Result<(), Failure> {
    if condition {
        Ok(())
    } else {
        Err(Failure::Internal(message.into()))
    }
}

fn failed(error: impl Debug) -> Failure {
    Failure::Internal(format!("{error:?}"))
}

fn probe() -> Result<(), Failure> {
    require(vars_os().next().is_none(), "inherited environment visible")?;
    require(args_os().next().is_none(), "inherited arguments visible")?;
    require(fs::read_dir("/").is_err(), "host root directory readable")?;
    let mut stdin_bytes = [0];
    require(
        io::stdin().read(&mut stdin_bytes).map_err(failed)? == 0,
        "stdin inherited",
    )?;
    require(
        TcpListener::bind("127.0.0.1:0").is_err(),
        "inbound TCP listener granted",
    )?;
    require(UdpSocket::bind("127.0.0.1:0").is_err(), "UDP socket granted")?;
    let addresses = ("localhost", 80).to_socket_addrs().map_err(failed)?.collect::<Vec<_>>();
    require(
        !addresses.is_empty() && addresses.iter().all(|address| address.ip().is_loopback()),
        "localhost name lookup did not return loopback addresses",
    )?;
    let _time = (SystemTime::now(), Instant::now());
    require(
        wasi::random::random::get_random_bytes(16).len() == 16,
        "WASI randomness unavailable",
    )
}

fn scratch(parameters: &str) -> Result<(), Failure> {
    let parameters: serde_json::Value = serde_json::from_str(parameters).map_err(failed)?;
    let host_path = parameters["hostPath"]
        .as_str()
        .ok_or_else(|| Failure::InvalidParameters("hostPath is missing".into()))?;
    let host_filename = parameters["hostFilename"]
        .as_str()
        .ok_or_else(|| Failure::InvalidParameters("hostFilename is missing".into()))?;
    let initial_directory = wasi::cli::environment::initial_cwd()
        .ok_or_else(|| Failure::Internal("initial working directory is missing".into()))?;
    require(initial_directory == "/tmp", "initial working directory is not scratch")?;
    // Reactor components do not run libc's command startup. Match the SDK's
    // dispatch initialization before exercising standard-library relative paths.
    set_current_dir(initial_directory).map_err(failed)?;
    let working_directory = current_dir().map_err(failed)?;
    require(
        working_directory.to_str() == Some("/tmp"),
        &format!("scratch is not the working directory: {working_directory:?}"),
    )?;
    require(
        fs::read_dir(".").map_err(failed)?.next().is_none(),
        "prior-run scratch files visible",
    )?;
    fs::create_dir("nested").map_err(failed)?;
    fs::write("nested/value.txt", b"scratch content").map_err(failed)?;
    require(
        fs::read("/tmp/nested/value.txt").map_err(failed)? == b"scratch content",
        "relative and absolute scratch paths differ",
    )?;
    fs::rename("nested/value.txt", "/tmp/nested/renamed.txt").map_err(failed)?;
    require(
        fs::read("nested/renamed.txt").map_err(failed)? == b"scratch content",
        "scratch rename lost the file contents",
    )?;
    fs::remove_file("nested/renamed.txt").map_err(failed)?;
    // Leave a file behind so the next run detects accidental scratch reuse.
    fs::write("retained.txt", b"must remain private to this run").map_err(failed)?;
    require(fs::read(host_path).is_err(), "existing host file readable")?;
    require(
        fs::OpenOptions::new().write(true).open(host_path).is_err(),
        "existing host file openable for writing",
    )?;
    for path in [format!("/tmp/../{host_filename}"), format!("../{host_filename}")] {
        require(fs::read(&path).is_err(), "host file readable outside scratch")?;
        require(
            fs::write(&path, b"escaped scratch").is_err(),
            "host file writable outside scratch",
        )?;
    }
    // The guest's /tmp can have the same spelling as the host's temporary
    // directory. Creating this name must write only the guest's scratch file;
    // the host test checks that its sentinel retains the original bytes.
    let shadow = format!("/tmp/{host_filename}");
    fs::write(&shadow, b"guest scratch content").map_err(failed)?;
    require(
        fs::read(&shadow).map_err(failed)? == b"guest scratch content",
        "scratch shadow does not contain the guest's bytes",
    )?;
    Ok(())
}

fn tcp(parameters: &str) -> Result<(), Failure> {
    let parameters: serde_json::Value = serde_json::from_str(parameters).map_err(failed)?;
    let address = parameters["address"]
        .as_str()
        .ok_or_else(|| Failure::InvalidParameters("address is missing".into()))?;
    let mut socket = TcpStream::connect(address).map_err(failed)?;
    socket.write_all(b"guest-tcp").map_err(failed)?;
    let mut bytes = [0; 8];
    socket.read_exact(&mut bytes).map_err(failed)?;
    require(&bytes == b"host-tcp", "TCP response body differs")
}

fn http(parameters: &str) -> Result<(), Failure> {
    use wasi::http::{
        outgoing_handler,
        types::{
            Fields,
            IncomingBody,
            Method,
            OutgoingBody,
            OutgoingRequest,
            Scheme,
        },
    };
    let parameters: serde_json::Value = serde_json::from_str(parameters).map_err(failed)?;
    let authority = parameters["authority"]
        .as_str()
        .ok_or_else(|| Failure::InvalidParameters("authority is missing".into()))?;
    let request = OutgoingRequest::new(Fields::new());
    request.set_method(&Method::Get).map_err(failed)?;
    request.set_scheme(Some(&Scheme::Http)).map_err(failed)?;
    request.set_authority(Some(authority)).map_err(failed)?;
    request.set_path_with_query(Some("/host-test")).map_err(failed)?;
    OutgoingBody::finish(request.body().map_err(failed)?, None).map_err(failed)?;
    let response = outgoing_handler::handle(request, None).map_err(failed)?;
    response.subscribe().block();
    let response = response
        .get()
        .ok_or_else(|| Failure::Internal("HTTP response absent".into()))?
        .map_err(failed)?
        .map_err(failed)?;
    require(response.status() == 200, "HTTP response status differs")?;
    let body = response.consume().map_err(failed)?;
    let stream = body.stream().map_err(failed)?;
    let bytes = stream.blocking_read(1024).map_err(failed)?;
    drop(stream);
    drop(IncomingBody::finish(body));
    require(bytes == b"host-http", "HTTP response body differs")
}
