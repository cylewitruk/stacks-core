use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, HealthConfig, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
    StartContainerOptionsBuilder, StopContainerOptions, WaitContainerOptions,
};
use futures::StreamExt as _;
use stacks_bench::paths::AppDataDir;

use crate::cli::common::{CliContext, ExecCommand};

const METABASE_IMAGE_TAG: &str = "v0.58.1.3";

#[derive(clap::Args, Debug)]
pub struct MetabaseArgs {
    /// Port to expose Metabase on (default: 3000)
    #[arg(long, default_value = "3000")]
    pub port: u16,

    /// Metabase Docker image tag to use (defaults to [`METABASE_IMAGE_TAG`]).
    #[arg(long, default_value = METABASE_IMAGE_TAG)]
    pub image_tag: String,
}

impl ExecCommand for MetabaseArgs {
    type Output = serde_json::Value;

    async fn exec(&self, ctx: &CliContext) -> anyhow::Result<Self::Output> {
        self.run(ctx).await?;
        Ok(serde_json::json!({
            "message": "Metabase stopped",
            "port": self.port,
        }))
    }
}

impl MetabaseArgs {
    async fn run(&self, ctx: &CliContext) -> anyhow::Result<()> {
        let interactive = ctx.interactive();
        let app_data = ctx.app_data_dir();
        let db_path = app_data.app_db_path();

        if !db_path.exists() {
            anyhow::bail!(
                "Database not found at {:?}. Run a benchmark first.",
                db_path
            );
        }

        let pg_data_dir = app_data.postgres_data_dir();

        if interactive {
            let status = if pg_data_dir.exists()
                && pg_data_dir
                    .read_dir()
                    .map(|mut i| i.next().is_some())
                    .unwrap_or(false)
            {
                "Loading existing dashboards/users (Postgres)"
            } else {
                "Initializing new configuration (Postgres)"
            };

            eprintln!("Starting Metabase on http://localhost:{}", self.port);
            eprintln!("  - App DB:      {:?}", db_path);
            eprintln!("  - Postgres DB: {:?}", pg_data_dir);
            eprintln!("  - Status:      {status}");
        }

        run_metabase_container(app_data, self.port, self.image_tag.clone(), interactive).await
    }
}

async fn run_metabase_container(
    app_data: &AppDataDir,
    port: u16,
    image_tag: String,
    interactive: bool,
) -> Result<()> {
    let pg_data_dir = app_data.postgres_data_dir();
    let app_db_dir = app_data.app_db_dir();

    let docker =
        Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;

    let mb_image = format!("metabase/metabase:{image_tag}");
    let pg_image = "postgres:18.1-alpine3.22";

    let net_name = "stacks-bench-net";
    let pg_container_name = "stacks-bench-postgres";
    let mb_container_name = "stacks-bench-metabase";

    // 1. Check/Pull images
    for image in [&mb_image, pg_image] {
        if docker.inspect_image(image).await.is_err() {
            if interactive {
                eprintln!("Image {} not found locally. Pulling...", image);
            }
            let mut pull_stream = docker.create_image(
                Some(
                    CreateImageOptionsBuilder::default()
                        .from_image(image)
                        .build(),
                ),
                None,
                None,
            );

            while let Some(msg) = pull_stream.next().await {
                match msg {
                    Ok(_) => {}
                    Err(e) => {
                        bail!(
                            "Docker pull failed: {e}. \nHint: This is usually a Docker Desktop issue. Try restarting Docker or running 'docker system prune'."
                        );
                    }
                }
            }
        } else {
            if interactive {
                eprintln!("Image {} found locally. Skipping pull.", image);
            }
        }
    }

    // 2. Cleanup existing resources (containers & network)
    // We ignore errors here (e.g. if they don't exist)
    let _ = docker
        .remove_container(
            mb_container_name,
            Some(RemoveContainerOptionsBuilder::new().force(true).build()),
        )
        .await;
    let _ = docker
        .remove_container(
            pg_container_name,
            Some(RemoveContainerOptionsBuilder::new().force(true).build()),
        )
        .await;
    let _ = docker.remove_network(net_name).await;

    // 3. Create Network
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: net_name.to_string(),
            ..Default::default()
        })
        .await
        .context("Failed to create docker network")?;

    // 4. Start Postgres
    std::fs::create_dir_all(&pg_data_dir).context("Failed to create postgres data dir")?;

    let pg_host_config = HostConfig {
        binds: Some(vec![format!(
            "{}:/var/lib/postgresql/data",
            pg_data_dir.to_string_lossy()
        )]),
        network_mode: Some(net_name.to_string()),
        auto_remove: Some(false),
        ..Default::default()
    };

    let pg_config = ContainerCreateBody {
        image: Some(pg_image.to_string()),
        env: Some(vec![
            "POSTGRES_DB=metabase".to_string(),
            "POSTGRES_USER=metabase".to_string(),
            "POSTGRES_PASSWORD=metabase".to_string(),
        ]),
        host_config: Some(pg_host_config),
        healthcheck: Some(HealthConfig {
            test: Some(vec![
                "CMD-SHELL".to_string(),
                "pg_isready -U metabase".to_string(),
            ]),
            start_interval: Some(0),      // Start immediately
            interval: Some(100_000_000),  // 100ms
            timeout: Some(5_000_000_000), // 5s
            retries: Some(100),
            start_period: Some(2_000_000_000), // 2s grace
        }),
        ..Default::default()
    };

    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(pg_container_name)
                    .build(),
            ),
            pg_config,
        )
        .await
        .context("Failed to create Postgres container")?;

    docker
        .start_container(
            pg_container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await
        .context("Failed to start Postgres container")?;

    if interactive {
        eprintln!("Postgres started. Waiting for readiness...");
    }

    // Wait loop for Postgres Health
    let start_wait = Instant::now();
    loop {
        if start_wait.elapsed() > Duration::from_secs(30) {
            bail!("Timeout waiting for Postgres to become ready.");
        }

        let inspect = docker
            .inspect_container(
                pg_container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;

        if let Some(state) = inspect.state {
            // Fail fast if container died
            if state.running == Some(false) {
                bail!(
                    "Postgres container stopped unexpectedly. Check logs with: docker logs {}",
                    pg_container_name
                );
            }

            if let Some(health) = state.health
                && health.status == Some(bollard::models::HealthStatusEnum::HEALTHY)
            {
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if interactive {
        eprintln!("Postgres is ready.");
    }

    // 5. Start Metabase
    let mb_host_config = HostConfig {
        binds: Some(vec![format!("{}:/data", app_db_dir.to_string_lossy())]),
        port_bindings: Some(HashMap::from([(
            "3000/tcp".to_string(),
            Some(vec![PortBinding {
                host_port: Some(port.to_string()),
                ..Default::default()
            }]),
        )])),
        network_mode: Some(net_name.to_string()),
        auto_remove: Some(false),
        ..Default::default()
    };

    let mb_config = ContainerCreateBody {
        image: Some(mb_image.to_string()),
        env: Some(vec![
            "MB_DB_TYPE=postgres".to_string(),
            "MB_DB_DBNAME=metabase".to_string(),
            "MB_DB_PORT=5432".to_string(),
            "MB_DB_USER=metabase".to_string(),
            "MB_DB_PASS=metabase".to_string(),
            "MB_DB_HOST=stacks-bench-postgres".to_string(),
        ]),
        host_config: Some(mb_host_config),
        ..Default::default()
    };

    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::default()
                    .name(mb_container_name)
                    .build(),
            ),
            mb_config,
        )
        .await
        .context("Failed to create Metabase container")?;

    docker
        .start_container(
            mb_container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await
        .context("Failed to start Metabase container")?;

    if interactive {
        eprintln!("\nMetabase is running (backed by Postgres).");
        eprintln!("Open http://localhost:{} in your browser.", port);
        eprintln!("Press Ctrl-C to stop.");
    }

    // 6. Wait for Ctrl-C or Container Exit
    let wait_stream = docker.wait_container(mb_container_name, None::<WaitContainerOptions>);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            if interactive { eprintln!("\nStopping containers..."); }
            let _ = docker.stop_container(mb_container_name, None::<StopContainerOptions>).await;
            let _ = docker.stop_container(pg_container_name, None::<StopContainerOptions>).await;
            if interactive { eprintln!("Stopped."); }
        }
        _ = async {
            wait_stream.collect::<Vec<_>>().await
        } => {
            if interactive { eprintln!("\nMetabase container exited unexpectedly."); }
            // Ensure postgres is stopped too
            let _ = docker.stop_container(pg_container_name, None::<StopContainerOptions>).await;
        }
    }

    Ok(())
}
