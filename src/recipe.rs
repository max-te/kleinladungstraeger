use std::path::Path;

use miette::{Context, IntoDiagnostic, Result};
use oci_spec::image::Config as ExecConfig;
use secrecy::SecretString;
use serde::Deserialize;
use serde::{de::Error, Deserializer};
use serde_with::{serde_as, DeserializeAs};
use std::fmt::Display;
use std::str::FromStr;

#[serde_as]
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(untagged)]
pub enum Authorization {
    UserPassword(
        #[serde_as(as = "ShellExpanded")] String,
        #[serde_as(as = "ShellExpanded")] SecretString,
    ),
    Token(#[serde_as(as = "ShellExpanded")] SecretString),
    #[default]
    None,
}

#[serde_as]
#[derive(Deserialize, Debug, Clone)]
pub struct BaseSource {
    #[serde(default)]
    pub auth: Authorization,
    #[serde_as(as = "ShellExpanded")]
    pub registry: String,
    #[serde_as(as = "ShellExpanded")]
    pub repo: String,
    #[serde_as(as = "ShellExpanded")]
    pub tag: String,
}

#[serde_as]
#[derive(Deserialize, Debug, Clone)]
pub struct Target {
    pub auth: Authorization,
    #[serde_as(as = "ShellExpanded")]
    pub registry: String,
    #[serde_as(as = "ShellExpanded")]
    pub repo: String,
    #[serde_as(as = "Vec<ShellExpanded>")]
    #[serde(default)]
    tags: Vec<String>,
    #[serde_as(as = "Option<ShellExpanded>")]
    tag: Option<String>,
}

impl Target {
    pub fn tags(&self) -> Vec<String> {
        let mut nonempty_tags: Vec<String> = self
            .tags
            .iter()
            .filter(|t| !t.is_empty())
            .cloned()
            .collect();
        if let Some(tag) = &self.tag {
            nonempty_tags.push(tag.clone());
        }
        nonempty_tags
    }
}

#[serde_as]
#[derive(Deserialize, Debug, Clone)]
pub struct ImageModification {
    pub execution_config: Option<ExecConfig>,
    #[serde_as(as = "ShellExpanded")]
    pub app_layer_folder: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Recipe {
    pub base: BaseSource,
    pub target: Target,
    pub modification: ImageModification,
}

#[allow(dead_code)]
struct ShellExpanded;

impl<'de, T> DeserializeAs<'de, T> for ShellExpanded
where
    T: FromStr + serde::Deserialize<'de>,
    <T as FromStr>::Err: Display,
{
    fn deserialize_as<D>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer).map_err(Error::custom)?;
        let expanded = shellexpand::env(&s).map_err(Error::custom)?;
        expanded.parse().map_err(Error::custom)
    }
}

pub fn load_recipe(file: impl AsRef<Path>) -> Result<Recipe> {
    toml::from_str(
        &std::fs::read_to_string(file)
            .into_diagnostic()
            .context("Failed to read recipe")?,
    )
    .into_diagnostic()
    .context("Failed to parse recipe")
}
