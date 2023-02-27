use schemars::JsonSchema;
/// mirrord file operations support 2 modes of configuration:
///
/// 1. [`FsUserConfig::Simple`]: controls only the option for enabling read-only, read-write,
/// or disable file operations;
///
/// 2. [`FsUserConfig::Advanced`]: All of the above, plus allows setting up
/// [`mirrord_layer::file::filter::FileFilter`] to control which files should be opened
/// locally or remotely.
use serde::Deserialize;

pub use self::{advanced::*, mode::*};
use crate::{
    config::{from_env::FromEnv, source::MirrordConfigSource, ConfigError, MirrordConfig},
    util::MirrordToggleableConfig,
};

pub mod advanced;
pub mod mode;

/// Changes file operations behavior based on user configuration.
///
/// Defaults to [`FsUserConfig::Simple`], with [`FsModeConfig::Read`].
///
/// See the file operations [reference](https://mirrord.dev/docs/reference/fileops/)
/// for more details.
///
/// ## Examples
///
/// - Read-write file operations:
///
/// ```toml
/// # mirrord-config.toml
///
/// [feature]
/// fs = "write"
/// ```
/// - Read `/lib` locally, `/etc` remotely and `/var/run` read write remotely. Rest local
///
/// ```yaml
/// # mirrord-config.yaml
///
/// [fs]
/// mode = read
/// read_write = ["/var/run"]
/// read_only = ["/etc"]
/// local = ["/lib"]
/// ```
#[derive(Deserialize, PartialEq, Eq, Clone, Debug, JsonSchema)]
#[serde(untagged, rename_all = "lowercase")]
pub enum FsUserConfig {
    /// Basic configuration that controls the env vars `MIRRORD_FILE_OPS` and `MIRRORD_FILE_RO_OPS`
    /// (default).
    Simple(FsModeConfig),

    /// Allows the user to specify both [`FsModeConfig`] (as above), and configuration for the
    /// overrides.
    Advanced(AdvancedFsUserConfig),
}

impl Default for FsUserConfig {
    fn default() -> Self {
        FsUserConfig::Simple(FsModeConfig::Read)
    }
}

impl MirrordConfig for FsUserConfig {
    type Generated = FsConfig;

    fn generate_config(self) -> Result<Self::Generated, ConfigError> {
        let config = match self {
            FsUserConfig::Simple(mode) => FsConfig {
                mode: mode.generate_config()?,
                read_write: FromEnv::new("MIRRORD_FILE_READ_WRITE_PATTERN")
                    .source_value()
                    .transpose()?,
                read_only: FromEnv::new("MIRRORD_FILE_READ_ONLY_PATTERN")
                    .source_value()
                    .transpose()?,
                local: FromEnv::new("MIRRORD_FILE_LOCAL_PATTERN")
                    .source_value()
                    .transpose()?,
            },
            FsUserConfig::Advanced(advanced) => advanced.generate_config()?,
        };

        Ok(config)
    }
}

impl MirrordToggleableConfig for FsUserConfig {
    fn disabled_config() -> Result<Self::Generated, ConfigError> {
        let mode = FsModeConfig::disabled_config()?;
        let read_write = FromEnv::new("MIRRORD_FILE_READ_WRITE_PATTERN")
            .source_value()
            .transpose()?;
        let read_only = FromEnv::new("MIRRORD_FILE_READ_ONLY_PATTERN")
            .source_value()
            .transpose()?;
        let local = FromEnv::new("MIRRORD_FILE_LOCAL_PATTERN")
            .source_value()
            .transpose()?;

        Ok(FsConfig {
            mode,
            read_write,
            read_only,
            local,
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::config::MirrordConfig;

    #[rstest]
    fn fs_config_default() {
        let expect = FsConfig {
            mode: FsModeConfig::Read,
            ..Default::default()
        };

        let fs_config = FsUserConfig::default().generate_config().unwrap();

        assert_eq!(fs_config, expect);
    }
}
