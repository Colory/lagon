use super::{download_deployment, filesystem::rm_deployment, Deployment, Deployments};
use crate::{cronjob::Cronjob, get_region, serverless::Workers};
use anyhow::Result;
use futures::StreamExt;
use lagon_runtime_isolate::IsolateEvent;
use lagon_serverless_downloader::Downloader;
use lagon_serverless_pubsub::{PubSubListener, PubSubMessage, PubSubMessageKind};
use log::{error, warn};
use metrics::increment_counter;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::{runtime::Handle, sync::Mutex};

pub async fn clear_deployment_cache(deployment_id: String, workers: Workers, reason: String) {
    if let Some((_, tx)) = workers.remove(&deployment_id) {
        tx.send_async(IsolateEvent::Terminate(reason))
            .await
            .unwrap_or(());
    }
}

async fn run<D, P>(
    downloader: Arc<D>,
    deployments: Deployments,
    workers: Workers,
    cronjob: Arc<Mutex<Cronjob>>,
    pubsub: Arc<Mutex<P>>,
) -> Result<()>
where
    D: Downloader,
    P: PubSubListener,
{
    let mut pubsub = pubsub.lock().await;
    let mut stream = pubsub.get_stream()?;

    while let Some(Ok(PubSubMessage { kind, payload })) = stream.next().await {
        let value: Value = serde_json::from_str(&payload)?;

        let cron = value["cron"].as_str();
        let cron_region = value["cronRegion"].as_str().unwrap().to_string();

        // Ignore deployments that have a cron set but where
        // the region isn't this node' region, except for undeploys
        // because we might remove the cron from the old region
        if cron.is_some() && &cron_region != get_region() && kind != PubSubMessageKind::Undeploy {
            continue;
        }

        let cron = cron.map(|cron| cron.to_string());

        let deployment = Deployment {
            id: value["deploymentId"].as_str().unwrap().to_string(),
            function_id: value["functionId"].as_str().unwrap().to_string(),
            function_name: value["functionName"].as_str().unwrap().to_string(),
            assets: value["assets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect(),
            domains: value["domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect(),
            environment_variables: value["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.to_owned(), v.as_str().unwrap().to_string()))
                .collect::<HashMap<_, _>>(),
            memory: value["memory"].as_u64().unwrap() as usize,
            tick_timeout: value["tickTimeout"].as_u64().unwrap() as usize,
            total_timeout: value["totalTimeout"].as_u64().unwrap() as usize,
            is_production: value["isProduction"].as_bool().unwrap(),
            cron,
        };

        let workers = Arc::clone(&workers);

        match kind {
            PubSubMessageKind::Deploy => {
                match download_deployment(&deployment, Arc::clone(&downloader)).await {
                    Ok(_) => {
                        increment_counter!(
                            "lagon_deployments",
                            "status" => "success",
                            "deployment" => deployment.id.clone(),
                            "function" => deployment.function_id.clone(),
                        );

                        let domains = deployment.get_domains();
                        let deployment = Arc::new(deployment);

                        for domain in &domains {
                            deployments.insert(domain.clone(), Arc::clone(&deployment));
                        }

                        if deployment.should_run_cron() {
                            let mut cronjob = cronjob.lock().await;
                            let id = deployment.id.clone();

                            if let Err(error) = cronjob.add(deployment).await {
                                error!(deployment = id; "Failed to register cron: {}", error);
                            }
                        }
                    }
                    Err(error) => {
                        increment_counter!(
                            "lagon_deployments",
                            "status" => "error",
                            "deployment" => deployment.id.clone(),
                            "function" => deployment.function_id.clone(),
                        );
                        error!(
                            deployment = deployment.id;
                            "Failed to download deployment: {}", error
                        );
                    }
                };
            }
            PubSubMessageKind::Undeploy => {
                match rm_deployment(&deployment.id) {
                    Ok(_) => {
                        increment_counter!(
                            "lagon_undeployments",
                            "status" => "success",
                            "deployment" => deployment.id.clone(),
                            "function" => deployment.function_id.clone(),
                        );

                        let domains = deployment.get_domains();

                        for domain in &domains {
                            deployments.remove(domain);
                        }

                        clear_deployment_cache(
                            deployment.id.clone(),
                            workers,
                            String::from("undeployment"),
                        )
                        .await;

                        if deployment.should_run_cron() {
                            let mut cronjob = cronjob.lock().await;

                            if let Err(error) = cronjob.remove(&deployment.id).await {
                                error!(deployment = deployment.id; "Failed to remove cron: {}", error);
                            }
                        }
                    }
                    Err(error) => {
                        increment_counter!(
                            "lagon_undeployments",
                            "status" => "error",
                            "deployment" => deployment.id.clone(),
                            "function" => deployment.function_id.clone(),
                        );
                        error!(deployment = deployment.id; "Failed to delete deployment: {}", error);
                    }
                };
            }
            PubSubMessageKind::Promote => {
                increment_counter!(
                    "lagon_promotion",
                    "deployment" => deployment.id.clone(),
                    "function" => deployment.function_id.clone(),
                );

                let previous_id = value["previousDeploymentId"].as_str().unwrap();

                if let Some(deployment) = deployments.get(previous_id) {
                    let mut unpromoted_deployment = deployment.as_ref().clone();
                    unpromoted_deployment.is_production = false;

                    for domain in deployment.get_domains() {
                        deployments.remove(&domain);
                    }

                    let unpromoted_deployment = Arc::new(unpromoted_deployment);

                    for domain in unpromoted_deployment.get_domains() {
                        deployments.insert(domain, Arc::clone(&unpromoted_deployment));
                    }
                }

                let deployment = Arc::new(deployment);
                let domains = deployment.get_domains();

                for domain in &domains {
                    deployments.insert(domain.clone(), Arc::clone(&deployment));
                }

                clear_deployment_cache(previous_id.to_string(), workers, String::from("promotion"))
                    .await;

                let mut cronjob = cronjob.lock().await;

                if let Err(error) = cronjob.remove(&previous_id.to_string()).await {
                    error!(deployment = deployment.id; "Failed to remove cron: {}", error);
                }

                if deployment.should_run_cron() {
                    let id = deployment.id.clone();

                    if let Err(error) = cronjob.add(deployment).await {
                        error!(deployment = id; "Failed to register cron: {}", error);
                    }
                }
            }
            _ => warn!("Unknown message kind: {:?}, {}", kind, payload),
        };
    }

    Ok(())
}

pub fn listen_pub_sub<D, P>(
    downloader: Arc<D>,
    deployments: Deployments,
    workers: Workers,
    cronjob: Arc<Mutex<Cronjob>>,
    pubsub: Arc<Mutex<P>>,
) where
    D: Downloader + Send + Sync + 'static,
    P: PubSubListener + 'static,
{
    let handle = Handle::current();
    std::thread::spawn(move || {
        handle.block_on(async {
            loop {
                if let Err(error) = run(
                    Arc::clone(&downloader),
                    Arc::clone(&deployments),
                    Arc::clone(&workers),
                    Arc::clone(&cronjob),
                    Arc::clone(&pubsub),
                )
                .await
                {
                    error!("Pub/sub error: {}", error);
                }
            }
        });
    });
}
