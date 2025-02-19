use crate::{
    clickhouse::{LogRow, RequestRow},
    cronjob::Cronjob,
    deployments::{cache::run_cache_clear_task, pubsub::listen_pub_sub, Deployments},
    get_region, SNAPSHOT_BLOB,
};
use anyhow::Result;
use clickhouse::{inserter::Inserter, Client};
use dashmap::DashMap;
use futures::lock::Mutex;
use hyper::{
    header::HOST,
    http::response::Builder,
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use lagon_runtime_http::{RunResult, X_LAGON_ID};
use lagon_runtime_isolate::{
    options::{IsolateOptions, Metadata},
    Isolate, IsolateEvent, IsolateRequest,
};
use lagon_runtime_utils::{
    assets::{find_asset, handle_asset},
    response::{handle_response, ResponseEvent, FAVICON_URL, PAGE_403, PAGE_404},
    DEPLOYMENTS_DIR,
};
use lagon_serverless_downloader::Downloader;
use lagon_serverless_pubsub::PubSubListener;
use log::{as_debug, error, info, warn};
use metrics::{decrement_gauge, histogram, increment_counter, increment_gauge};
use std::{
    collections::HashSet,
    convert::Infallible,
    env,
    future::Future,
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, UNIX_EPOCH},
};
use tokio::{runtime::Handle, sync::Mutex as TokioMutex};

pub type Workers = Arc<DashMap<String, flume::Sender<IsolateEvent>>>;

async fn handle_error(
    result: RunResult,
    function_id: String,
    deployment_id: String,
    request_id: &String,
    inserters: Arc<Mutex<(Inserter<RequestRow>, Inserter<LogRow>)>>,
) {
    let (level, message) = match result {
        RunResult::Timeout => {
            increment_counter!("lagon_isolate_timeouts", "deployment" => deployment_id.clone(), "function" => function_id.clone());

            let message = "Function execution timed out";
            warn!(deployment = deployment_id, function = function_id, request = request_id; "{}", message);

            ("warn", message.into())
        }
        RunResult::MemoryLimit => {
            increment_counter!("lagon_isolate_memory_limits", "deployment" => deployment_id.clone(), "function" => function_id.clone());

            let message = "Function execution memory limit reached";
            warn!(deployment = deployment_id, function = function_id, request = request_id; "{}", message);

            ("warn", message.into())
        }
        RunResult::Error(error) => {
            increment_counter!("lagon_isolate_errors", "deployment" => deployment_id.clone(), "function" => function_id.clone());

            let message = format!("Function execution error: {}", error);
            error!(deployment = deployment_id, function = function_id, request = request_id; "{}", message);

            ("error", message)
        }
        _ => ("warn", "Unknown result".into()),
    };

    if let Err(error) = inserters
        .lock()
        .await
        .1
        .write(&LogRow {
            function_id,
            deployment_id,
            level: level.to_string(),
            message,
            region: get_region().clone(),
            timestamp: UNIX_EPOCH.elapsed().unwrap().as_secs() as u32,
        })
        .await
    {
        error!("Error while writing log: {}", error);
    }
}

async fn handle_request(
    req: Request<Body>,
    ip: String,
    deployments: Deployments,
    last_requests: Arc<DashMap<String, Instant>>,
    workers: Workers,
    inserters: Arc<Mutex<(Inserter<RequestRow>, Inserter<LogRow>)>>,
    log_sender: flume::Sender<(String, String, Metadata)>,
) -> Result<Response<Body>> {
    let request_id = match req.headers().get(X_LAGON_ID) {
        Some(x_lagon_id) => x_lagon_id.to_str().unwrap_or("").to_string(),
        None => String::new(),
    };

    let hostname = match req.headers().get(HOST) {
        Some(hostname) => hostname.to_str()?.to_string(),
        None => {
            increment_counter!(
                "lagon_ignored_requests",
                "reason" => "No hostname",
            );
            warn!(req = as_debug!(req), ip = ip, request = request_id; "No Host header found in request");

            return Ok(Builder::new().status(404).body(PAGE_404.into())?);
        }
    };

    let deployment = match deployments.get(&hostname) {
        Some(entry) => Arc::clone(entry.value()),
        None => {
            increment_counter!(
                "lagon_ignored_requests",
                "reason" => "No deployment",
                "hostname" => hostname.clone(),
            );
            warn!(req = as_debug!(req), ip = ip, hostname = hostname, request = request_id; "No deployment found for hostname");

            return Ok(Response::builder().status(404).body(PAGE_404.into())?);
        }
    };

    if deployment.cron.is_some() {
        increment_counter!(
            "lagon_ignored_requests",
            "reason" => "Cron",
            "hostname" => hostname.clone(),
        );
        warn!(req = as_debug!(req), ip = ip, hostname = hostname, request = request_id; "Cron deployment cannot be called directly");

        return Ok(Response::builder().status(403).body(PAGE_403.into())?);
    }

    let request_id_handle = request_id.clone();
    let (sender, receiver) = flume::unbounded();
    let mut bytes_in = 0;

    let url = req.uri().path();

    if let Some(asset) = find_asset(url, &deployment.assets) {
        let root = Path::new(env::current_dir().unwrap().as_path())
            .join(DEPLOYMENTS_DIR)
            .join(&deployment.id);

        let run_result = match handle_asset(root, asset) {
            Ok(response) => RunResult::Response(response, None),
            Err(error) => {
                error!(deployment = &deployment.id, asset = asset, request = request_id_handle; "Error while handing asset: {}", error);

                RunResult::Error("Could not retrieve asset.".into())
            }
        };

        sender.send_async(run_result).await.unwrap_or(());
    } else if url == FAVICON_URL {
        sender
            .send_async(RunResult::Response(
                Response::builder().status(404).body(Body::empty())?,
                None,
            ))
            .await
            .unwrap_or(());
    } else {
        last_requests.insert(deployment.id.clone(), Instant::now());

        let (parts, body) = req.into_parts();
        let body = hyper::body::to_bytes(body).await?;

        bytes_in = body.len() as u32;
        let request = (parts, body);

        let deployment = Arc::clone(&deployment);
        let isolate_workers = Arc::clone(&workers);

        let isolate_sender = workers.entry(deployment.id.clone()).or_insert_with(|| {
            let handle = Handle::current();
            let (sender, receiver) = flume::unbounded();

            std::thread::Builder::new().name(String::from("isolate-") + deployment.id.as_str()).spawn(move || {
                handle.block_on(async move {
                    increment_gauge!("lagon_isolates", 1.0, "deployment" => deployment.id.clone(), "function" => deployment.function_id.clone());
                    info!(deployment = deployment.id, function = deployment.function_id, request = request_id_handle; "Creating new isolate");

                    let code = deployment.get_code().unwrap_or_else(|error| {
                        error!(deployment = deployment.id, request = request_id_handle; "Error while getting deployment code: {}", error);

                        "".into()
                    });
                    let options = IsolateOptions::new(code)
                        .environment_variables(deployment.environment_variables.clone())
                        .memory(deployment.memory)
                        .tick_timeout(Duration::from_millis(deployment.tick_timeout as u64))
                        .total_timeout(Duration::from_millis(
                            deployment.total_timeout as u64,
                        ))
                        .metadata(Some((
                            deployment.id.clone(),
                            deployment.function_id.clone(),
                        )))
                        .on_drop_callback(Box::new(|metadata| {
                            if let Some(metadata) = metadata.as_ref().as_ref() {
                                let labels = [
                                    ("deployment", metadata.0.clone()),
                                    ("function", metadata.1.clone()),
                                ];

                                decrement_gauge!("lagon_isolates", 1.0, &labels);
                                info!(deployment = metadata.0, function = metadata.1; "Dropping isolate");
                            }
                        }))
                        .on_statistics_callback(Box::new(|metadata, statistics| {
                            if let Some(metadata) = metadata.as_ref().as_ref() {
                                let labels = [
                                    ("deployment", metadata.0.clone()),
                                    ("function", metadata.1.clone()),
                                ];

                                histogram!(
                                    "lagon_isolate_memory_usage",
                                    statistics as f64,
                                    &labels
                                );
                            }
                        }))
                        .log_sender(log_sender)
                        .snapshot_blob(SNAPSHOT_BLOB);

                    let mut isolate = Isolate::new(options, receiver);
                    isolate.evaluate();
                    isolate.run_event_loop().await;

                    // When the event loop is completed, that means a) the isolate was terminate due to limits
                    // or b) the isolate was dropped because of cache expiration. In the first case, the isolate
                    // isn't removed from the workers map
                    isolate_workers.remove(&deployment.id);
                });
            }).unwrap();

            sender
        });

        isolate_sender
            .send_async(IsolateEvent::Request(IsolateRequest { request, sender }))
            .await
            .unwrap_or(());
    }

    handle_response(receiver, Arc::clone(&deployment), move |event| {
        let inserters = Arc::clone(&inserters);
        let request_id = request_id.clone();
        let deployment = Arc::clone(&deployment);

        async move {
            match event {
                ResponseEvent::Bytes(bytes, cpu_time_micros) => {
                    let timestamp = UNIX_EPOCH.elapsed().unwrap().as_secs() as u32;

                    inserters
                        .lock()
                        .await
                        .0
                        .write(&RequestRow {
                            function_id: deployment.function_id.clone(),
                            deployment_id: deployment.id.clone(),
                            region: get_region().clone(),
                            bytes_in,
                            bytes_out: bytes as u32,
                            cpu_time_micros,
                            timestamp,
                        })
                        .await
                        .unwrap_or(());
                }
                ResponseEvent::StreamDoneNoDataError => {
                    handle_error(
                        RunResult::Error(
                            "The stream was done before sending a response/data".into(),
                        ),
                        deployment.function_id.clone(),
                        deployment.id.clone(),
                        &request_id,
                        inserters,
                    )
                    .await;
                }
                ResponseEvent::UnexpectedStreamResult(result) => {
                    handle_error(
                        result,
                        deployment.function_id.clone(),
                        deployment.id.clone(),
                        &request_id,
                        inserters,
                    )
                    .await;
                }
                ResponseEvent::LimitsReached(result) | ResponseEvent::Error(result) => {
                    handle_error(
                        result,
                        deployment.function_id.clone(),
                        deployment.id.clone(),
                        &request_id,
                        inserters,
                    )
                    .await;
                }
            }

            Ok(())
        }
    })
    .await
}

pub async fn start<D, P>(
    deployments: Deployments,
    addr: SocketAddr,
    downloader: Arc<D>,
    pubsub: P,
    client: Client,
) -> Result<impl Future<Output = ()> + Send>
where
    D: Downloader + Send + Sync + 'static,
    P: PubSubListener + Unpin + 'static,
{
    let last_requests = Arc::new(DashMap::new());
    let workers = Arc::new(DashMap::new());
    let pubsub = Arc::new(TokioMutex::new(pubsub));

    let insertion_interval = Duration::from_secs(1);
    let inserters = Arc::new(Mutex::new((
        client
            .inserter::<RequestRow>("serverless.requests")?
            .with_period(Some(insertion_interval)),
        client
            .inserter::<LogRow>("serverless.logs")?
            .with_period(Some(insertion_interval)),
    )));

    let (log_sender, log_receiver) = flume::unbounded::<(String, String, Metadata)>();
    let cronjob = Arc::new(TokioMutex::new(
        Cronjob::new(log_sender.clone(), Arc::clone(&inserters)).await,
    ));

    let mut cron_deployments = HashSet::new();

    for deployment in deployments.iter() {
        let deployment = deployment.value();

        // Make sure we only register the cron once, since each
        // deployment can have multiple domains
        if cron_deployments.contains(&deployment.id) {
            continue;
        }

        cron_deployments.insert(deployment.id.clone());

        if deployment.should_run_cron() {
            let mut cronjob = cronjob.lock().await;

            if let Err(error) = cronjob.add(deployment.clone()).await {
                error!("Failed to register cron: {}", error);
            }
        }
    }

    drop(cron_deployments);

    listen_pub_sub(
        Arc::clone(&downloader),
        Arc::clone(&deployments),
        Arc::clone(&workers),
        Arc::clone(&cronjob),
        pubsub,
    );
    run_cache_clear_task(Arc::clone(&last_requests), Arc::clone(&workers));

    let inserters_handle = Arc::clone(&inserters);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(insertion_interval).await;

            let mut inserters = inserters_handle.lock().await;

            if let Err(error) = inserters.0.commit().await {
                error!("Error while committing requests: {}", error);
            }

            if let Err(error) = inserters.1.commit().await {
                error!("Error while committing logs: {}", error);
            }
        }
    });

    let inserters_handle = Arc::clone(&inserters);
    tokio::spawn(async move {
        while let Ok(log) = log_receiver.recv_async().await {
            let mut inserters = inserters_handle.lock().await;
            if let Err(error) = inserters
                .1
                .write(&LogRow {
                    function_id: log
                        .2
                        .as_ref()
                        .map_or_else(String::new, |metadata| metadata.1.clone()),
                    deployment_id: log
                        .2
                        .as_ref()
                        .map_or_else(String::new, |metadata| metadata.0.clone()),
                    level: log.0,
                    message: log.1,
                    region: get_region().clone(),
                    timestamp: UNIX_EPOCH.elapsed().unwrap().as_secs() as u32,
                })
                .await
            {
                error!("Error while writing log: {}", error);
            }
        }
    });

    let server = Server::bind(&addr).serve(make_service_fn(move |conn: &AddrStream| {
        let deployments = Arc::clone(&deployments);
        let last_requests = Arc::clone(&last_requests);
        let workers = Arc::clone(&workers);
        let inserters = Arc::clone(&inserters);
        let log_sender = log_sender.clone();

        let addr = conn.remote_addr();
        let ip = addr.ip().to_string();

        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                handle_request(
                    req,
                    ip.clone(),
                    Arc::clone(&deployments),
                    Arc::clone(&last_requests),
                    Arc::clone(&workers),
                    Arc::clone(&inserters),
                    log_sender.clone(),
                )
            }))
        }
    }));

    Ok(async move {
        if let Err(error) = server.await {
            error!("Server error: {}", error);
        }
    })
}
