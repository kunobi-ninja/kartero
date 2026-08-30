//! S3-compatible PUT (path-style) for the archive pass.
//!
//! R2 and MinIO speak this. The collector does not use it.

use anyhow::{Context, Result, bail};
use ring::hmac;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
const UNSIGNED_EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub struct S3Store {
    client: reqwest::Client,
    config: S3Config,
}

impl S3Store {
    pub fn new(config: S3Config) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        Ok(Self { client, config })
    }

    pub async fn put(&self, key: &str, body: &[u8]) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.put_at(key, body, now).await
    }

    async fn put_at(&self, key: &str, body: &[u8], unix_secs: u64) -> Result<()> {
        let (date, amz_date) = amz_timestamps(unix_secs);
        let payload_hash = sha256_hex(body);
        let host = host_of(&self.config.endpoint)?;
        let canonical_uri = object_uri(&self.config.bucket, key);
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "PUT\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{date}/{region}/s3/aws4_request\n{hash}",
            region = self.config.region,
            hash = sha256_hex(canonical_request.as_bytes()),
        );
        let signature = hex_encode(&signing_signature(
            &self.config.secret_access_key,
            &date,
            &self.config.region,
            string_to_sign.as_bytes(),
        ));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{date}/{region}/s3/aws4_request, SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key_id,
            region = self.config.region,
        );
        let url = format!(
            "{endpoint}{canonical_uri}",
            endpoint = self.config.endpoint.trim_end_matches('/'),
        );
        let response = self
            .client
            .put(url)
            .header("host", host)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", amz_date)
            .header("authorization", authorization)
            .body(body.to_vec())
            .send()
            .await
            .context("uploading archive object")?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        bail!("archive store rejected PUT with {status}: {body}");
    }
}

fn host_of(endpoint: &str) -> Result<String> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .with_context(|| format!("archive endpoint {endpoint} must be an http(s) URL"))?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        bail!("archive endpoint {endpoint} has no host");
    }
    Ok(host.to_string())
}

fn object_uri(bucket: &str, key: &str) -> String {
    let mut uri = format!("/{}", encode_path_segment(bucket));
    for segment in key.split('/') {
        if segment.is_empty() {
            continue;
        }
        uri.push('/');
        uri.push_str(&encode_path_segment(segment));
    }
    uri
}

fn encode_path_segment(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

fn signing_signature(secret: &str, date: &str, region: &str, string_to_sign: &[u8]) -> Vec<u8> {
    let mut key = Vec::from("AWS4");
    key.extend_from_slice(secret.as_bytes());
    let date_key = hmac_sha256(&key, date.as_bytes());
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, b"s3");
    let signing_key = hmac_sha256(&service_key, b"aws4_request");
    hmac_sha256(&signing_key, string_to_sign)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data).as_ref().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn amz_timestamps(unix_secs: u64) -> (String, String) {
    let (year, month, day, hour, minute, second) = civil_from_unix(unix_secs as i64);
    let date = format!("{year:04}{month:02}{day:02}");
    let amz = format!("{date}T{hour:02}{minute:02}{second:02}Z");
    (date, amz)
}

/// Howard Hinnant's civil-from-days, plus time of day.
fn civil_from_unix(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let z = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sha256_is_the_published_constant() {
        assert_eq!(sha256_hex(b""), UNSIGNED_EMPTY_SHA256);
    }

    #[test]
    fn unix_epoch_formats_as_amz_date() {
        let (date, amz) = amz_timestamps(0);
        assert_eq!(date, "19700101");
        assert_eq!(amz, "19700101T000000Z");
    }

    #[test]
    fn aws_example_instant_formats() {
        // 2015-08-30 12:36:00 UTC
        let (date, amz) = amz_timestamps(1_440_938_160);
        assert_eq!(date, "20150830");
        assert_eq!(amz, "20150830T123600Z");
    }

    #[test]
    fn object_uri_encodes_segments_and_keeps_slashes() {
        assert_eq!(
            object_uri("bucket", "kache/bench/bench-firefox.zip"),
            "/bucket/kache/bench/bench-firefox.zip"
        );
        assert_eq!(object_uri("b", "a b/c"), "/b/a%20b/c");
    }

    #[test]
    fn host_of_strips_scheme_and_path() {
        assert_eq!(
            host_of("https://abc.r2.cloudflarestorage.com").unwrap(),
            "abc.r2.cloudflarestorage.com"
        );
        assert_eq!(
            host_of("https://s3.amazonaws.com/ignored").unwrap(),
            "s3.amazonaws.com"
        );
        assert!(host_of("not-a-url").is_err());
    }
}
