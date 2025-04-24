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
    #[serde(default)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::env;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_target_tags() {
        let target = Target {
            auth: Authorization::None,
            registry: "registry".to_string(),
            repo: "repo".to_string(),
            tags: vec!["tag1".to_string(), "".to_string(), "tag2".to_string()],
            tag: Some("tag3".to_string()),
        };

        let tags = target.tags();
        assert_eq!(tags, vec!["tag1", "tag2", "tag3"]);
    }

    #[test]
    fn test_shell_expanded_deserialize() {
        env::set_var("TEST_VAR", "test_value");

        let toml_content = r#"
            [base]
            registry = "$TEST_VAR/registry"
            repo = "repo"
            tag = "tag"

            [target]
            registry = "registry"
            repo = "repo"
            tag = "tag"

            [modification]
            app_layer_folder = "folder"
        "#;

        let recipe: Recipe = toml::from_str(toml_content).unwrap();
        assert_eq!(recipe.base.registry, "test_value/registry");
    }

    #[test]
    fn test_load_recipe() -> miette::Result<()> {
        let mut file = NamedTempFile::new().unwrap();
        let content = r#"
            [base]
            registry = "registry"
            repo = "repo"
            tag = "tag"

            [target]
            registry = "registry"
            repo = "repo"
            tag = "tag"

            [modification]
            app_layer_folder = "folder"

            [modification.execution_config]
            Cmd = ["sh", "-c"]
        "#;
        file.write_all(content.as_bytes()).unwrap();

        let recipe = load_recipe(file.path())?;
        assert_eq!(recipe.base.registry, "registry");
        assert_eq!(recipe.target.repo, "repo");
        assert_eq!(recipe.modification.app_layer_folder, "folder");
        assert_eq!(
            recipe
                .modification
                .execution_config
                .unwrap()
                .cmd()
                .as_ref()
                .unwrap()
                .as_ref(),
            vec!["sh", "-c"]
        );
        Ok(())
    }

    #[test]
    fn test_authorization_deserialization() {
        let toml_content = r#"
            auth1 = ["user", "pass"]
            auth2 = "token"
            # auth3 unset
        "#;

        #[derive(Deserialize, Debug)]
        struct TestAuth {
            #[serde(default)]
            auth1: Authorization,
            #[serde(default)]
            auth2: Authorization,
            #[serde(default)]
            auth3: Authorization,
        }

        let auths: TestAuth = toml::from_str(toml_content).unwrap();

        if let Authorization::UserPassword(user, pass) = &auths.auth1 {
            assert_eq!(user, "user");
            assert!(pass.expose_secret() == "pass");
        } else {
            panic!("Expected UserPassword");
        }

        if let Authorization::Token(token) = &auths.auth2 {
            assert!(token.expose_secret() == "token");
        } else {
            panic!("Expected Token");
        }

        assert!(matches!(auths.auth3, Authorization::None));
    }
}
