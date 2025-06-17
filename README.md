# Kleinladungsträger

Kleinladungsträger (klt) builds an OCI/docker image based on a recipe.
The recipe describes a base image and modifications to apply.

The recipe is a toml file like this:

```toml
[base]
image = "gcr.io/distroless/cc-debian12:latest"

[target]
registry = "ghcr.io"
repo = "max-te/kleinladungstraeger"
tags = ["latest", "distroless"]
auth = ["max-te", "$GITHUB_TOKEN"]

[modification]
app_layer_folder = "target/docker/usr/bin"

[modification.execution_config]
Cmd = ["/klt"]

[modification.execution_config.Labels]
"org.opencontainers.image.source" = "https://github.com/max-te/kleinladungstraeger"

[modification.annotations]
"org.opencontainers.image.source" = "https://github.com/max-te/kleinladungstraeger"

```

The `base` section describes the base image.
The `target` section describes the target image.
The `modification` section describes the modifications to apply.

The `app_layer_folder` is a path to a folder that will be added as a layer to the image.
Note that klt achieves its effictiency by not doing the same thing as the `COPY` command in Dockerfiles:
It does not follow symlinks in the base image.

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

The `annotations` section allows defining annotations for the image manifest.

## Related Work

- [regclient](https://github.com/regclient/regclient)
- [oci-client](https://github.com/oras-project/rust-oci-client)
