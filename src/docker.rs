use anyhow::{Result, anyhow};
use bollard::models::ContainerSummary;
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptions, ListImagesOptions, LogsOptions,
    RemoveContainerOptionsBuilder, RemoveImageOptions, StartContainerOptionsBuilder,
    StatsOptionsBuilder, StopContainerOptionsBuilder,
};
use bollard::secret::ImageSummary;
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::{env, path::Path};

#[cfg(unix)]
use libc::geteuid;

#[derive(Clone)]
pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    pub async fn connect_default() -> Result<Self> {
        let socket = resolve_docker_socket_path()?;
        let inner = Docker::connect_with_unix(&socket, 120, API_DEFAULT_VERSION)?;
        Ok(Self { inner })
    }

    pub async fn inspect(
        &self,
        id: &str,
    ) -> anyhow::Result<bollard::models::ContainerInspectResponse> {
        Ok(self
            .inner
            .inspect_container(id, None::<InspectContainerOptions>)
            .await?)
    }

    pub async fn pause(&self, id: &str) -> Result<()> {
        self.inner.pause_container(id).await.map_err(Into::into)
    }

    pub async fn unpause(&self, id: &str) -> Result<()> {
        self.inner.unpause_container(id).await.map_err(Into::into)
    }

    pub async fn remove(&self, id: &str, force: bool, volumes: bool) -> Result<()> {
        let opts: bollard::query_parameters::RemoveContainerOptions =
            RemoveContainerOptionsBuilder::default()
                .force(force)
                .v(volumes)
                .link(false)
                .build();
        self.inner.remove_container(id, Some(opts)).await?;
        Ok(())
    }

    pub async fn list_containers(&self, all: bool) -> Result<Vec<ContainerSummary>> {
        let filters: HashMap<String, Vec<String>> = HashMap::new();
        let opts = ListContainersOptions {
            all,
            filters: Some(filters),
            ..Default::default()
        };
        let list = self.inner.list_containers(Some(opts)).await?;
        Ok(list)
    }

    pub async fn logs_stream(
        &self,
        id: &str,
        follow: bool,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>> {
        let opts = LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            tail: "200".to_string(),
            timestamps: false,
            ..Default::default()
        };

        let s = self.inner.logs(id, Some(opts)).map(|out| match out {
            Ok(bollard::container::LogOutput::StdOut { message })
            | Ok(bollard::container::LogOutput::StdErr { message })
            | Ok(bollard::container::LogOutput::Console { message }) => {
                Ok(String::from_utf8_lossy(&message).to_string())
            }
            Ok(_) => Ok(String::new()),
            Err(e) => Err(anyhow!(e)),
        });

        Ok(Box::pin(s))
    }

    pub async fn stats_stream_live(
        &self,
        id: &str,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<Value>> + Send>>> {
        let opts = StatsOptionsBuilder::default()
            .stream(true)
            .one_shot(false)
            .build();

        let s = self.inner.stats(id, Some(opts)).map(|it| {
            it.map_err(|e| anyhow!(e))
                .and_then(|stat| serde_json::to_value(stat).map_err(|e| anyhow!(e)))
        });

        Ok(Box::pin(s))
    }

    // ----- Actions -----

    pub async fn start(&self, id: &str) -> Result<()> {
        let opts: bollard::query_parameters::StartContainerOptions =
            StartContainerOptionsBuilder::default().build();
        self.inner.start_container(id, Some(opts)).await?;
        Ok(())
    }

    pub async fn stop(&self, id: &str, timeout_secs: i64) -> Result<()> {
        let opts = StopContainerOptionsBuilder::default()
            .t(timeout_secs.try_into().unwrap())
            .build();
        self.inner.stop_container(id, Some(opts)).await?;
        Ok(())
    }

    // ================== IMAGES ==================

    pub async fn list_images(&self, all: bool) -> Result<Vec<ImageSummary>> {
        let opts = ListImagesOptions {
            all,
            ..Default::default()
        };
        let images = self.inner.list_images(Some(opts)).await?;
        Ok(images)
    }

    pub async fn inspect_image(&self, id: &str) -> Result<bollard::models::ImageInspect> {
        Ok(self.inner.inspect_image(id).await?)
    }

    pub async fn remove_image(&self, id: &str, force: bool, noprune: bool) -> Result<()> {
        let opts = RemoveImageOptions { force, noprune };
        let _ = self.inner.remove_image(id, Some(opts), None).await?;
        Ok(())
    }
}

fn resolve_docker_socket_path() -> Result<String> {
    if let Ok(host) = env::var("DOCKER_HOST") {
        if let Some(path) = host.strip_prefix("unix://") {
            if Path::new(path).exists() {
                return Ok(path.to_string());
            }
        } else if host.starts_with('/') && Path::new(&host).exists() {
            return Ok(host);
        }
    }
    #[cfg(unix)]
    {
        let uid = unsafe { geteuid() };
        let cand = format!("/run/user/{}/docker.sock", uid);
        if Path::new(&cand).exists() {
            return Ok(cand);
        }
    }

    let default = "/var/run/docker.sock";
    if Path::new(default).exists() {
        Ok(default.to_string())
    } else {
        Err(anyhow!(
            "No Docker socket found. Tried DOCKER_HOST, /run/user/$UID/docker.sock and {}",
            default
        ))
    }
}
