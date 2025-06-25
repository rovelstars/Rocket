use bollard::Docker;
use bollard::errors::Error;

pub async fn get_docker_connection() -> Result<Docker, Error> {
  // Connect to the local Docker daemon via Unix socket
  let docker = Docker::connect_with_local_defaults()?;
  Ok(docker)
}
