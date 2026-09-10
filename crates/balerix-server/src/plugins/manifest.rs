//! Reading and validating a package's manifest (plugins spec §2).

use std::path::Path;

use balerix_api::PluginManifest;
use balerix_core::validate_manifest;

use super::PluginError;

pub const MANIFEST_FILE: &str = "balerix-plugin.yaml";
pub const MISE_FILE: &str = "mise.toml";

fn read(package: &Path, file: &'static str) -> Result<String, PluginError> {
    match std::fs::read_to_string(package.join(file)) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(match file {
            MANIFEST_FILE => PluginError::ManifestParse("missing".into()),
            _ => PluginError::Manifest(balerix_core::ManifestError::MiseToml {
                path: String::new(),
                message: "missing".into(),
            }),
        }),
        Err(e) => Err(PluginError::io(&package.join(file), e)),
    }
}

/// Parses `balerix-plugin.yaml`, reads `mise.toml`, applies the spec §2
/// rules through `balerix_core::validate_manifest`.
pub fn read_manifest(package: &Path) -> Result<PluginManifest, PluginError> {
    let text = read(package, MANIFEST_FILE)?;
    let manifest: PluginManifest =
        serde_norway::from_str(&text).map_err(|e| PluginError::ManifestParse(e.to_string()))?;
    let mise = read(package, MISE_FILE)?;
    validate_manifest(&manifest, &mise)?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "apiVersion: balerix/v1\nkind: Plugin\nname: web\nversion: 0.1.0\nprotocol: 1\nstart: serve\n";
    const MISE: &str = "[tools]\n[tasks.serve]\nrun = \"x\"\n";

    fn package(manifest: &str, mise: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("balerix-plugin.yaml"), manifest).unwrap();
        if let Some(m) = mise {
            std::fs::write(dir.path().join("mise.toml"), m).unwrap();
        }
        dir
    }

    #[test]
    fn reads_a_valid_package() {
        let dir = package(MANIFEST, Some(MISE));
        let m = read_manifest(dir.path()).unwrap();
        assert_eq!(m.name, "web");
        assert_eq!(m.start, "serve");
    }

    #[test]
    fn missing_or_malformed_files_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_manifest(dir.path()).unwrap_err().to_string(),
            "balerix-plugin.yaml: missing"
        );
        let dir = package(MANIFEST, None);
        assert_eq!(
            read_manifest(dir.path()).unwrap_err().to_string(),
            "mise.toml: missing"
        );
        let dir = package("apiVersion: [\n", Some(MISE));
        let e = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(e.starts_with("balerix-plugin.yaml: "), "{e}");
        let dir = package(&MANIFEST.replace("protocol: 1", "protocol: 9"), Some(MISE));
        assert_eq!(
            read_manifest(dir.path()).unwrap_err().to_string(),
            "balerix-plugin.yaml: protocol: this daemon speaks protocol 1, got 9"
        );
    }
}
