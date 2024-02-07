use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive};
use axum::response::Sse;
use axum::routing::post;
use axum::{debug_handler, Json};
use axum::{routing::get, Router};
use clap::Parser;
use futures::stream::StreamExt;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// REST service to execute series of command lines at specific moments of time
#[derive(Parser)]
struct Args {
    #[clap(flatten)]
    listen_spec: tokio_listener::ListenerAddressPositional,

    /// Prepend this command to all executed chunks
    prefix: Option<String>,
}

/// Single step in a scenario
#[derive(Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
struct ScenarioEntry {
    /// Relative timestamp, in milliseconds
    t_ms: u64,

    /// Command line to execute, as indidivual argv elements
    cmd: Vec<String>,
}

/// Timed sequence of command lines to be executed remotely
#[derive(Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
struct Scenario {
    /// Steps to be executed in this scenario
    entries: Vec<ScenarioEntry>,
}

/// Current status of the service
#[derive(Serialize, Clone, Copy)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(tag = "status")]
enum Status {
    /// No scenario has been started
    Idle,
    /// Scenario was finished successfully
    Completed,
    /// Scenario was finished with error
    Errored,
    /// Scenario was aborted externally
    Aborted,
    /// Scenario is ongoing: waiting for a specified moment to execute an entry
    WaitingForLine {
        /// index of a scenario entry
        line: usize,
    },
    /// Scenario is ongoing: waiting for a spawned process to finish
    ExecutingLine {
        /// index of a scenario entry
        line: usize,
    },
}

/// Information about execution outcome of a specific step from a scenario
#[derive(Serialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
struct ReportEntry {
    /// Content of stdout of the executed command line. Non-UTF-8 gets mangled.
    out: String,
    /// Content of stderr of the executed command line. Non-UTF-8 gets mangled.
    err: String,
    /// Exit code of the executed command line
    exitcode: i32,
    /// Number of milliseconds spent in executing the command.
    timespan_ms: u64,
}
/// Summary of a completed scenario
#[derive(Serialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
struct Report {
    /// Whether the scenario finished with error, i.e. non-successful exit code of a command or missed deadline to execute some command
    error: bool,
    /// Whether the scenario was aborted by /abort call. No detailed information about the steps would be provided in this case.
    aborted: bool,
    /// Output of command lines executions involved in the scenario
    entries: Vec<ReportEntry>,
}

enum AppState {
    Idle,
    Running {
        watch: tokio::sync::watch::Receiver<Status>,
        jh: tokio::task::JoinHandle<()>,
    },
    Completed {
        report: Arc<Report>,
    },
}

impl AppState {
    fn get_simple_status(&self) -> Status {
        match self {
            AppState::Idle => Status::Idle,
            AppState::Running { watch, .. } => *watch.borrow(),
            AppState::Completed { report } => {
                if report.aborted {
                    Status::Aborted
                } else {
                    if report.error {
                        Status::Errored
                    } else {
                        Status::Completed
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct App {
    state: Arc<Mutex<AppState>>,
    monitor: tokio::sync::broadcast::Sender<Status>,
    prefix: Option<String>,
    cached_scenario: Arc<Mutex<Arc<Scenario>>>,
}

impl App {
    fn st(&self) -> std::sync::MutexGuard<'_, AppState> {
        self.state.lock().unwrap()
    }

    fn new(prefix: Option<String>) -> Self {
        App {
            state: Arc::new(Mutex::new(AppState::Idle)),
            monitor: tokio::sync::broadcast::Sender::new(1),
            prefix,
            cached_scenario: Arc::new(Mutex::new(Arc::new(Scenario { entries: vec![] }))),
        }
    }

    async fn start_agent(
        self,
        scenario: Arc<Scenario>,
        updates: tokio::sync::watch::Sender<Status>,
    ) {
        let start = Instant::now();
        let mut report = Report {
            error: false,
            aborted: false,
            entries: Vec::with_capacity(scenario.entries.len()),
        };
        let prefix = self.prefix.clone();
        for (i, scenario_entry) in scenario.entries.iter().enumerate() {
            let _ = updates.send(Status::WaitingForLine { line: i });

            let ts = start + Duration::from_millis(scenario_entry.t_ms);
            let sleeper = tokio::time::sleep_until(ts);

            if sleeper.is_elapsed() {
                eprintln!("Too late");
                report.error = true;
                break;
            }

            sleeper.await;

            let _ = updates.send(Status::ExecutingLine { line: i });

            if prefix.is_none() && scenario_entry.cmd.is_empty() {
                continue;
            }
            let mut chunks = scenario_entry.cmd.iter();

            let mut cmd = tokio::process::Command::new(
                prefix.clone().unwrap_or_else(||chunks.next().unwrap().clone()),
            );
            for ch in chunks {
                cmd.arg(ch);
            }
            let now = Instant::now();
            match cmd.output().await {
                Ok(ret) => {
                    let re = ReportEntry {
                        timespan_ms: Instant::now().duration_since(now).as_millis() as u64,
                        exitcode: ret.status.code().unwrap_or(-1),
                        out: String::from_utf8_lossy(&ret.stdout).to_string(),
                        err: String::from_utf8_lossy(&ret.stderr).to_string(),
                    };
                    report.entries.push(re);
                    if !ret.status.success() {
                        eprintln!("Command failed");
                        report.error = true;
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    report.error = true;
                    break;
                }
            }
        }
        if report.error {
            let _ = updates.send(Status::Errored);
        } else {
            let _ = updates.send(Status::Completed);
        }
        *self.st() = AppState::Completed {
            report: Arc::new(report),
        };
    }
}


#[debug_handler]
#[cfg_attr(feature = "utoipa", utoipa::path(
    post,
    path = "/status",
    responses(
        (status = 200, description = "Curent status of the scheduledexec instance", body=Status),
    )
))]
async fn status(State(app): State<App>) -> Json<Status> {
    let s = app.st();
    Json(s.get_simple_status())
}

#[debug_handler]
#[cfg_attr(feature = "utoipa", utoipa::path(
    post,
    path = "/downloadreport",
    responses(
        (status = 200, description = "Report of a finished scenario", body=Report),
        (status = 404, description = "No finished scenario"),
    )
))]
async fn downloadreport(State(app): State<App>) -> Result<Json<Arc<Report>>, StatusCode> {
    let s = app.st();
    match &*s {
        AppState::Completed { report } => Ok(Json(report.clone())),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

#[debug_handler]
#[cfg_attr(feature = "utoipa", utoipa::path(
    post,
    path = "/getscenario",
    responses(
        (status = 200, description = "Recently started scenario or empty scenario if nothing was started so far", body = Scenario),
    )
))]
async fn getscenario(State(app): State<App>) -> Json<Arc<Scenario>> {
    Json(app.cached_scenario.lock().unwrap().clone())
}

#[debug_handler]
#[cfg_attr(feature = "utoipa", utoipa::path(
    post,
    path = "/monitor",
    responses(
        (status = 200, description = "Obtain SSE stream with /status updates", content_type="text/event-stream", body = ()),
    )
))]
async fn monitor(
    State(app): State<App>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let initial: Status = app.st().get_simple_status();

    let stream = tokio_stream::wrappers::BroadcastStream::new(app.monitor.subscribe());
    // Note: this is where events get lost for sluggish readers
    let stream = stream.filter_map(|x: Result<Status, BroadcastStreamRecvError>| async { x.ok() });
    let stream = futures::stream::once(async move { initial }).chain(stream);
    let stream = stream.map(|x| Ok(Event::default().data(serde_json::to_string(&x).unwrap())));

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Upload and start a scenario
#[cfg_attr(feature = "utoipa", utoipa::path(
    post,
    path = "/start",
    responses(
        (status = 200, description = "Scenario has been started"),
        (status = 409, description = "Scenario is already running"),
    )
))]
#[debug_handler]
async fn start(State(app): State<App>, Json(mut scenario): Json<Scenario>) -> StatusCode {
    let mut s = app.st();
    if matches!(*s, AppState::Running { .. }) {
        return StatusCode::CONFLICT;
    }

    scenario.entries.sort_unstable_by_key(|x| x.t_ms);
    let scenario = Arc::new(scenario);

    let (tx, rx) = tokio::sync::watch::channel(Status::Idle);

    *app.cached_scenario.lock().unwrap() = scenario.clone();

    let app2 = app.clone();
    let jh = tokio::spawn(async move { app2.start_agent(scenario, tx).await });

    let mut rx2 = rx.clone();
    let br = app.monitor.clone();

    *s = AppState::Running { watch: rx, jh };

    tokio::spawn(async move {
        while let Ok(()) = rx2.changed().await {
            let _ = br.send(*rx2.borrow());
        }
    });

    StatusCode::OK
}

/// Abort the ongoing scenario
#[cfg_attr(feature = "utoipa", utoipa::path(
    get,
    path = "/abort",
    responses(
        (status = 200, description = "Scenario has been aborted"),
        (status = 404, description = "No scenario was ongoing"),
    )
))]
#[debug_handler]
async fn abort(State(app): State<App>) -> StatusCode {
    let mut s = app.st();
    match &*s {
        AppState::Running { jh, .. } => {
            jh.abort();
            *s = AppState::Completed {
                report: Arc::new(Report {
                    error: false,
                    aborted: true,
                    entries: vec![],
                }),
            };
            let _ = app.monitor.send(Status::Aborted);
            StatusCode::OK
        }
        _ => StatusCode::NOT_FOUND,
    }
}

/// Show short help about REST paths
#[cfg_attr(feature = "utoipa", utoipa::path(
    get,
    path = "/",
    responses(
        (status = 200, description = "Plain text")
    )
))]
#[debug_handler]
async fn help() -> &'static str {
    "/start - begin executing a scenario
/abort - abort currenty executed scenario
/status - query information
/monitor - subscribe to events     
/getscenario - get current or last scenario
/report - download final scenario report
/api-docs/openapi.json - download JSON with OpenAPI specification
"
}

#[cfg(feature = "utoipa")]
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        openapi,
        help,
        abort,
        start,
        status,
        monitor,
        getscenario,
        downloadreport,
    ),
    components(schemas(Scenario, ScenarioEntry, ReportEntry, Report, Status))
)]
struct OpenApiDoc;


#[cfg(feature = "utoipa")]
#[debug_handler]
#[utoipa::path(
    get,
    path = "/api-docs/openapi.json",
    responses(
        (status = 200, description = "JSON file")
    )
)]
async fn openapi() -> Result<String, StatusCode> {
    use utoipa::OpenApi;
    match OpenApiDoc::openapi().to_pretty_json() {
        Ok(x) => Ok(x),
        Err(e) => {
            eprintln!("Error: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Args = Args::parse();
    let l = a.listen_spec.bind().await?;
    let state = App::new(a.prefix);
    let app = Router::new()
        .route("/", get(help))
        .route("/status", get(status))
        .route("/monitor", get(monitor))
        .route("/getscenario", get(getscenario))
        .route("/report", get(downloadreport))
        .route("/abort", post(abort))
        .route("/start", post(start));

    #[cfg(feature = "utoipa")]
    let app = app.route("/api-docs/openapi.json", get(openapi));

    let app = app.with_state(state);

    tokio_listener::axum07::serve(l, app.into_make_service()).await?;
    Ok(())
}
