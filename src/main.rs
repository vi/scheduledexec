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

#[derive(Parser)]
struct Args {
    #[clap(flatten)]
    listen_spec: tokio_listener::ListenerAddressPositional,

    /// Prepend this command to all executed chunks
    prefix: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ScenarioEntry {
    /// Relative timestamp, in milliseconds
    t_ms: u64,

    /// Command line to execute
    cmd: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Scenario {
    entries: Vec<ScenarioEntry>,
}

#[derive(Serialize, Clone, Copy)]
enum Status {
    Idle,
    Completed,
    Errored,
    Aborted,
    WaitingForLine(usize),
    ExecutingLine(usize),
}

#[derive(Serialize)]
struct ReportEntry {
    out: String,
    err: String,
    exitcode: i32,
    timespan_ms: u64,
}
#[derive(Serialize)]
struct Report {
    error: bool,
    aborted: bool,
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
    cached_scenario: Arc<Mutex<Scenario>>,
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
            cached_scenario: Arc::new(Mutex::new(Scenario { entries: vec![] })),
        }
    }

    async fn start_agent(
        self,
        mut scenario: Scenario,
        updates: tokio::sync::watch::Sender<Status>,
    ) {
        scenario.entries.sort_unstable_by_key(|x| x.t_ms);
        let start = Instant::now();
        let mut report = Report {
            error: false,
            aborted: false,
            entries: Vec::with_capacity(scenario.entries.len()),
        };
        let prefix = self.prefix.clone();
        for (i, scenario_entry) in scenario.entries.into_iter().enumerate() {
            let _ = updates.send(Status::WaitingForLine(i));

            let ts = start + Duration::from_millis(scenario_entry.t_ms);
            let sleeper = tokio::time::sleep_until(ts);

            if sleeper.is_elapsed() {
                eprintln!("Too late");
                report.error = true;
                break;
            }

            sleeper.await;

            let _ = updates.send(Status::ExecutingLine(i));

            if prefix.is_none() && scenario_entry.cmd.is_empty() {
                continue;
            }
            let mut chunks = scenario_entry.cmd.into_iter();

            let mut cmd =
                tokio::process::Command::new(prefix.clone().unwrap_or(chunks.next().unwrap()));
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
async fn status(State(app): State<App>) -> Json<Status> {
    let s = app.st();
    Json(s.get_simple_status())
}

#[debug_handler]
async fn downloadreport(State(app): State<App>) -> Result<Json<Arc<Report>>, StatusCode> {
    let s = app.st();
    match &*s {
        AppState::Completed { report } => Ok(Json(report.clone())),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

#[debug_handler]
async fn getscenario(State(app): State<App>) -> Json<Scenario> {
    Json(app.cached_scenario.lock().unwrap().clone())
}

#[debug_handler]
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

#[debug_handler]
async fn start(State(app): State<App>, Json(scenario): Json<Scenario>) -> StatusCode {
    let mut s = app.st();
    if matches!(*s, AppState::Running { .. }) {
        return StatusCode::CONFLICT;
    }

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

#[debug_handler]
async fn help() -> &'static str {
    "/start - begin executing a scenario
/abort - abort currenty executed scenario
/status - query information
/monitor - subscribe to events     
/getscenario - get current or last scenario
/report - download final scenario report
"
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
        .route("/start", post(start))
        .with_state(state);

    tokio_listener::axum07::serve(l, app.into_make_service()).await?;
    Ok(())
}
