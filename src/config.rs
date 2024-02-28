use std::path::Path;

use miette::{IntoDiagnostic, Result};
use oci_spec::image::Config as ExecConfig;
use secrecy::SecretString;
use serde::Deserialize;
use serde::{de::Error, Deserializer};
use std::fmt::Display;
use std::str::FromStr;

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(untagged)]
pub enum Authorization {
    UserPassword(
        #[serde(deserialize_with = "with_expand_envs")] String,
        #[serde(deserialize_with = "with_expand_envs")] SecretString,
    ),
    Token(#[serde(deserialize_with = "with_expand_envs")] SecretString),
    #[default]
    None,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BaseSource {
    #[serde(default)]
    pub auth: Authorization,
    #[serde(deserialize_with = "with_expand_envs")]
    pub registry: String,
    #[serde(deserialize_with = "with_expand_envs")]
    pub repo: String,
    #[serde(deserialize_with = "with_expand_envs")]
    pub tag: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Target {
    pub auth: Authorization,
    #[serde(deserialize_with = "with_expand_envs")]
    pub registry: String,
    #[serde(deserialize_with = "with_expand_envs")]
    pub repo: String,
    #[serde(deserialize_with = "with_expand_envs")]
    pub tag: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ImageModification {
    pub execution_config: Option<ExecConfig>,
    #[serde(deserialize_with = "with_expand_envs")]
    pub app_layer_folder: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BuilderConfig {
    pub base: BaseSource,
    pub target: Target,
    pub modification: ImageModification,
}

fn with_expand_envs<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr + serde::Deserialize<'de>,
    <T as FromStr>::Err: Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrAnything<T> {
        String(String),
        Anything(T),
    }

    match StringOrAnything::<T>::deserialize(deserializer)? {
        StringOrAnything::String(s) => match shellexpand::env(&s) {
            Ok(value) => value.parse::<T>().map_err(Error::custom),
            Err(err) => Err(Error::custom(err)),
        },
        StringOrAnything::Anything(anything) => Ok(anything),
    }
}

pub fn load_config(file: impl AsRef<Path>) -> Result<BuilderConfig> {
    toml::from_str(&std::fs::read_to_string(file).into_diagnostic()?).into_diagnostic()
}
