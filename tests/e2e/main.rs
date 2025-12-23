use std::fs;
use tempfile::TempDir;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};
use tokio::process::Command;

#[tokio::test]
async fn simple_build_to_registry() -> Result<(), Box<dyn std::error::Error>> {
    // Create temporary directory for certificates
    let cert_dir = TempDir::new()?;
    let cert_path = cert_dir.path().join("cert.pem");
    let key_path = cert_dir.path().join("key.pem");

    // Generate self-signed certificate
    let subject_alt_names = vec!["localhost".to_string()];
    let certified_key = rcgen::generate_simple_self_signed(subject_alt_names)?;

    fs::write(&cert_path, certified_key.cert.pem())?;
    fs::write(&key_path, certified_key.key_pair.serialize_pem())?;

    // Start registry container with HTTPS and mounted certificates
    let container = GenericImage::new("registry", "3")
        .with_exposed_port(5000.tcp())
        .with_wait_for(WaitFor::message_on_stderr("listening on"))
        .with_mount(Mount::bind_mount(
            cert_dir.path().to_str().unwrap(),
            "/certs",
        ))
        .with_env_var("REGISTRY_HTTP_TLS_CERTIFICATE", "/certs/cert.pem")
        .with_env_var("REGISTRY_HTTP_TLS_KEY", "/certs/key.pem")
        .start()
        .await
        .unwrap();
    let host = container.get_host().await?;
    let host_port = container.get_host_port_ipv4(5000).await?;
    let url = format!("{host}:{host_port}");

    let result = Command::new(env!("CARGO_BIN_EXE_klt"))
        .arg("tests/e2e/simple.toml")
        .env("REGISTRY", &url)
        // SSL_CERT_FILE is a convention for specifying the path to a CA root certificate
        // rustls picks it up automatically via openssl-probe
        .env("SSL_CERT_FILE", cert_path.to_str().unwrap())
        .spawn()?
        .wait_with_output()
        .await?;
    assert!(result.status.success());

    Ok(())
}
