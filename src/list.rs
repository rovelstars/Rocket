use bollard::query_parameters::{ListContainersOptionsBuilder};

pub async fn list_package_containers(
    docker: &bollard::Docker,
    package_name: &str,
) -> Result<Vec<String>, String> {
    let filters = {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "name".to_string(),
            vec![format!("{}", package_name)],
        );
        map
    };
    println!("[list_package_containers] filters: {:?}", filters);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();

    let containers_vec = docker
        .list_containers(Some(options))
        .await
        .map_err(|e| format!("Failed to list containers: {}", e))?;

    let mut containers = Vec::new();
    for container in containers_vec {
        if let Some(names) = &container.names {
            if let Some(name) = names.first() {
                // if let Some(version) = version {
                //     if name.contains(version) {
                //         containers.push(name.clone());
                //     }
                // } else {
                    containers.push(name.clone());
                //}
            }
        }
    }

    Ok(containers)
}
