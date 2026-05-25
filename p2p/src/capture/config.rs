//! Capture config: `[debug.p2p_capture]` deserialization + validation.
//!
//! The activation switch is `enabled = true`. When set, `path` becomes
//! mandatory — there is no built-in default because picking the wrong
//! filesystem (tmpfs, full system partition) is an operational footgun
//! the operator must consciously avoid.

use serde::Deserialize;
use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    #[serde(default = "default_size_mb")]
    pub size_mb: u64,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub include_ips: Vec<IpAddr>,
    #[serde(default)]
    pub exclude_ips: Vec<IpAddr>,
}

fn default_size_mb() -> u64 {
    1024
}
fn default_sync_interval() -> u64 {
    60
}

/// Resolved + validated config. Hot-path-friendly types.
#[derive(Debug, Clone)]
pub struct ResolvedCaptureConfig {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sync_interval_secs: u64,
    pub filter: FilterMode,
}

#[derive(Debug, Clone)]
pub enum FilterMode {
    None,
    Include(HashSet<IpAddr>),
    Exclude(HashSet<IpAddr>),
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureConfigError {
    #[error("p2p_capture.path is required when p2p_capture.enabled = true")]
    MissingPath,
    #[error("p2p_capture.path parent directory does not exist: {0}")]
    ParentDirMissing(PathBuf),
    #[error(
        "p2p_capture.include_ips and exclude_ips are mutually exclusive; set one or the other"
    )]
    FiltersMutuallyExclusive,
    #[error("p2p_capture.size_mb must be >= 1 (got {0})")]
    SizeTooSmall(u64),
}

impl CaptureConfig {
    /// Validate and convert to ResolvedCaptureConfig.
    /// Returns Ok(None) if `enabled = false`.
    pub fn resolve(self) -> Result<Option<ResolvedCaptureConfig>, CaptureConfigError> {
        if !self.enabled {
            return Ok(None);
        }
        let path = self.path.ok_or(CaptureConfigError::MissingPath)?;
        let parent = path
            .parent()
            .ok_or_else(|| CaptureConfigError::ParentDirMissing(path.clone()))?;
        // A path like "cap.ring" has parent = Some("") — treat the empty parent
        // as "current directory" (which exists). Real footgun is a non-empty
        // parent that doesn't exist.
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(CaptureConfigError::ParentDirMissing(parent.to_path_buf()));
        }
        if !self.include_ips.is_empty() && !self.exclude_ips.is_empty() {
            return Err(CaptureConfigError::FiltersMutuallyExclusive);
        }
        if self.size_mb == 0 {
            return Err(CaptureConfigError::SizeTooSmall(0));
        }
        let filter = if !self.include_ips.is_empty() {
            FilterMode::Include(self.include_ips.into_iter().collect())
        } else if !self.exclude_ips.is_empty() {
            FilterMode::Exclude(self.exclude_ips.into_iter().collect())
        } else {
            FilterMode::None
        };
        Ok(Some(ResolvedCaptureConfig {
            path,
            size_bytes: self.size_mb * 1024 * 1024,
            sync_interval_secs: self.sync_interval_secs,
            filter,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn disabled_returns_none() {
        let c = CaptureConfig {
            enabled: false,
            path: None,
            size_mb: 0,
            sync_interval_secs: 0,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(c.resolve().unwrap().is_none());
    }

    #[test]
    fn enabled_without_path_errors() {
        let c = CaptureConfig {
            enabled: true,
            path: None,
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(matches!(c.resolve(), Err(CaptureConfigError::MissingPath)));
    }

    #[test]
    fn missing_parent_dir_errors() {
        let c = CaptureConfig {
            enabled: true,
            path: Some(PathBuf::from("/nonexistent_dir_xyz/cap.ring")),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(matches!(
            c.resolve(),
            Err(CaptureConfigError::ParentDirMissing(_))
        ));
    }

    #[test]
    fn mutually_exclusive_filters_errors() {
        let dir = tempfile::tempdir().unwrap();
        let c = CaptureConfig {
            enabled: true,
            path: Some(dir.path().join("cap.ring")),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![IpAddr::from_str("1.2.3.4").unwrap()],
            exclude_ips: vec![IpAddr::from_str("5.6.7.8").unwrap()],
        };
        assert!(matches!(
            c.resolve(),
            Err(CaptureConfigError::FiltersMutuallyExclusive)
        ));
    }

    #[test]
    fn enabled_with_valid_path_resolves_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap.ring");
        let c = CaptureConfig {
            enabled: true,
            path: Some(path.clone()),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        let r = c.resolve().unwrap().unwrap();
        assert_eq!(r.path, path);
        assert_eq!(r.size_bytes, 1024 * 1024 * 1024);
        assert!(matches!(r.filter, FilterMode::None));
    }

    #[test]
    fn include_only_resolves_to_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        let c = CaptureConfig {
            enabled: true,
            path: Some(dir.path().join("cap.ring")),
            size_mb: 1024,
            sync_interval_secs: 60,
            include_ips: vec![IpAddr::from_str("1.2.3.4").unwrap()],
            exclude_ips: vec![],
        };
        let r = c.resolve().unwrap().unwrap();
        assert!(matches!(r.filter, FilterMode::Include(_)));
    }

    #[test]
    fn size_zero_errors() {
        let dir = tempfile::tempdir().unwrap();
        let c = CaptureConfig {
            enabled: true,
            path: Some(dir.path().join("cap.ring")),
            size_mb: 0,
            sync_interval_secs: 60,
            include_ips: vec![],
            exclude_ips: vec![],
        };
        assert!(matches!(
            c.resolve(),
            Err(CaptureConfigError::SizeTooSmall(0))
        ));
    }
}
