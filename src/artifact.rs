use anyhow::{Result, bail};
use std::io::{Cursor, Read};
use zip::ZipArchive;

pub const MAX_ZIP_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_UNCOMPRESSED_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 16;
pub const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;
pub const METRICS_FILE: &str = "metrics.otlp.json";
pub const SCHEMA_VERSION_FILE: &str = "schema_version";

#[derive(Debug, Clone)]
pub struct ArtifactPayload {
    pub schema_version: u32,
    pub metrics_json: Vec<u8>,
}

pub fn open(bytes: &[u8]) -> Result<ArtifactPayload> {
    if bytes.len() > MAX_ZIP_BYTES {
        bail!(
            "artifact zip is {} bytes, over the {MAX_ZIP_BYTES} bound",
            bytes.len()
        );
    }
    let mut zip = ZipArchive::new(Cursor::new(bytes))?;
    if zip.len() > MAX_ENTRIES {
        bail!(
            "artifact has {} entries, over the {MAX_ENTRIES} bound",
            zip.len()
        );
    }

    let mut schema_version = None;
    let mut metrics_json = None;
    let mut uncompressed_total = 0u64;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
            bail!("artifact entry {name} is not a safe path");
        }
        if name.contains('/') || name.contains('\\') {
            bail!("artifact entry {name} is nested; only root files are accepted");
        }
        if entry.is_dir() {
            continue;
        }
        let size = entry.size();
        uncompressed_total = uncompressed_total.saturating_add(size);
        if uncompressed_total > MAX_UNCOMPRESSED_BYTES {
            bail!("artifact uncompressed size exceeds {MAX_UNCOMPRESSED_BYTES} bytes");
        }
        if name == SCHEMA_VERSION_FILE {
            if schema_version.is_some() {
                bail!("artifact has duplicate {SCHEMA_VERSION_FILE}");
            }
            if size > 32 {
                bail!("{SCHEMA_VERSION_FILE} is unexpectedly large");
            }
            let mut buf = String::new();
            entry.read_to_string(&mut buf)?;
            schema_version = Some(buf.trim().parse::<u32>()?);
            continue;
        }
        if name == METRICS_FILE {
            if metrics_json.is_some() {
                bail!("artifact has duplicate {METRICS_FILE}");
            }
            if size > MAX_JSON_BYTES as u64 {
                bail!("{METRICS_FILE} is {size} bytes, over the {MAX_JSON_BYTES} bound");
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            metrics_json = Some(buf);
            continue;
        }
        // Human-readable bench summary may ride along. Ignore anything else
        // at the root rather than treating extra files as fatal.
    }

    let Some(metrics_json) = metrics_json else {
        bail!("artifact has no {METRICS_FILE}");
    };
    let Some(schema_version) = schema_version else {
        bail!("artifact has no {SCHEMA_VERSION_FILE}");
    };
    Ok(ArtifactPayload {
        schema_version,
        metrics_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            for (name, body) in files {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn accepts_root_metrics_and_version() {
        let bytes = zip_of(&[
            (SCHEMA_VERSION_FILE, b"1\n"),
            (METRICS_FILE, b"{\"resourceMetrics\":[]}\n"),
        ]);
        let payload = open(&bytes).unwrap();
        assert_eq!(payload.schema_version, 1);
        assert!(payload.metrics_json.starts_with(b"{"));
    }

    #[test]
    fn rejects_path_traversal() {
        let bytes = zip_of(&[("../metrics.otlp.json", b"{}")]);
        assert!(open(&bytes).unwrap_err().to_string().contains("safe path"));
    }

    #[test]
    fn rejects_nested_paths() {
        let bytes = zip_of(&[("dir/metrics.otlp.json", b"{}")]);
        assert!(open(&bytes).unwrap_err().to_string().contains("nested"));
    }

    #[test]
    fn rejects_missing_schema_sidecar() {
        let missing = zip_of(&[(METRICS_FILE, b"{}")]);
        assert!(
            open(&missing)
                .unwrap_err()
                .to_string()
                .contains(SCHEMA_VERSION_FILE)
        );
    }
}
