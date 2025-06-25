use bollard::container::ListContainersOptions;
use futures_util::stream::TryStreamExt;

pub async fn list_containers() -> Result<Vec<bollard::container::APIContainers>, Error> {
  let docker = get_docker_connection().await?;
  let containers = docker
    .list_containers(Some(ListContainersOptions::<String> {
      all: true,
      ..Default::default()
    }))
    .await?;
  Ok(containers)
}