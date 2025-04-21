# kleinladungsträger

This project builds a docker image based on a recipe.
The recipe describes a base image and modifications to apply.

The recipe is a toml file with the following structure:

```toml
[base]
registry = "gcr.io"
repo = "distroless/cc-debian12"
tag = "latest"

[target]
registry = "git.jmteegen.eu"
repo = "max/kleinladungstraeger"
tag = "latest"
auth = "$GITEA_TOKEN"

[modification]
app_layer_folder = "target/docker"

[modification.execution_config]
Cmd = ["/klt"]
```

The `base` section describes the base image.
The `target` section describes the target image.
The `modification` section describes the modifications to apply.

The `app_layer_folder` is a path to a folder that will be added as a layer to the image.
The `execution_config` section allows patching the execution config of the image,
supported keys are:
- `Cmd`
- `User`
- `WorkingDir`
- `StopSignal`
- `Env`
- `Volumes`
- `Labels`
Values must be in the format specified by the [OCI Image Specification](https://github.com/opencontainers/image-spec/blob/c05acf7eb327dae4704a4efe01253a0e60af6b34/config.md?plain=1#L131-L209).
