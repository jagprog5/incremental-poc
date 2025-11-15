use axum::routing::{get, post, put};
use axum::{Json, Router, extract::State};
use clap::Parser;
use notify::{RecommendedWatcher, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio;

type AppState = Arc<RwLock<ServerState>>;

/// keeps track of the changed paths
///
/// this is simple - it does not understand if a folder is renamed then all the
/// contained files also count as renamed
///
/// it also doesn't differentiate between folder and files paths. possibly
/// problematic, since a notify rename event doesn't know if it's a folder or
/// file being renamed
///
/// however it does know if a file is created then removed, it's like it was
/// never created in the first place
#[derive(Debug, Default)]
struct FileSystemChanges {
    // hashset in case of large size O(1)
    removed: HashSet<PathBuf>,
    // contains both creation and modification
    new: HashSet<PathBuf>,
}

#[derive(Debug)]
enum ServerState {
    Ok(FileSystemChanges),
    /// needs full rescan
    ///
    /// number of file changes passed configured limit
    TooManyChanges,
    /// needs full rescan due to erroneous state
    ///
    /// e.g. the server couldn't keep up with the rate of changes produced
    /// 
    /// has priority over TooManyChanges (can transition from it to this)
    ChangesErroneousDropped,
}

impl Default for ServerState {
    fn default() -> Self {
        ServerState::Ok(FileSystemChanges::default())
    }
}

#[derive(Parser, Debug)]
#[command(name = "incremental-agent")]
#[command(about = "Keeps track of changes since previous scan", long_about = None)]
struct Cli {
    /// Path to the file or folder to watch. Watches recursively if a folder is
    /// specified
    file_path: PathBuf,

    /// Maximum number of changes to track
    #[arg(long = "limit", default_value_t = 10000)]
    change_limit: usize,

    /// Set the logging level (error, warn, info, debug, trace)
    #[arg(short = 'l', long = "log-level", default_value = "info")]
    log_level: String,

    /// Bind address for the HTTP server
    #[arg(long = "bind", default_value = "0.0.0.0")]
    bind_addr: String,

    /// Port for the HTTP server
    #[arg(long = "port", default_value_t = 8080)]
    port: u16,
}

async fn reset_handler(State(state): State<AppState>) {
    let mut state = state.write().unwrap();
    *state = ServerState::Ok(FileSystemChanges::default());
}

#[derive(Serialize)]
struct FileSystemChangeStats {
    removed: usize,
    new: usize,
}

/// mirrors ServerState
#[derive(Serialize)]
enum ServerStats {
    Ok(FileSystemChangeStats),
    TooManyChanges,
    ChangesErroneousDropped,
}

impl ServerStats {
    fn from_state(state: &ServerState) -> Self {
        match state {
            ServerState::Ok(changes) => ServerStats::Ok(FileSystemChangeStats {
                removed: changes.removed.len(),
                new: changes.new.len(),
            }),
            ServerState::TooManyChanges => ServerStats::TooManyChanges,
            ServerState::ChangesErroneousDropped => ServerStats::ChangesErroneousDropped,
        }
    }
}

async fn stats_handler(State(state): State<AppState>) -> Json<ServerStats> {
    let guard = state.read().unwrap();
    Json(ServerStats::from_state(&*guard))
}

#[derive(Deserialize)]
struct ChangesRequest {
    size: usize,
}

#[derive(Serialize)]
struct ReturnVal {
    paths: Vec<PathBuf>,
    done: bool,
}

#[derive(Serialize)]
enum ServerNewChangesResponse {
    New(ReturnVal),
    TooManyChanges,
    ChangesErroneousDropped,
}

#[derive(Serialize)]
enum ServerRemoveChangesResponse {
    Removed(ReturnVal),
    TooManyChanges,
    ChangesErroneousDropped,
}

/// calling this endpoint not only gets a page of changes, it also drains it
/// from the server's state. the scanner should continually call this until it
/// is completely drained
async fn drain_new_handler(
    State(state): State<AppState>,
    Json(mut req): Json<ChangesRequest>,
) -> Json<ServerNewChangesResponse> {
    let mut state = state.write().unwrap();

    let ok_state = match &mut *state {
        ServerState::Ok(v) => v,
        ServerState::TooManyChanges => return axum::Json(ServerNewChangesResponse::TooManyChanges),
        ServerState::ChangesErroneousDropped => return axum::Json(ServerNewChangesResponse::ChangesErroneousDropped),
    };

    if req.size > 1000 {
        req.size = 1000;
    }

    let mut drained = ReturnVal { paths: Vec::new(), done: false };

    for _ in 0..req.size {
        // this is the fastest but not very efficient for hash set
        let elem = ok_state.new.iter().next().cloned();
        match elem {
            Some(elem) => {
                drained.paths.push(elem.clone());
                ok_state.new.remove(&elem);
            },
            None => {
                drained.done = true;
                break;
            },
        }
    }
    axum::Json(ServerNewChangesResponse::New(drained))
}

async fn drain_removed_handler(
    State(state): State<AppState>,
    Json(mut req): Json<ChangesRequest>,
) -> Json<ServerRemoveChangesResponse> {
    let mut state = state.write().unwrap();

    let ok_state = match &mut *state {
        ServerState::Ok(v) => v,
        ServerState::TooManyChanges => return axum::Json(ServerRemoveChangesResponse::TooManyChanges),
        ServerState::ChangesErroneousDropped => return axum::Json(ServerRemoveChangesResponse::ChangesErroneousDropped),
    };

    if req.size > 1000 {
        req.size = 1000;
    }

    let mut drained = ReturnVal { paths: Vec::new(), done: false };

    for _ in 0..req.size {
        // this is the fastest but not very efficient for hash set
        let elem = ok_state.removed.iter().next().cloned();
        match elem {
            Some(elem) => {
                drained.paths.push(elem.clone());
                ok_state.removed.remove(&elem);
            },
            None => {
                drained.done = true;
                break;
            },
        }
    }
    axum::Json(ServerRemoveChangesResponse::Removed(drained))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&cli.log_level))
        .init();
    log::debug!("{:?}", cli);

    let server_state: AppState = Arc::new(RwLock::new(ServerState::default()));
    let mut watcher = {
        let server_state = server_state.clone();
        RecommendedWatcher::new(
            move |event: Result<notify::Event, notify::Error>| {
                let mut state_lock = server_state.write().unwrap();

                let mut event = match event {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("notify sent err: {:?}", e); // never?
                        *state_lock = ServerState::ChangesErroneousDropped;
                        return;
                    }
                };
                
                if event.need_rescan() {
                    // to test:
                    // sysctl -w fs.inotify.max_queued_events=2
                    // while :; do touch stress/file_{1..20000}; rm -f stress/file_{1..20000}; done
                    //
                    // set back to default:
                    // sysctl -w fs.inotify.max_queued_events=16384
                    log::error!("notify sent rescan event"); // e.g. server can't keep up
                    *state_lock = ServerState::ChangesErroneousDropped;
                    return;
                }

                let ok_state = match &mut *state_lock {
                    ServerState::Ok(v) => v,
                    _ => return,
                };

                match event.kind {
                    notify::EventKind::Create(_) => {
                        // from testing, this contains many files in one (notify lib accumulates them?)
                        event.paths.drain(..).for_each(|path| {
                            ok_state.removed.remove(&path);
                            ok_state.new.insert(path);
                        });
                    },
                    notify::EventKind::Remove(_) => {
                        event.paths.drain(..).for_each(|path| {
                            ok_state.new.remove(&path);
                            ok_state.removed.insert(path);
                        });
                    },
                    notify::EventKind::Modify(modify_kind) => {
                        match modify_kind {
                            notify::event::ModifyKind::Name(rename_mode) => {
                                match rename_mode {
                                    notify::event::RenameMode::To => {
                                        event.paths.drain(..).for_each(|path| {
                                            ok_state.removed.remove(&path);
                                            ok_state.new.insert(path);
                                        });
                                    },
                                    notify::event::RenameMode::From => {
                                        event.paths.drain(..).for_each(|path| {
                                            ok_state.new.remove(&path);
                                            ok_state.removed.insert(path);
                                        });
                                    },
                                    notify::event::RenameMode::Both => {
                                        if event.paths.len() == 2  { // always
                                            // "in this exact order (from, to)"
                                            let to_path = event.paths.pop().unwrap();
                                            let from_path = event.paths.pop().unwrap();

                                            ok_state.new.remove(&from_path);
                                            ok_state.removed.insert(from_path);
                                            ok_state.removed.remove(&to_path);
                                            ok_state.new.insert(to_path);
                                        } else {
                                            debug_assert!(false, "notify rename both with != 2 paths");
                                        }
                                    },
                                    _ => { // any or other
                                        event.paths.drain(..).for_each(|path| {
                                            // treat as BOTH added and removed.
                                            // the exact action is unknown and
                                            // this is abnormal - safest marking
                                            // for the underlying scanner is to
                                            // treat it as both
                                            log::warn!("notify unknown rename event for {:?}", path);
                                            ok_state.new.insert(path.clone());
                                            ok_state.removed.insert(path);
                                        });
                                    }
                                }
                            },
                            _ => {
                                event.paths.drain(..).for_each(|path| {
                                    ok_state.removed.remove(&path);
                                    ok_state.new.insert(path);
                                });
                            }
                        }
                    },
                    _ => {}
                }

                let total_changes = ok_state.new.len() + ok_state.removed.len();
                if total_changes > cli.change_limit {
                    log::warn!("change limit exceeded: {}", cli.change_limit);
                    *state_lock = ServerState::TooManyChanges;
                }
            },
            notify::Config::default(),
        )?
    };

    watcher.watch(&cli.file_path, notify::RecursiveMode::Recursive)?;

    let app = Router::new()
        .route("/reset", put(reset_handler))
        .route("/stats", get(stats_handler))
        .route("/drain_new", post(drain_new_handler))
        .route("/drain_removed", post(drain_removed_handler)).with_state(server_state);

    let addr = format!("{}:{}", cli.bind_addr, cli.port);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
    Ok(())
}
