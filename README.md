# Rocket

Rocket is a Rust based CLI tool for managing Docker containers which compiles packages for our OS.
The name "Rocket" is inspired by the whole Universe System, where each planet is a package and the rocket is the tool that helps us manage them. Gotta keep the astronauts happy 😏

## Logic

Rocket uses a simple logic to manage building of packages:

1. All packages are stored in a `packages` directory. We aim to have a separate git repository for packages in future, so that updating the packages is easier.
2. Each package has a `Dockerfile` which is used to build the package. This Dockerfile includes the necessary steps to build the package and install it in the container.
3. It also includes various "labels" which are used to identify the package, such as `name`, `version`, and `description`. Notably, it requires `ship` label which links to the built archive of the package, which will be pulled from the planet (package container).
4. The archives will be stored in a `ship` directory, which is used to store the built packages. You can use the env variable `ROCKET_SHIP_DIR` to change the location of the ship directory.
5. By default, Rocket will try to build all packages in the `packages` directory. You can use the `--package` flag to build a specific package.
